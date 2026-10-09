//! Scripted runs for testing and screenshots without a human at the keyboard.
//!
//! `client --scenario scenarios/animation/viewmodel.ron` loads the level once, waits until it has
//! streamed in and every shader is compiled, then runs the steps in order: place the
//! camera, hold buttons, toggle render features, take screenshots, measure frame times.
//! Screenshots and `report.txt` go to `--out` (default `target/scenarios/<name>/`).
//! `--screenshot <path>` is shorthand for "wait until ready, take one screenshot, quit".
//! With `menu: true` the client starts at the main menu, to script it with `Click` steps.
//! Scenarios use the default settings (see `settings`) unless `--settings` is given.
//!
//! `Measure` reports the frame times (average, p50, p95, max); with `--diagnostics` also each
//! render pass's GPU time, and with `BF2_PROFILE_FRAMES=1` in a build with
//! `--features game_server/profile` every system's time per frame.
//!
//! `ExpectLog` and `ForbidLog` steps turn a scenario into a check: they watch the log (captured
//! only while a scenario runs) and fail the run with exit code 1. Every run writes
//! `result.txt` (`PASS`, or `FAIL: <reason>`) next to its screenshots. `ExpectLog` doubles as a
//! condition wait: it returns as soon as its text is logged, so a step that just needs to know
//! something happened (a spawn, a pickup, a round change) can use it in place of a guessed
//! `Wait`, as long as there is a log line to watch for.
//!
//! A scripted run's own local server (singleplayer or a listen server; not `--connect`) uses a
//! fast deploy: death to respawn takes a fraction of a second instead of the real game's 10 s
//! (`main::FAST_DEPLOY_SECONDS`), so `WaitSpawned`/`WaitDeployScreen` below return in about a
//! frame. A scenario that checks the deploy countdown itself (its text, or a screenshot timed
//! from it) sets the `respawn_time` field to keep the real timing, e.g. `respawn_time: Some(10.0)`.
//! `WaitSpawned`, `WaitDeployScreen`, `WaitInVehicle` and `WaitBotsDeployed` replace a fixed
//! `Wait` guessed to outlast a respawn, a death, boarding a vehicle or bots spawning in: they
//! return as soon as the condition holds and fail the scenario (like `ExpectLog`) if it doesn't
//! within their timeout, so they're never slower than the old guess and usually much faster.
//!
//! ```ron
//! (
//!     level: "strike_at_karkand",
//!     bots: 0,
//!     steps: [
//!         WaitReady,
//!         Teleport((-184.0, 155.6, -80.0), 20.0, -5.0),
//!         Wait(1.0),
//!         Screenshot("hip"),
//!         Hold([Aim]),
//!         Wait(0.5),
//!         Screenshot("zoomed"),
//!         Measure("zoomed", 3.0),
//!         Quit,
//!     ],
//! )
//! ```

use std::{
    fmt::Write as _,
    path::{Path, PathBuf},
    sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    },
};

use bevy::{
    gltf::Gltf,
    input::{
        ButtonState,
        gamepad::{
            GamepadButton, GamepadConnection, GamepadConnectionEvent, RawGamepadAxisChangedEvent,
            RawGamepadButtonChangedEvent, RawGamepadEvent,
        },
        keyboard::{Key, KeyboardInput, NativeKey, NativeKeyCode},
    },
    window::PrimaryWindow,
    pbr::ScreenSpaceAmbientOcclusion,
    platform::collections::HashSet,
    prelude::*,
    render::{
        Render, RenderApp, RenderSystems,
        render_resource::PipelineCache,
        view::screenshot::{Screenshot, ScreenshotCaptured, save_to_disk},
    },
};
use game_data::JointInput;
use game_shared::{
    input::Buttons,
    level::LoadedLevel,
    protocol::{ControlledBy, Team},
    revive::Downed,
    soldier::{Health, Soldier, SoldierMotion},
    vehicle::{Seated, Vehicle, VehicleData, VehicleHealth},
};
use game_server::embedded::{crossbeam_channel::Receiver, link_player, link_soldier};
use serde::Deserialize;

use crate::{
    Cli,
    camera::{PlayerCamera, Spectator, ThirdPerson},
    combat::WeaponSelection,
    deploy::DeployScreen,
    local_input::LookState,
    menu::Screen,
    net::{ActiveMatch, LocalPlayer, LocalSoldier},
    render::environment::Sun,
    vehicles::{SeatRequest, VehicleView},
};

/// A scenario file. Fields other than `steps` override the command line.
#[derive(Deserialize, Debug, Clone, Default)]
#[serde(default)]
pub struct Scenario {
    pub level: Option<String>,
    pub mode: Option<String>,
    pub bots: Option<u32>,
    pub spectate: Option<bool>,
    pub third_person: Option<bool>,
    /// Our team, 1 or 2.
    pub team: Option<u8>,
    /// Start at the main menu instead of in a match (the other fields then don't apply).
    pub menu: bool,
    /// Layout size (16, 32, 64).
    pub size: Option<u32>,
    /// Seconds between death and respawn on this run's own local server (default: a fast
    /// deploy, see `main::FAST_DEPLOY_SECONDS`). Set this (e.g. `respawn_time: Some(10.0)`)
    /// for a scenario that checks the deploy countdown itself, its text or a screenshot timed
    /// from it; other scenarios should wait on `WaitSpawned` instead of a fixed `Wait`.
    pub respawn_time: Option<f32>,
    pub steps: Vec<Step>,
}

#[derive(Deserialize, Debug, Clone)]
pub enum Step {
    /// Until the level is loaded, assets stopped streaming in, no shader is compiling and
    /// (unless spectating) our soldier exists; at the main menu, until assets and shaders
    /// are done. Gives up after 90 s.
    WaitReady,
    /// Real-time seconds.
    Wait(f32),
    Frames(u32),
    /// Spectator camera position and look direction (yaw, pitch in degrees).
    Camera((f32, f32, f32), f32, f32),
    /// Moves our soldier (feet position) and sets the look direction. Singleplayer and
    /// listen server only: a remote server would move it back.
    Teleport((f32, f32, f32), f32, f32),
    /// Look direction: yaw, pitch in degrees.
    Look(f32, f32),
    /// For this many seconds, keeps aiming at the nearest enemy soldier in sight as drawn
    /// here (connected, that is in the past: tests lag compensation), this many meters
    /// above his feet when standing (scaled down crouching and prone), and fires while it
    /// has one. Logs whom.
    TrackEnemy(f32, f32),
    /// Moves our soldier next to the first entry point of the nearest vehicle with this
    /// template (e.g. `"usjep_hmmwv"`), facing it. Singleplayer and listen server only.
    NearVehicle(String),
    /// Rush: moves our soldier 1.4 m in front of the current stage's charge of this name
    /// (`"A"`, `"B"`), facing it (singleplayer and listen server only).
    NearCharge(String),
    /// Walks (sprinting, by input like a player) to the first entry point of the nearest
    /// vehicle with this template; works connected to a remote server. Gives up after 150 s.
    WalkToVehicle(String),
    /// Until someone drives a vehicle of this template (gives up after 60 s).
    WaitDriver(String),
    /// Until our soldier is spawned in and controllable: exists, alive (not critically
    /// wounded) and the deploy screen is closed. Replaces a fixed `Wait` after
    /// `Click("kit:N")`, `Down`/`Kill` with an automatic respawn, or giving up
    /// (`Key(KeyX)`); resolves in about a frame with the default fast deploy (see
    /// `Scenario::respawn_time`). Fails the scenario if this takes longer than the timeout,
    /// e.g. `WaitSpawned(20.0)`.
    WaitSpawned(f32),
    /// Until the deploy screen opens, e.g. after `Kill`/`Down` giving up, or a staged mode's
    /// next wave. Fails the scenario if this takes longer than the timeout.
    WaitDeployScreen(f32),
    /// Until our soldier is seated in any vehicle (`Seat`, walking aboard, or a bot handing
    /// the seat over). Fails the scenario if this takes longer than the timeout.
    WaitInVehicle(f32),
    /// Until at least this many bots have a soldier of their own (deployed in, not just
    /// registered): faster than guessing how long a match's worth of bots takes to spawn in.
    /// Fails the scenario if this takes longer than the timeout, e.g.
    /// `WaitBotsDeployed(16, 20.0)`.
    WaitBotsDeployed(u32, f32),
    /// Moves our soldier this many meters in front of the nearest vehicle of this template,
    /// facing it (singleplayer and listen server only).
    InFrontOf(String, f32),
    /// In a vehicle: moves to this seat (1-based), like pressing F1..F8.
    Seat(u8),
    /// Puts the nearest living teammate bots on foot straight into a seat (1-based; 0: every
    /// free seat) of the nearest vehicle of this template, as getting in would, e.g.
    /// `SeatBot("jep_vodnik", 1)` for a bot driver (singleplayer and listen server only).
    /// Logs whom; takes a frame.
    SeatBot(String, u8),
    /// Like `SeatBot` for every free seat, with one bot squad of our team: its leader drives
    /// the nearest vehicle of this template, his squad mates (on foot) fill the other seats.
    /// Logs whom; takes a frame.
    SeatSquad(String),
    /// Lifts the nearest vehicle of this template this many meters (still, level as it
    /// stands), e.g. a helicopter to hover out of reach of a parachute (singleplayer and
    /// listen server only).
    LiftVehicle(String, f32),
    /// For this many seconds, checks every frame that our vehicle stays at least this many
    /// meters above the ground and keeps at least this speed (m/s); fails otherwise. Logs the
    /// lowest height and speed seen, e.g. `VehicleKeeps("bot flies", 20.0, 0.0, 15.0)`.
    VehicleKeeps(String, f32, f32, f32),
    /// Logs who sits in which seat of the nearest vehicle of this template:
    /// `seats of jep_vodnik: 1 Player, 2 Bravo (bot), 3 free` (bots' names end in "(bot)").
    LogSeats(String),
    /// In a vehicle: look direction relative to its heading (yaw, pitch in degrees).
    VehicleLook(f32, f32),
    /// Logs our vehicle's position, speed and orientation, and adds it to the report.
    VehicleInfo(String),
    /// Logs every moving or occupied vehicle (works connected to a remote server too); with a
    /// label starting with "all", every vehicle.
    LogVehicles(String),
    /// Spectating: puts the camera `distance` meters behind and `height` meters above the
    /// fastest driven vehicle matching a filter (a template, `land`, `air`, `helicopter`,
    /// `sea`, or `""` for any), looking at it, e.g. `ChaseDriven("land", 18.0, 7.0)`. Logs
    /// which.
    ChaseDriven(String, f32, f32),
    /// Spectating in singleplayer: follows a bot for this many seconds, the camera
    /// `distance` meters behind and `height` meters above him, looking at him: `"cover"`
    /// picks one fighting from cover, `"squad"` a squad leader whose squad bounds (or moves
    /// together), `"fight"` one fighting, anything else a bot whose name contains it, e.g.
    /// `ChaseBot("cover", 8.0, 7.0, 3.0)`. Until one matches it keeps looking (the seconds
    /// count from then, for at most a minute). Logs whom, and what he does every second.
    ChaseBot(String, f32, f32, f32),
    /// Adds our vehicle's state (position, speed, altitude above ground, climb rate,
    /// attitude, engine) to the report every 0.25 s for this many seconds.
    VehicleTrace(String, f32),
    /// Flying: holds the stick at (roll right, pitch up), each -1..1; `Stick(0, 0)` centres it.
    Stick(f32, f32),
    /// Moves the vehicle we sit in to a position with a heading (degrees) and a forward
    /// speed (m/s). Singleplayer and listen server only.
    PlaceVehicle((f32, f32, f32), f32, f32),
    /// In a vehicle: once it stands still, gives full throttle and reports how long until the
    /// vehicle is seen to move (faster than 0.3 m/s), then lets go. Gives up after 3 s.
    ResponseTime(String),
    /// In a vehicle: steers right and reports how long until a joint is seen to turn (the
    /// steering, or a rudder), then lets go. Gives up after 3 s.
    SteerResponseTime(String),
    /// Flying: holds an input (steer right, throttle up, stick roll right, stick pitch up;
    /// each -1..1) for `seconds`, then lets go for as long, and reports how the drawn vehicle
    /// and the camera turn: when each first moved 0.5°, reached half and 90 % of the rate at
    /// the end of the hold, that rate, the rate curve, and how long after letting go it was
    /// down to 10 % (rates over 100 ms windows; times in game time, so a slow frame rate
    /// doesn't stretch them, and the frame rate). The axis follows the input: the stick's
    /// roll, else its pitch, else the heading. E.g.
    /// `RateResponse("yaw", (1.0, 0.0, 0.0, 0.0), 1.5)`.
    RateResponse(String, (f32, f32, f32, f32), f32),
    /// Like `RateResponse`, moving the mouse (counts per second right and up, through the
    /// jet mouse sensitivity or the mouse sensitivity) for `seconds` and stopping it; also
    /// reports how far the vehicle turned by then and by the end, e.g. one 150-count
    /// movement up in 0.5 s: `MouseResponse("pitch up", (0.0, 300.0), 0.5)`. The axis is the
    /// roll if the mouse moves sideways, else the pitch.
    MouseResponse(String, (f32, f32), f32),
    /// Moves the mouse at this many counts per second (right, up) until `Mouse(0, 0)`.
    Mouse(f32, f32),
    /// Buttons stay held until released.
    Hold(Vec<Button>),
    Release(Vec<Button>),
    /// Movement input (right, forward), each -1..1; `Move(0, 0)` stops.
    Move(f32, f32),
    /// Plugs in a synthetic gamepad (a real one plus this one both work; tests gamepad input
    /// without physical hardware, injected the same way `Key`/`HoldKey` inject real keyboard
    /// events: raw gamepad events that go through the normal connection and input systems, so
    /// `Actions`, the bindings page and gamepad menu navigation all see it as a real pad).
    GamepadConnect,
    GamepadDisconnect,
    /// Gamepad buttons stay held (digital, full press) until released, e.g.
    /// `GamepadHold([South, East])`.
    GamepadHold(Vec<GamepadButton>),
    GamepadRelease(Vec<GamepadButton>),
    /// The left stick (movement, x = right, y = forward), each -1..1; held until changed.
    GamepadLeftStick(f32, f32),
    /// The right stick (look, or the flight stick while piloting), each -1..1; held until
    /// changed.
    GamepadRightStick(f32, f32),
    /// The analog triggers, each 0..1 (left, right); held until changed.
    GamepadTriggers(f32, f32),
    /// Weapon index in the kit.
    Weapon(u8),
    ThirdPerson(bool),
    /// Scales the vehicle chase camera's distance (1 = normal).
    ChaseZoom(f32),
    Ssao(bool),
    Shadows(bool),
    /// Changes a setting for this run (`Settings::set`), e.g. `Setting("bloom", "on")`,
    /// `Setting("tone_mapping", "agx")`, `Setting("sky_light", "off")`.
    Setting(String, String),
    /// Presses and releases a key, e.g. `Key(Enter)`.
    Key(KeyCode),
    /// Types text into whatever takes typing (the chat box, after `Key(KeyT)`).
    Type(String),
    /// Presses a key and keeps it down until `ReleaseKey`.
    HoldKey(KeyCode),
    ReleaseKey(KeyCode),
    /// Clicks the UI button with this `Name`, e.g. `Click("kit:5")`.
    Click(String),
    /// Scrolls the UI element with this `Name` into view within its `ScrollArea` ancestor, the
    /// same way Tab/gamepad focus does (`menu::text_input::scroll_focus_into_view`,
    /// `menu::input::gamepad_menu_nav`), e.g. `ScrollIntoView("toggle:bloom")`.
    ScrollIntoView(String),
    /// Gives keyboard focus to the UI element with this `Name` directly, e.g.
    /// `Focus("field:address")`: scenarios can't synthesize the pointer click a real focus
    /// would come from (bevy_picking's hit-test needs an actual cursor position), so this
    /// skips straight to what the click would have done. `Key(Tab)` (`HoldKey(ShiftLeft)` first
    /// for Shift+Tab) moves focus for real from there, through the same path a keypress would.
    Focus(String),
    /// Logs the current text of the `EditableText` field with this `Name`
    /// (`LogField("field:address")`), for `ExpectLog` to check.
    LogField(String),
    /// Puts this text on the clipboard, for a `Key(...)` combo that pastes to read
    /// (`menu::text_input`'s fields and the chat box paste through Ctrl+V like anything else;
    /// there's no separate scenario-only paste path).
    SetClipboard(String),
    /// Our soldier dies outright (singleplayer and listen server only).
    Kill,
    /// Staged modes: the attackers take the current stage at once, Rush's charges of it
    /// destroyed or Breakthrough's flags of it theirs (singleplayer and listen server only).
    TakeStage,
    /// We move to team 1 or 2: our soldier dies, we leave our squad (singleplayer and listen
    /// server only).
    SetTeam(u8),
    /// Our soldier is critically wounded: man down, waiting for a medic (singleplayer and
    /// listen server only, like the steps below).
    Down,
    /// Our soldier loses this much health (keeping at least 1).
    Hurt(f32),
    /// The nearest teammate (without one, the nearest soldier) is moved 1.5 m in front of
    /// us, facing us, with this much health: 0 or less wounds him critically (a body for the
    /// shock paddles).
    Summon(f32),
    /// Our soldier is this many meters higher, under an open parachute (singleplayer and
    /// listen server only), e.g. `NearVehicle("jep_paratrooper"), Parachute(1.5)` to come down
    /// next to a vehicle.
    Parachute(f32),
    /// The nearest teammate (without one, the nearest soldier) not in a vehicle is moved
    /// beside us, this many meters to our right and ahead, facing this many degrees left of
    /// our way, under an open parachute (singleplayer and listen server only), e.g.
    /// `SummonParachute(0.0, 12.0, 90.0)`: ahead of us, seen from his right side.
    SummonParachute(f32, f32, f32),
    /// The nearest living enemy (failing that, the nearest at all) is moved this many meters
    /// in front of us, facing away (a target to spot or shoot).
    SummonEnemy(f32),
    /// Says something on the radio, as the commo rose would, e.g. `Radio(Spotted)`.
    Radio(game_shared::radio::RadioCommand),
    /// A commander request, as the deploy screen or the commander screen would send it,
    /// e.g. `Commander(Apply)`, `Commander(Use(asset: Artillery, target: (-184.0, 157.0, -100.0)))`.
    Commander(game_shared::commander::CommanderRequest),
    /// Clicks the commander screen's map at this world position (height ignored).
    CommanderClick((f32, f32, f32)),
    /// A squad request, as the deploy screen would send it: `Squad(Create)`, `Squad(Join(1))`,
    /// `Squad(Leave)`.
    Squad(game_shared::squad::SquadRequest),
    /// Asks the server for a kit class's primary weapon directly, past the deploy screen's
    /// own checks (the server must refuse what its rules don't allow), e.g.
    /// `SendLoadout("assault", "usrif_m24")`. An empty weapon asks for the kit's own.
    SendLoadout(String, String),
    /// The nearest vehicle loses this many hit points (keeping at least 1).
    DamageVehicle(f32),
    /// The vehicle nearest to a point loses this many hit points; at 0 it is destroyed.
    DamageVehicleAt((f32, f32, f32), f32),
    /// Adds our health and ammo, the nearest teammate's health and the nearest vehicle's
    /// hit points to the report.
    Vitals(String),
    /// Logs the local soldier's stance (`Standing`, `Crouching` or `Prone`) with this label,
    /// e.g. `LogStance("after first Z")`, for `ExpectLog("after first Z: stance Prone", ...)`.
    LogStance(String),
    /// Saves `<out>/<name>.png` (or `name` itself if it ends in `.png`).
    Screenshot(String),
    /// Frame time statistics over this many seconds, logged and added to the report.
    Measure(String, f32),
    Log(String),
    /// Plays an effect this many times around a point (spread over 40 m), emitting for
    /// the given seconds (0: the effect's own length), e.g.
    /// `Effect("e_exp_grenade", (-184.0, 157.0, -100.0), 20, 0.0)`.
    Effect(String, (f32, f32, f32), u32, f32),
    /// Adds our soldier's movement state, stamina (and the last prediction correction when
    /// connected) to the report every frame for this many seconds.
    Trace(String, f32),
    /// Watches our soldier's height for this many seconds and logs how high above where it
    /// started he got and how long he was off the ground, e.g. `JumpApex("in place", 1.5)`
    /// after `Hold([Jump])` logs `scenario: in place: apex 1.17 m, 0.78 s in the air`.
    JumpApex(String, f32),
    /// For this many seconds, watches that our soldier never hangs in the air: off the
    /// ground (not climbing, riding, swimming, under a parachute or mantling) without moving
    /// for more than 0.3 s, as when wedged between steep faces. Logs the longest such hang
    /// and fails the scenario past it, e.g. `NoHang("into the barrier", 1.5)` while walking
    /// into something.
    NoHang(String, f32),
    /// Puts our soldier this many meters from the foot of the newest grappling rope, this
    /// many degrees round from straight in front of it (90: beside it), looking at its
    /// middle, e.g. `ViewRope(6.0, 80.0)`; `ViewRope(1.0, 0.0)` and walking forward climbs
    /// it.
    ViewRope(f32, f32),
    /// Hosting: strings this many more grappling ropes along the same edge as the newest one,
    /// 1.5 m apart (for frame times with several ropes up), e.g. `ExtraRopes(4)`.
    ExtraRopes(u32),
    /// Passes once a log line containing the text has been logged since the scenario began
    /// (or since the line the previous `ExpectLog` matched, so expectations go in order),
    /// waiting up to this many seconds; otherwise the scenario fails, e.g.
    /// `ExpectLog("captured", 45.0)`.
    ExpectLog(String, f32),
    /// Fails the scenario as soon as a log line containing the text appears from this step
    /// on, e.g. `ForbidLog("ERROR ")` or `ForbidLog("prediction correction")`.
    ForbidLog(String),
    /// Ends a `ForbidLog` of this text: from here on it may appear.
    AllowLog(String),
    /// Hit registration (see `hitreg`): every bot becomes a target dummy that takes this pose
    /// in front of us (`game_server::dummy`: `stand`, `crouch`, `prone`, `strafe`, `reload`,
    /// ...; `""` lets them be bots again). Listen server only.
    Dummy(String),
    /// For this many seconds, checks every frame which hit zones rays through points of the
    /// nearest enemy as drawn meet; logs a summary under this label.
    HitGeometry(String, f32),
    /// Aims exactly at a part of the nearest enemy as drawn (`head`, `chest`, `lforearm`,
    /// `muzzle`, ..., `all` in turn) and fires this many single shots, logging each.
    ShootAt(String, String, u32),
    /// Zooms the deploy map by this factor (>1 in, <1 out), around its current view (a
    /// scenario has no real cursor to hover for the usual around-the-cursor anchor), e.g.
    /// `DeployZoom(2.0)` doubles it.
    DeployZoom(f32),
    /// Pans the deploy map by this share of its width/height (like a mouse drag), e.g.
    /// `DeployPan(0.2, -0.1)`.
    DeployPan(f32, f32),
    Quit,
}

#[derive(Deserialize, Debug, Clone, Copy)]
pub enum Button {
    Jump,
    Sprint,
    Crouch,
    Prone,
    Fire,
    Aim,
    Use,
    Reload,
    FireMode,
    Countermeasure,
}

impl Button {
    fn bits(self) -> Buttons {
        match self {
            Button::Jump => Buttons::JUMP,
            Button::Sprint => Buttons::SPRINT,
            Button::Crouch => Buttons::CROUCH,
            Button::Prone => Buttons::PRONE,
            Button::Fire => Buttons::FIRE,
            Button::Aim => Buttons::AIM,
            Button::Use => Buttons::USE,
            Button::Reload => Buttons::RELOAD,
            Button::FireMode => Buttons::FIRE_MODE,
            Button::Countermeasure => Buttons::COUNTERMEASURE,
        }
    }
}

/// Input the scenario holds, merged into every input frame.
#[derive(Resource, Default)]
pub struct ScenarioInput {
    pub buttons: Buttons,
    pub movement: Option<Vec2>,
    pub stick: Option<Vec2>,
    /// Mouse movement, counts per second (right, up), sent as real mouse motion.
    pub mouse: Vec2,
}

/// Sends the scenario's mouse movement as mouse motion, as a real mouse would, so it goes
/// through the sensitivity and whatever reads the mouse (aiming, the flight stick).
fn inject_mouse(input: Res<ScenarioInput>, time: Res<Time<Real>>, mut motion: MessageWriter<bevy::input::mouse::MouseMotion>) {
    if input.mouse != Vec2::ZERO {
        let delta = Vec2::new(input.mouse.x, -input.mouse.y) * time.delta_secs();
        motion.write(bevy::input::mouse::MouseMotion { delta });
    }
}

/// A synthetic gamepad's held state, resent as raw gamepad events every frame by
/// `inject_gamepad` (real controllers work the same way: gilrs polls and bevy_gilrs sends raw
/// events for the current state each frame, which is why they have to be resent, not just
/// sent once). `GamepadConnect` creates the entity; the other `Gamepad*` steps change what's
/// sent while it exists.
#[derive(Resource, Default)]
struct ScenarioGamepad {
    entity: Option<Entity>,
    buttons: HashSet<GamepadButton>,
    left_stick: Vec2,
    right_stick: Vec2,
    /// Left, right, 0..1.
    triggers: Vec2,
}

/// Resends the scenario's synthetic gamepad state as raw gamepad events every frame, exactly
/// like a real backend polling hardware would, so `Actions`, the bindings page and gamepad
/// menu navigation see it as an ordinary connected gamepad.
fn inject_gamepad(gamepad: Res<ScenarioGamepad>, mut events: MessageWriter<RawGamepadEvent>) {
    let Some(entity) = gamepad.entity else {
        return;
    };
    // Every trackable button, not just the held ones: a released button's analog value
    // (`Gamepad::get`, which `dpad()` and the triggers read) only changes when a raw event
    // says so and otherwise stays at its last value, so releasing one has to explicitly send
    // 0.0, the same way a real backend keeps reporting every button's state each poll.
    for button in GamepadButton::all() {
        let held = gamepad.buttons.contains(&button) as u8 as f32;
        events.write(RawGamepadEvent::Button(RawGamepadButtonChangedEvent::new(entity, button, held)));
    }
    for (button, value) in [
        (GamepadButton::LeftTrigger2, gamepad.triggers.x),
        (GamepadButton::RightTrigger2, gamepad.triggers.y),
    ] {
        events.write(RawGamepadEvent::Button(RawGamepadButtonChangedEvent::new(entity, button, value)));
    }
    use bevy::input::gamepad::GamepadAxis;
    for (axis, value) in [
        (GamepadAxis::LeftStickX, gamepad.left_stick.x),
        (GamepadAxis::LeftStickY, gamepad.left_stick.y),
        (GamepadAxis::RightStickX, gamepad.right_stick.x),
        (GamepadAxis::RightStickY, gamepad.right_stick.y),
    ] {
        events.write(RawGamepadEvent::Axis(RawGamepadAxisChangedEvent::new(entity, axis, value)));
    }
}

impl Scenario {
    pub fn load(path: &Path) -> anyhow::Result<Self> {
        let text = std::fs::read_to_string(path)?;
        let options = ron::Options::default()
            .with_default_extension(ron::extensions::Extensions::IMPLICIT_SOME);
        Ok(options.from_str(&text)?)
    }

    /// Applies the scenario's overrides to the command line.
    pub fn apply(&self, cli: &mut Cli) {
        if let Some(level) = &self.level {
            cli.level = Some(level.clone());
        }
        if let Some(mode) = &self.mode {
            cli.mode = mode.clone();
        }
        if let Some(bots) = self.bots {
            cli.bots = bots;
        }
        if let Some(spectate) = self.spectate {
            cli.spectate = spectate;
        }
        if let Some(third_person) = self.third_person {
            cli.third_person = third_person;
        }
        if let Some(team) = self.team {
            cli.team = team;
        }
        if let Some(size) = self.size {
            cli.size = size;
        }
        if let Some(respawn_time) = self.respawn_time {
            cli.respawn_time = Some(respawn_time);
        }
    }

    /// `--screenshot <path>`: one screenshot once everything is loaded.
    pub fn screenshot(path: &Path, delay: f32) -> Self {
        Self {
            steps: vec![
                Step::WaitReady,
                Step::Wait(delay),
                Step::Screenshot(path.to_string_lossy().into_owned()),
                Step::Quit,
            ],
            ..default()
        }
    }
}

pub struct ScenarioPlugin {
    pub scenario: Scenario,
    pub out: PathBuf,
}

impl Plugin for ScenarioPlugin {
    fn build(&self, app: &mut App) {
        let pipelines = WaitingPipelines::default();
        app.insert_resource(Runner {
            steps: self.scenario.steps.clone(),
            out: self.out.clone(),
            ..default()
        })
        .insert_resource(pipelines.clone())
        .init_resource::<ScenarioInput>()
        .init_resource::<ScenarioGamepad>()
        .init_resource::<Readiness>()
        .add_plugins(crate::hitreg::HitregPlugin)
        .add_systems(
            bevy::app::PreUpdate,
            (inject_gamepad, inject_mouse).before(bevy::input::InputSystems),
        )
        .add_systems(
            Update,
            (
                (
                    note_asset_events::<Image>,
                    note_asset_events::<Mesh>,
                    note_asset_events::<Gltf>,
                    note_asset_events::<StandardMaterial>,
                    note_asset_events::<AnimationClip>,
                    note_asset_events::<Shader>,
                ),
                run_scenario.in_set(ScenarioSystems),
            )
                .chain(),
        );
        if let Some(render_app) = app.get_sub_app_mut(RenderApp) {
            render_app
                .insert_resource(pipelines)
                .add_systems(Render, count_waiting_pipelines.in_set(RenderSystems::Cleanup));
        }
    }
}

/// Runs the scenario's steps in `Update`. UI reacting to `Key`/`Click` steps runs after it.
#[derive(SystemSet, Debug, Clone, PartialEq, Eq, Hash)]
pub struct ScenarioSystems;

/// Pipelines still compiling, counted in the render world.
#[derive(Resource, Clone, Default)]
struct WaitingPipelines(Arc<AtomicUsize>);

fn count_waiting_pipelines(cache: Res<PipelineCache>, waiting: Res<WaitingPipelines>) {
    waiting.0.store(cache.waiting_pipelines().count(), Ordering::Relaxed);
}

/// When assets last changed, as a sign that loading is still going on.
#[derive(Resource, Default)]
struct Readiness {
    last_asset_event: f32,
    /// The last event, logged when `WaitReady` times out.
    last_asset: String,
    last_pipeline_wait: f32,
}

fn note_asset_events<A: Asset>(
    mut events: MessageReader<AssetEvent<A>>,
    time: Res<Time<Real>>,
    mut readiness: ResMut<Readiness>,
    server: Res<AssetServer>,
) {
    if let Some(event) = events.read().last() {
        readiness.last_asset_event = time.elapsed_secs();
        if time.elapsed_secs() > 60.0 {
            let path = match event {
                AssetEvent::Added { id }
                | AssetEvent::Modified { id }
                | AssetEvent::LoadedWithDependencies { id }
                | AssetEvent::Removed { id }
                | AssetEvent::Unused { id } => server.get_path(*id),
            };
            readiness.last_asset = format!("{event:?} {path:?}");
        }
    }
}

#[derive(Resource, Default)]
struct Runner {
    steps: Vec<Step>,
    next: usize,
    out: PathBuf,
    /// Real time the current step started.
    step_started: Option<f32>,
    frames: u32,
    screenshot_pending: bool,
    samples: Vec<f32>,
    report: String,
    /// Keys pressed by the last `Key` step, released the next frame.
    released: Vec<KeyCode>,
    /// Where something was when the current step began.
    mark: Option<Vec3>,
    /// `NoHang`: our soldier's last position, how long he has hung in the air so far and
    /// the longest hang (seconds).
    hang: Option<(Vec3, f32, f32)>,
    /// The bot `ChaseBot` follows (a player entity of the server's world).
    chased: Option<Entity>,
    /// `ChaseBot`: the server's answer on the bot, pending, and the last one.
    chase_answer: Option<Receiver<Option<ChaseView>>>,
    chase_view: Option<ChaseView>,
    /// Text of the last `Type` step, typed the next frame.
    typed: String,
    /// Captured log lines before this index are done with for `ExpectLog`.
    log_cursor: usize,
    /// `ForbidLog` texts and the log index they apply from.
    forbidden: Vec<(String, usize)>,
    finished: bool,
    /// `RateResponse`: per frame the time since the input and the vehicle's and the
    /// camera's angle about the measured axis (degrees).
    rates: Vec<(f32, f32, f32)>,
    /// `RateResponse`: the game time it began at.
    rate_started: f64,
}

/// What a player can do, for [`run_scenario`].
#[derive(bevy::ecs::system::SystemParam)]
struct PlayerControls<'w, 's> {
    input: ResMut<'w, ScenarioInput>,
    look: ResMut<'w, LookState>,
    third_person: ResMut<'w, ThirdPerson>,
    chase_zoom: ResMut<'w, crate::camera::ChaseZoom>,
    selection: ResMut<'w, WeaponSelection>,
    keyboard: MessageWriter<'w, KeyboardInput>,
    window: Single<'w, 's, Entity, With<PrimaryWindow>>,
    buttons: Query<'w, 's, (Entity, &'static Name, &'static mut Interaction)>,
    effects: MessageWriter<'w, crate::effects::SpawnEffect>,
    radio: MessageWriter<'w, game_shared::radio::RadioRequest>,
    commander: MessageWriter<'w, game_shared::commander::CommanderRequest>,
    squad: MessageWriter<'w, game_shared::squad::SquadRequest>,
    loadouts: MessageWriter<'w, game_shared::arsenal::LoadoutRequest>,
    commander_screen: ResMut<'w, crate::commander::CommanderScreen>,
    deploy_screen: ResMut<'w, DeployScreen>,
    gamepad: ResMut<'w, ScenarioGamepad>,
    /// `gamepad_connection_system` (which attaches/detaches the `Gamepad` component) listens
    /// for this directly, not for `RawGamepadEvent::Connection` (that variant only feeds the
    /// aggregate `GamepadEvent` stream for observers, per `gamepad_event_processing_system`).
    gamepad_events: MessageWriter<'w, GamepadConnectionEvent>,
    clipboard: ResMut<'w, bevy::clipboard::Clipboard>,
    /// Every named UI element, for `Focus` (text inputs aren't `Button`s, so they're not in
    /// `buttons`) and `ScrollIntoView`.
    named: Query<'w, 's, (Entity, &'static Name)>,
    input_focus: ResMut<'w, bevy::input_focus::InputFocus>,
    fields: Query<'w, 's, (&'static Name, &'static bevy::text::EditableText)>,
}

/// The vehicles around, for [`run_scenario`].
#[derive(bevy::ecs::system::SystemParam)]
struct Vehicles<'w, 's> {
    vehicles: Query<'w, 's, (&'static Vehicle, &'static VehicleView, &'static VehicleData)>,
    all: Query<'w, 's, (Entity, &'static Vehicle, &'static VehicleView)>,
    riders: Query<'w, 's, &'static Seated>,
    seated: Query<'w, 's, &'static Seated, With<LocalSoldier>>,
    seat: ResMut<'w, SeatRequest>,
    spatial: avian3d::prelude::SpatialQuery<'w, 's>,
    states: Query<'w, 's, &'static game_shared::vehicle::VehicleState>,
    prediction: Res<'w, crate::vehicle_prediction::VehiclePredictionStats>,
    health: Query<'w, 's, (Entity, &'static VehicleView, &'static VehicleHealth)>,
    /// The camera as last placed, for `RateResponse`.
    camera: Query<'w, 's, &'static Transform, With<PlayerCamera>>,
    /// The pilot's stick and free look, for `VehicleLook`.
    flight: ResMut<'w, crate::vehicles::FlightStick>,
    /// Game time, for `RateResponse`.
    game_time: Res<'w, Time<Virtual>>,
    /// Our inputs and the vehicles' acknowledged ones: how many ticks behind the simulation is.
    history: Res<'w, crate::local_input::InputHistory>,
    motions: Query<'w, 's, &'static game_shared::vehicle::VehicleMotion>,
}

impl Vehicles<'_, '_> {
    /// The vehicle's latest replicated placement (what is drawn is a moment older).
    fn latest(&self, vehicle: Entity) -> Option<Transform> {
        let motion = self.motions.get(vehicle).ok()?;
        Some(Transform::from_translation(motion.position).with_rotation(motion.rotation))
    }

    /// `label t: at (x, y, z) 320 km/h, 85 m up (climb 3.2 m/s), heading 12, pitch 4, roll -30, engine 0.8`
    fn flight_line(&self, label: &str, elapsed: f32) -> String {
        let Some((view, state, ack)) = self.seated.single().ok().and_then(|s| {
            let ack = self.motions.get(s.vehicle).map_or(0, |m| m.ack);
            Some((self.vehicles.get(s.vehicle).ok()?.1, self.states.get(s.vehicle).ok()?, ack))
        }) else {
            return format!("{label} {elapsed:5.2}: not in a vehicle");
        };
        let lag = self.history.latest().map_or(0, |f| f.seq.wrapping_sub(ack) as i64);
        let t = view.transform;
        let filter = avian3d::prelude::SpatialQueryFilter::from_mask(game_shared::physics::GameLayer::World);
        let altitude = self
            .spatial
            .cast_ray(t.translation, Dir3::NEG_Y, 2000.0, true, &filter)
            .map_or(f32::NAN, |hit| hit.distance);
        let forward = t.rotation * Vec3::NEG_Z;
        let right = t.rotation * Vec3::X;
        format!(
            "{label} {elapsed:5.2}: at ({:.1}, {:.1}, {:.1}) {:.0} km/h, {altitude:.1} m up (climb {:.1} m/s), heading {:.0}, pitch {:.0}, roll {:.0}, engine {:.2} gear {}, replayed {} corrected {:.3}, input {lag} ticks ahead of the simulation",
            t.translation.x,
            t.translation.y,
            t.translation.z,
            view.velocity.length() * 3.6,
            view.velocity.y,
            crate::vehicles::heading(t.rotation).to_degrees(),
            forward.y.clamp(-1.0, 1.0).asin().to_degrees(),
            (-right.y).clamp(-1.0, 1.0).asin().to_degrees(),
            state.engine,
            state.gear + 1,
            self.prediction.replayed,
            self.prediction.last_correction,
        )
    }
}

/// Our soldier and the others, for [`run_scenario`].
#[derive(bevy::ecs::system::SystemParam)]
struct Soldiers<'w, 's> {
    local: Query<'w, 's, (&'static mut SoldierMotion, &'static mut Health), With<LocalSoldier>>,
    local_inventory: Query<'w, 's, &'static game_shared::weapons::Inventory, With<LocalSoldier>>,
    others: Query<
        'w,
        's,
        (Entity, &'static ControlledBy, &'static mut SoldierMotion, &'static mut Health),
        (With<Soldier>, Without<LocalSoldier>, Without<Seated>),
    >,
    teams: Query<'w, 's, &'static Team>,
    local_team: Query<'w, 's, &'static Team, With<LocalPlayer>>,
    drawn: Query<
        'w,
        's,
        (Entity, &'static ControlledBy, &'static crate::prediction::SoldierRender),
        (With<Soldier>, Without<LocalSoldier>),
    >,
    spatial: avian3d::prelude::SpatialQuery<'w, 's>,
    /// Grappling ropes and ziplines, for `ViewRope`.
    ropes: Query<'w, 's, (Entity, &'static game_shared::rope::Rope)>,
    /// Players, whether bots, their squads and teams, for `WaitBotsDeployed`, `SeatBot` and
    /// `SeatSquad`.
    bots: Query<
        'w,
        's,
        (
            &'static game_shared::protocol::Player,
            Option<&'static game_shared::squad::SquadMember>,
            &'static Team,
        ),
    >,
    /// Who controls each soldier, seated or not.
    owners: Query<'w, 's, &'static ControlledBy, With<Soldier>>,
    /// Our own server, for the steps that change its world.
    server: ServerSide<'w>,
    /// Who sits where, for `SeatBot` and `LogSeats`.
    crews: Query<'w, 's, (&'static Seated, &'static ControlledBy)>,
    players: Query<'w, 's, &'static game_shared::protocol::Player>,
    /// Rush's charges and the mode, for `NearCharge`.
    charges: Query<'w, 's, &'static game_shared::modes::Charge>,
    modes: Query<'w, 's, &'static game_shared::modes::ModeState>,
    /// Whether our soldier is critically wounded, for `WaitSpawned`.
    downed: Query<'w, 's, (), (With<LocalSoldier>, With<Downed>)>,
}

/// Our own server (singleplayer, hosting; `local_server`), for the steps that change its
/// world: they send it closures, run on its thread before its next update, so what they do
/// shows here a frame or two later, replicated like everything else.
#[derive(bevy::ecs::system::SystemParam)]
struct ServerSide<'w> {
    server: Option<Res<'w, crate::local_server::LocalServer>>,
    map: Res<'w, bevy_replicon::shared::server_entity_map::ServerEntityMap>,
}

impl ServerSide<'_> {
    /// Runs `task` on the server's world; connected to another server, warns instead.
    fn run(&self, step: &str, task: impl FnOnce(&mut World) + Send + 'static) {
        match &self.server {
            Some(server) => server.run(task),
            None => warn!("scenario: {step} needs our own server (singleplayer or hosting)"),
        }
    }

    fn query<R: Send + 'static>(&self, task: impl FnOnce(&mut World) -> R + Send + 'static) -> Option<Receiver<R>> {
        self.server.as_ref().map(|server| server.query(task))
    }

    /// The server's entity for one of ours.
    fn entity(&self, ours: Entity) -> Option<Entity> {
        self.map.to_server().get(&ours).copied()
    }
}

/// Changes our soldier's movement state on the server.
fn move_us(world: &mut World, change: impl FnOnce(&mut SoldierMotion)) {
    match link_soldier(world).and_then(|soldier| world.get_mut::<SoldierMotion>(soldier)) {
        Some(mut motion) => change(&mut motion),
        None => warn!("scenario: our soldier is gone on the server"),
    }
}

/// Puts our soldier's feet at `position`, standing still, on the server.
fn place_us(server: &ServerSide, step: &str, position: Vec3) {
    server.run(step, move |world| {
        move_us(world, |motion| {
            motion.position = position;
            motion.velocity = Vec3::ZERO;
            motion.mantle = 0.0;
        })
    });
}

/// Puts our soldier's feet at `local` in a vehicle's frame (ours: `vehicle`), where the
/// server has the vehicle now, standing still.
fn place_us_at_vehicle(server: &ServerSide, step: &str, vehicle: Entity, local: Vec3) {
    let Some(theirs) = server.entity(vehicle) else {
        warn!("scenario: {step}: {vehicle} isn't the server's");
        return;
    };
    server.run(step, move |world| {
        use avian3d::prelude::{Position, Rotation};
        let (Some(position), Some(rotation)) = (world.get::<Position>(theirs).copied(), world.get::<Rotation>(theirs).copied()) else {
            warn!("scenario: vehicle {theirs} is gone on the server");
            return;
        };
        let target = position.0 + rotation.0 * local;
        move_us(world, |motion| {
            motion.position = target;
            motion.velocity = Vec3::ZERO;
            motion.mantle = 0.0;
        });
    });
}

/// Changes our soldier's health on the server.
fn hurt_us(server: &ServerSide, step: &str, change: impl FnOnce(&mut Health) + Send + 'static) {
    server.run(step, move |world| {
        match link_soldier(world).and_then(|soldier| world.get_mut::<Health>(soldier)) {
            Some(mut health) => change(&mut health),
            None => warn!("scenario: our soldier is gone on the server"),
        }
    });
}

/// Changes another soldier (ours: `soldier`) on the server.
fn move_soldier(server: &ServerSide, step: &str, soldier: Entity, change: impl FnOnce(&mut SoldierMotion, &mut Health) + Send + 'static) {
    let Some(theirs) = server.entity(soldier) else {
        warn!("scenario: {step}: {soldier} isn't the server's");
        return;
    };
    server.run(step, move |world| {
        let mut entity = world.entity_mut(theirs);
        let (Some(mut motion), Some(mut health)) = (entity.get::<SoldierMotion>().copied(), entity.get::<Health>().cloned()) else {
            warn!("scenario: soldier {theirs} is gone on the server");
            return;
        };
        change(&mut motion, &mut health);
        entity.insert((motion, health));
    });
}

/// Changes a vehicle (ours: `vehicle`) on the server.
fn on_vehicle(server: &ServerSide, step: &str, vehicle: Entity, change: impl FnOnce(&mut EntityWorldMut) + Send + 'static) {
    let Some(theirs) = server.entity(vehicle) else {
        warn!("scenario: {step}: {vehicle} isn't the server's");
        return;
    };
    server.run(step, move |world| match world.get_entity_mut(theirs) {
        Ok(mut entity) => change(&mut entity),
        Err(_) => warn!("scenario: vehicle {theirs} is gone on the server"),
    });
}

/// Seats bots (our soldier entities) in a vehicle on the server, as getting in would.
fn seat_bots(server: &ServerSide, step: &str, vehicle: Entity, seats: Vec<(Entity, u8, bool)>) {
    let Some(vehicle) = server.entity(vehicle) else {
        warn!("scenario: {step}: {vehicle} isn't the server's");
        return;
    };
    let seats: Vec<(Entity, u8, bool)> = seats
        .into_iter()
        .filter_map(|(bot, seat, open)| Some((server.entity(bot)?, seat, open)))
        .collect();
    server.run(step, move |world| {
        for (bot, seat, open) in seats {
            let Some(hitbox) = world.get::<game_shared::soldier::Hitbox>(bot).cloned() else {
                continue;
            };
            let mut commands = world.commands();
            game_server::vehicles::seat_soldier(&mut commands, bot, &hitbox, vehicle, seat, open);
        }
        world.flush();
    });
}

/// What `ChaseBot` shows of the bot it follows, from the server.
#[derive(Clone, Debug)]
struct ChaseView {
    player: Entity,
    name: String,
    motion: SoldierMotion,
    doing: String,
    target: bool,
    suppression: f32,
    cover: Option<bool>,
}

/// On the server: the bot that best matches a `ChaseBot` filter (see `Step::ChaseBot`).
fn chase_pick(world: &mut World, what: &str) -> Option<Entity> {
    use game_server::{Controls, bots::BotBrain};
    use game_shared::{protocol::Player, squad::SquadMember};
    let mut query = world.query::<(Entity, &BotBrain, &Player, Option<&Controls>, Option<&SquadMember>, &Team)>();
    let bots: Vec<_> = query
        .iter(world)
        .map(|(player, brain, info, controls, member, team)| {
            (player, brain.fighting_from_cover(), brain.combat_state(), info.name.clone(), controls.map(|c| c.0), member.copied(), *team)
        })
        .collect();
    let position = |soldier: Option<Entity>| soldier.and_then(|s| world.get::<SoldierMotion>(s)).map(|m| m.position);
    let bounding = |team: Team, squad: u8| {
        world
            .get_resource::<game_server::ai::squad::SquadTactics>()
            .and_then(|t| t.squads.get(&(team, squad)))
            .is_some_and(|t| t.bounding)
    };
    bots.iter()
        .filter_map(|(player, cover, combat, name, soldier, member, team)| {
            let at = position(*soldier)?;
            let score = match what {
                "cover" => cover.map(|up| if up { 2.0 } else { 1.0 }),
                "fight" => combat.0.then_some(1.0 + combat.1),
                "squad" => {
                    let member = member.filter(|m| m.leader)?;
                    // Squad members near the leader.
                    let near = bots
                        .iter()
                        .filter(|b| b.0 != *player && b.6 == *team && b.5.is_some_and(|m| m.squad == member.squad))
                        .filter(|b| position(b.4).is_some_and(|p| p.distance(at) < 35.0))
                        .count();
                    (near >= 2).then_some(near as f32 + if bounding(*team, member.squad) { 10.0 } else { 0.0 })
                }
                filter => name.contains(filter).then_some(1.0),
            };
            Some((score?, *player))
        })
        .max_by(|a, b| a.0.total_cmp(&b.0))
        .map(|(_, player)| player)
}

/// On the server: what a bot (a player entity) is doing, where.
fn chase_view(world: &mut World, player: Entity) -> Option<ChaseView> {
    let brain = world.get::<game_server::bots::BotBrain>(player)?;
    let (target, suppression) = brain.combat_state();
    let (doing, cover) = (brain.doing().to_string(), brain.fighting_from_cover());
    let name = world.get::<game_shared::protocol::Player>(player)?.name.clone();
    let soldier = world.get::<game_server::Controls>(player)?.0;
    let motion = *world.get::<SoldierMotion>(soldier)?;
    Some(ChaseView { player, name, motion, doing, target, suppression, cover })
}

#[derive(Clone, Copy, PartialEq)]
enum Progress {
    Done,
    Waiting,
}

/// The logical key a `Key`/`HoldKey`/`ReleaseKey` step's physical `KeyCode` would normally
/// carry. `on_focused_keyboard_input` (a focused `EditableText`'s editing, in
/// `menu::text_input` and the chat box) keys entirely off the logical key, not the physical
/// one, so without this a scenario couldn't press Ctrl, Shift, an arrow, Home/End,
/// Backspace/Delete or Tab in any way that field would recognize. Limited to the keys that
/// don't insert a character themselves (`Type` already sends a proper `Key::Character` for
/// that) plus the four letters our own scenarios combine with Ctrl (A/C/V/X: select all, copy,
/// paste, cut), so this can't make a `Key(KeyX)` used for some keybound action also type an
/// "x" into a field that happens to be focused.
fn scenario_logical_key(code: KeyCode) -> Key {
    match code {
        KeyCode::ShiftLeft | KeyCode::ShiftRight => Key::Shift,
        KeyCode::ControlLeft | KeyCode::ControlRight => Key::Control,
        KeyCode::AltLeft | KeyCode::AltRight => Key::Alt,
        KeyCode::SuperLeft | KeyCode::SuperRight => Key::Super,
        KeyCode::ArrowLeft => Key::ArrowLeft,
        KeyCode::ArrowRight => Key::ArrowRight,
        KeyCode::ArrowUp => Key::ArrowUp,
        KeyCode::ArrowDown => Key::ArrowDown,
        KeyCode::Home => Key::Home,
        KeyCode::End => Key::End,
        KeyCode::Backspace | KeyCode::NumpadBackspace => Key::Backspace,
        KeyCode::Delete => Key::Delete,
        KeyCode::Enter | KeyCode::NumpadEnter => Key::Enter,
        KeyCode::Escape => Key::Escape,
        KeyCode::Tab => Key::Tab,
        KeyCode::KeyA => Key::Character("a".into()),
        KeyCode::KeyC => Key::Character("c".into()),
        KeyCode::KeyV => Key::Character("v".into()),
        KeyCode::KeyX => Key::Character("x".into()),
        _ => Key::Unidentified(NativeKey::Unidentified),
    }
}

/// `TakeStage`: the current stage's objectives go to the attackers (Rush: its charges are
/// destroyed; Breakthrough: its flags are theirs), and the mode's own rules move the front on.
fn take_stage(world: &mut World) {
    use game_shared::{
        conquest::FlagState,
        modes::{Charge, ChargeState, ModeState, Sector},
    };
    let Some(mode) = world.query::<&ModeState>().iter(world).next().copied() else {
        warn!("scenario: no staged mode to take a stage of");
        return;
    };
    let mut taken = 0;
    if mode.kind == game_data::modes::ModeKind::Rush {
        for (charge, mut state) in world.query::<(&Charge, &mut ChargeState)>().iter_mut(world) {
            if charge.stage == mode.stage {
                *state = ChargeState::Destroyed;
                taken += 1;
            }
        }
    } else {
        for (sector, mut flag) in world.query::<(&Sector, &mut FlagState)>().iter_mut(world) {
            if sector.0 == mode.stage {
                *flag = FlagState::held_by(mode.attacker);
                taken += 1;
            }
        }
    }
    info!("scenario: taking {} ({taken} objectives handed to the attackers)", mode.stage_label());
}

/// `SetTeam`, on the server: we play for `team` from now on, out of our squad and with no
/// spawn picked.
fn set_team(world: &mut World, team: Team) {
    let Some(player) = link_player(world) else {
        warn!("scenario: no local player to move to {team:?}");
        return;
    };
    let mut entity = world.entity_mut(player);
    if let Some(mut current) = entity.get_mut::<Team>() {
        *current = team;
    }
    if let Some(mut deployment) = entity.get_mut::<game_shared::conquest::Deployment>() {
        deployment.control_point = None;
        deployment.on_squad_leader = false;
    }
    entity.remove::<game_shared::squad::SquadMember>();
    info!("scenario: we play for {team:?} now");
}

#[allow(clippy::too_many_arguments, clippy::type_complexity)]
fn run_scenario(
    mut commands: Commands,
    time: Res<Time<Real>>,
    active: Res<ActiveMatch>,
    screen: Res<State<Screen>>,
    mut runner: ResMut<Runner>,
    mut readiness: ResMut<Readiness>,
    waiting_pipelines: Res<WaitingPipelines>,
    level: Option<Res<LoadedLevel>>,
    player: PlayerControls,
    mut vehicles: Vehicles,
    soldiers: Soldiers,
    (prediction, rendered, diagnostics, mut was_swimming, mut hitreg): (
        Res<crate::prediction::PredictionStats>,
        Query<&crate::prediction::SoldierRender, With<LocalSoldier>>,
        Res<bevy::diagnostic::DiagnosticsStore>,
        Local<bool>,
        ResMut<crate::hitreg::HitregTask>,
    ),
    mut spectator: Query<&mut Spectator>,
    camera: Query<Entity, With<PlayerCamera>>,
    mut suns: Query<&mut DirectionalLight, With<Sun>>,
    mut exit: MessageWriter<AppExit>,
) {
    let PlayerControls {
        mut input,
        mut look,
        mut third_person,
        mut chase_zoom,
        mut selection,
        mut keyboard,
        window,
        mut buttons,
        mut effects,
        mut radio,
        mut commander,
        mut squad,
        mut loadouts,
        mut commander_screen,
        mut deploy_screen,
        mut gamepad,
        mut gamepad_events,
        mut clipboard,
        named,
        mut input_focus,
        fields,
    } = player;
    let Soldiers {
        local: soldier,
        local_inventory,
        others,
        teams,
        local_team,
        drawn,
        spatial,
        ropes,
        bots,
        owners,
        server,
        crews,
        players,
        charges,
        modes,
        downed,
    } = soldiers;
    let now = time.elapsed_secs();
    if runner.finished {
        return;
    }
    // Logged once on entry and exit (not every tick), so `ExpectLog` can watch for it.
    if let Ok((m, _)) = soldier.single() {
        if m.swimming != *was_swimming {
            let water = level.as_ref().and_then(|l| l.desc.water.as_ref()).map(|w| w.height);
            let depth = water.map_or(0.0, |w| w - m.position.y);
            if m.swimming {
                info!("scenario: started swimming (depth {depth:.2} m)");
            } else {
                info!("scenario: stopped swimming (depth {depth:.2} m, grounded {})", m.grounded);
            }
            *was_swimming = m.swimming;
        }
    }
    let forbidden = runner
        .forbidden
        .iter()
        .find_map(|(text, from)| log_capture::find(text, *from).map(|(_, line)| (text.clone(), line)));
    if let Some((text, line)) = forbidden {
        finish(&mut runner, &mut exit, now, Some(format!("forbidden log text \"{text}\": {line}")));
        return;
    }
    // Typed characters: text without a key the game reacts to.
    for c in std::mem::take(&mut runner.typed).chars() {
        for state in [ButtonState::Pressed, ButtonState::Released] {
            keyboard.write(KeyboardInput {
                key_code: KeyCode::Unidentified(NativeKeyCode::Unidentified),
                logical_key: Key::Character(c.to_string().into()),
                state,
                text: (state == ButtonState::Pressed).then(|| c.to_string().into()),
                repeat: false,
                window: *window,
            });
        }
    }
    // Keys go through the same input events as a real keyboard, so every system sees them.
    let mut key_event = |key_code: KeyCode, state: ButtonState| {
        keyboard.write(KeyboardInput {
            key_code,
            logical_key: scenario_logical_key(key_code),
            state,
            text: None,
            repeat: false,
            window: *window,
        });
    };
    for key in runner.released.drain(..) {
        key_event(key, ButtonState::Released);
    }
    if waiting_pipelines.0.load(Ordering::Relaxed) > 0 {
        readiness.last_pipeline_wait = now;
    }
    // Several instant steps can run in one frame; waits end the frame.
    for _ in 0..32 {
        let Some(step) = runner.steps.get(runner.next).cloned() else {
            return;
        };
        let started = *runner.step_started.get_or_insert(now);
        let elapsed = now - started;
        let progress = match &step {
            Step::WaitReady => {
                // At the main menu there is nothing to wait for but assets and shaders. A
                // match started by a `Click` just before shows up a frame later.
                runner.frames += 1;
                let in_place = match screen.get() {
                    Screen::Menu => runner.frames > 1,
                    Screen::Loading => false,
                    Screen::InGame => level.is_some() && (active.spectating() || !soldier.is_empty()),
                };
                let ready = in_place
                    && now - readiness.last_asset_event > 1.0
                    && now - readiness.last_pipeline_wait > 0.5;
                if ready || elapsed > 90.0 {
                    let note = if ready {
                        String::new()
                    } else {
                        format!(
                            " (timed out; last asset event {:.1} s ago: {}, last pipeline wait {:.1} s ago)",
                            now - readiness.last_asset_event,
                            readiness.last_asset,
                            now - readiness.last_pipeline_wait
                        )
                    };
                    info!("scenario: ready after {now:.1} s{note}");
                    Progress::Done
                } else {
                    Progress::Waiting
                }
            }
            Step::Wait(seconds) => done_if(elapsed >= *seconds),
            Step::Frames(frames) => {
                runner.frames += 1;
                done_if(runner.frames > *frames)
            }
            Step::Camera(position, yaw, pitch) => {
                if let Ok(mut spectator) = spectator.single_mut() {
                    spectator.position = Vec3::from(*position);
                }
                set_look(&mut look, *yaw, *pitch);
                Progress::Done
            }
            Step::Teleport(position, yaw, pitch) => {
                if soldier.single().is_ok() {
                    place_us(&server, "Teleport", Vec3::from(*position));
                } else {
                    warn!("scenario: no soldier to teleport");
                }
                set_look(&mut look, *yaw, *pitch);
                Progress::Done
            }
            Step::Look(yaw, pitch) => {
                set_look(&mut look, *yaw, *pitch);
                Progress::Done
            }
            Step::TrackEnemy(seconds, height) => {
                let my_team = local_team.single().ok().copied();
                let eye = rendered.single().ok().map(|r| r.eye_position());
                let world = avian3d::prelude::SpatialQueryFilter::from_mask(game_shared::physics::GameLayer::World);
                let in_sight = |eye: Vec3, at: Vec3| {
                    Dir3::new(at - eye).is_ok_and(|dir| spatial.cast_ray(eye, dir, eye.distance(at), true, &world).is_none())
                };
                let target = eye.and_then(|eye| {
                    drawn
                        .iter()
                        .filter(|(_, c, _)| teams.get(c.0).ok().copied() != my_team)
                        .map(|(entity, _, render)| {
                            let scale = match render.stance {
                                game_shared::soldier::Stance::Standing => 1.0,
                                game_shared::soldier::Stance::Crouching => 0.65,
                                game_shared::soldier::Stance::Prone => 0.2,
                            };
                            (entity, render.position + Vec3::Y * height * scale)
                        })
                        .filter(|(_, at)| in_sight(eye, *at))
                        .min_by(|a, b| a.1.distance(eye).total_cmp(&b.1.distance(eye)))
                        .map(|(entity, at)| (entity, at - eye))
                });
                if let Some((entity, to)) = target {
                    look.yaw = (-to.x).atan2(-to.z);
                    look.pitch = (to.y / to.length().max(0.01)).asin();
                    if runner.frames % 30 == 0 {
                        info!("scenario: tracking {entity:?} {:.1} m away", to.length());
                    }
                }
                let done = elapsed >= *seconds;
                input.buttons.set(Buttons::FIRE, target.is_some() && !done);
                runner.frames += 1;
                done_if(done)
            }
            Step::NearVehicle(template) => {
                let origin = soldier.single().map(|(m, _)| m.position).unwrap_or_default();
                let nearest = nearest_vehicle(&vehicles, template, origin)
                    .and_then(|(e, view)| Some((e, view, vehicles.vehicles.get(e).ok()?.2)));
                match (nearest, soldier.single()) {
                    (Some((vehicle, view, data)), Ok(_)) => {
                        let desc = &data.0.desc;
                        // Beside the door, outside the hull, within reach of the entry point.
                        let local = match desc.entry_points.first() {
                            Some(entry) => {
                                let side = (desc.physics.bounds[0][0] - 0.8).max(entry.position[0] - entry.radius * 0.6);
                                Vec3::new(side, (entry.position[1] - 1.0).min(0.0), entry.position[2])
                            }
                            None => Vec3::new(desc.physics.bounds[0][0] - 0.8, 0.0, 0.0),
                        };
                        // Where the server has it now: what is drawn here is a moment old.
                        place_us_at_vehicle(&server, "NearVehicle", vehicle, local);
                        let at = vehicles.latest(vehicle).unwrap_or(view.transform);
                        let to = at.translation - at.transform_point(local);
                        look.yaw = (-to.x).atan2(-to.z);
                        look.pitch = -0.2;
                    }
                    _ => warn!("scenario: no {template} (or no soldier) to go to"),
                }
                Progress::Done
            }
            Step::NearCharge(name) => {
                let stage = modes.iter().next().map(|m| m.stage);
                let charge = charges.iter().find(|c| Some(c.stage) == stage && c.name == *name);
                match (charge, soldier.single()) {
                    (Some(charge), Ok(_)) => {
                        let front = Quat::from_rotation_y(charge.yaw) * Vec3::NEG_Z;
                        let target = charge.position + front * 1.4 + Vec3::Y * 0.1;
                        place_us(&server, "NearCharge", target);
                        let to = charge.position + Vec3::Y * 0.6 - (target + Vec3::Y * 1.6);
                        look.yaw = (-to.x).atan2(-to.z);
                        look.pitch = (to.y / to.length().max(0.01)).asin();
                    }
                    _ => warn!("scenario: no charge {name} (or no soldier) to go to"),
                }
                Progress::Done
            }
            Step::WaitDriver(template) => {
                let driven = vehicles.riders.iter().any(|seated| {
                    seated.seat == 0
                        && vehicles.all.get(seated.vehicle).is_ok_and(|(_, v, _)| v.template == *template)
                });
                done_if(driven || elapsed > 60.0)
            }
            Step::WaitSpawned(seconds) => {
                let spawned = !soldier.is_empty() && !deploy_screen.open && downed.is_empty();
                if spawned {
                    Progress::Done
                } else if elapsed > *seconds {
                    finish(&mut runner, &mut exit, now, Some(format!("not spawned within {seconds} s")));
                    Progress::Waiting
                } else {
                    Progress::Waiting
                }
            }
            Step::WaitDeployScreen(seconds) => {
                if deploy_screen.open {
                    Progress::Done
                } else if elapsed > *seconds {
                    finish(&mut runner, &mut exit, now, Some(format!("the deploy screen didn't open within {seconds} s")));
                    Progress::Waiting
                } else {
                    Progress::Waiting
                }
            }
            Step::WaitInVehicle(seconds) => {
                if vehicles.seated.single().is_ok() {
                    Progress::Done
                } else if elapsed > *seconds {
                    finish(&mut runner, &mut exit, now, Some(format!("not seated in a vehicle within {seconds} s")));
                    Progress::Waiting
                } else {
                    Progress::Waiting
                }
            }
            Step::WaitBotsDeployed(count, seconds) => {
                // Bots with a soldier (replicated: works on any server).
                let deployed = owners
                    .iter()
                    .filter(|owner| bots.get(owner.0).is_ok_and(|(player, ..)| player.is_bot))
                    .count() as u32;
                if deployed >= *count {
                    Progress::Done
                } else if elapsed > *seconds {
                    finish(&mut runner, &mut exit, now, Some(format!("only {deployed}/{count} bots deployed within {seconds} s")));
                    Progress::Waiting
                } else {
                    Progress::Waiting
                }
            }
            Step::InFrontOf(template, distance) => {
                // A driven one if there is, else the nearest.
                let origin = soldier.single().map(|(m, _)| m.position).unwrap_or_default();
                let driven: Vec<Entity> = vehicles
                    .riders
                    .iter()
                    .filter(|seated| seated.seat == 0)
                    .map(|seated| seated.vehicle)
                    .collect();
                let nearest = vehicles
                    .all
                    .iter()
                    .filter(|(_, v, _)| v.template == *template)
                    .min_by(|a, b| {
                        let d = |(e, _, v): &(Entity, &Vehicle, &VehicleView)| {
                            (!driven.contains(e), v.transform.translation.distance(origin))
                        };
                        let (da, db) = (d(a), d(b));
                        da.0.cmp(&db.0).then(da.1.total_cmp(&db.1))
                    })
                    .and_then(|(e, ..)| Some((e, vehicles.vehicles.get(e).ok()?)));
                match (nearest, soldier.single()) {
                    (Some((vehicle, (_, view, data))), Ok(_)) => {
                        let front = data.0.desc.physics.bounds[0][2];
                        let local = Vec3::new(0.0, 0.5, front - distance);
                        place_us_at_vehicle(&server, "InFrontOf", vehicle, local);
                        let at = vehicles.latest(vehicle).unwrap_or(view.transform);
                        let to = at.translation - at.transform_point(local);
                        look.yaw = (-to.x).atan2(-to.z);
                        look.pitch = -0.1;
                    }
                    _ => warn!("scenario: no {template} (or no soldier) to stand in front of"),
                }
                Progress::Done
            }
            Step::WalkToVehicle(template) => {
                let (origin, speed) = soldier
                    .single()
                    .map(|(m, _)| (m.position, Vec2::new(m.velocity.x, m.velocity.z).length()))
                    .unwrap_or_default();
                // The nearest entry point of the nearest such vehicle.
                let target = vehicles
                    .vehicles
                    .iter()
                    .filter(|(v, ..)| v.template == *template)
                    .flat_map(|(_, view, data)| {
                        let entries = &data.0.desc.entry_points;
                        let points: Vec<(Vec3, f32)> = match entries.is_empty() {
                            true => vec![(view.transform.translation, 3.0)],
                            false => entries
                                .iter()
                                .map(|e| (view.transform.transform_point(Vec3::from_array(e.position)), e.radius))
                                .collect(),
                        };
                        points
                    })
                    .min_by(|a, b| a.0.distance(origin).total_cmp(&b.0.distance(origin)));
                let to = target.map(|(point, radius)| (point - (origin + Vec3::Y), radius));
                match to {
                    Some((to, radius)) if Vec2::new(to.x, to.z).length() > radius * 0.7 && elapsed < 150.0 => {
                        look.yaw = (-to.x).atan2(-to.z);
                        look.pitch = 0.0;
                        // Stuck on something: jump and sidestep for a moment.
                        let stuck = elapsed > 1.0 && speed < 1.0;
                        if stuck {
                            runner.mark = Some(Vec3::splat(now + 0.8));
                        }
                        let sidestepping = runner.mark.is_some_and(|until| now < until.x);
                        input.movement = Some(if sidestepping { Vec2::new(1.0, 0.3) } else { Vec2::Y });
                        input.buttons.set(Buttons::JUMP, stuck);
                        input.buttons.insert(Buttons::SPRINT);
                        Progress::Waiting
                    }
                    _ => {
                        match to {
                            None => warn!("scenario: no {template} to walk to"),
                            Some((to, _)) if elapsed >= 150.0 => {
                                warn!("scenario: gave up walking to {template}, {:.1} m to go", to.length())
                            }
                            _ => {}
                        }
                        runner.mark = None;
                        input.movement = None;
                        input.buttons.remove(Buttons::SPRINT | Buttons::JUMP);
                        Progress::Done
                    }
                }
            }
            Step::Seat(seat) => {
                vehicles.seat.0 = *seat;
                Progress::Done
            }
            Step::SeatSquad(template) => {
                if runner.frames > 0 {
                    Progress::Done
                } else {
                    runner.frames = 1;
                    let origin = soldier.single().map(|(m, _)| m.position).unwrap_or_default();
                    let my_team = local_team.single().ok().copied();
                    // Bots of our team on foot and alive: soldier, name, squad, leads it.
                    let on_foot: Vec<(Entity, String, u8, bool)> = others
                        .iter()
                        .filter(|(.., health)| health.current > 0.0)
                        .filter_map(|(soldier, owner, ..)| {
                            let (player, member, team) = bots.get(owner.0).ok()?;
                            let member = member.filter(|_| player.is_bot && Some(*team) == my_team)?;
                            Some((soldier, player.name.clone(), member.squad, member.leader))
                        })
                        .collect();
                    // The squad with the most members on foot, led by one of them.
                    let squad = on_foot
                        .iter()
                        .filter(|(.., leader)| *leader)
                        .max_by_key(|(_, _, squad, _)| on_foot.iter().filter(|m| m.2 == *squad).count())
                        .map(|(_, _, squad, _)| *squad);
                    let vehicle = nearest_vehicle(&vehicles, template, origin).and_then(|(e, _)| Some((e, vehicles.vehicles.get(e).ok()?.2)));
                    match (vehicle, squad) {
                        (Some((vehicle, data)), Some(squad)) => {
                            let taken: Vec<u8> = crews.iter().filter(|(s, _)| s.vehicle == vehicle).map(|(s, _)| s.seat).collect();
                            let mut free = (0..data.0.desc.seats.len() as u8).filter(|s| !taken.contains(s));
                            // The leader first.
                            let mut members: Vec<&(Entity, String, u8, bool)> = on_foot.iter().filter(|m| m.2 == squad).collect();
                            members.sort_by_key(|m| !m.3);
                            let mut seats = Vec::new();
                            for (bot, name, _, leader) in members {
                                let Some(seat) = free.next() else { break };
                                let open = data.0.desc.seats[seat as usize].open;
                                seats.push((*bot, seat, open));
                                info!(
                                    "scenario: seated {name}{} of squad {squad} in {template} seat {}",
                                    if *leader { " (leader)" } else { "" },
                                    seat + 1
                                );
                            }
                            seat_bots(&server, "SeatSquad", vehicle, seats);
                        }
                        _ => warn!("scenario: no {template} or no bot squad on foot to seat"),
                    }
                    Progress::Waiting
                }
            }
            Step::SeatBot(template, seat) => {
                // The seats taken show once the commands are applied: one frame.
                if runner.frames > 0 {
                    Progress::Done
                } else {
                    runner.frames = 1;
                    let origin = soldier.single().map(|(m, _)| m.position).unwrap_or_default();
                    let my_team = local_team.single().ok().copied();
                    match nearest_vehicle(&vehicles, template, origin).and_then(|(e, view)| Some((e, view, vehicles.vehicles.get(e).ok()?.2))) {
                        Some((vehicle, view, data)) => {
                            let taken: Vec<u8> = crews.iter().filter(|(s, _)| s.vehicle == vehicle).map(|(s, _)| s.seat).collect();
                            let count = data.0.desc.seats.len() as u8;
                            let wanted: Vec<u8> = match *seat {
                                0 => (0..count).filter(|s| !taken.contains(s)).collect(),
                                seat if seat <= count && !taken.contains(&(seat - 1)) => vec![seat - 1],
                                _ => Vec::new(),
                            };
                            let at = view.transform.translation;
                            let mut candidates: Vec<(Entity, String, f32)> = others
                                .iter()
                                .filter(|(.., health)| health.current > 0.0)
                                .filter_map(|(soldier, owner, motion, _)| {
                                    let (player, _, team) = bots.get(owner.0).ok()?;
                                    (player.is_bot && Some(*team) == my_team)
                                        .then(|| (soldier, player.name.clone(), motion.position.distance(at)))
                                })
                                .collect();
                            // Nearest last, to pop.
                            candidates.sort_by(|a, b| b.2.total_cmp(&a.2));
                            let mut seats = Vec::new();
                            for seat in wanted {
                                let Some((bot, name, _)) = candidates.pop() else {
                                    warn!("scenario: no teammate bot on foot for {template} seat {}", seat + 1);
                                    break;
                                };
                                let open = data.0.desc.seats[seat as usize].open;
                                seats.push((bot, seat, open));
                                info!("scenario: seated {name} in {template} seat {}", seat + 1);
                            }
                            seat_bots(&server, "SeatBot", vehicle, seats);
                        }
                        None => warn!("scenario: no {template} to seat bots in"),
                    }
                    Progress::Waiting
                }
            }
            Step::LiftVehicle(template, meters) => {
                let origin = soldier.single().map(|(m, _)| m.position).unwrap_or_default();
                match nearest_vehicle(&vehicles, template, origin).and_then(|(e, _)| Some((e, vehicles.motions.get(e).ok()?))) {
                    Some((vehicle, motion)) => {
                        use avian3d::prelude::{AngularVelocity, LinearVelocity, Position};
                        let position = motion.position + Vec3::Y * *meters;
                        on_vehicle(&server, "LiftVehicle", vehicle, move |vehicle| {
                            vehicle.insert((Position(position), LinearVelocity(Vec3::ZERO), AngularVelocity(Vec3::ZERO)));
                            // Not a crash.
                            vehicle.remove::<game_server::vehicles::LastVelocity>();
                        });
                        info!("scenario: lifted {template} to {position:.1}");
                    }
                    None => warn!("scenario: no {template} to lift"),
                }
                Progress::Done
            }
            Step::VehicleKeeps(label, height, speed, seconds) => {
                if runner.frames == 0 {
                    runner.frames = 1;
                    runner.mark = None;
                }
                let state = vehicles.seated.single().ok().and_then(|s| vehicles.vehicles.get(s.vehicle).ok()).map(|(_, view, _)| {
                    let filter = avian3d::prelude::SpatialQueryFilter::from_mask(game_shared::physics::GameLayer::World);
                    let t = view.transform.translation;
                    let altitude = vehicles.spatial.cast_ray(t, Dir3::NEG_Y, 2000.0, true, &filter).map_or(2000.0, |hit| hit.distance);
                    (altitude, view.velocity.length())
                });
                let (low, slow) = runner.mark.map_or((f32::MAX, f32::MAX), |m| (m.x, m.y));
                let (low, slow) = match state {
                    Some((altitude, velocity)) => (low.min(altitude), slow.min(velocity)),
                    None => (-1.0, -1.0),
                };
                runner.mark = Some(Vec3::new(low, slow, 0.0));
                if low < *height || slow < *speed {
                    let line = format!("{label}: at least {height:.1} m up at {speed:.1} m/s expected, lowest {low:.1} m, slowest {slow:.1} m/s after {elapsed:.1} s");
                    writeln!(runner.report, "{line}").ok();
                    finish(&mut runner, &mut exit, now, Some(line));
                    return;
                }
                if elapsed >= *seconds {
                    let line = format!("{label}: lowest {low:.1} m up, slowest {slow:.1} m/s over {seconds:.0} s");
                    info!("scenario: {line}");
                    writeln!(runner.report, "{line}").ok();
                    runner.mark = None;
                    Progress::Done
                } else {
                    Progress::Waiting
                }
            }
            Step::LogSeats(template) => {
                let origin = soldier.single().map(|(m, _)| m.position).unwrap_or_default();
                let line = match nearest_vehicle(&vehicles, template, origin).and_then(|(e, _)| Some((e, vehicles.vehicles.get(e).ok()?.2))) {
                    Some((vehicle, data)) => {
                        let seats: Vec<String> = (0..data.0.desc.seats.len() as u8)
                            .map(|seat| {
                                let who = crews
                                    .iter()
                                    .find(|(s, _)| s.vehicle == vehicle && s.seat == seat)
                                    .and_then(|(_, c)| players.get(c.0).ok())
                                    .map_or("free".to_string(), |p| p.name.clone());
                                format!("{} {who}", seat + 1)
                            })
                            .collect();
                        format!("seats of {template}: {}", seats.join(", "))
                    }
                    None => format!("seats of {template}: no such vehicle"),
                };
                info!("scenario: {line}");
                writeln!(runner.report, "{line}").ok();
                Progress::Done
            }
            Step::VehicleLook(yaw, pitch) => {
                let heading = vehicles
                    .seated
                    .single()
                    .ok()
                    .and_then(|s| vehicles.vehicles.get(s.vehicle).ok())
                    .map_or(0.0, |(_, view, _)| crate::vehicles::heading(view.transform.rotation));
                look.yaw = heading + yaw.to_radians();
                look.pitch = pitch.to_radians();
                // Piloting: the free look (it swings back ahead unless free look stays held,
                // `HoldKey(AltLeft)`), as far as it reaches.
                vehicles.flight.look = Vec2::new(
                    yaw.to_radians(),
                    pitch.to_radians().clamp(-crate::vehicles::FREE_LOOK_DOWN, crate::vehicles::FREE_LOOK_UP),
                );
                Progress::Done
            }
            Step::LogVehicles(label) => {
                let mut lines = Vec::new();
                let all = label.starts_with("all");
                for (entity, vehicle, view) in &vehicles.all {
                    let riders = vehicles.riders.iter().filter(|s| s.vehicle == entity).count();
                    if riders == 0 && view.speed.abs() < 0.5 && !all {
                        continue;
                    }
                    let p = view.transform.translation;
                    let hp = vehicles.health.get(entity).map_or(f32::NAN, |(.., h)| h.current);
                    lines.push(format!(
                        "{} at ({:.1}, {:.1}, {:.1}) heading {:.0}, {:.1} km/h, {hp:.0} hp, {riders} aboard",
                        vehicle.template,
                        p.x,
                        p.y,
                        p.z,
                        crate::vehicles::heading(view.transform.rotation).to_degrees(),
                        view.speed * 3.6
                    ));
                }
                let line = format!("{label}: {} vehicles; {}", vehicles.all.iter().count(), lines.join("; "));
                info!("scenario: {line}");
                writeln!(runner.report, "{line}").ok();
                Progress::Done
            }
            Step::ChaseDriven(filter, distance, height) => {
                let matches = |vehicle: &Vehicle, data: &VehicleData| {
                    let category = format!("{:?}", data.0.desc.category).to_lowercase();
                    filter.is_empty() || vehicle.template == *filter || category == filter.to_lowercase()
                };
                let driven = vehicles
                    .riders
                    .iter()
                    .filter(|seated| seated.seat == 0)
                    .filter_map(|seated| vehicles.vehicles.get(seated.vehicle).ok())
                    .filter(|(vehicle, _, data)| matches(vehicle, data))
                    .max_by(|a, b| a.1.velocity.length().total_cmp(&b.1.velocity.length()));
                match (driven, spectator.single_mut()) {
                    (Some((vehicle, view, _)), Ok(mut camera)) => {
                        let t = view.transform;
                        let back = (t.rotation * Vec3::Z).with_y(0.0).normalize_or(Vec3::Z);
                        let position = t.translation + back * *distance + Vec3::Y * *height;
                        camera.position = position;
                        let to = t.translation - position;
                        look.yaw = (-to.x).atan2(-to.z);
                        look.pitch = to.y.atan2(to.with_y(0.0).length());
                        let line = format!(
                            "chase {} at ({:.0}, {:.0}, {:.0}), {:.0} km/h",
                            vehicle.template,
                            t.translation.x,
                            t.translation.y,
                            t.translation.z,
                            view.velocity.length() * 3.6
                        );
                        info!("scenario: {line}");
                        writeln!(runner.report, "{line}").ok();
                    }
                    _ => warn!("scenario: no driven vehicle matching `{filter}` (or not spectating)"),
                }
                Progress::Done
            }
            Step::ChaseBot(what, seconds, distance, height) => {
                // The bots' minds are the server's: it answers a frame or two later.
                let answer = runner.chase_answer.as_ref().and_then(|rx| rx.try_recv().ok());
                if answer.is_some() {
                    runner.chase_answer = None;
                }
                let mut just_chosen = false;
                if runner.frames == 0 {
                    match answer {
                        Some(Some(view)) => {
                            let line = format!("chase bot {} ({what}: {}) after {elapsed:.1} s", view.name, view.doing);
                            info!("scenario: {line}");
                            writeln!(runner.report, "{line}").ok();
                            runner.chased = Some(view.player);
                            runner.chase_view = Some(view);
                            runner.frames = 1;
                            runner.samples.clear();
                            runner.step_started = Some(now);
                            just_chosen = true;
                        }
                        Some(None) if elapsed > 60.0 => {
                            warn!("scenario: no bot matching `{what}` to chase");
                            runner.chased = None;
                            runner.frames = 1;
                        }
                        _ => {}
                    }
                    if runner.frames == 0 && runner.chase_answer.is_none() {
                        let what = what.clone();
                        runner.chase_answer = server.query(move |world| {
                            let player = chase_pick(world, &what)?;
                            chase_view(world, player)
                        });
                        if runner.chase_answer.is_none() {
                            warn!("scenario: ChaseBot needs our own server (singleplayer or hosting)");
                            runner.chased = None;
                            runner.frames = 1;
                        }
                    }
                } else if let Some(Some(view)) = answer {
                    runner.chase_view = Some(view);
                }
                if let (Some(player), Ok(mut camera)) = (runner.chased.filter(|_| runner.frames > 0), spectator.single_mut())
                    && let Some(view) = runner.chase_view.clone()
                {
                    let motion = view.motion;
                    let forward = Quat::from_rotation_y(motion.yaw) * Vec3::NEG_Z;
                    let target = motion.position + Vec3::Y * 1.0;
                    let wanted = motion.position - forward * *distance + Vec3::Y * *height;
                    // Smoothly, so turning bots don't whip the camera round.
                    camera.position = if runner.frames == 1 { wanted } else { camera.position.lerp(wanted, 0.08) };
                    let to = target - camera.position;
                    look.yaw = (-to.x).atan2(-to.z);
                    look.pitch = to.y.atan2(to.with_y(0.0).length());
                    // What he does, every second.
                    if !just_chosen && elapsed >= runner.samples.len() as f32 {
                        runner.samples.push(elapsed);
                        let line = format!(
                            "chase {:.0}s: {} at ({:.0}, {:.0}, {:.0}) {} ({:?}{}{}, suppression {:.2})",
                            elapsed,
                            view.name,
                            motion.position.x,
                            motion.position.y,
                            motion.position.z,
                            view.doing,
                            motion.stance,
                            if view.target { ", enemy in sight" } else { "" },
                            match view.cover {
                                Some(true) => ", up from cover",
                                Some(false) => ", down in cover",
                                None => "",
                            },
                            view.suppression,
                        );
                        info!("scenario: {line}");
                        writeln!(runner.report, "{line}").ok();
                    }
                    if runner.chase_answer.is_none() {
                        runner.chase_answer = server.query(move |world| chase_view(world, player));
                    }
                    runner.frames += 1;
                }
                let done = !just_chosen && runner.frames > 0 && (runner.chased.is_none() || elapsed >= *seconds);
                if done {
                    runner.chase_answer = None;
                    runner.chase_view = None;
                }
                done_if(done)
            }
            Step::VehicleTrace(label, seconds) => {
                let due = runner.frames == 0 || elapsed >= runner.frames as f32 * 0.25;
                if due {
                    runner.frames += 1;
                    let line = vehicles.flight_line(label, elapsed);
                    info!("scenario: {line}");
                    writeln!(runner.report, "{line}").ok();
                }
                done_if(elapsed >= *seconds)
            }
            Step::ResponseTime(label) | Step::SteerResponseTime(label) => {
                let steering = matches!(step, Step::SteerResponseTime(_));
                let current = vehicles
                    .seated
                    .single()
                    .ok()
                    .and_then(|s| vehicles.vehicles.get(s.vehicle).ok())
                    .map(|(_, view, _)| view);
                let view = current.map(|view| match steering {
                    true => Vec3::new(view.joints.iter().map(|j| j[0].abs() + j[1].abs()).sum(), 0.0, 0.0),
                    false => view.velocity,
                });
                // Throttle: from standing still (waiting up to 3 s for it).
                let still = steering || current.is_none_or(|view| view.velocity.length() < 0.2);
                if runner.frames == 0 && !still && elapsed < 3.0 {
                    return;
                }
                if runner.frames == 0 {
                    runner.mark = view;
                    runner.step_started = Some(now);
                    input.movement = Some(if steering { Vec2::X } else { Vec2::Y });
                }
                let elapsed = now - runner.step_started.unwrap_or(now);
                runner.frames += 1;
                let threshold = if steering { 0.02 } else { 0.3 };
                let moved = view.zip(runner.mark).is_some_and(|(now, start)| now.distance(start) > threshold);
                if moved || elapsed > 3.0 || view.is_none() {
                    let line = match (moved, view) {
                        (true, _) => format!("{label}: moved after {:.0} ms", elapsed * 1000.0),
                        (false, Some(_)) => format!("{label}: didn't move within 3 s"),
                        (false, None) => format!("{label}: not in a vehicle"),
                    };
                    info!("scenario: {line}");
                    writeln!(runner.report, "{line}").ok();
                    input.movement = None;
                    Progress::Done
                } else {
                    Progress::Waiting
                }
            }
            Step::RateResponse(..) | Step::MouseResponse(..) => {
                // The input held: movement (steer, throttle), stick (roll, pitch), mouse.
                let (label, seconds, movement, stick, mouse) = match &step {
                    Step::RateResponse(label, (steer, throttle, roll, pitch), seconds) => {
                        (label, seconds, Vec2::new(*steer, *throttle), Vec2::new(*roll, *pitch), Vec2::ZERO)
                    }
                    Step::MouseResponse(label, mouse, seconds) => (label, seconds, Vec2::ZERO, Vec2::ZERO, Vec2::from(*mouse)),
                    _ => unreachable!(),
                };
                let rotation = vehicles
                    .seated
                    .single()
                    .ok()
                    .and_then(|s| vehicles.vehicles.get(s.vehicle).ok())
                    .map(|(_, view, _)| view.transform.rotation);
                let camera_rotation = vehicles.camera.single().ok().map(|t| t.rotation);
                let (Some(rotation), Some(camera_rotation)) = (rotation, camera_rotation) else {
                    let line = format!("{label}: not in a vehicle");
                    info!("scenario: {line}");
                    writeln!(runner.report, "{line}").ok();
                    runner.next += 1;
                    runner.step_started = None;
                    runner.frames = 0;
                    continue;
                };
                // 0 heading (world up), 1 pitch, 2 roll (right wing down), in degrees.
                let axis = if stick.x != 0.0 || mouse.x != 0.0 {
                    2
                } else if stick.y != 0.0 || mouse.y != 0.0 {
                    1
                } else {
                    0
                };
                let angle = |rotation: Quat| {
                    let forward = rotation * Vec3::NEG_Z;
                    let right = rotation * Vec3::X;
                    match axis {
                        0 => (-forward.x).atan2(-forward.z),
                        1 => forward.y.clamp(-1.0, 1.0).asin(),
                        _ => (-right.y).clamp(-1.0, 1.0).asin(),
                    }
                    .to_degrees()
                };
                if runner.frames == 0 {
                    runner.rates.clear();
                    runner.rate_started = vehicles.game_time.elapsed_secs_f64();
                    input.movement = Some(movement).filter(|m| *m != Vec2::ZERO);
                    input.stick = Some(stick).filter(|s| *s != Vec2::ZERO);
                    input.mouse = mouse;
                }
                // Unwrapped across ±180°.
                let unwrap = |now: f32, before: f32| before + (now - before + 540.0).rem_euclid(360.0) - 180.0;
                let (vehicle_angle, camera_angle) = match runner.rates.last() {
                    Some(&(_, v, c)) => (unwrap(angle(rotation), v), unwrap(angle(camera_rotation), c)),
                    None => (angle(rotation), angle(camera_rotation)),
                };
                // Game time: a slow frame rate (a busy machine) doesn't stretch the timings.
                let game = (vehicles.game_time.elapsed_secs_f64() - runner.rate_started) as f32;
                if runner.rates.last().is_none_or(|s| game > s.0) {
                    runner.rates.push((game, vehicle_angle, camera_angle));
                }
                runner.frames += 1;
                if game >= *seconds && (input.movement.is_some() || input.stick.is_some() || input.mouse != Vec2::ZERO) {
                    input.movement = None;
                    input.stick = None;
                    input.mouse = Vec2::ZERO;
                }
                if game < *seconds * 2.0 {
                    Progress::Waiting
                } else {
                    let fps = runner.frames as f32 / elapsed.max(0.001);
                    let mut line = format!("{} ({fps:.0} fps)", rate_report(label, &runner.rates, *seconds));
                    if mouse != Vec2::ZERO {
                        // How far one mouse movement turned it: when the mouse stopped, and
                        // once it had settled.
                        let start = runner.rates.first().map_or(0.0, |s| s.1);
                        let at = |t: f32| runner.rates.iter().find(|s| s.0 >= t).or(runner.rates.last()).map_or(0.0, |s| s.1 - start);
                        let counts = mouse * *seconds;
                        line += &format!(
                            "\n{label}: mouse ({:.0}, {:.0}) counts: turned {:.1}° when it stopped, {:.1}° after {:.1} s",
                            counts.x,
                            counts.y,
                            at(*seconds),
                            at(*seconds * 2.0),
                            *seconds * 2.0
                        );
                    }
                    for line in line.lines() {
                        info!("scenario: {line}");
                    }
                    writeln!(runner.report, "{line}").ok();
                    Progress::Done
                }
            }
            Step::PlaceVehicle(position, heading, speed) => {
                match vehicles.seated.single() {
                    Ok(seated) => {
                        use avian3d::prelude::{AngularVelocity, LinearVelocity, Position, Rotation};
                        let rotation = Quat::from_rotation_y(heading.to_radians());
                        let (position, speed) = (Vec3::from(*position), *speed);
                        on_vehicle(&server, "PlaceVehicle", seated.vehicle, move |vehicle| {
                            vehicle.insert((
                                Position(position),
                                Rotation(rotation),
                                LinearVelocity(rotation * Vec3::NEG_Z * speed),
                                AngularVelocity(Vec3::ZERO),
                            ));
                            // Not a crash.
                            vehicle.remove::<game_server::vehicles::LastVelocity>();
                        });
                    }
                    Err(_) => warn!("scenario: not in a vehicle to place"),
                }
                Progress::Done
            }
            Step::Mouse(right, up) => {
                input.mouse = Vec2::new(*right, *up);
                Progress::Done
            }
            Step::Stick(roll, pitch) => {
                let stick = Vec2::new(*roll, *pitch);
                input.stick = (stick != Vec2::ZERO).then_some(stick);
                Progress::Done
            }
            Step::VehicleInfo(label) => {
                let line = match vehicles.seated.single().ok().and_then(|s| {
                    vehicles.vehicles.get(s.vehicle).ok().map(|(_, v, d)| (s, v, d))
                }) {
                    Some((seated, view, data)) => {
                        let t = view.transform;
                        let up = (t.rotation * Vec3::Y).angle_between(Vec3::Y).to_degrees();
                        let model = &data.0;
                        // Aimed joints as yaw/pitch in degrees.
                        let aims: Vec<String> = model
                            .desc
                            .parts
                            .iter()
                            .enumerate()
                            .filter(|(_, p)| {
                                p.joint.as_ref().is_some_and(|j| {
                                    j.axes.iter().any(|a| {
                                        matches!(a.input, Some(JointInput::AimYaw | JointInput::AimPitch))
                                    })
                                })
                            })
                            .filter_map(|(i, p)| {
                                let a = view.joints.get(model.joint_index[i]?)?;
                                Some(format!("{} {:.0}/{:.0}", p.name, a[0].to_degrees(), a[1].to_degrees()))
                            })
                            .collect();
                        let wheels: Vec<String> = view.wheels.iter().map(|w| format!("{w:.2}")).collect();
                        // Where the camera looks: pitch from the horizon, and from the hull.
                        let camera_forward = vehicles.camera.single().map_or(Vec3::NEG_Z, |c| c.rotation * Vec3::NEG_Z);
                        let hull_forward = t.rotation.inverse() * camera_forward;
                        let camera = format!(
                            "camera pitch {:.0} deg ({:.0} from the hull)",
                            camera_forward.y.clamp(-1.0, 1.0).asin().to_degrees(),
                            hull_forward.y.clamp(-1.0, 1.0).asin().to_degrees()
                        );
                        format!(
                            "{label}: {} seat {} at ({:.2}, {:.2}, {:.2}), {:.1} km/h, heading {:.0} deg, tilt {:.1} deg, {camera}, aim [{}], wheels down [{}]",
                            model.desc.name,
                            seated.seat + 1,
                            t.translation.x,
                            t.translation.y,
                            t.translation.z,
                            view.speed * 3.6,
                            crate::vehicles::heading(t.rotation).to_degrees(),
                            up,
                            aims.join(", "),
                            wheels.join(" ")
                        )
                    }
                    None => format!("{label}: not in a vehicle"),
                };
                info!("scenario: {line}");
                writeln!(runner.report, "{line}").ok();
                Progress::Done
            }
            Step::Hold(buttons) => {
                buttons.iter().for_each(|b| input.buttons.insert(b.bits()));
                Progress::Done
            }
            Step::Release(buttons) => {
                buttons.iter().for_each(|b| input.buttons.remove(b.bits()));
                Progress::Done
            }
            Step::Move(right, forward) => {
                let movement = Vec2::new(*right, *forward);
                input.movement = (movement != Vec2::ZERO).then_some(movement);
                Progress::Done
            }
            Step::GamepadConnect => {
                let entity = commands.spawn_empty().id();
                gamepad.entity = Some(entity);
                gamepad_events.write(GamepadConnectionEvent::new(
                    entity,
                    GamepadConnection::Connected {
                        name: "Scenario gamepad".into(),
                        vendor_id: None,
                        product_id: None,
                    },
                ));
                Progress::Done
            }
            Step::GamepadDisconnect => {
                if let Some(entity) = gamepad.entity.take() {
                    gamepad_events.write(GamepadConnectionEvent::new(entity, GamepadConnection::Disconnected));
                    commands.entity(entity).despawn();
                }
                gamepad.buttons.clear();
                gamepad.left_stick = Vec2::ZERO;
                gamepad.right_stick = Vec2::ZERO;
                gamepad.triggers = Vec2::ZERO;
                Progress::Done
            }
            Step::GamepadHold(held) => {
                gamepad.buttons.extend(held.iter().copied());
                Progress::Done
            }
            Step::GamepadRelease(released) => {
                for button in released {
                    gamepad.buttons.remove(button);
                }
                Progress::Done
            }
            Step::GamepadLeftStick(x, y) => {
                gamepad.left_stick = Vec2::new(*x, *y);
                Progress::Done
            }
            Step::GamepadRightStick(x, y) => {
                gamepad.right_stick = Vec2::new(*x, *y);
                Progress::Done
            }
            Step::GamepadTriggers(left, right) => {
                gamepad.triggers = Vec2::new(*left, *right);
                Progress::Done
            }
            Step::Weapon(index) => {
                selection.index = *index;
                Progress::Done
            }
            Step::ThirdPerson(on) => {
                third_person.0 = *on;
                Progress::Done
            }
            Step::ChaseZoom(zoom) => {
                chase_zoom.0 = *zoom;
                Progress::Done
            }
            Step::Ssao(on) => {
                for camera in &camera {
                    if *on {
                        commands
                            .entity(camera)
                            .insert((ScreenSpaceAmbientOcclusion::default(), Msaa::Off));
                    } else {
                        commands
                            .entity(camera)
                            .remove::<ScreenSpaceAmbientOcclusion>()
                            .insert(Msaa::default());
                    }
                }
                Progress::Done
            }
            Step::Shadows(on) => {
                for mut sun in &mut suns {
                    sun.shadow_maps_enabled = *on;
                }
                Progress::Done
            }
            Step::Setting(key, value) => {
                let (key, value) = (key.clone(), value.clone());
                commands.queue(move |world: &mut World| {
                    if let Err(err) = world.resource_mut::<crate::settings::Settings>().set(&key, &value) {
                        warn!("scenario: {err}");
                    }
                });
                Progress::Done
            }
            Step::Key(key) => {
                key_event(*key, ButtonState::Pressed);
                runner.released.push(*key);
                Progress::Done
            }
            Step::Type(text) => {
                runner.typed.push_str(text);
                Progress::Done
            }
            Step::HoldKey(key) => {
                key_event(*key, ButtonState::Pressed);
                Progress::Done
            }
            Step::ReleaseKey(key) => {
                key_event(*key, ButtonState::Released);
                Progress::Done
            }
            Step::Click(name) => {
                match buttons.iter_mut().find(|(_, n, _)| n.as_str() == name) {
                    Some((_, _, mut interaction)) => *interaction = Interaction::Pressed,
                    None => warn!("scenario: no button named {name}"),
                }
                Progress::Done
            }
            Step::ScrollIntoView(name) => {
                match named.iter().find(|(_, n)| n.as_str() == name) {
                    Some((entity, _)) => commands.trigger(bevy::ui_widgets::ScrollIntoView { entity }),
                    None => warn!("scenario: no element named {name} to scroll into view"),
                }
                Progress::Done
            }
            Step::Focus(name) => {
                match named.iter().find(|(_, n)| n.as_str() == name) {
                    Some((entity, _)) => {
                        input_focus.set(entity, bevy::input_focus::FocusCause::Navigated);
                        commands.trigger(bevy::ui_widgets::ScrollIntoView { entity });
                    }
                    None => warn!("scenario: no element named {name} to focus"),
                }
                Progress::Done
            }
            Step::LogField(name) => {
                match fields.iter().find(|(n, _)| n.as_str() == name) {
                    Some((_, editable)) => info!("scenario: field {name}: {:?}", editable.value().to_string()),
                    None => warn!("scenario: no field named {name}"),
                }
                Progress::Done
            }
            Step::SetClipboard(value) => {
                if let Err(err) = clipboard.set_text(value.clone()) {
                    warn!("scenario: couldn't set the clipboard: {err:?}");
                }
                Progress::Done
            }
            Step::Kill => {
                // Beyond what a medic can bring back.
                hurt_us(&server, "Kill", |health| health.current = -1000.0);
                Progress::Done
            }
            Step::TakeStage => {
                server.run("TakeStage", take_stage);
                Progress::Done
            }
            Step::SetTeam(team) => {
                let team = game_shared::conquest::team_from_id(*team);
                server.run("SetTeam", move |world| {
                    if let Some(soldier) = link_soldier(world)
                        && let Some(mut health) = world.get_mut::<Health>(soldier)
                    {
                        health.current = -1000.0;
                    }
                    set_team(world, team);
                });
                Progress::Done
            }
            Step::Down => {
                hurt_us(&server, "Down", |health| health.current = 0.0);
                Progress::Done
            }
            Step::Hurt(amount) => {
                let amount = *amount;
                hurt_us(&server, "Hurt", move |health| health.current = (health.current - amount).max(1.0));
                Progress::Done
            }
            Step::SummonEnemy(distance) => {
                let team = local_team.single().ok().copied();
                match soldier.single() {
                    Ok((me, _)) => {
                        let origin = me.position;
                        let forward = Quat::from_rotation_y(look.yaw) * Vec3::NEG_Z;
                        let spot = origin + Vec3::new(forward.x, 0.0, forward.z).normalize_or(Vec3::NEG_Z) * *distance;
                        // The nearest living enemy (a body down and bleeding out is no use
                        // as a target); failing that, the nearest at all.
                        let nearest = others
                            .iter()
                            .filter(|(_, owner, ..)| {
                                let other = teams.get(owner.0).ok().copied();
                                other != team && other.is_some_and(|t| t != Team::Spectator)
                            })
                            .min_by(|a, b| {
                                (a.3.current <= 0.0, a.2.position.distance(origin))
                                    .partial_cmp(&(b.3.current <= 0.0, b.2.position.distance(origin)))
                                    .unwrap_or(std::cmp::Ordering::Equal)
                            });
                        match nearest {
                            Some((entity, ..)) => {
                                let yaw = look.yaw;
                                move_soldier(&server, "SummonEnemy", entity, move |motion, _| {
                                    motion.position = spot + Vec3::Y * 0.5;
                                    motion.velocity = Vec3::ZERO;
                                    motion.yaw = yaw;
                                });
                                info!("scenario: summoned enemy {entity} to {spot:.1}");
                            }
                            None => warn!("scenario: no enemy to summon"),
                        }
                    }
                    Err(_) => warn!("scenario: no soldier to summon an enemy to"),
                }
                Progress::Done
            }
            Step::Radio(command) => {
                radio.write(game_shared::radio::RadioRequest { command: *command });
                Progress::Done
            }
            Step::Commander(request) => {
                commander.write(*request);
                Progress::Done
            }
            Step::Squad(request) => {
                squad.write(*request);
                Progress::Done
            }
            Step::SendLoadout(class, weapon) => {
                let pick = game_shared::arsenal::ClassPick {
                    primary: (!weapon.is_empty()).then(|| weapon.clone()),
                    sidearm: None,
                };
                loadouts.write(game_shared::arsenal::LoadoutRequest {
                    picks: vec![(class.clone(), pick)],
                });
                Progress::Done
            }
            Step::CommanderClick((x, y, z)) => {
                commander_screen.click = Some(Vec3::new(*x, *y, *z));
                Progress::Done
            }
            Step::DeployZoom(factor) => {
                let center = deploy_screen.view.center;
                deploy_screen.view.zoom_at(*factor, center);
                Progress::Done
            }
            Step::DeployPan(dx, dy) => {
                deploy_screen.view.pan_uv(Vec2::new(*dx, *dy));
                Progress::Done
            }
            Step::Summon(health) => {
                let team = local_team.single().ok().copied();
                match soldier.single() {
                    Ok((me, _)) => {
                        let origin = me.position;
                        let forward = Quat::from_rotation_y(look.yaw) * Vec3::NEG_Z;
                        let spot = origin + Vec3::new(forward.x, 0.0, forward.z).normalize_or(Vec3::NEG_Z) * 1.5;
                        let any_teammate =
                            others.iter().any(|(_, owner, ..)| teams.get(owner.0).ok().copied() == team);
                        let nearest = others
                            .iter()
                            .filter(|(_, owner, ..)| !any_teammate || teams.get(owner.0).ok().copied() == team)
                            .min_by(|a, b| a.2.position.distance(origin).total_cmp(&b.2.position.distance(origin)));
                        match nearest {
                            Some((entity, ..)) => {
                                let (yaw, health) = (look.yaw, *health);
                                move_soldier(&server, "Summon", entity, move |motion, soldier_health| {
                                    motion.position = spot + Vec3::Y * 0.1;
                                    motion.velocity = Vec3::ZERO;
                                    motion.yaw = yaw + std::f32::consts::PI;
                                    soldier_health.current = health;
                                });
                                info!("scenario: summoned {entity} to {spot:.1} with {health} health");
                            }
                            None => warn!("scenario: no teammate to summon"),
                        }
                    }
                    Err(_) => warn!("scenario: no soldier to summon a teammate to"),
                }
                Progress::Done
            }
            Step::Parachute(height) => {
                match soldier.single() {
                    Ok((motion, _)) => {
                        let height = *height;
                        server.run("Parachute", move |world| {
                            move_us(world, |motion| {
                                motion.position.y += height;
                                motion.velocity = Vec3::ZERO;
                                motion.grounded = false;
                                motion.climbing = false;
                                motion.riding = false;
                                motion.swimming = false;
                                motion.parachute = true;
                            })
                        });
                        info!("scenario: parachute opened at {:.1}", motion.position + Vec3::Y * height);
                    }
                    Err(_) => warn!("scenario: no soldier to open a parachute"),
                }
                Progress::Done
            }
            Step::SummonParachute(right, ahead, turn) => {
                let team = local_team.single().ok().copied();
                match soldier.single() {
                    Ok((me, _)) => {
                        let origin = me.position;
                        let facing = Quat::from_rotation_y(look.yaw);
                        let spot = origin + facing * Vec3::new(*right, 0.0, -*ahead);
                        let any_teammate =
                            others.iter().any(|(_, owner, ..)| teams.get(owner.0).ok().copied() == team);
                        let nearest = others
                            .iter()
                            .filter(|(_, owner, ..)| !any_teammate || teams.get(owner.0).ok().copied() == team)
                            .min_by(|a, b| a.2.position.distance(origin).total_cmp(&b.2.position.distance(origin)));
                        match nearest {
                            Some((entity, ..)) => {
                                let heading = look.yaw + turn.to_radians();
                                move_soldier(&server, "SummonParachute", entity, move |motion, _| {
                                    motion.position = spot;
                                    motion.velocity = Quat::from_rotation_y(heading) * Vec3::new(0.0, -4.5, -8.0);
                                    motion.yaw = heading;
                                    motion.grounded = false;
                                    motion.climbing = false;
                                    motion.riding = false;
                                    motion.swimming = false;
                                    motion.parachute = true;
                                });
                                info!("scenario: {entity} parachuting at {spot:.1}");
                            }
                            None => warn!("scenario: no soldier to put under a parachute"),
                        }
                    }
                    Err(_) => warn!("scenario: no soldier to summon a parachutist to"),
                }
                Progress::Done
            }
            Step::DamageVehicle(amount) => {
                let origin = soldier.single().map(|(m, _)| m.position).unwrap_or_default();
                let nearest = vehicles
                    .health
                    .iter()
                    .min_by(|a, b| {
                        let d = |v: &VehicleView| v.transform.translation.distance(origin);
                        d(a.1).total_cmp(&d(b.1))
                    });
                match nearest {
                    Some((vehicle, _, health)) => {
                        let current = (health.current - amount).max(1.0);
                        on_vehicle(&server, "DamageVehicle", vehicle, move |vehicle| {
                            if let Some(mut health) = vehicle.get_mut::<VehicleHealth>() {
                                health.current = current;
                            }
                        });
                        info!("scenario: vehicle down to {:.0}/{:.0}", current, health.max);
                    }
                    None => warn!("scenario: no vehicle to damage"),
                }
                Progress::Done
            }
            Step::DamageVehicleAt(point, amount) => {
                let origin = Vec3::from(*point);
                let nearest = vehicles.health.iter().min_by(|a, b| {
                    let d = |v: &VehicleView| v.transform.translation.distance(origin);
                    d(a.1).total_cmp(&d(b.1))
                });
                match nearest {
                    Some((vehicle, _, health)) if health.current > 0.0 => {
                        let current = (health.current - amount).max(0.0);
                        on_vehicle(&server, "DamageVehicleAt", vehicle, move |vehicle| {
                            if let Some(mut health) = vehicle.get_mut::<VehicleHealth>() {
                                health.current = current;
                            }
                        });
                        info!("scenario: vehicle down to {:.0}/{:.0}", current, health.max);
                    }
                    Some(_) => warn!("scenario: that vehicle is a wreck already"),
                    None => warn!("scenario: no vehicle to damage"),
                }
                Progress::Done
            }
            Step::Vitals(name) => {
                let team = local_team.single().ok().copied();
                let (origin, own) = soldier
                    .single()
                    .map_or((Vec3::ZERO, -1.0), |(m, h)| (m.position, h.current));
                let ammo = local_inventory.single().map(|i| format!("{:?}", i.ammo)).unwrap_or_default();
                let mate = others
                    .iter()
                    .filter(|(_, owner, ..)| teams.get(owner.0).ok().copied() == team)
                    .min_by(|a, b| a.2.position.distance(origin).total_cmp(&b.2.position.distance(origin)))
                    .map_or(-1.0, |(.., h)| h.current);
                let vehicle = vehicles
                    .health
                    .iter()
                    .min_by(|a, b| {
                        let d = |v: &VehicleView| v.transform.translation.distance(origin);
                        d(a.1).total_cmp(&d(b.1))
                    })
                    .map_or(-1.0, |(.., h)| h.current);
                let line = format!("{name}: health {own:.1}, ammo {ammo}, teammate {mate:.1}, vehicle {vehicle:.1}");
                info!("scenario: {line}");
                writeln!(runner.report, "{line}").ok();
                Progress::Done
            }
            Step::LogStance(label) => {
                let stance = soldier.single().map(|(m, _)| m.stance).unwrap_or_default();
                info!("scenario: {label}: stance {stance:?}");
                Progress::Done
            }
            Step::Screenshot(name) => {
                if runner.frames == 0 {
                    let path = if name.ends_with(".png") {
                        PathBuf::from(name)
                    } else {
                        runner.out.join(format!("{name}.png"))
                    };
                    if let Some(parent) = path.parent() {
                        let _ = std::fs::create_dir_all(parent);
                    }
                    runner.screenshot_pending = true;
                    commands
                        .spawn(Screenshot::primary_window())
                        .observe(save_to_disk(path))
                        .observe(|_: On<ScreenshotCaptured>, mut runner: ResMut<Runner>| {
                            runner.screenshot_pending = false;
                        });
                }
                runner.frames += 1;
                done_if(!runner.screenshot_pending || elapsed > 10.0)
            }
            Step::Measure(name, seconds) => {
                if elapsed > 0.0 {
                    runner.samples.push(time.delta_secs() * 1000.0);
                } else if game_server::profile::enabled() {
                    // Start the system timings of this measurement afresh.
                    game_server::profile::take_report(1, 0);
                }
                if elapsed >= *seconds {
                    let frames = runner.samples.len() as u32;
                    let line = frame_stats(name, &mut runner.samples);
                    info!("scenario: {line}");
                    writeln!(runner.report, "{line}").ok();
                    let gpu = gpu_pass_times(&diagnostics);
                    if !gpu.is_empty() {
                        writeln!(runner.report, "{name}: GPU passes (ms, recent frames)
{gpu}").ok();
                    }
                    if game_server::profile::enabled() {
                        let systems = game_server::profile::take_report(frames, 300).replace("ms/tick", "ms/frame").replace("calls/tick", "calls/frame");
                        writeln!(runner.report, "{name}: systems (BF2_PROFILE_FRAMES)
{systems}").ok();
                    }
                    Progress::Done
                } else {
                    Progress::Waiting
                }
            }
            Step::JumpApex(label, seconds) => {
                if let Ok((m, _)) = soldier.single() {
                    // x: the starting height, y: the highest, z: seconds in the air.
                    let started = runner.mark.filter(|_| elapsed > 0.0);
                    let mut mark = started.unwrap_or(Vec3::new(m.position.y, m.position.y, 0.0));
                    mark.y = mark.y.max(m.position.y);
                    if !m.grounded {
                        mark.z += time.delta_secs();
                    }
                    runner.mark = Some(mark);
                }
                let done = elapsed >= *seconds;
                if done && let Some(mark) = runner.mark.take() {
                    let line = format!("{label}: apex {:.2} m, {:.2} s in the air", mark.y - mark.x, mark.z);
                    info!("scenario: {line}");
                    writeln!(runner.report, "{line}").ok();
                }
                done_if(done)
            }
            Step::NoHang(label, seconds) => {
                const LONGEST: f32 = 0.3;
                if elapsed == 0.0 {
                    runner.hang = None;
                }
                if let Ok((m, _)) = soldier.single() {
                    let (last, mut hung, mut longest) = runner.hang.unwrap_or((m.position, 0.0, 0.0));
                    let aloft = !m.grounded && !m.climbing && !m.riding && !m.swimming && !m.parachute && !m.mantling();
                    if aloft && m.position.distance(last) < 1e-3 {
                        hung += time.delta_secs();
                    } else {
                        hung = 0.0;
                    }
                    longest = longest.max(hung);
                    runner.hang = Some((m.position, hung, longest));
                }
                if elapsed >= *seconds {
                    let longest = runner.hang.take().map_or(0.0, |h| h.2);
                    let line = format!("{label}: longest hang in the air {longest:.2} s");
                    info!("scenario: {line}");
                    writeln!(runner.report, "{line}").ok();
                    if longest > LONGEST {
                        finish(&mut runner, &mut exit, now, Some(format!("{label}: hung in the air for {longest:.2} s")));
                        Progress::Waiting
                    } else {
                        Progress::Done
                    }
                } else {
                    Progress::Waiting
                }
            }
            Step::ViewRope(distance, angle) => {
                let rope = ropes
                    .iter()
                    .filter(|(_, r)| r.kind == game_data::RopeKind::Grapple)
                    .max_by_key(|(e, _)| e.index_u32())
                    .map(|(_, r)| *r);
                match (rope, soldier.single()) {
                    (Some(rope), Ok(_)) => {
                        let out = Quat::from_rotation_y(angle.to_radians()) * rope.out();
                        let feet = rope.end + out * (*distance + game_shared::rope::OFF_WALL) + Vec3::Y * 0.3;
                        let middle = (rope.top + rope.end) * 0.5 + Vec3::Y * 1.0;
                        let to = middle - (feet + Vec3::Y * 1.65);
                        place_us(&server, "ViewRope", feet);
                        let yaw = (-to.x).atan2(-to.z).to_degrees();
                        let pitch = to.y.atan2(Vec2::new(to.x, to.z).length()).to_degrees().min(45.0);
                        set_look(&mut look, yaw, pitch);
                        info!("scenario: at the rope from {feet:.1}");
                    }
                    (None, _) => warn!("scenario: no grappling rope to look at"),
                    (_, Err(_)) => warn!("scenario: no soldier to move to the rope"),
                }
                Progress::Done
            }
            Step::ExtraRopes(count) => {
                let newest = ropes
                    .iter()
                    .filter(|(_, r)| r.kind == game_data::RopeKind::Grapple)
                    .max_by_key(|(e, _)| e.index_u32())
                    .map(|(_, r)| *r);
                if let Some(rope) = newest {
                    let along = Vec3::Y.cross(rope.out());
                    let mut strung = 0;
                    let mut extras = Vec::new();
                    for i in 1..=*count as i32 {
                        let side = if i % 2 == 0 { -1.0 } else { 1.0 } * ((i + 1) / 2) as f32 * 1.5;
                        let hook = rope.anchor - rope.out() * 0.5 + along * side + Vec3::Y * 0.05;
                        if let Some(extra) = game_shared::rope::grapple(&spatial, hook, hook + rope.out() * 12.0, rope.length) {
                            extras.push(game_shared::rope::Rope { links: rope.links, ..extra });
                            strung += 1;
                        }
                    }
                    server.run("ExtraRopes", move |world| {
                        for rope in extras {
                            world.spawn((
                                rope,
                                game_shared::rope::RopeLife { owner: Entity::PLACEHOLDER, remaining: 60.0 },
                                bevy_replicon::prelude::Replicated,
                            ));
                        }
                    });
                    info!("scenario: strung {strung} more ropes");
                } else {
                    warn!("scenario: no grappling rope to string more beside");
                }
                Progress::Done
            }
            Step::Trace(name, seconds) => {
                if let Ok((m, _)) = soldier.single() {
                    let (p, v) = (m.position, m.velocity);
                    let water = level.as_ref().and_then(|l| l.desc.water.as_ref()).map(|w| w.height);
                    let depth = water.map(|w| w - p.y);
                    writeln!(
                        runner.report,
                        "{name} {elapsed:6.3} pos {:8.3} {:7.3} {:8.3} vel {:6.2} {:6.2} {:6.2} {} {:?} eye {:7.3} stamina {:.3}{}{}{} correction {:.4}",
                        p.x,
                        p.y,
                        p.z,
                        v.x,
                        v.y,
                        v.z,
                        if m.grounded { "G" } else { "-" },
                        m.stance,
                        rendered.single().map_or(0.0, |r| r.eye_position().y),
                        m.stamina,
                        if m.sprinting { " sprint" } else { "" },
                        if m.can_fire() { "" } else { " nofire" },
                        match (m.swimming, depth) {
                            (true, Some(d)) => format!(" swimming (depth {d:.2})"),
                            (false, Some(d)) if d > 0.0 => format!(" wading (depth {d:.2})"),
                            _ => String::new(),
                        },
                        prediction.last_correction,
                    )
                    .ok();
                }
                done_if(elapsed >= *seconds)
            }
            Step::Effect(name, position, count, seconds) => {
                crate::effects::spawn_around(&mut effects, name, Vec3::from(*position), *count, *seconds);
                Progress::Done
            }
            Step::Log(text) => {
                info!("scenario: {text}");
                writeln!(runner.report, "{text}").ok();
                Progress::Done
            }
            Step::ExpectLog(text, seconds) => {
                if let Some((at, _)) = log_capture::find(text, runner.log_cursor) {
                    runner.log_cursor = at + 1;
                    info!("scenario: saw \"{text}\" after {elapsed:.1} s");
                    Progress::Done
                } else if elapsed > *seconds {
                    let reason = format!("no log line with \"{text}\" within {seconds} s");
                    finish(&mut runner, &mut exit, now, Some(reason));
                    Progress::Waiting
                } else {
                    Progress::Waiting
                }
            }
            Step::ForbidLog(text) => {
                let from = log_capture::len();
                runner.forbidden.push((text.clone(), from));
                Progress::Done
            }
            Step::AllowLog(text) => {
                runner.forbidden.retain(|(forbidden, _)| forbidden != text);
                Progress::Done
            }
            Step::Dummy(pose) => {
                let pose = (!pose.is_empty()).then(|| pose.clone());
                server.run("Dummy", move |world| match world.get_resource_mut::<game_server::dummy::DummyControl>() {
                    Some(mut control) => control.pose = pose,
                    None => warn!("scenario: no server to make dummies on"),
                });
                Progress::Done
            }
            Step::HitGeometry(label, seconds) => match (elapsed == 0.0, &hitreg.request) {
                (true, _) => {
                    hitreg.request = Some(crate::hitreg::Request::Geometry {
                        label: label.clone(),
                        seconds: *seconds,
                    });
                    Progress::Waiting
                }
                (false, request) => done_if(request.is_none()),
            },
            Step::ShootAt(label, part, shots) => match (elapsed == 0.0, &hitreg.request) {
                (true, _) => {
                    hitreg.request = Some(crate::hitreg::Request::Shoot {
                        label: label.clone(),
                        part: part.clone(),
                        shots: *shots,
                    });
                    Progress::Waiting
                }
                (false, request) => done_if(request.is_none()),
            },
            Step::Quit => {
                finish(&mut runner, &mut exit, now, None);
                Progress::Waiting
            }
        };
        if progress == Progress::Waiting {
            return;
        }
        runner.next += 1;
        runner.step_started = None;
        runner.frames = 0;
        runner.samples.clear();
    }
}

/// Ends the run: writes `report.txt` (if anything was reported) and `result.txt`, and quits
/// with exit code 1 on a failure.
fn finish(runner: &mut Runner, exit: &mut MessageWriter<AppExit>, now: f32, failure: Option<String>) {
    runner.finished = true;
    // `--screenshot` runs have no output folder: nothing to write next to them.
    let files = !runner.out.as_os_str().is_empty();
    if files {
        let _ = std::fs::create_dir_all(&runner.out);
    }
    if files && !runner.report.is_empty() {
        let _ = std::fs::write(runner.out.join("report.txt"), &runner.report);
    }
    match failure {
        None => {
            if files {
                let _ = std::fs::write(runner.out.join("result.txt"), "PASS\n");
            }
            info!("scenario: finished after {now:.1} s");
            exit.write(AppExit::Success);
        }
        Some(reason) => {
            if files {
                let _ = std::fs::write(runner.out.join("result.txt"), format!("FAIL: {reason}\n"));
            }
            error!("scenario: FAILED after {now:.1} s: {reason}");
            exit.write(AppExit::error());
        }
    }
}

/// `RateResponse`'s report from its samples (seconds since the input, the vehicle's and the
/// camera's angle in degrees), the input held for `hold` seconds. Rates are taken over
/// 100 ms windows centred on each moment, so a slow frame doesn't read as a spike.
fn rate_report(label: &str, samples: &[(f32, f32, f32)], hold: f32) -> String {
    // The angle at a moment, between the frames around it.
    let at = |t: f32, pick: fn(&(f32, f32, f32)) -> f32| -> f32 {
        let i = samples.partition_point(|s| s.0 < t);
        match (i.checked_sub(1).and_then(|i| samples.get(i)), samples.get(i)) {
            (Some(a), Some(b)) if b.0 > a.0 => pick(a) + (pick(b) - pick(a)) * (t - a.0) / (b.0 - a.0),
            (_, Some(b)) => pick(b),
            (Some(a), None) => pick(a),
            (None, None) => 0.0,
        }
    };
    const WINDOW: f32 = 0.1;
    let rate = |t: f32, pick: fn(&(f32, f32, f32)) -> f32| (at(t + WINDOW * 0.5, pick) - at(t - WINDOW * 0.5, pick)) / WINDOW;
    let end = samples.last().map_or(0.0, |s| s.0);
    let times: Vec<f32> = (0..).map(|i| i as f32 * 0.01).take_while(|t| *t <= end - WINDOW * 0.5).collect();
    let describe = |name: &str, pick: fn(&(f32, f32, f32)) -> f32| {
        // Settled: the average over the last 0.2 s of the hold.
        let full = (at(hold, pick) - at(hold - 0.2, pick)) / 0.2;
        let sign = if full < 0.0 { -1.0 } else { 1.0 };
        let start = samples.first().map_or(0.0, pick);
        let ms = |t: Option<&f32>, from: f32| t.map_or("never".to_string(), |t| format!("{:.0} ms", (t - from) * 1000.0));
        let visible = samples.iter().find(|s| (pick(s) - start).abs() >= 0.5).map(|s| s.0);
        let reach = |share: f32| times.iter().find(|t| **t <= hold && rate(**t, pick) * sign >= full.abs() * share);
        let stop = times.iter().find(|t| **t >= hold && rate(**t, pick) * sign <= full.abs() * 0.1);
        let peak = times.iter().filter(|t| **t <= hold).map(|t| rate(*t, pick) * sign).fold(0.0, f32::max);
        format!(
            "{name}: moved 0.5° after {}, 50 % {}, 90 % {}, {:.1} deg/s at the end (peak {:.1}); let go: 10 % after {}",
            ms(visible.as_ref(), 0.0),
            ms(reach(0.5), 0.0),
            ms(reach(0.9), 0.0),
            full,
            peak * sign,
            ms(stop, hold),
        )
    };
    // The curve every 50 ms.
    let curve: Vec<String> = (1..=((hold * 2.0 / 0.05) as usize))
        .map(|i| i as f32 * 0.05)
        .filter(|t| *t <= end - WINDOW * 0.5)
        .map(|t| format!("{:.0}:{:.0}/{:.0}", t * 1000.0, rate(t, |s| s.1), rate(t, |s| s.2)))
        .collect();
    format!(
        "{label}: {}\n{label}: {}\n{label} curve ms:vehicle/camera deg/s: {}",
        describe("vehicle", |s| s.1),
        describe("camera", |s| s.2),
        curve.join(" ")
    )
}

/// The vehicle of this template nearest to a point.
fn nearest_vehicle<'a>(vehicles: &'a Vehicles, template: &str, origin: Vec3) -> Option<(Entity, &'a VehicleView)> {
    vehicles
        .all
        .iter()
        .filter(|(_, v, _)| v.template == template)
        .min_by(|a, b| a.2.transform.translation.distance(origin).total_cmp(&b.2.transform.translation.distance(origin)))
        .map(|(entity, _, view)| (entity, view))
}

fn done_if(done: bool) -> Progress {
    if done { Progress::Done } else { Progress::Waiting }
}

fn set_look(look: &mut LookState, yaw: f32, pitch: f32) {
    look.yaw = yaw.to_radians();
    look.pitch = pitch.to_radians();
}

/// The GPU (and render-encoding CPU) time of each render pass, averaged over the recent
/// frames, from `RenderDiagnosticsPlugin` (`--diagnostics`); empty without it.
fn gpu_pass_times(diagnostics: &bevy::diagnostic::DiagnosticsStore) -> String {
    let mut rows: Vec<(f64, String)> = diagnostics
        .iter()
        .filter(|d| d.path().as_str().ends_with("/elapsed_gpu") || d.path().as_str().ends_with("/elapsed_cpu"))
        .filter_map(|d| Some((d.average()?, d.path().as_str().to_string())))
        .collect();
    rows.sort_by(|a, b| b.0.total_cmp(&a.0));
    let mut out = String::new();
    for (ms, path) in rows.into_iter().take(40) {
        writeln!(out, "  {ms:>7.3}  {path}").ok();
    }
    out
}

/// `name: avg 5.8 ms (172 fps), p50 5.6 ms, p95 7.9 ms, max 12.1 ms over 520 frames`
fn frame_stats(name: &str, samples: &mut [f32]) -> String {
    if samples.is_empty() {
        return format!("{name}: no frames");
    }
    samples.sort_by(f32::total_cmp);
    let avg = samples.iter().sum::<f32>() / samples.len() as f32;
    let at = |q: f32| samples[((samples.len() - 1) as f32 * q) as usize];
    format!(
        "{name}: avg {avg:.2} ms ({:.0} fps), p50 {:.2} ms, p95 {:.2} ms, max {:.2} ms over {} frames",
        1000.0 / avg,
        at(0.5),
        at(0.95),
        samples[samples.len() - 1],
        samples.len()
    )
}

/// Log lines kept for `ExpectLog`/`ForbidLog`: [`log_capture_layer`] records them while a
/// scenario runs ([`enable_log_capture`]).
mod log_capture {
    use std::{
        fmt::Write as _,
        sync::{
            Mutex,
            atomic::{AtomicBool, Ordering},
        },
    };

    use bevy::log::{
        tracing::{self, field::Field},
        tracing_subscriber::{Layer, layer::Context},
    };

    pub static ENABLED: AtomicBool = AtomicBool::new(false);
    static LINES: Mutex<Vec<String>> = Mutex::new(Vec::new());
    /// A long soak logs a lot; later lines are dropped rather than growing without bound.
    const MAX_LINES: usize = 200_000;

    pub fn len() -> usize {
        LINES.lock().unwrap().len()
    }

    /// The first line at or after `from` that contains `text`.
    pub fn find(text: &str, from: usize) -> Option<(usize, String)> {
        let lines = LINES.lock().unwrap();
        lines.iter().enumerate().skip(from).find(|(_, line)| line.contains(text)).map(|(i, line)| (i, line.clone()))
    }

    pub struct CaptureLayer;

    impl<S: tracing::Subscriber> Layer<S> for CaptureLayer {
        fn on_event(&self, event: &tracing::Event<'_>, _: Context<'_, S>) {
            if !ENABLED.load(Ordering::Relaxed) {
                return;
            }
            let meta = event.metadata();
            let mut line = format!("{} {}: ", meta.level(), meta.target());
            event.record(&mut Visitor(&mut line));
            let mut lines = LINES.lock().unwrap();
            if lines.len() < MAX_LINES {
                lines.push(line);
            }
        }
    }

    struct Visitor<'a>(&'a mut String);

    impl tracing::field::Visit for Visitor<'_> {
        fn record_str(&mut self, field: &Field, value: &str) {
            if field.name() == "message" {
                self.0.push_str(value);
            } else {
                let _ = write!(self.0, " {}={value}", field.name());
            }
        }

        fn record_debug(&mut self, field: &Field, value: &dyn std::fmt::Debug) {
            if field.name() == "message" {
                let _ = write!(self.0, "{value:?}");
            } else {
                let _ = write!(self.0, " {}={value:?}", field.name());
            }
        }
    }
}

/// The log layer behind `ExpectLog`/`ForbidLog` (for `LogPlugin::custom_layer`).
/// With `BF2_PROFILE_FRAMES` set, also times every system (`game_server::profile`; needs a
/// build with `--features game_server/profile`) and `Measure` steps add the most expensive
/// ones per frame to the report.
pub fn log_capture_layer(app: &mut App) -> Option<bevy::log::BoxedLayer> {
    let capture: bevy::log::BoxedLayer = Box::new(log_capture::CaptureLayer);
    if std::env::var_os("BF2_PROFILE_FRAMES").is_none() {
        return Some(capture);
    }
    let layers: Vec<bevy::log::BoxedLayer> = [Some(capture), game_server::profile::layer(app)].into_iter().flatten().collect();
    Some(Box::new(layers))
}

/// Starts keeping log lines for the scenario's assertions.
pub fn enable_log_capture() {
    log_capture::ENABLED.store(true, Ordering::Relaxed);
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Every scenario in the repo still parses: a renamed or removed step breaks old scenarios
    /// only at run time otherwise.
    #[test]
    fn all_scenarios_parse() {
        let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../scenarios");
        let mut files = Vec::new();
        let mut dirs = vec![root];
        while let Some(dir) = dirs.pop() {
            for entry in std::fs::read_dir(&dir).unwrap() {
                let path = entry.unwrap().path();
                if path.is_dir() {
                    dirs.push(path);
                } else if path.extension().is_some_and(|e| e == "ron") {
                    files.push(path);
                }
            }
        }
        assert!(files.len() > 100, "found only {} scenarios", files.len());
        let failures: Vec<String> = files
            .iter()
            .filter_map(|path| Scenario::load(path).err().map(|e| format!("{}: {e}", path.display())))
            .collect();
        assert!(failures.is_empty(), "{} scenarios don't parse:\n{}", failures.len(), failures.join("\n"));
    }
}
