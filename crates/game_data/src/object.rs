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
    /// Hit points and what happens when they run out, for objects that can be destroyed.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub armor: Option<ArmorDesc>,
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
    /// Destroyable objects: what replaces `mesh` and `collision` (same indices) once the
    /// object is destroyed. A part without either just disappears.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub wreck_mesh: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub wreck_collision: Option<String>,
    /// Destroyable objects: material of the collision surface, the damage table column for
    /// direct hits. Unset: the armor's material.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub hit_material: Option<u32>,
    /// Soldiers climb this part: a BF2 `Ladder` object, climbed on its local +Z side (its
    /// wall brackets reach back to -Z).
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub ladder: bool,
    /// Relative to the object's origin.
    #[serde(flatten)]
    pub placement: Placement,
}

/// Hit points of a destroyable object (BF2 `Armor` component).
#[derive(Serialize, Deserialize, Debug, Clone, Default)]
pub struct ArmorDesc {
    pub hit_points: f32,
    /// Material for explosion damage (the damage table column).
    pub material: u32,
    /// Blast when the object is destroyed (fuel barrels and tanks).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub explosion: Option<ExplosionDesc>,
    /// What flies off and what it sounds like when the object is destroyed.
    #[serde(default, skip_serializing_if = "DestructionEffect::is_empty")]
    pub effect: DestructionEffect,
}

#[derive(Serialize, Deserialize, Debug, Clone, Default)]
pub struct ExplosionDesc {
    /// Damage at the center, falling off to 0 at `radius` meters.
    pub damage: f32,
    pub radius: f32,
    /// The damage table row.
    pub material: u32,
}

#[derive(Serialize, Deserialize, Debug, Clone, Default)]
pub struct DestructionEffect {
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub debris: Vec<DebrisPiece>,
    /// `.wav` paths relative to the imported root; one is picked at random.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub sounds: Vec<String>,
    /// Dust, splinters or smoke (BF2 sprite particle systems) are part of the effect.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub dust: bool,
}

impl DestructionEffect {
    pub fn is_empty(&self) -> bool {
        self.debris.is_empty() && self.sounds.is_empty() && !self.dust
    }
}

/// A mesh thrown off by a destruction effect. Velocities and spin are in the object's space.
#[derive(Serialize, Deserialize, Debug, Clone, Default)]
pub struct DebrisPiece {
    pub mesh: String,
    #[serde(default, skip_serializing_if = "is_zero")]
    pub mesh_index: u32,
    /// Start position relative to the object's origin.
    pub position: [f32; 3],
    /// Start velocity in m/s along x, y and z.
    pub velocity: [Spread; 3],
    /// Angular velocity in rad/s around x, y and z.
    #[serde(default)]
    pub spin: [Spread; 3],
    /// Seconds.
    pub life: Spread,
}

/// A random value between `min` and `max`, negated half of the time if `mirror`.
#[derive(Serialize, Deserialize, Debug, Clone, Copy, Default, PartialEq)]
pub struct Spread {
    pub min: f32,
    pub max: f32,
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub mirror: bool,
}

impl Spread {
    /// The value for a uniform random `t` in 0..1 and a random `flip`.
    pub fn sample(&self, t: f32, flip: bool) -> f32 {
        let value = self.min + (self.max - self.min) * t;
        if self.mirror && flip { -value } else { value }
    }
}

fn is_zero(v: &u32) -> bool {
    *v == 0
}
