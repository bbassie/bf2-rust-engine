//! Soldier bodies (`soldiers/<name>.ron`) and team setup.

use serde::{Deserialize, Serialize};

/// A soldier body: skinned model with its animations.
#[derive(Serialize, Deserialize, Debug, Clone, Default)]
pub struct SoldierDesc {
    pub name: String,
    /// `.glb` with skeleton, skinned third-person mesh and animation clips, relative to the
    /// imported root.
    pub mesh: String,
    /// Names of the animation clips in the `.glb`.
    #[serde(default)]
    pub animations: Vec<String>,
}

/// One side of a match.
#[derive(Serialize, Deserialize, Debug, Clone, Default)]
pub struct TeamDesc {
    pub name: String,
    /// Kit slots (BF2 has 7): which kit and which soldier body wears it.
    #[serde(default)]
    pub kits: Vec<KitSlot>,
    /// Starting tickets per layout size: `(size, tickets)`.
    #[serde(default)]
    pub tickets: Vec<(u32, u32)>,
    /// Tickets lost per minute while the other side holds more of the map.
    #[serde(default)]
    pub ticket_loss_per_minute: f32,
}

#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct KitSlot {
    pub kit: String,
    pub soldier: String,
}
