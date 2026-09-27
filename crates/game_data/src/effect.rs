//! Particle effects (`effects/<name>.ron`) and the tables that pick them: bullet impacts per
//! surface (`effects/impacts.ron`), muzzle flashes and detonations per weapon
//! (`effects/weapons.ron`) and what a level's surfaces are made of
//! (`levels/<name>/surfaces.ron`).
//!
//! An effect is a set of sprite emitters, light flashes, flash meshes, debris and sounds, all
//! placed in the effect's frame: +Y is up (impacts turn it along the surface normal), -Z is
//! forward (muzzle flashes turn it along the barrel).

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

use crate::DebrisPiece;

#[derive(Serialize, Deserialize, Debug, Clone, Default)]
pub struct EffectDesc {
    pub name: String,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub emitters: Vec<EmitterDesc>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub lights: Vec<LightDesc>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub meshes: Vec<FlashMeshDesc>,
    /// Meshes thrown off, which bounce and come to rest.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub debris: Vec<DebrisPiece>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub sounds: Vec<EffectSound>,
}

/// Which views show a part of an effect: the shooter's own muzzle flash has a first-person
/// and a third-person version.
#[derive(Serialize, Deserialize, Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum Views {
    #[default]
    Both,
    FirstPerson,
    ThirdPerson,
}

impl Views {
    pub fn is_both(&self) -> bool {
        *self == Views::Both
    }

    pub fn shows(self, first_person: bool) -> bool {
        match self {
            Views::Both => true,
            Views::FirstPerson => first_person,
            Views::ThirdPerson => !first_person,
        }
    }
}

#[derive(Serialize, Deserialize, Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum Blend {
    /// Soft alpha blending, drawn back to front.
    #[default]
    Alpha,
    /// Adds light (fire, flashes, sparks).
    Additive,
}

/// How a sprite is oriented.
#[derive(Serialize, Deserialize, Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum Facing {
    /// Faces the camera, turned by its rotation.
    #[default]
    Camera,
    /// Stretched along its velocity (sparks, splinters).
    Velocity,
    /// Lies in the effect's XZ plane (ripples, shock waves on the ground).
    Horizontal,
}

/// Where particles start and which way they go.
#[derive(Serialize, Deserialize, Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum EmitShape {
    /// Anywhere in the box `±extent` around the emitter, flying along `direction`.
    #[default]
    Box,
    /// In the box, flying outwards in the plane perpendicular to `direction` (rings).
    Ring,
    /// In the box, flying away from the emitter's center.
    Sphere,
}

/// A value over a particle's life (or an emitter's emission time) `t` in 0..1:
/// `a t³ + b t² + c t + d` for `[a, b, c, d]`, as BF2 stores its graphs.
#[derive(Serialize, Deserialize, Debug, Clone, Copy, PartialEq)]
#[serde(transparent)]
pub struct Curve(pub [f32; 4]);

impl Curve {
    pub const ONE: Curve = Curve([0.0, 0.0, 0.0, 1.0]);

    pub fn zero() -> Self {
        Curve([0.0; 4])
    }

    pub fn at(&self, t: f32) -> f32 {
        let [a, b, c, d] = self.0;
        ((a * t + b) * t + c) * t + d
    }

    pub fn is_one(&self) -> bool {
        *self == Self::ONE
    }
}

impl Default for Curve {
    fn default() -> Self {
        Self::ONE
    }
}

/// A texture animation: `count` frames in rows of `columns`, each `frame_size` (a fraction
/// of the emitter's `uv_rect`), played at `fps` (0 = a still frame).
#[derive(Serialize, Deserialize, Debug, Clone, Copy, PartialEq)]
pub struct Frames {
    pub count: u32,
    pub columns: u32,
    pub frame_size: [f32; 2],
    pub fps: f32,
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub once: bool,
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub random_start: bool,
}

/// Sprite particles. Times in seconds, distances in meters, angles in degrees.
#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct EmitterDesc {
    /// `.dds` relative to the imported root.
    pub texture: String,
    /// The part of `texture` to use: offset and size in 0..1 (atlases pack many sprites).
    #[serde(default = "full_rect")]
    pub uv_rect: [f32; 4],
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub frames: Option<Frames>,
    #[serde(default)]
    pub blend: Blend,
    #[serde(default)]
    pub facing: Facing,
    /// `Velocity` sprites are lengthened by their speed times this many seconds.
    #[serde(default, skip_serializing_if = "is_zero")]
    pub stretch: f32,
    #[serde(default, skip_serializing_if = "Views::is_both")]
    pub views: Views,
    /// Emitter position in the effect's frame.
    #[serde(default)]
    pub position: [f32; 3],
    #[serde(default)]
    pub shape: EmitShape,
    /// Half size of the box particles start in.
    #[serde(default)]
    pub extent: [f32; 3],
    /// Wait before emitting.
    #[serde(default)]
    pub delay: f32,
    /// How long it emits.
    pub duration: f32,
    /// Particles per second while emitting.
    pub rate: f32,
    /// The first `burst` seconds' worth of particles come out at once.
    #[serde(default)]
    pub burst: f32,
    /// Emits again and again while the effect lasts.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub looping: bool,
    /// Particle life, `[min, max]`.
    pub life: [f32; 2],
    /// Start direction (normalized, or zero for particles that start at rest).
    pub direction: [f32; 3],
    /// Random deviation of the start direction around the X, Y and Z axes: `±spread`.
    #[serde(default)]
    pub spread: [f32; 3],
    /// Start speed `[min, max]`, times `speed_curve` over the emission time.
    pub speed: [f32; 2],
    #[serde(default, skip_serializing_if = "Curve::is_one")]
    pub speed_curve: Curve,
    /// Upward acceleration (negative falls), times `gravity_curve`.
    #[serde(default)]
    pub gravity: f32,
    #[serde(default, skip_serializing_if = "Curve::is_one")]
    pub gravity_curve: Curve,
    /// Air resistance: `dv/dt = -drag |v| v`, times `drag_curve`.
    #[serde(default)]
    pub drag: f32,
    #[serde(default, skip_serializing_if = "Curve::is_one")]
    pub drag_curve: Curve,
    /// Diameter `[min, max]`, times `size_curve`.
    pub size: [f32; 2],
    #[serde(default, skip_serializing_if = "Curve::is_one")]
    pub size_curve: Curve,
    #[serde(default, skip_serializing_if = "Curve::is_one")]
    pub opacity_curve: Curve,
    /// Linear RGB, may exceed 1 for glowing particles. Blended by `color_curve`.
    pub colors: [[f32; 3]; 2],
    #[serde(default = "Curve::zero")]
    pub color_curve: Curve,
    /// Particles are up to this fraction darker.
    #[serde(default)]
    pub brightness_jitter: f32,
    /// Random start rotation `±rotation`.
    #[serde(default)]
    pub rotation: f32,
    /// Spin in radians per second `[min, max]`, times `spin_curve`.
    #[serde(default)]
    pub spin: [f32; 2],
    #[serde(default, skip_serializing_if = "Curve::is_one")]
    pub spin_curve: Curve,
}

fn is_zero(v: &f32) -> bool {
    *v == 0.0
}

fn full_rect() -> [f32; 4] {
    [0.0, 0.0, 1.0, 1.0]
}

/// A short flash of light (a point light).
#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct LightDesc {
    pub position: [f32; 3],
    /// Linear RGB, may exceed 1.
    pub color: [f32; 3],
    /// Meters the light reaches.
    pub radius: f32,
    /// Seconds; the light fades out over them.
    pub life: f32,
    #[serde(default, skip_serializing_if = "Views::is_both")]
    pub views: Views,
}

/// A small additive mesh shown for a moment: the star of a muzzle flash.
#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct FlashMeshDesc {
    /// `.dds` relative to the imported root.
    pub texture: String,
    pub positions: Vec<[f32; 3]>,
    pub uvs: Vec<[f32; 2]>,
    pub indices: Vec<u32>,
    /// Placement in the effect's frame.
    pub position: [f32; 3],
    /// Uniform scale `[min, max]`.
    pub scale: [f32; 2],
    /// Random roll around the flash's forward (-Z) axis in steps of this many degrees
    /// (0 = none).
    #[serde(default)]
    pub roll_step: f32,
    pub life: f32,
    /// Brightness multiplier.
    #[serde(default = "one")]
    pub intensity: f32,
    #[serde(default, skip_serializing_if = "Views::is_both")]
    pub views: Views,
}

fn one() -> f32 {
    1.0
}

#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct EffectSound {
    /// `.wav`/`.ogg` paths relative to the imported root; one is picked at random.
    pub files: Vec<String>,
    #[serde(default = "one")]
    pub volume: f32,
    #[serde(default, skip_serializing_if = "Views::is_both")]
    pub views: Views,
}

/// `effects/impacts.ron`: the effect for a projectile of material `a` hitting a surface of
/// material `b` (ids as in `materials.ron`).
#[derive(Serialize, Deserialize, Debug, Clone, Default)]
pub struct ImpactTable {
    pub effects: BTreeMap<(u32, u32), String>,
}

impl ImpactTable {
    pub fn effect(&self, projectile: u32, surface: u32) -> Option<&str> {
        self.effects.get(&(projectile, surface)).map(String::as_str)
    }
}

/// `effects/weapons.ron`: effects of weapons, by weapon name.
#[derive(Serialize, Deserialize, Debug, Clone, Default)]
pub struct WeaponEffectTable {
    pub weapons: BTreeMap<String, WeaponEffects>,
}

#[derive(Serialize, Deserialize, Debug, Clone, Default)]
pub struct WeaponEffects {
    /// Effect at the muzzle for every shot.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub muzzle: Option<String>,
    /// Muzzle position in the weapon model's frame (-Z forward).
    #[serde(default)]
    pub muzzle_offset: [f32; 3],
    /// Effect where the projectile detonates (grenades, rockets); replaces the impact.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub detonation: Option<String>,
}

/// `levels/<name>/surfaces.ron`: materials of the level's surfaces, for impact effects.
#[derive(Serialize, Deserialize, Debug, Clone, Default)]
pub struct SurfaceMap {
    /// One material id byte per heightmap sample, same layout as the heightmap. Relative
    /// to the level folder.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub terrain: Option<String>,
    /// Material of each static mesh part: `(glb path, mesh index)` as in
    /// [`crate::ObjectPart`]. Mostly the material covering most of its collision surface.
    #[serde(default)]
    pub meshes: BTreeMap<(String, u32), u32>,
}
