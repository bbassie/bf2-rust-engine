//! Special Forces gear that changes what a soldier can breathe: the gas mask, and tear gas.
//!
//! BF2 tear gas (`gasCloudType TearGas` on the grenade's smoke) blurs the eyes and makes
//! soldiers without a gas mask cough; it also hurts a little (`gasCloudDamage`). The server
//! marks tear gas clouds with [`TearGas`] and puts [`SoldierGear`] on soldiers; both
//! replicate, so every client can tell who is coughing without further messages.

use bevy::prelude::*;
use serde::{Deserialize, Serialize};

use crate::projectile::SmokeCloud;

/// What a soldier wears (replicated). Set by the server from [`GearRequest`]s.
#[derive(Component, Serialize, Deserialize, Clone, Copy, Debug, Default, PartialEq)]
pub struct SoldierGear {
    pub gas_mask: bool,
}

/// Client -> server: put the gas mask on or take it off. Ignored for soldiers without one.
#[derive(Message, Serialize, Deserialize, Clone, Copy, Debug)]
pub struct GearRequest {
    pub gas_mask: bool,
}

/// A [`SmokeCloud`] that is tear gas (replicated).
#[derive(Component, Serialize, Deserialize, Clone, Copy, Debug, PartialEq)]
pub struct TearGas {
    /// Hit points per second to those inside without a gas mask.
    pub damage: f32,
}

/// How deep in tear gas a point is, 0..1 (1 at the heart of a thick cloud).
pub fn gas_exposure<'a>(point: Vec3, clouds: impl IntoIterator<Item = (&'a SmokeCloud, &'a TearGas)>) -> f32 {
    clouds
        .into_iter()
        .map(|(cloud, _)| {
            let radius = cloud.current_radius().max(0.1);
            let inside = 1.0 - (point.distance(cloud.position) / radius).clamp(0.0, 1.0);
            (inside * 2.0).min(1.0) * cloud.density()
        })
        .fold(0.0, f32::max)
}
