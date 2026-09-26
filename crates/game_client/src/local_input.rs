//! Turning keyboard and mouse into [`InputFrame`]s, once per simulation tick.

use std::{collections::VecDeque, f32::consts::FRAC_PI_2};

use bevy::{
    input::mouse::AccumulatedMouseMotion,
    prelude::*,
    window::{CursorGrabMode, CursorOptions},
};
use game_shared::input::{Buttons, INPUT_REDUNDANCY, InputFrame, InputPacket};

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

/// Where the player is looking. Updated every frame from the mouse, sampled every tick.
#[derive(Resource)]
pub struct LookState {
    pub yaw: f32,
    pub pitch: f32,
    pub sensitivity: f32,
    /// Set when jump is pressed between ticks so short taps aren't lost.
    jump_latched: bool,
}

impl Default for LookState {
    fn default() -> Self {
        Self {
            yaw: 0.0,
            pitch: 0.0,
            sensitivity: 0.0022,
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

fn grab_cursor(
    mut cursor: Single<&mut CursorOptions>,
    window: Single<&Window>,
    mouse: Res<ButtonInput<MouseButton>>,
    keys: Res<ButtonInput<KeyCode>>,
) {
    if mouse.just_pressed(MouseButton::Left) && !cursor_locked(&cursor) {
        cursor.visible = false;
        cursor.grab_mode = CursorGrabMode::Locked;
    }
    if keys.just_pressed(KeyCode::Escape) || !window.focused {
        cursor.visible = true;
        cursor.grab_mode = CursorGrabMode::None;
    }
}

fn mouse_look(
    motion: Res<AccumulatedMouseMotion>,
    keys: Res<ButtonInput<KeyCode>>,
    cursor: Single<&CursorOptions>,
    mut look: ResMut<LookState>,
) {
    if !cursor_locked(&cursor) {
        return;
    }
    look.yaw -= motion.delta.x * look.sensitivity;
    look.pitch = (look.pitch - motion.delta.y * look.sensitivity)
        .clamp(-FRAC_PI_2 + 0.02, FRAC_PI_2 - 0.02);
    if keys.just_pressed(KeyCode::Space) {
        look.jump_latched = true;
    }
}

pub fn build_input(
    keys: Res<ButtonInput<KeyCode>>,
    mouse: Res<ButtonInput<MouseButton>>,
    cursor: Single<&CursorOptions>,
    mut look: ResMut<LookState>,
    mut history: ResMut<InputHistory>,
    mut packets: MessageWriter<InputPacket>,
    cli: Res<crate::Cli>,
) {
    if cli.debug_walk {
        look.yaw += 0.01;
    }
    let mut frame = InputFrame {
        seq: history.next_seq,
        yaw: look.yaw,
        pitch: look.pitch,
        ..default()
    };
    history.next_seq = history.next_seq.wrapping_add(1);

    if cli.debug_walk {
        frame.set_movement(Vec2::Y);
        frame.buttons.set(Buttons::SPRINT, true);
        frame.buttons.set(Buttons::JUMP, frame.seq % 150 == 0);
    } else if cursor_locked(&cursor) {
        let axis = |pos: KeyCode, neg: KeyCode| keys.pressed(pos) as i8 as f32 - keys.pressed(neg) as i8 as f32;
        frame.set_movement(Vec2::new(
            axis(KeyCode::KeyD, KeyCode::KeyA),
            axis(KeyCode::KeyW, KeyCode::KeyS),
        ));
        let mut set = |button: Buttons, down: bool| frame.buttons.set(button, down);
        set(Buttons::JUMP, keys.pressed(KeyCode::Space) || look.jump_latched);
        set(Buttons::SPRINT, keys.pressed(KeyCode::ShiftLeft));
        set(Buttons::CROUCH, keys.pressed(KeyCode::ControlLeft));
        set(Buttons::PRONE, keys.pressed(KeyCode::KeyZ));
        set(Buttons::FIRE, mouse.pressed(MouseButton::Left));
        set(Buttons::AIM, mouse.pressed(MouseButton::Right));
        set(Buttons::USE, keys.pressed(KeyCode::KeyE));
        set(Buttons::RELOAD, keys.pressed(KeyCode::KeyR));
    }
    look.jump_latched = false;

    history.frames.push_back(frame);
    while history.frames.len() > InputHistory::MAX {
        history.frames.pop_front();
    }
    let start = history.frames.len().saturating_sub(INPUT_REDUNDANCY);
    packets.write(InputPacket {
        frames: history.frames.range(start..).copied().collect(),
    });
}
