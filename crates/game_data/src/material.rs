//! The damage table: `materials.ron`.
//!
//! Every surface and every projectile has a material id. A hit deals the projectile's
//! damage times the factor for (projectile material, surface material); explosions use the
//! explosion's material against the victim's armor material.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

#[derive(Serialize, Deserialize, Debug, Clone, Default)]
pub struct MaterialTable {
    /// Material names by id, for logs and editing.
    #[serde(default)]
    pub names: BTreeMap<u32, String>,
    /// Damage factors by `(attacker, target)`. Pairs not listed deal full damage.
    #[serde(default)]
    pub damage: BTreeMap<(u32, u32), f32>,
}

impl MaterialTable {
    pub fn damage_mod(&self, attacker: u32, target: u32) -> f32 {
        self.damage.get(&(attacker, target)).copied().unwrap_or(1.0)
    }

    pub fn name(&self, id: u32) -> &str {
        self.names.get(&id).map_or("?", String::as_str)
    }
}
