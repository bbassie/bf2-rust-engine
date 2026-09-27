//! Vegetation that is only drawn, never collided with: `levels/<name>/vegetation.ron`.
//!
//! Undergrowth (grass, flowers, small plants) is generated around the camera from a material
//! map; overgrowth is trees and bushes placed from density maps.

use serde::{Deserialize, Serialize};

use crate::Placement;

#[derive(Serialize, Deserialize, Debug, Clone, Default)]
pub struct VegetationDesc {
    #[serde(default)]
    pub undergrowth: Option<UndergrowthDesc>,
    #[serde(default)]
    pub overgrowth: Vec<OvergrowthDesc>,
}

/// Grass and small plants scattered on the terrain near the camera.
#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct UndergrowthDesc {
    /// `u8` material id per terrain sample (same grid and orientation as the heightmap),
    /// relative to the level folder. Ids index [`Self::materials`] by [`UndergrowthMaterial::id`].
    pub material_map: String,
    /// Texture all plant meshes sample, relative to the level folder. Alpha is coverage.
    pub atlas: String,
    /// Plants are drawn up to this distance in meters...
    pub view_distance: f32,
    /// ...and shrink into the ground over this last fraction of it.
    #[serde(default = "default_fade")]
    pub fade: f32,
    /// Wind sway in meters of a vertex with sway weight 1.
    #[serde(default)]
    pub sway: f32,
    /// Brightness multiplier of the atlas colour.
    #[serde(default = "one")]
    pub brightness: f32,
    /// Coverage below this alpha is cut out.
    #[serde(default = "default_alpha_cutoff")]
    pub alpha_cutoff: f32,
    #[serde(default)]
    pub materials: Vec<UndergrowthMaterial>,
}

#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct UndergrowthMaterial {
    pub id: u8,
    pub name: String,
    /// Plant types that grow together on this material.
    #[serde(default)]
    pub types: Vec<UndergrowthType>,
}

#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct UndergrowthType {
    pub name: String,
    /// Plants per square meter.
    pub density: f32,
    /// Uniform scale, picked at random between the two values.
    pub scale: [f32; 2],
    /// Patchiness: 0 grows evenly, 1 only in the patches of a noise pattern, -1 only in its gaps.
    #[serde(default)]
    pub variation: f32,
    /// How much the plant takes on the colour of the ground it grows on (0..1).
    #[serde(default)]
    pub ground_tint: f32,
    /// Tilt plants randomly instead of standing them upright.
    #[serde(default)]
    pub skew: bool,
    pub mesh: UndergrowthMesh,
}

/// A small plant mesh: root at the origin, +Y up, meters.
#[derive(Serialize, Deserialize, Debug, Clone, Default)]
pub struct UndergrowthMesh {
    pub positions: Vec<[f32; 3]>,
    /// Texture coordinates in the atlas.
    pub uvs: Vec<[f32; 2]>,
    /// How much each vertex moves in the wind (0 at the root).
    pub sway: Vec<f32>,
    /// Triangle list.
    pub indices: Vec<u16>,
}

/// Instances of one tree or bush mesh.
#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct OvergrowthDesc {
    /// `.glb` path relative to the imported root.
    pub mesh: String,
    pub instances: Vec<Placement>,
}

fn default_fade() -> f32 {
    0.5
}

fn default_alpha_cutoff() -> f32 {
    0.15
}

fn one() -> f32 {
    1.0
}
