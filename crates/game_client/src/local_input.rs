//! Turning keyboard and mouse into [`InputFrame`]s, once per simulation tick.

use std::{
    collections::VecDeque,
    f32::consts::{FRAC_PI_2, TAU},
};

use bevy::{
    input::{
        gamepad::{Gamepad, GamepadButton},
        mouse::AccumulatedMouseMotion,
    },
    prelude::*,
    window::{CursorGrabMode, CursorOptions},
};
use game_shared::{
    input::{Buttons, INPUT_REDUNDANCY, InputFrame, InputPacket},
    vehicle::Seated,
};

use crate::{
    menu::{Menu, Screen},
    net::LocalSoldier,
    settings::{Action, Actions, Settings, gamepad_activity},
};

pub struct LocalInputPlugin;

impl Plugin for LocalInputPlugin {
    fn build(&self, app: &mut App) {
        app.init_resource::<LookState>()
            .init_resource::<InputHistory>()
            .add_systems(Update, (grab_cursor, mouse_look).chain().in_set(LookSystems))
            .add_systems(FixedUpdate, build_input.in_set(LocalInputSystems));
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

pub fn cursor_locked(cursor: &CursorOptions) -> bool {
    cursor.grab_mode != CursorGrabMode::None
}

/// A click in the game takes the mouse; menus, the deploy screen and losing focus give it
/// back (Esc opens the in-game menu, see `menu`).
fn grab_cursor(
    mut cursor: Single<&mut CursorOptions>,
    window: Single<&Window>,
    mouse: Res<ButtonInput<MouseButton>>,
    deploy: Res<crate::deploy::DeployScreen>,
    commander: Res<crate::commander::CommanderScreen>,
    screen: Res<State<Screen>>,
    menu: Res<Menu>,
    gamepads: Query<&Gamepad>,
) {
    let playing = *screen.get() == Screen::InGame && !menu.paused && !deploy.open && !commander.open;
    if !playing || !window.focused {
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
}

#[allow(clippy::too_many_arguments)]
pub fn build_input(
    actions: Actions,
    cursor: Single<&CursorOptions>,
    mut look: ResMut<LookState>,
    mut history: ResMut<InputHistory>,
    mut packets: MessageWriter<InputPacket>,
    cli: Res<crate::Cli>,
    selection: Res<crate::combat::WeaponSelection>,
    seat: Res<crate::vehicles::SeatRequest>,
    flight: Res<crate::vehicles::FlightStick>,
    view: Res<crate::combat::ViewTick>,
    scenario: Option<Res<crate::scenario::ScenarioInput>>,
    active: Res<crate::net::ActiveMatch>,
    downed: Query<&game_shared::revive::Downed, With<crate::net::LocalSoldier>>,
    seated: Query<&Seated, With<LocalSoldier>>,
    real: Res<Time<Real>>,
    mut delayed: Local<VecDeque<(f64, InputPacket)>>,
) {
    if active.setup.is_none() {
        return;
    }
    if cli.debug_walk {
        look.yaw += 0.01;
    }
    let mut frame = InputFrame {
        seq: history.next_seq,
        yaw: look.yaw,
        pitch: look.pitch,
        weapon: selection.index,
        seat: seat.0,
        view_tick: view.tick,
        ..default()
    };
    history.next_seq = history.next_seq.wrapping_add(1);

    if cli.debug_walk {
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
            && seated.single().is_ok_and(|s| s.seat == 0)
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
        set(Buttons::SPRINT, Action::Sprint);
        set(Buttons::CROUCH, Action::Crouch);
        set(Buttons::PRONE, Action::Prone);
        set(Buttons::FIRE, Action::Fire);
        set(Buttons::AIM, Action::Zoom);
        set(Buttons::USE, Action::Use);
        set(Buttons::RELOAD, Action::Reload);
        set(Buttons::FIRE_MODE, Action::FireMode);
        set(Buttons::COUNTERMEASURE, Action::Countermeasures);
        if look.jump_latched {
            frame.buttons.insert(Buttons::JUMP);
        }
        if flight.active {
            frame.set_stick(flight.stick);
        }
    }
    if let Some(scenario) = scenario {
        frame.buttons |= scenario.buttons;
        if let Some(movement) = scenario.movement {
            frame.set_movement(movement);
        }
        if let Some(stick) = scenario.stick {
            frame.set_stick(stick);
        }
    }
    look.jump_latched = false;
    // Critically wounded: the server lies us still whatever we press; predict the same.
    if let Ok(downed) = downed.single() {
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
    if cli.input_delay == 0 {
        packets.write(packet);
        return;
    }
    // Debug: a slow connection.
    let now = real.elapsed_secs_f64();
    delayed.push_back((now + cli.input_delay as f64 / 1000.0, packet));
    while delayed.front().is_some_and(|(at, _)| *at <= now) {
        if let Some((_, packet)) = delayed.pop_front() {
            packets.write(packet);
        }
    }
}
