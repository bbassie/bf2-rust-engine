//! Physics conventions shared by all gameplay code.

use avian3d::prelude::*;

/// BF2's world gravity, m/s². Everything that falls uses it, times the object's
/// `gravityModifier` from its template (projectiles, vehicles; soldiers and ropes have 1):
///
/// - `BF2.exe` and `bf2_w32ded.exe` each contain one gravity constant, -14.73, stored by the
///   physics world's constructor next to a setter and getter (the `physics.gravity` console
///   variable); neither contains 9.81 or any other gravity-like value (9.8 appears only
///   inside an unrelated lookup table), and no level or template sets it.
/// - Modders measured it on projectiles: Project Reality's mortar work found g = 14.7 from
///   45° and vertical shots, the Airsoft mod about 14.8 from its BB trajectories.
/// - `gravityModifier` is one `ObjectTemplate` property for every kind of object, so it
///   scales that same world gravity: BF2 has nothing else for it to scale.
///
/// Derive every fall from this (server, client prediction, bots' aim, HUD predictions), so
/// they agree.
pub const WORLD_GRAVITY: f32 = 14.73;

/// Gravity on something with this `gravityModifier`, m/s² (positive: downwards).
pub fn gravity(modifier: f32) -> f32 {
    WORLD_GRAVITY * modifier
}

/// Collision layers. Soldiers are only hit by queries (bullets, movement of other things),
/// they never push or get pushed by the physics solver.
#[derive(PhysicsLayer, Clone, Copy, Debug, Default)]
pub enum GameLayer {
    /// Terrain and static level geometry, built from whichever of BF2's soldier, vehicle or
    /// projectile collision the object has (soldiers, projectiles, cameras, footsteps and the
    /// infantry nav grid all use this layer; see [`crate::statics`]).
    #[default]
    World,
    Soldier,
    Vehicle,
    Projectile,
    /// Boxes around ladders, only found by movement queries (see [`crate::ladder`]).
    Ladder,
    /// Grappling ropes (climbed like ladders) and zipline wires, only found by movement
    /// queries (see [`crate::rope`]).
    Rope,
    Zipline,
    /// Terrain and the static geometry BF2 gives vehicle-type collision (hull only): trees,
    /// walls, buildings. Small plants that only have soldier or projectile collision in BF2
    /// aren't members, so vehicles drive through or flatten them instead of getting stuck
    /// (see [`crate::statics`]). Used for vehicle suspension raycasts, the vehicle hull's own
    /// solid contacts and the vehicle nav grid; soldiers and projectiles are unaffected.
    VehicleGround,
}

impl GameLayer {
    /// Layers a walking soldier collides with.
    pub fn soldier_movement_mask() -> LayerMask {
        [GameLayer::World, GameLayer::Vehicle].into()
    }

    /// Layers a driving vehicle (hull contacts, wheel/track suspension raycasts) collides
    /// with: terrain and vehicle-solid statics, plus other vehicles. Deliberately excludes
    /// [`GameLayer::World`], which also carries statics BF2 gives no vehicle collision (small
    /// plants).
    pub fn vehicle_movement_mask() -> LayerMask {
        [GameLayer::VehicleGround, GameLayer::Vehicle].into()
    }
}
