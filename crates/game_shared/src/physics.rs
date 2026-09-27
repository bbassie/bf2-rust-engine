//! Physics conventions shared by all gameplay code.

use avian3d::prelude::*;

/// Collision layers. Soldiers are only hit by queries (bullets, movement of other things),
/// they never push or get pushed by the physics solver.
#[derive(PhysicsLayer, Clone, Copy, Debug, Default)]
pub enum GameLayer {
    /// Terrain and static level geometry.
    #[default]
    World,
    Soldier,
    Vehicle,
    Projectile,
    /// Boxes around ladders, only found by movement queries (see [`crate::ladder`]).
    Ladder,
}

impl GameLayer {
    /// Layers a walking soldier collides with.
    pub fn soldier_movement_mask() -> LayerMask {
        [GameLayer::World, GameLayer::Vehicle].into()
    }
}
