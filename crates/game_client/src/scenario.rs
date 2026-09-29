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
//! `ExpectLog` and `ForbidLog` steps turn a scenario into a check: they watch the log (captured
//! only while a scenario runs) and fail the run with exit code 1. Every run writes
//! `result.txt` (`PASS`, or `FAIL: <reason>`) next to its screenshots.
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
    soldier::{Health, Soldier, SoldierMotion},
    vehicle::{Seated, Vehicle, VehicleData, VehicleHealth},
};
use serde::Deserialize;

use crate::{
    Cli,
    camera::{PlayerCamera, Spectator, ThirdPerson},
    combat::WeaponSelection,
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
    /// Moves our soldier this many meters in front of the nearest vehicle of this template,
    /// facing it (singleplayer and listen server only).
    InFrontOf(String, f32),
    /// In a vehicle: moves to this seat (1-based), like pressing F1..F8.
    Seat(u8),
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
    /// Our soldier dies outright (singleplayer and listen server only).
    Kill,
    /// Our soldier is critically wounded: man down, waiting for a medic (singleplayer and
    /// listen server only, like the steps below).
    Down,
    /// Our soldier loses this much health (keeping at least 1).
    Hurt(f32),
    /// The nearest teammate (without one, the nearest soldier) is moved 1.5 m in front of
    /// us, facing us, with this much health: 0 or less wounds him critically (a body for the
    /// shock paddles).
    Summon(f32),
    /// The nearest enemy is moved this many meters in front of us, facing away (a target to
    /// spot).
    SummonEnemy(f32),
    /// Says something on the radio, as the commo rose would, e.g. `Radio(Spotted)`.
    Radio(game_shared::radio::RadioCommand),
    /// A commander request, as the deploy screen or the commander screen would send it,
    /// e.g. `Commander(Apply)`, `Commander(Use(asset: Artillery, target: (-184.0, 157.0, -100.0)))`.
    Commander(game_shared::commander::CommanderRequest),
    /// Clicks the commander screen's map at this world position (height ignored).
    CommanderClick((f32, f32, f32)),
    /// The nearest vehicle loses this many hit points (keeping at least 1).
    DamageVehicle(f32),
    /// The vehicle nearest to a point loses this many hit points; at 0 it is destroyed.
    DamageVehicleAt((f32, f32, f32), f32),
    /// Adds our health and ammo, the nearest teammate's health and the nearest vehicle's
    /// hit points to the report.
    Vitals(String),
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
    /// Passes once a log line containing the text has been logged since the scenario began
    /// (or since the line the previous `ExpectLog` matched, so expectations go in order),
    /// waiting up to this many seconds; otherwise the scenario fails, e.g.
    /// `ExpectLog("captured", 45.0)`.
    ExpectLog(String, f32),
    /// Fails the scenario as soon as a log line containing the text appears from this step
    /// on, e.g. `ForbidLog("ERROR ")` or `ForbidLog("prediction correction")`.
    ForbidLog(String),
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
        .add_systems(
            bevy::app::PreUpdate,
            inject_gamepad.before(bevy::input::InputSystems),
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
    /// Text of the last `Type` step, typed the next frame.
    typed: String,
    /// Captured log lines before this index are done with for `ExpectLog`.
    log_cursor: usize,
    /// `ForbidLog` texts and the log index they apply from.
    forbidden: Vec<(String, usize)>,
    finished: bool,
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
    buttons: Query<'w, 's, (&'static Name, &'static mut Interaction)>,
    effects: MessageWriter<'w, crate::effects::SpawnEffect>,
    radio: MessageWriter<'w, game_shared::radio::RadioRequest>,
    commander: MessageWriter<'w, game_shared::commander::CommanderRequest>,
    commander_screen: ResMut<'w, crate::commander::CommanderScreen>,
    gamepad: ResMut<'w, ScenarioGamepad>,
    /// `gamepad_connection_system` (which attaches/detaches the `Gamepad` component) listens
    /// for this directly, not for `RawGamepadEvent::Connection` (that variant only feeds the
    /// aggregate `GamepadEvent` stream for observers, per `gamepad_event_processing_system`).
    gamepad_events: MessageWriter<'w, GamepadConnectionEvent>,
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
    health: Query<'w, 's, (&'static VehicleView, &'static mut VehicleHealth)>,
}

impl Vehicles<'_, '_> {
    /// `label t: at (x, y, z) 320 km/h, 85 m up (climb 3.2 m/s), heading 12, pitch 4, roll -30, engine 0.8`
    fn flight_line(&self, label: &str, elapsed: f32) -> String {
        let Some((view, state)) = self
            .seated
            .single()
            .ok()
            .and_then(|s| Some((self.vehicles.get(s.vehicle).ok()?.1, self.states.get(s.vehicle).ok()?)))
        else {
            return format!("{label} {elapsed:5.2}: not in a vehicle");
        };
        let t = view.transform;
        let filter = avian3d::prelude::SpatialQueryFilter::from_mask(game_shared::physics::GameLayer::World);
        let altitude = self
            .spatial
            .cast_ray(t.translation, Dir3::NEG_Y, 2000.0, true, &filter)
            .map_or(f32::NAN, |hit| hit.distance);
        let forward = t.rotation * Vec3::NEG_Z;
        let right = t.rotation * Vec3::X;
        format!(
            "{label} {elapsed:5.2}: at ({:.1}, {:.1}, {:.1}) {:.0} km/h, {altitude:.1} m up (climb {:.1} m/s), heading {:.0}, pitch {:.0}, roll {:.0}, engine {:.2} gear {}, replayed {} corrected {:.3}",
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
    /// Rush's charges and the mode, for `NearCharge`.
    charges: Query<'w, 's, &'static game_shared::modes::Charge>,
    modes: Query<'w, 's, &'static game_shared::modes::ModeState>,
}

#[derive(Clone, Copy, PartialEq)]
enum Progress {
    Done,
    Waiting,
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
    (prediction, rendered): (
        Res<crate::prediction::PredictionStats>,
        Query<&crate::prediction::SoldierRender, With<LocalSoldier>>,
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
        mut commander_screen,
        mut gamepad,
        mut gamepad_events,
    } = player;
    let Soldiers {
        local: mut soldier,
        local_inventory,
        mut others,
        teams,
        local_team,
        drawn,
        spatial,
        charges,
        modes,
    } = soldiers;
    let now = time.elapsed_secs();
    if runner.finished {
        return;
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
            logical_key: Key::Unidentified(NativeKey::Unidentified),
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
                if let Ok((mut motion, _)) = soldier.single_mut() {
                    motion.position = Vec3::from(*position);
                    motion.velocity = Vec3::ZERO;
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
                let nearest = vehicles
                    .vehicles
                    .iter()
                    .filter(|(v, ..)| v.template == *template)
                    .min_by(|a, b| {
                        let d = |v: &VehicleView| v.transform.translation.distance(origin);
                        d(a.1).total_cmp(&d(b.1))
                    });
                match (nearest, soldier.single_mut()) {
                    (Some((_, view, data)), Ok((mut motion, _))) => {
                        let desc = &data.0.desc;
                        // Beside the door, outside the hull, within reach of the entry point.
                        let local = match desc.entry_points.first() {
                            Some(entry) => {
                                let side = (desc.physics.bounds[0][0] - 0.8).max(entry.position[0] - entry.radius * 0.6);
                                Vec3::new(side, (entry.position[1] - 1.0).min(0.0), entry.position[2])
                            }
                            None => Vec3::new(desc.physics.bounds[0][0] - 0.8, 0.0, 0.0),
                        };
                        let target = view.transform.transform_point(local);
                        motion.position = target;
                        motion.velocity = Vec3::ZERO;
                        let to = view.transform.translation - target;
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
                match (charge, soldier.single_mut()) {
                    (Some(charge), Ok((mut motion, _))) => {
                        let front = Quat::from_rotation_y(charge.yaw) * Vec3::NEG_Z;
                        let target = charge.position + front * 1.4 + Vec3::Y * 0.1;
                        motion.position = target;
                        motion.velocity = Vec3::ZERO;
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
                    .and_then(|(e, ..)| vehicles.vehicles.get(e).ok());
                match (nearest, soldier.single_mut()) {
                    (Some((_, view, data)), Ok((mut motion, _))) => {
                        let front = data.0.desc.physics.bounds[0][2];
                        let target = view.transform.transform_point(Vec3::new(0.0, 0.5, front - distance));
                        motion.position = target;
                        motion.velocity = Vec3::ZERO;
                        let to = view.transform.translation - target;
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
            Step::VehicleLook(yaw, pitch) => {
                let heading = vehicles
                    .seated
                    .single()
                    .ok()
                    .and_then(|s| vehicles.vehicles.get(s.vehicle).ok())
                    .map_or(0.0, |(_, view, _)| crate::vehicles::heading(view.transform.rotation));
                look.yaw = heading + yaw.to_radians();
                look.pitch = pitch.to_radians();
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
                    let hp = vehicles.health.get(entity).map_or(f32::NAN, |(_, h)| h.current);
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
            Step::PlaceVehicle(position, heading, speed) => {
                match vehicles.seated.single() {
                    Ok(seated) => {
                        use avian3d::prelude::{AngularVelocity, LinearVelocity, Position, Rotation};
                        let rotation = Quat::from_rotation_y(heading.to_radians());
                        commands.entity(seated.vehicle).insert((
                            Position(Vec3::from(*position)),
                            Rotation(rotation),
                            LinearVelocity(rotation * Vec3::NEG_Z * *speed),
                            AngularVelocity(Vec3::ZERO),
                        ));
                    }
                    Err(_) => warn!("scenario: not in a vehicle to place"),
                }
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
                        format!(
                            "{label}: {} seat {} at ({:.2}, {:.2}, {:.2}), {:.1} km/h, heading {:.0} deg, tilt {:.1} deg, aim [{}], wheels down [{}]",
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
                match buttons.iter_mut().find(|(n, _)| n.as_str() == name) {
                    Some((_, mut interaction)) => *interaction = Interaction::Pressed,
                    None => warn!("scenario: no button named {name}"),
                }
                Progress::Done
            }
            Step::Kill => {
                // Beyond what a medic can bring back.
                for (_, mut health) in &mut soldier {
                    health.current = -1000.0;
                }
                Progress::Done
            }
            Step::Down => {
                for (_, mut health) in &mut soldier {
                    health.current = 0.0;
                }
                Progress::Done
            }
            Step::Hurt(amount) => {
                for (_, mut health) in &mut soldier {
                    health.current = (health.current - amount).max(1.0);
                }
                Progress::Done
            }
            Step::SummonEnemy(distance) => {
                let team = local_team.single().ok().copied();
                match soldier.single() {
                    Ok((me, _)) => {
                        let origin = me.position;
                        let forward = Quat::from_rotation_y(look.yaw) * Vec3::NEG_Z;
                        let spot = origin + Vec3::new(forward.x, 0.0, forward.z).normalize_or(Vec3::NEG_Z) * *distance;
                        let nearest = others
                            .iter_mut()
                            .filter(|(_, owner, ..)| {
                                let other = teams.get(owner.0).ok().copied();
                                other != team && other.is_some_and(|t| t != Team::Spectator)
                            })
                            .min_by(|a, b| a.2.position.distance(origin).total_cmp(&b.2.position.distance(origin)));
                        match nearest {
                            Some((entity, _, mut motion, _)) => {
                                motion.position = spot + Vec3::Y * 0.5;
                                motion.velocity = Vec3::ZERO;
                                motion.yaw = look.yaw;
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
            Step::CommanderClick((x, y, z)) => {
                commander_screen.click = Some(Vec3::new(*x, *y, *z));
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
                            .iter_mut()
                            .filter(|(_, owner, ..)| !any_teammate || teams.get(owner.0).ok().copied() == team)
                            .min_by(|a, b| a.2.position.distance(origin).total_cmp(&b.2.position.distance(origin)));
                        match nearest {
                            Some((entity, _, mut motion, mut soldier_health)) => {
                                motion.position = spot + Vec3::Y * 0.1;
                                motion.velocity = Vec3::ZERO;
                                motion.yaw = look.yaw + std::f32::consts::PI;
                                soldier_health.current = *health;
                                info!("scenario: summoned {entity} to {spot:.1} with {health} health");
                            }
                            None => warn!("scenario: no teammate to summon"),
                        }
                    }
                    Err(_) => warn!("scenario: no soldier to summon a teammate to"),
                }
                Progress::Done
            }
            Step::DamageVehicle(amount) => {
                let origin = soldier.single().map(|(m, _)| m.position).unwrap_or_default();
                let nearest = vehicles
                    .health
                    .iter_mut()
                    .min_by(|a, b| {
                        let d = |v: &VehicleView| v.transform.translation.distance(origin);
                        d(a.0).total_cmp(&d(b.0))
                    });
                match nearest {
                    Some((_, mut health)) => {
                        health.current = (health.current - amount).max(1.0);
                        info!("scenario: vehicle down to {:.0}/{:.0}", health.current, health.max);
                    }
                    None => warn!("scenario: no vehicle to damage"),
                }
                Progress::Done
            }
            Step::DamageVehicleAt(point, amount) => {
                let origin = Vec3::from(*point);
                let nearest = vehicles.health.iter_mut().min_by(|a, b| {
                    let d = |v: &VehicleView| v.transform.translation.distance(origin);
                    d(a.0).total_cmp(&d(b.0))
                });
                match nearest {
                    Some((_, mut health)) if health.current > 0.0 => {
                        health.current = (health.current - amount).max(0.0);
                        info!("scenario: vehicle down to {:.0}/{:.0}", health.current, health.max);
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
                        d(a.0).total_cmp(&d(b.0))
                    })
                    .map_or(-1.0, |(_, h)| h.current);
                let line = format!("{name}: health {own:.1}, ammo {ammo}, teammate {mate:.1}, vehicle {vehicle:.1}");
                info!("scenario: {line}");
                writeln!(runner.report, "{line}").ok();
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
                }
                if elapsed >= *seconds {
                    let line = frame_stats(name, &mut runner.samples);
                    info!("scenario: {line}");
                    writeln!(runner.report, "{line}").ok();
                    Progress::Done
                } else {
                    Progress::Waiting
                }
            }
            Step::Trace(name, seconds) => {
                if let Ok((m, _)) = soldier.single() {
                    let (p, v) = (m.position, m.velocity);
                    writeln!(
                        runner.report,
                        "{name} {elapsed:6.3} pos {:8.3} {:7.3} {:8.3} vel {:6.2} {:6.2} {:6.2} {} {:?} eye {:7.3} stamina {:.3}{}{} correction {:.4}",
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
    let _ = std::fs::create_dir_all(&runner.out);
    if !runner.report.is_empty() {
        let _ = std::fs::write(runner.out.join("report.txt"), &runner.report);
    }
    match failure {
        None => {
            let _ = std::fs::write(runner.out.join("result.txt"), "PASS\n");
            info!("scenario: finished after {now:.1} s");
            exit.write(AppExit::Success);
        }
        Some(reason) => {
            let _ = std::fs::write(runner.out.join("result.txt"), format!("FAIL: {reason}\n"));
            error!("scenario: FAILED after {now:.1} s: {reason}");
            exit.write(AppExit::error());
        }
    }
}

fn done_if(done: bool) -> Progress {
    if done { Progress::Done } else { Progress::Waiting }
}

fn set_look(look: &mut LookState, yaw: f32, pitch: f32) {
    look.yaw = yaw.to_radians();
    look.pitch = pitch.to_radians();
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
pub fn log_capture_layer(_app: &mut App) -> Option<bevy::log::BoxedLayer> {
    Some(Box::new(log_capture::CaptureLayer))
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
