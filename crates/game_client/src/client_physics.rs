//! The client's physics. The client simulates nothing itself (the server does, on its own
//! thread when we host): avian only keeps the colliders of the level, soldiers and vehicles
//! where they are, for the client's own queries (prediction's move-and-slide, the camera,
//! hit checks, footsteps). So it does less than avian's defaults every tick:
//!
//! - one solver substep instead of six: there is no dynamic body to solve;
//! - colliders are synced from their transforms only when those moved: avian's own sync
//!   decomposes the transform of every body every tick, the level's thousands of statics
//!   included (Karkand 3,300, Archipelago 6,600+), which never move.
//!
//! `BF2_PERF_EXP=clientphysics` keeps avian's defaults, for comparisons.

use avian3d::{
    physics_transform::{PhysicsTransformConfig, PhysicsTransformSystems},
    prelude::*,
};
use bevy::prelude::*;

pub struct ClientPhysicsPlugin;

impl Plugin for ClientPhysicsPlugin {
    fn build(&self, app: &mut App) {
        if crate::perf_experiment("clientphysics") {
            return;
        }
        app.insert_resource(SubstepCount(1))
            .insert_resource(PhysicsTransformConfig {
                transform_to_position: false,
                ..default()
            })
            .add_systems(
                FixedPostUpdate,
                moved_to_position.in_set(PhysicsTransformSystems::TransformToPosition),
            );
    }
}

/// Avian's `transform_to_position` for the bodies whose transform changed since the last tick.
fn moved_to_position(
    mut bodies: Query<(&GlobalTransform, &mut Position, &mut Rotation), Changed<GlobalTransform>>,
    length_unit: Res<PhysicsLengthUnit>,
) {
    // Changes below 0.01 mm and 0.1 degrees are ignored, like avian does.
    let distance_tolerance = length_unit.0 * 1e-5;
    let rotation_tolerance = 0.1f32.to_radians();
    for (global, mut position, mut rotation) in &mut bodies {
        let transform = global.compute_transform();
        if position.0.distance(transform.translation) > distance_tolerance {
            position.0 = transform.translation;
        }
        let wanted = Rotation::from(transform.rotation);
        if rotation.angle_between(wanted).abs() > rotation_tolerance {
            *rotation = wanted;
        }
    }
}
