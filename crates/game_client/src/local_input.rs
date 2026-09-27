//! Turning keyboard and mouse into [`InputFrame`]s, once per simulation tick.

use std::{collections::VecDeque, f32::consts::FRAC_PI_2};

use bevy::{
    input::mouse::AccumulatedMouseMotion,
    prelude::*,
    window::{CursorGrabMode, CursorOptions},
};
use game_shared::input::{Buttons, INPUT_REDUNDANCY, InputFrame, InputPacket};

use crate::{
    menu::{Menu, Screen},
    settings::{Action, Actions},
};

pub struct LocalInputPlugin;

impl Plugin for LocalInputPlugin {
    fn build(&self, app: &mut App) {
        app.init_resource::<LookState>()
            .init_resource::<InputHistory>()
            .add_systems(Update, (grab_cursor, mouse_look).chain())
            .add_systems(FixedUpdate, build_input.in_set(LocalInputSystems));
    }
}

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
    screen: Res<State<Screen>>,
    menu: Res<Menu>,
) {
    let playing = *screen.get() == Screen::InGame && !menu.paused && !deploy.open;
    if !playing || !window.focused {
        if cursor_locked(&cursor) {
            cursor.visible = true;
            cursor.grab_mode = CursorGrabMode::None;
        }
    } else if mouse.just_pressed(MouseButton::Left) && !cursor_locked(&cursor) {
        cursor.visible = false;
        cursor.grab_mode = CursorGrabMode::Locked;
    }
}

fn mouse_look(
    motion: Res<AccumulatedMouseMotion>,
    actions: Actions,
    cursor: Single<&CursorOptions>,
    mut look: ResMut<LookState>,
) {
    if !cursor_locked(&cursor) {
        return;
    }
    let sensitivity = look.sensitivity * look.zoom_scale;
    let vertical = if look.invert_y { -motion.delta.y } else { motion.delta.y };
    look.yaw -= motion.delta.x * sensitivity;
    look.pitch = (look.pitch - vertical * sensitivity).clamp(-FRAC_PI_2 + 0.02, FRAC_PI_2 - 0.02);
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
    view: Res<crate::combat::ViewTick>,
    scenario: Option<Res<crate::scenario::ScenarioInput>>,
    active: Res<crate::net::ActiveMatch>,
    downed: Query<&game_shared::revive::Downed, With<crate::net::LocalSoldier>>,
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
        frame.set_movement(Vec2::new(
            actions.axis(Action::MoveRight, Action::MoveLeft),
            actions.axis(Action::MoveForward, Action::MoveBack),
        ));
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
        if look.jump_latched {
            frame.buttons.insert(Buttons::JUMP);
        }
    }
    if let Some(scenario) = scenario {
        frame.buttons |= scenario.buttons;
        if let Some(movement) = scenario.movement {
            frame.set_movement(movement);
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
    packets.write(InputPacket {
        frames: history.frames.range(start..).copied().collect(),
    });
}
