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
    /// First-person arms (`.glb` with the `1p_setup` skeleton), relative to the imported root.
    #[serde(default)]
    pub mesh_1p: Option<String>,
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
    /// Voice language (`English`, `Mec`, ...).
    #[serde(default)]
    pub language: String,
    #[serde(default)]
    pub voice: TeamVoice,
}

/// The commander's announcements for a team, in its language (`.wav` paths; one is picked
/// at random).
#[derive(Serialize, Deserialize, Debug, Clone, Default)]
pub struct TeamVoice {
    #[serde(default)]
    pub we_captured: Vec<String>,
    #[serde(default)]
    pub we_lost: Vec<String>,
    #[serde(default)]
    pub enemy_captured: Vec<String>,
    #[serde(default)]
    pub bleed_start: Vec<String>,
    #[serde(default)]
    pub bleed_end: Vec<String>,
    #[serde(default)]
    pub low_tickets: Vec<String>,
}

#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct KitSlot {
    pub kit: String,
    pub soldier: String,
}
