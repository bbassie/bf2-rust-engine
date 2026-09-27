//! Kits (`kits/<name>.ron`) and handheld weapons (`weapons/<name>.ron`).

use serde::{Deserialize, Serialize};

/// A kit: the loadout of one soldier class.
#[derive(Serialize, Deserialize, Debug, Clone, Default)]
pub struct KitDesc {
    pub name: String,
    /// Class, e.g. `Assault`, `Medic`, `AT`.
    #[serde(default)]
    pub kind: String,
    /// Weapon names, in the order the kit lists them.
    pub weapons: Vec<String>,
}

#[derive(Serialize, Deserialize, Debug, Clone, Copy, PartialEq, Eq)]
pub enum FireMode {
    Single,
    Burst,
    Auto,
}

/// A handheld weapon.
#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct WeaponDesc {
    pub name: String,
    /// Localization key or plain name.
    #[serde(default)]
    pub display_name: String,
    /// Inventory slot (BF2 `itemIndex`): 1 knife, 2 pistol, 3 primary, 4 grenade, 5+ gadgets.
    #[serde(default)]
    pub slot: u32,
    /// First-person and third-person models (`.glb`, one mesh per part), relative to the
    /// imported root. Third-person part `n` attaches to skeleton bone `mesh{n+1}`.
    #[serde(default)]
    pub mesh_1p: Option<String>,
    #[serde(default)]
    pub mesh_3p: Option<String>,
    /// Upper-body animation set (`.glb`) for third person.
    #[serde(default)]
    pub animations_3p: Option<String>,
    /// Arms-and-weapon animation set (`.glb`) for first person. First-person part `n`
    /// attaches to bone `mesh{n+1}` of the `1p_setup` skeleton.
    #[serde(default)]
    pub animations_1p: Option<String>,
    pub rounds_per_minute: f32,
    /// Selectable modes, first is the default.
    pub fire_modes: Vec<FireMode>,
    pub magazine_size: u32,
    pub magazines: u32,
    /// Seconds.
    pub reload_time: f32,
    /// Seconds until the weapon can be used after switching to it.
    pub deploy_time: f32,
    /// Projectiles per trigger pull (shotguns fire several).
    #[serde(default = "one")]
    pub projectiles_per_shot: u32,
    pub projectile: ProjectileDesc,
    pub deviation: DeviationDesc,
    pub recoil: RecoilDesc,
    /// Field-of-view multipliers per zoom step; 0 means "not zoomed".
    #[serde(default)]
    pub zoom_factors: Vec<f32>,
    #[serde(default)]
    pub zoom: ZoomDesc,
    #[serde(default)]
    pub sounds: WeaponSounds,
}

/// How zooming looks (BF2 `DefaultZoomComp`).
#[derive(Serialize, Deserialize, Debug, Clone, Default)]
pub struct ZoomDesc {
    /// First-person model that replaces `mesh_1p` while zoomed (BF2 `zoomLod`): the view
    /// through the scope, or the sights with a blurred rear sight. One part, on bone `mesh1`
    /// in the zoom pose. It is modelled for the unzoomed view model field of view.
    #[serde(default)]
    pub mesh_1p: Option<String>,
    /// Seconds from pressing zoom until `mesh_1p` replaces the weapon.
    #[serde(default)]
    pub delay: f32,
    /// Seconds from pressing zoom until the field of view narrows.
    #[serde(default)]
    pub fov_delay: f32,
    /// Bolt-action rifles leave the zoom after every shot.
    #[serde(default)]
    pub out_after_fire: bool,
}

#[derive(Serialize, Deserialize, Debug, Clone, Default)]
pub struct ProjectileDesc {
    /// Meters per second.
    pub velocity: f32,
    pub damage: f32,
    /// Damage never drops below this.
    pub min_damage: f32,
    /// Damage starts dropping at this distance (meters) and reaches `min_damage` at
    /// `falloff_end`. Both 0 = no falloff.
    #[serde(default)]
    pub falloff_start: f32,
    #[serde(default)]
    pub falloff_end: f32,
    /// Multiplier on gravity (bullets are ~1 in BF2 unless set).
    #[serde(default = "one_f")]
    pub gravity: f32,
    /// Seconds before the projectile disappears.
    pub time_to_live: f32,
    /// Material id in the damage table.
    #[serde(default)]
    pub material: u32,
    /// Rockets and grenades: damage at the center of the explosion, falling off to 0 at
    /// `explosion_radius` meters. 0 for bullets.
    #[serde(default)]
    pub explosion_damage: f32,
    #[serde(default)]
    pub explosion_radius: f32,
}

/// BF2 deviation (spread) settings, in degrees. See docs/formats/gameplay-data.md §6.4.
#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct DeviationDesc {
    /// Base cone.
    pub min: f32,
    pub stand: f32,
    pub crouch: f32,
    pub prone: f32,
    pub zoom: f32,
    /// Added per shot: `[max, add, decay per 1/30 s]`.
    pub fire: [f32; 3],
    /// From movement: `[max, per forward speed, per strafe speed, decay]`.
    pub speed: [f32; 4],
    /// From jumping: `[max, add, decay]`.
    pub misc: [f32; 3],
}

impl Default for DeviationDesc {
    fn default() -> Self {
        Self {
            min: 0.5,
            stand: 1.0,
            crouch: 1.0,
            prone: 1.0,
            zoom: 1.0,
            fire: [0.0; 3],
            speed: [0.0; 4],
            misc: [0.0; 3],
        }
    }
}

/// Camera kick per shot in degrees, as uniform ranges.
#[derive(Serialize, Deserialize, Debug, Clone, Default)]
pub struct RecoilDesc {
    pub up: [f32; 2],
    pub left_right: [f32; 2],
    #[serde(default = "one_f")]
    pub zoom_modifier: f32,
}

#[derive(Serialize, Deserialize, Debug, Clone, Default)]
pub struct WeaponSounds {
    /// `.wav` paths relative to the imported root.
    #[serde(default)]
    pub fire_1p: Option<String>,
    #[serde(default)]
    pub fire_3p: Option<String>,
    #[serde(default)]
    pub reload_1p: Option<String>,
}

fn one() -> u32 {
    1
}

fn one_f() -> f32 {
    1.0
}
