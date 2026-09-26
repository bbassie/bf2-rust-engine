//! Infantry movement.
//!
//! [`step_soldier`] is the single source of truth for how a soldier moves. The server runs
//! it for authority, clients run it for prediction and replay, and bots feed it the same
//! [`InputFrame`]s humans do.

use core::time::Duration;

use avian3d::prelude::*;
use bevy::prelude::*;
use serde::{Deserialize, Serialize};

use crate::{
    input::{Buttons, InputFrame},
    physics::GameLayer,
};

pub const SOLDIER_RADIUS: f32 = 0.3;
pub const SOLDIER_HEIGHT: f32 = 1.8;
/// Offset from the feet to the capsule center.
pub const SOLDIER_CENTER: Vec3 = Vec3::new(0.0, SOLDIER_HEIGHT * 0.5, 0.0);

pub struct SoldierPlugin;

impl Plugin for SoldierPlugin {
    fn build(&self, app: &mut App) {
        app.init_resource::<SoldierTuning>()
            .init_resource::<SoldierShapes>()
            .add_observer(add_soldier_physics);
    }
}

/// A soldier body. Replicated; the owning player is in [`crate::protocol::ControlledBy`].
#[derive(Component, Serialize, Deserialize, Clone, Copy, Debug, Default)]
#[require(Transform, SoldierMotion, InputAck, Health)]
pub struct Soldier;

#[derive(Serialize, Deserialize, Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum Stance {
    #[default]
    Standing,
    Crouching,
    Prone,
}

impl Stance {
    pub fn eye_height(self) -> f32 {
        match self {
            Stance::Standing => 1.65,
            Stance::Crouching => 1.05,
            Stance::Prone => 0.35,
        }
    }
}

/// Complete movement state of a soldier. Authoritative on the server, replicated to clients.
#[derive(Component, Serialize, Deserialize, Clone, Copy, Debug, Default, PartialEq)]
pub struct SoldierMotion {
    /// Feet position.
    pub position: Vec3,
    pub velocity: Vec3,
    pub yaw: f32,
    pub pitch: f32,
    pub grounded: bool,
    pub stance: Stance,
}

impl SoldierMotion {
    pub fn at(position: Vec3, yaw: f32) -> Self {
        Self {
            position,
            yaw,
            ..default()
        }
    }

    pub fn eye_position(&self) -> Vec3 {
        self.position + Vec3::Y * self.stance.eye_height()
    }

    pub fn view_rotation(&self) -> Quat {
        Quat::from_euler(EulerRot::YXZ, self.yaw, self.pitch, 0.0)
    }

    pub fn body_transform(&self) -> Transform {
        Transform::from_translation(self.position).with_rotation(Quat::from_rotation_y(self.yaw))
    }
}

/// Sequence number of the last input the server applied to this soldier.
/// Clients use it to know which of their predicted inputs to replay.
#[derive(Component, Serialize, Deserialize, Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct InputAck(pub u32);

#[derive(Component, Serialize, Deserialize, Clone, Copy, Debug, PartialEq)]
pub struct Health {
    pub current: f32,
    pub max: f32,
}

impl Default for Health {
    fn default() -> Self {
        Self {
            current: 100.0,
            max: 100.0,
        }
    }
}

/// Movement tuning. Will eventually come from the imported soldier template.
#[derive(Resource, Clone, Debug)]
pub struct SoldierTuning {
    pub run_speed: f32,
    pub sprint_speed: f32,
    pub crouch_speed: f32,
    pub prone_speed: f32,
    pub ground_accel: f32,
    pub air_accel: f32,
    pub jump_speed: f32,
    pub gravity: f32,
    /// Steepest walkable slope, radians.
    pub max_slope: f32,
    /// How far the soldier snaps down to stay on the ground when walking downhill or off steps.
    pub snap_distance: f32,
}

impl Default for SoldierTuning {
    fn default() -> Self {
        Self {
            run_speed: 4.0,
            sprint_speed: 6.2,
            crouch_speed: 1.8,
            prone_speed: 0.8,
            ground_accel: 30.0,
            air_accel: 3.0,
            jump_speed: 4.2,
            gravity: 14.0,
            max_slope: 50f32.to_radians(),
            snap_distance: 0.35,
        }
    }
}

/// Shapes used for soldier movement queries.
#[derive(Resource)]
pub struct SoldierShapes {
    pub standing: Collider,
}

impl Default for SoldierShapes {
    fn default() -> Self {
        Self {
            standing: Collider::capsule(SOLDIER_RADIUS, SOLDIER_HEIGHT - 2.0 * SOLDIER_RADIUS),
        }
    }
}

/// Gives every soldier (spawned on the server or replicated to a client) a hitbox.
/// The hitbox does not take part in the physics solver; it is only found by queries.
fn add_soldier_physics(add: On<Add, Soldier>, mut commands: Commands, shapes: Res<SoldierShapes>) {
    commands.entity(add.entity).insert(RigidBody::Kinematic).with_child((
        shapes.standing.clone(),
        Transform::from_translation(SOLDIER_CENTER),
        CollisionLayers::new(GameLayer::Soldier, LayerMask::NONE),
    ));
}

/// Advances one soldier by one tick.
pub fn step_soldier(
    m: &mut SoldierMotion,
    input: &InputFrame,
    dt: f32,
    tuning: &SoldierTuning,
    shapes: &SoldierShapes,
    mover: &MoveAndSlide,
) {
    m.yaw = input.yaw;
    m.pitch = input.pitch.clamp(-1.55, 1.55);
    m.stance = if input.pressed(Buttons::PRONE) {
        Stance::Prone
    } else if input.pressed(Buttons::CROUCH) {
        Stance::Crouching
    } else {
        Stance::Standing
    };

    // Desired horizontal velocity.
    let intent = input.movement_vec();
    let wish_dir = Quat::from_rotation_y(m.yaw) * Vec3::new(intent.x, 0.0, -intent.y);
    let sprinting =
        input.pressed(Buttons::SPRINT) && intent.y > 0.5 && m.stance == Stance::Standing;
    let speed = match m.stance {
        Stance::Standing if sprinting => tuning.sprint_speed,
        Stance::Standing => tuning.run_speed,
        Stance::Crouching => tuning.crouch_speed,
        Stance::Prone => tuning.prone_speed,
    };
    let accel = if m.grounded {
        tuning.ground_accel
    } else {
        tuning.air_accel
    };
    let horizontal = Vec3::new(m.velocity.x, 0.0, m.velocity.z);
    let horizontal = move_towards(horizontal, wish_dir * speed, accel * dt);
    m.velocity.x = horizontal.x;
    m.velocity.z = horizontal.z;

    // Vertical velocity.
    let mut jumped = false;
    if m.grounded {
        if input.pressed(Buttons::JUMP) && m.stance == Stance::Standing {
            m.velocity.y = tuning.jump_speed;
            jumped = true;
        } else {
            m.velocity.y = 0.0;
        }
    } else {
        m.velocity.y -= tuning.gravity * dt;
    }

    let filter = SpatialQueryFilter::from_mask(GameLayer::soldier_movement_mask());
    let min_ground_normal_y = tuning.max_slope.cos();
    let was_grounded = m.grounded;

    let out = mover.move_and_slide(
        &shapes.standing,
        m.position + SOLDIER_CENTER,
        Quat::IDENTITY,
        m.velocity,
        Duration::from_secs_f32(dt),
        &MoveAndSlideConfig::default(),
        &filter,
        |hit| {
            // Too steep to walk: treat as a wall so walking into it can't climb it.
            if was_grounded && hit.normal.y > 0.0 && hit.normal.y < min_ground_normal_y {
                let flat = Vec3::new(hit.normal.x, 0.0, hit.normal.z);
                if let Ok(dir) = Dir3::new(flat) {
                    *hit.normal = dir;
                }
            }
            MoveAndSlideHitResponse::Accept
        },
    );
    m.position = out.position - SOLDIER_CENTER;
    m.velocity = out.projected_velocity;

    // Ground detection, snapping down slopes and small steps while walking.
    let probe = if was_grounded && !jumped {
        tuning.snap_distance
    } else {
        0.05
    };
    let ground = mover.spatial_query.cast_shape(
        &shapes.standing,
        m.position + SOLDIER_CENTER,
        Quat::IDENTITY,
        Dir3::NEG_Y,
        &ShapeCastConfig::from_max_distance(probe),
        &filter,
    );
    m.grounded = false;
    if let Some(hit) = ground
        && m.velocity.y <= 0.01
        && hit.normal1.y >= min_ground_normal_y
    {
        m.position.y -= (hit.distance - 0.01).max(0.0);
        m.velocity.y = 0.0;
        m.grounded = true;
    }
}

fn move_towards(current: Vec3, target: Vec3, max_delta: f32) -> Vec3 {
    let delta = target - current;
    let len = delta.length();
    if len <= max_delta || len < 1e-6 {
        target
    } else {
        current + delta / len * max_delta
    }
}
