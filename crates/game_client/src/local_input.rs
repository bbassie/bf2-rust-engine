//! Turning keyboard and mouse into [`InputFrame`]s, once per simulation tick.
//!
//! Crouch, prone and sprint can each be held or toggled ([`Settings::crouch_mode`] and
//! friends; BF2 defaults: prone toggles, the other two are held). Toggling is resolved here,
//! client-side only ([`apply_stance_buttons`]): the server and prediction only ever see the
//! resulting [`Buttons`], exactly as if the player had held the key down.

use std::{
    collections::VecDeque,
    f32::consts::{FRAC_PI_2, TAU},
};

use bevy::{
    ecs::system::SystemParam,
    input::{
        gamepad::{Gamepad, GamepadButton},
        mouse::AccumulatedMouseMotion,
    },
    prelude::*,
    window::{CursorGrabMode, CursorOptions},
};
use game_shared::{
    input::{Buttons, INPUT_REDUNDANCY, InputFrame, InputPacket},
    revive::Downed,
    soldier::SoldierMotion,
    vehicle::Seated,
};

use crate::{
    menu::{Menu, Screen},
    net::LocalSoldier,
    settings::{Action, Actions, Settings, StanceMode, gamepad_activity},
};

pub struct LocalInputPlugin;

impl Plugin for LocalInputPlugin {
    fn build(&self, app: &mut App) {
        app.init_resource::<LookState>()
            .init_resource::<InputHistory>()
            .init_resource::<StanceToggles>()
            .add_systems(Update, (grab_cursor, mouse_look).chain().in_set(LookSystems))
            .add_systems(FixedUpdate, (reset_stance_toggles, build_input).chain().in_set(LocalInputSystems));
    }
}

/// Runs in `Update` when the mouse turns the view.
#[derive(SystemSet, Debug, Clone, PartialEq, Eq, Hash)]
pub struct LookSystems;

/// Runs in `FixedUpdate` when this tick's [`InputFrame`] is produced.
#[derive(SystemSet, Debug, Clone, PartialEq, Eq, Hash)]
pub struct LocalInputSystems;

/// Radians per mouse count at sensitivity 1.
pub const BASE_SENSITIVITY: f32 = 0.0022;

/// Where the player is looking. Updated every frame from the mouse, sampled every tick.
#[derive(Resource)]
pub struct LookState {
    pub yaw: f32,
    pub pitch: f32,
    pub sensitivity: f32,
    pub invert_y: bool,
    /// Sensitivity multiplier while zoomed in.
    pub zoom_scale: f32,
    /// Exponential smoothing (0..0.95) applied to the mouse delta before it turns the view.
    pub smoothing: f32,
    /// The smoothed mouse delta kept between frames.
    smoothed_delta: Vec2,
    /// Set when jump is pressed between ticks so short taps aren't lost.
    jump_latched: bool,
    /// Same idea for the stance keys: `build_input` runs in `FixedUpdate`, which can run less
    /// often than `Update` sees key presses, so a quick tap could otherwise never register as
    /// `just_pressed` there. Latching the edge (not just the held state) matters for these
    /// three because a toggled stance (`Settings::{prone,crouch,sprint}_mode`) flips on the
    /// press, not the hold.
    prone_latched: bool,
    crouch_latched: bool,
    sprint_latched: bool,
}

impl Default for LookState {
    fn default() -> Self {
        Self {
            yaw: 0.0,
            pitch: 0.0,
            sensitivity: BASE_SENSITIVITY,
            invert_y: false,
            zoom_scale: 1.0,
            smoothing: 0.0,
            smoothed_delta: Vec2::ZERO,
            jump_latched: false,
            prone_latched: false,
            crouch_latched: false,
            sprint_latched: false,
        }
    }
}

impl LookState {
    pub fn rotation(&self) -> Quat {
        Quat::from_euler(EulerRot::YXZ, self.yaw, self.pitch, 0.0)
    }
}

/// Recently produced input frames, kept for prediction replay.
#[derive(Resource, Default)]
pub struct InputHistory {
    pub frames: VecDeque<InputFrame>,
    next_seq: u32,
}

impl InputHistory {
    const MAX: usize = 256;

    pub fn latest(&self) -> Option<&InputFrame> {
        self.frames.back()
    }
}

/// Whether gameplay input is captured: the cursor is locked, or a scripted run is in a
/// playable screen ([`SCRIPTED_CAPTURE`]).
pub fn cursor_locked(cursor: &CursorOptions) -> bool {
    cursor.grab_mode != CursorGrabMode::None || SCRIPTED_CAPTURE.load(std::sync::atomic::Ordering::Relaxed)
}

/// A scripted run (scenario) never grabs the OS cursor, so agents' test runs don't take the
/// mouse from whoever is using the machine; instead, while it is in a playable screen, its
/// `Key`/`HoldKey` steps count as captured input. Set by `grab_cursor`.
static SCRIPTED_CAPTURE: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

/// A click in the game takes the mouse; menus, the deploy screen, the scoreboard's mute chips
/// and losing focus give it back (Esc opens the in-game menu, see `menu`).
fn grab_cursor(
    mut cursor: Single<&mut CursorOptions>,
    window: Single<&Window>,
    mouse: Res<ButtonInput<MouseButton>>,
    deploy: Res<crate::deploy::DeployScreen>,
    commander: Res<crate::commander::CommanderScreen>,
    screen: Res<State<Screen>>,
    menu: Res<Menu>,
    gamepads: Query<&Gamepad>,
    // The scoreboard's mute chips want the mouse (`voice::ui`).
    scoreboard_mouse: Res<crate::voice::ScoreboardCursor>,
    // A scripted run never grabs the cursor: see `SCRIPTED_CAPTURE`.
    scenario: Option<Res<crate::scenario::ScenarioInput>>,
) {
    let playing =
        *screen.get() == Screen::InGame && !menu.paused && !deploy.open && !commander.open && !scoreboard_mouse.0;
    if scenario.is_some() {
        SCRIPTED_CAPTURE.store(playing, std::sync::atomic::Ordering::Relaxed);
        if cursor.grab_mode != CursorGrabMode::None {
            cursor.visible = true;
            cursor.grab_mode = CursorGrabMode::None;
        }
        return;
    }
    let focused = window.focused;
    if !playing || !focused {
        if cursor_locked(&cursor) {
            cursor.visible = true;
            cursor.grab_mode = CursorGrabMode::None;
        }
        return;
    }
    if cursor_locked(&cursor) {
        return;
    }
    // A click grabs it, same as ever; moving a gamepad also does, since a controller player
    // may never click the mouse at all.
    let gamepad_active = gamepads.iter().any(|g| gamepad_activity(g) > 0.05);
    if mouse.just_pressed(MouseButton::Left) || gamepad_active {
        cursor.visible = false;
        cursor.grab_mode = CursorGrabMode::Locked;
    }
}

/// Radial dead zone: below `deadzone` the stick reads as centered, above it the remaining
/// travel is rescaled to still reach the full range at the edge.
pub fn deadzone(stick: Vec2, deadzone: f32) -> Vec2 {
    let len = stick.length();
    if len <= deadzone || len <= 0.0001 {
        return Vec2::ZERO;
    }
    stick * ((len - deadzone) / (1.0 - deadzone) / len).min(1.0 / len.max(0.0001))
}

fn mouse_look(
    motion: Res<AccumulatedMouseMotion>,
    actions: Actions,
    cursor: Single<&CursorOptions>,
    flight: Res<crate::vehicles::FlightStick>,
    settings: Res<Settings>,
    time: Res<Time>,
    mut look: ResMut<LookState>,
) {
    // Piloting: the mouse and the gamepad's right stick move the flight stick instead of
    // looking around (see `vehicles::fly`).
    if !cursor_locked(&cursor) || flight.active {
        return;
    }
    let sensitivity = look.sensitivity * look.zoom_scale;
    let delta = if look.smoothing > 0.0 {
        // Exponential smoothing: heavier smoothing lags more but shakes less.
        let alpha = 1.0 - look.smoothing;
        look.smoothed_delta = look.smoothed_delta.lerp(motion.delta, alpha.max(0.02));
        look.smoothed_delta
    } else {
        motion.delta
    };
    let vertical = if look.invert_y { -delta.y } else { delta.y };
    look.yaw -= delta.x * sensitivity;
    look.pitch = (look.pitch - vertical * sensitivity).clamp(-FRAC_PI_2 + 0.02, FRAC_PI_2 - 0.02);
    // Gamepad look: the right stick, read as a per-second turn rate (unlike the mouse delta,
    // which is already a per-frame pixel count).
    if let Some(gamepad) = actions.gamepad() {
        let stick = deadzone(gamepad.right_stick(), settings.gamepad.look_deadzone);
        if stick != Vec2::ZERO {
            let rate = BASE_SENSITIVITY * 60.0 * settings.gamepad.look_sensitivity.max(0.0);
            let gv = if settings.gamepad.invert_look_y { -stick.y } else { stick.y };
            look.yaw -= stick.x * rate * time.delta_secs();
            look.pitch = (look.pitch - gv * rate * time.delta_secs()).clamp(-FRAC_PI_2 + 0.02, FRAC_PI_2 - 0.02);
        }
    }
    // Keeps `f32` precision from degrading over a long session of turning the same way.
    look.yaw = look.yaw.rem_euclid(TAU);
    if actions.just_pressed(Action::Jump) {
        look.jump_latched = true;
    }
    if actions.just_pressed(Action::Prone) {
        look.prone_latched = true;
    }
    if actions.just_pressed(Action::Crouch) {
        look.crouch_latched = true;
    }
    if actions.just_pressed(Action::Sprint) {
        look.sprint_latched = true;
    }
}

/// Client-side latch for stances set to "toggle" in [`Settings`] (`prone_mode`, `crouch_mode`,
/// `sprint_mode`); a stance left on "hold" ignores its field here and just reads the key every
/// tick. Only the resulting [`Buttons`] are ever sent ([`build_input`]), so the server and
/// prediction don't need to know toggling exists.
#[derive(Resource, Default)]
pub struct StanceToggles {
    prone: bool,
    crouch: bool,
    sprint: bool,
}

impl StanceToggles {
    fn reset(&mut self) {
        *self = Self::default();
    }
}

/// BF2 doesn't carry a toggled stance across death, a respawn, boarding a vehicle, or grabbing
/// a ladder or rope: reset it there instead of leaving a stale toggle to reassert itself later.
fn reset_stance_toggles(
    mut toggles: ResMut<StanceToggles>,
    reset_now: Query<Entity, (With<LocalSoldier>, Or<(Added<LocalSoldier>, Added<Seated>, Added<Downed>)>)>,
    motion: Query<&SoldierMotion, With<LocalSoldier>>,
    mut was_climbing: Local<bool>,
) {
    let climbing = motion.single().is_ok_and(|m| m.climbing || m.on_rope);
    if !reset_now.is_empty() || (climbing && !*was_climbing) {
        toggles.reset();
    }
    *was_climbing = climbing;
}

/// Resolves this tick's crouch, prone and sprint buttons per `Settings::{crouch,prone,sprint}_mode`
/// (BF2 defaults: prone toggles, crouch and sprint are held). Mirrors BF2: pressing crouch or
/// jump while a toggled prone is on stands back up (crouch to a crouch, jump the rest of the
/// way), since prone otherwise outranks crouch once both bits are set (`soldier::wanted_stance`).
#[allow(clippy::too_many_arguments)]
fn apply_stance_buttons(
    actions: &Actions,
    settings: &Settings,
    toggles: &mut StanceToggles,
    frame: &mut InputFrame,
    prone_edge: bool,
    crouch_edge: bool,
    sprint_edge: bool,
) {
    fn resolve(mode: StanceMode, edge: bool, held: bool, toggle: &mut bool) -> bool {
        match mode {
            StanceMode::Hold => held,
            StanceMode::Toggle => {
                if edge {
                    *toggle = !*toggle;
                }
                *toggle
            }
        }
    }
    let crouch = resolve(settings.crouch_mode, crouch_edge, actions.pressed(Action::Crouch), &mut toggles.crouch);
    let mut prone = resolve(settings.prone_mode, prone_edge, actions.pressed(Action::Prone), &mut toggles.prone);
    let sprint = resolve(settings.sprint_mode, sprint_edge, actions.pressed(Action::Sprint), &mut toggles.sprint);
    if prone && (crouch_edge || frame.buttons.contains(Buttons::JUMP)) {
        prone = false;
        toggles.prone = false;
    }
    frame.buttons.set(Buttons::CROUCH, crouch);
    frame.buttons.set(Buttons::PRONE, prone);
    frame.buttons.set(Buttons::SPRINT, sprint);
}

/// Context `build_input` only reads, bundled into one `SystemParam` to stay under Bevy's
/// 16-parameter limit for a function system (each field here would otherwise be its own
/// argument).
#[derive(SystemParam)]
pub(crate) struct FrameContext<'w, 's> {
    cli: Res<'w, crate::Cli>,
    selection: Res<'w, crate::combat::WeaponSelection>,
    seat: Res<'w, crate::vehicles::SeatRequest>,
    flight: Res<'w, crate::vehicles::FlightStick>,
    view: Res<'w, crate::combat::ViewTick>,
    scenario: Option<Res<'w, crate::scenario::ScenarioInput>>,
    active: Res<'w, crate::net::ActiveMatch>,
    downed: Query<'w, 's, &'static Downed, With<crate::net::LocalSoldier>>,
    seated: Query<'w, 's, &'static Seated, With<LocalSoldier>>,
}

#[allow(clippy::too_many_arguments)]
pub fn build_input(
    actions: Actions,
    settings: Res<Settings>,
    mut toggles: ResMut<StanceToggles>,
    cursor: Single<&CursorOptions>,
    mut look: ResMut<LookState>,
    mut history: ResMut<InputHistory>,
    mut packets: MessageWriter<InputPacket>,
    ctx: FrameContext,
    real: Res<Time<Real>>,
    mut delayed: Local<VecDeque<(f64, InputPacket)>>,
) {
    if ctx.active.setup.is_none() {
        return;
    }
    if ctx.cli.debug_walk {
        look.yaw += 0.01;
    }
    let mut frame = InputFrame {
        seq: history.next_seq,
        yaw: look.yaw,
        pitch: look.pitch,
        weapon: ctx.selection.index,
        seat: ctx.seat.0,
        view_tick: ctx.view.tick,
        ..default()
    };
    history.next_seq = history.next_seq.wrapping_add(1);

    if ctx.cli.debug_walk {
        frame.set_movement(Vec2::Y);
        frame.buttons.set(Buttons::SPRINT, true);
        frame.buttons.set(Buttons::JUMP, frame.seq % 150 == 0);
    } else if cursor_locked(&cursor) {
        let keyboard_move = Vec2::new(
            actions.axis(Action::MoveRight, Action::MoveLeft),
            actions.axis(Action::MoveForward, Action::MoveBack),
        );
        // Gamepad: the left stick, added to WASD (so either or both can be used). Driving or
        // piloting a vehicle (any seat 0), the triggers throttle/brake instead of the stick's
        // Y, like a racing pad; steering still comes from the stick's X.
        let gamepad_move = actions
            .gamepad()
            .map(|gamepad| deadzone(gamepad.left_stick(), actions.gamepad_settings().move_deadzone))
            .unwrap_or_default();
        let mut movement = (keyboard_move + gamepad_move).clamp_length_max(1.0);
        if let Some(gamepad) = actions.gamepad()
            && ctx.seated.single().is_ok_and(|s| s.seat == 0)
        {
            let throttle = gamepad.get(GamepadButton::RightTrigger2).unwrap_or(0.0);
            let brake = gamepad.get(GamepadButton::LeftTrigger2).unwrap_or(0.0);
            if throttle > 0.02 || brake > 0.02 {
                movement.y = (throttle - brake).clamp(-1.0, 1.0);
            }
        }
        frame.set_movement(movement);
        let mut set = |button: Buttons, action: Action| frame.buttons.set(button, actions.pressed(action));
        set(Buttons::JUMP, Action::Jump);
        set(Buttons::FIRE, Action::Fire);
        set(Buttons::AIM, Action::Zoom);
        set(Buttons::USE, Action::Use);
        set(Buttons::RELOAD, Action::Reload);
        set(Buttons::FIRE_MODE, Action::FireMode);
        set(Buttons::COUNTERMEASURE, Action::Countermeasures);
        if look.jump_latched {
            frame.buttons.insert(Buttons::JUMP);
        }
        apply_stance_buttons(
            &actions,
            &settings,
            &mut toggles,
            &mut frame,
            look.prone_latched,
            look.crouch_latched,
            look.sprint_latched,
        );
        if ctx.flight.active {
            frame.set_stick(ctx.flight.stick);
            // --- Flight controls: helicopter pedals on the mouse (vehicles::fly) ---
            if let Some(steer) = ctx.flight.steer {
                frame.set_movement(Vec2::new(steer, frame.movement_vec().y));
            }
            // --- end flight controls ---
        }
    }
    if let Some(scenario) = &ctx.scenario {
        frame.buttons |= scenario.buttons;
        if let Some(movement) = scenario.movement {
            frame.set_movement(movement);
        }
        if let Some(stick) = scenario.stick {
            frame.set_stick(stick);
        }
    }
    // --- Quick actions (quick_actions): the melee and grenade keys ---
    frame.buttons.set(Buttons::QUICK, ctx.selection.quick);
    if let Some(fire) = ctx.selection.quick_fire {
        frame.buttons.set(Buttons::FIRE, fire);
    }
    // --- end quick actions ---
    look.jump_latched = false;
    look.prone_latched = false;
    look.crouch_latched = false;
    look.sprint_latched = false;
    // Critically wounded: the server lies us still whatever we press; predict the same.
    if let Ok(downed) = ctx.downed.single() {
        frame = game_shared::revive::downed_input(frame, downed);
    }

    history.frames.push_back(frame);
    while history.frames.len() > InputHistory::MAX {
        history.frames.pop_front();
    }
    let start = history.frames.len().saturating_sub(INPUT_REDUNDANCY);
    let packet = InputPacket {
        frames: history.frames.range(start..).copied().collect(),
    };
    if ctx.cli.input_delay == 0 {
        packets.write(packet);
        return;
    }
    // Debug: a slow connection.
    let now = real.elapsed_secs_f64();
    delayed.push_back((now + ctx.cli.input_delay as f64 / 1000.0, packet));
    while delayed.front().is_some_and(|(at, _)| *at <= now) {
        if let Some((_, packet)) = delayed.pop_front() {
            packets.write(packet);
        }
    }
}
