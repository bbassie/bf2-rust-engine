//! Object templates: `templates/<name>.ron`.

use serde::{Deserialize, Serialize};

use crate::Placement;

/// What an object looks like and collides with, flattened into parts.
#[derive(Serialize, Deserialize, Debug, Clone, Default)]
pub struct ObjectDesc {
    /// Template name, lowercase.
    pub name: String,
    /// Original template type, e.g. `SimpleObject`, `Bundle`, `PlayerControlObject`.
    #[serde(default)]
    pub kind: String,
    #[serde(default)]
    pub parts: Vec<ObjectPart>,
}

/// One visual and/or collision piece of an object.
#[derive(Serialize, Deserialize, Debug, Clone, Default)]
pub struct ObjectPart {
    /// `.glb` path relative to the imported root.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub mesh: Option<String>,
    /// Mesh index inside the `.glb` (bundled meshes have one mesh per part).
    #[serde(default, skip_serializing_if = "is_zero")]
    pub mesh_index: u32,
    /// Collision `.glb` path relative to the imported root. Meshes inside are named
    /// `part{N}_{projectile|vehicle|soldier|ai}`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub collision: Option<String>,
    #[serde(default, skip_serializing_if = "is_zero")]
    pub collision_part: u32,
    /// Relative to the object's origin.
    #[serde(flatten)]
    pub placement: Placement,
}

fn is_zero(v: &u32) -> bool {
    *v == 0
}
