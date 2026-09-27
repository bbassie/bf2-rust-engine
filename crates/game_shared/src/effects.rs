//! Cosmetic effects the server asks clients to play.
//!
//! Clients work out most effects themselves (muzzle flashes and impacts from `ShotFired`
//! and their own tracers, destruction from `DestroyedStatics`). For the rest the server
//! writes `ToClients<PlayEffect>`; the client plays it like any local effect.

use bevy::prelude::*;
use serde::{Deserialize, Serialize};

/// Server -> clients: play the effect `name` (an `imported/effects/<name>.ron`).
#[derive(Message, Serialize, Deserialize, Clone, Debug)]
pub struct PlayEffect {
    pub name: String,
    pub position: Vec3,
    /// Where the effect's +Y points: the surface normal, or up.
    pub up: Vec3,
    /// Seconds to keep emitting (smoke grenades); 0 for the effect's own length.
    pub duration: f32,
}

impl PlayEffect {
    pub fn new(name: impl Into<String>, position: Vec3) -> Self {
        Self {
            name: name.into(),
            position,
            up: Vec3::Y,
            duration: 0.0,
        }
    }
}
