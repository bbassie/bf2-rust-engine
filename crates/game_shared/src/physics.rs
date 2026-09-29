//! Physics conventions shared by all gameplay code.

use avian3d::prelude::*;

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
