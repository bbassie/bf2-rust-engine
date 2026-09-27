//! Object templates: `templates/<name>.ron`.

use serde::{Deserialize, Serialize};

use crate::{EffectPlacement, Placement, SoundDesc};

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
    /// Distance (m) from the camera to the object's origin beyond which it isn't drawn
    /// (BF2 stops drawing small objects early; big ones reach the fog). `None`: drawn as far
    /// as the view reaches.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub draw_distance: Option<f32>,
}

/// A lower-detail version of a part's mesh.
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq)]
pub struct MeshLod {
    /// `.glb` path relative to the imported root, laid out like the full-detail mesh (the
    /// part's `mesh_index` picks the same piece).
    pub mesh: String,
    /// Distance (m) from the camera from which this LOD replaces the more detailed one.
    pub distance: f32,
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
    /// Lower-detail versions of `mesh`, most detailed first, with the distances they take
    /// over at. Only for drawing: collision always uses `collision`.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub lods: Vec<MeshLod>,
    /// The same for `wreck_mesh`.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub wreck_lods: Vec<MeshLod>,
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
    /// BF2's effects for the destruction (`armor.addArmorEffect` at 0 hit points), in the
    /// object's frame. They throw the debris and play the sounds below themselves; those are
    /// for when the effects can't be played.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub effects: Vec<EffectPlacement>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub debris: Vec<DebrisPiece>,
    /// Sounds of the effect, all played at once (each picks one of its files).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub sounds: Vec<SoundDesc>,
    /// Dust, splinters or smoke (BF2 sprite particle systems) are part of the effect.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub dust: bool,
}

impl DestructionEffect {
    pub fn is_empty(&self) -> bool {
        self.effects.is_empty() && self.debris.is_empty() && self.sounds.is_empty() && !self.dust
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

#[cfg(test)]
mod tests {
    use super::*;

    /// Parts are flattened maps in RON; the LOD structs inside must read back.
    #[test]
    fn lods_round_trip_through_ron() {
        let object = ObjectDesc {
            name: "house".into(),
            parts: vec![ObjectPart {
                mesh: Some("house.glb".into()),
                lods: vec![MeshLod {
                    mesh: "house_lod1.glb".into(),
                    distance: 35.0,
                }],
                ..Default::default()
            }],
            draw_distance: Some(470.0),
            ..Default::default()
        };
        let text = ron::ser::to_string_pretty(&object, ron::ser::PrettyConfig::default()).unwrap();
        let back: ObjectDesc = ron::from_str(&text).unwrap();
        assert_eq!(back.parts[0].lods, object.parts[0].lods);
        assert_eq!(back.draw_distance, Some(470.0));
        // Old imports without the fields still load.
        let old: ObjectDesc = ron::from_str(r#"(name: "x", parts: [{"mesh": Some("x.glb"), "position": (0.0, 0.0, 0.0)}])"#).unwrap();
        assert!(old.parts[0].lods.is_empty() && old.draw_distance.is_none());
    }
}
