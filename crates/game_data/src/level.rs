//! Level description: `levels/<name>/level.ron`.

use serde::{Deserialize, Serialize};

use crate::{Placement, SoundDesc, TeamDesc};

/// Everything needed to load a level on the client and the server.
#[derive(Serialize, Deserialize, Debug, Clone, Default)]
pub struct LevelDesc {
    /// Folder name, e.g. `strike_at_karkand`.
    pub name: String,
    /// Human readable name, e.g. `Strike at Karkand`.
    pub display_name: String,
    #[serde(default)]
    pub terrain: Option<TerrainDesc>,
    #[serde(default)]
    pub water: Option<WaterDesc>,
    #[serde(default)]
    pub environment: EnvironmentDesc,
    /// Static, non-networked objects (buildings, props, bridges).
    #[serde(default)]
    pub statics: Vec<StaticInstance>,
    /// Road meshes draped over the terrain (visual only).
    #[serde(default)]
    pub roads: Vec<RoadDesc>,
    /// Available game mode layouts (conquest 16/32/64, ...).
    #[serde(default)]
    pub game_modes: Vec<GameModeDesc>,
    /// Team 1 and team 2.
    #[serde(default)]
    pub teams: Vec<TeamDesc>,
    /// Top-down map image (`.dds`, relative to the imported root) covering the whole
    /// terrain, north up.
    #[serde(default)]
    pub minimap: Option<String>,
    /// Models for control point flags.
    #[serde(default)]
    pub flag_models: FlagModels,
    /// A neutral control point's map icon (the teams' icons are in [`TeamDesc::icons`]).
    #[serde(default)]
    pub neutral_icons: crate::TeamIcons,
    /// How the layouts' vehicles show on the maps, by template (lowercase).
    #[serde(default)]
    pub vehicle_icons: std::collections::BTreeMap<String, crate::VehicleIcon>,
    /// Conquest: tickets per minute a team loses once it holds no control point and has
    /// nobody alive.
    #[serde(default = "default_ticket_loss_at_end")]
    pub ticket_loss_at_end_per_minute: f32,
    /// Draw-only grass, plants and trees: a [`crate::VegetationDesc`] file relative to the
    /// level folder.
    #[serde(default)]
    pub vegetation: Option<String>,
}

/// A square heightmap terrain.
#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct TerrainDesc {
    /// Path relative to the level folder of a little-endian `u16` heightmap
    /// (`resolution * resolution` samples, row-major, row 0 at -Z).
    pub heightmap: String,
    /// Samples per side.
    pub resolution: u32,
    /// Distance in meters between adjacent samples.
    pub spacing: f32,
    /// Meters per heightmap unit: `height = sample * height_scale + origin.y`.
    pub height_scale: f32,
    /// World position of sample (0, 0) in meters.
    pub origin: [f32; 3],
    /// Optional color map texture(s), relative to the level folder. Tiled in a grid
    /// of `color_map_tiles x color_map_tiles` over the whole terrain.
    #[serde(default)]
    pub color_maps: Vec<String>,
    #[serde(default = "one")]
    pub color_map_tiles: u32,
    /// Up to 6 tiling ground textures (grass, rock, gravel, ...) blended per patch.
    #[serde(default)]
    pub detail_textures: Vec<TerrainDetailDesc>,
    /// Per patch (same order as `color_maps`): two weight maps, relative to the level
    /// folder. Weight of detail texture `i` is channel B, G, R (`i % 3`) of map `i / 3`.
    /// Empty strings where a patch has none.
    #[serde(default)]
    pub detail_weights: Vec<[String; 2]>,
    /// Per patch baked lighting: G = sun visibility, B = sky light. Relative to the level folder.
    #[serde(default)]
    pub lightmaps: Vec<String>,
    /// Low-detail scenery terrain around this one, so the world doesn't end at its edge.
    #[serde(default)]
    pub surrounding: Option<SurroundingTerrainDesc>,
}

/// A coarse heightmap covering a 3x3 grid of terrain-sized cells centred on the terrain.
/// Only the 8 outer cells are drawn; nothing collides with it.
#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct SurroundingTerrainDesc {
    /// Little-endian `u16` heightmap relative to the level folder (row 0 at -Z).
    pub heightmap: String,
    /// Samples per side.
    pub resolution: u32,
    pub spacing: f32,
    /// Meters per heightmap unit.
    pub height_scale: f32,
    /// World position of sample (0, 0).
    pub origin: [f32; 3],
    /// Colour map per cell, row-major from -Z (index 4 is the centre and unused). Relative to
    /// the level folder; empty where a cell has none.
    #[serde(default)]
    pub color_maps: Vec<String>,
    /// Linear colour multiplier that matches the colour maps to the terrain's at the seam.
    #[serde(default = "no_tint")]
    pub tint: [f32; 3],
}

/// One tiling ground texture.
#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct TerrainDetailDesc {
    /// Texture path relative to the imported root.
    pub texture: String,
    /// Meters per repeat when projected from above.
    pub top_tile_size: f32,
    /// Meters per repeat on steep faces (U, V), used when `tri_planar`.
    pub side_tile_size: [f32; 2],
    /// Also project onto steep faces (cliffs) instead of stretching the top projection.
    #[serde(default)]
    pub tri_planar: bool,
}

impl TerrainDesc {
    /// World-space extent of the terrain along X and Z.
    pub fn world_size(&self) -> f32 {
        (self.resolution.saturating_sub(1)) as f32 * self.spacing
    }
}

#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct WaterDesc {
    pub height: f32,
    #[serde(default = "default_water_color")]
    pub color: [f32; 4],
    /// Water deeper than this (meters) hides the ground below completely.
    #[serde(default = "default_water_opaque_depth")]
    pub opaque_depth: f32,
    /// Sun glint colour (rgb) and strength (a).
    #[serde(default = "default_water_specular")]
    pub specular: [f32; 4],
    #[serde(default = "default_water_specular_power")]
    pub specular_power: f32,
    /// Drift of the wave pattern in meters per second along X and Z.
    #[serde(default)]
    pub wave_drift: [f32; 2],
    /// How fast the wave pattern changes shape (normal map slices per second).
    #[serde(default = "default_water_wave_speed")]
    pub wave_speed: f32,
    /// Tiling normal map (`.dds`, relative to the imported root, xyz = world normal with +Y
    /// up); a 3D texture animates through its slices.
    #[serde(default)]
    pub normal_map: Option<String>,
    /// Cube map the surface reflects (`.dds`, relative to the imported root). Without one it
    /// reflects the fog colour.
    #[serde(default)]
    pub reflection_map: Option<String>,
}

#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct EnvironmentDesc {
    /// Direction the sunlight travels (pointing away from the sun).
    pub sun_direction: [f32; 3],
    pub sun_color: [f32; 3],
    pub ambient_color: [f32; 3],
    pub sky_color: [f32; 3],
    pub fog_color: [f32; 3],
    /// Fog start and end distance in meters.
    pub fog_range: [f32; 2],
    /// Maximum view distance in meters.
    pub view_distance: f32,
    #[serde(default)]
    pub sky: Option<SkyDesc>,
    /// How the level's world (static objects, terrain, trees) is lit, beyond the dynamic
    /// `sun_color` / `ambient_color` above. `None` for levels made without it: renderers
    /// then light everything from those two.
    #[serde(default)]
    pub lighting: Option<WorldLighting>,
}

/// The light of a level's world, as BF2's `sky.con` sets it for its shaders. Colours are
/// gamma-space light factors: BF2 multiplies texture colours by them without converting to
/// linear light, and some exceed 1.
///
/// BF2 lights each kind of object with its own colours (visibilities come from the baked
/// lightmaps, 0..1):
/// - Static objects (`RaShaderSTM.fx`): `texture x 2 x (0.65 x sky visibility x static_sky
///   + sun visibility x N.L x static_sun + lamp light x point)`.
/// - Terrain (`TerrainShader*.fx`): `colormap x detail x (2 x sun visibility x
///   terrain_sun + sky visibility x terrain_gi)`, detail textures averaging 0.5 (so `x 2`
///   keeps the colour map's brightness). The terrain lightmap's sun visibility already
///   contains N.L, times `terrain_sun_scale`. Undergrowth is lit the same way.
/// - Trees (`RaShaderLeaf.fx`, trunks): `texture x 2 x (tree_sun x N.L + tree_ambient / 2)`.
/// - Soldiers and vehicles (`SkinnedMesh.fx`, `RaShaderBM.fx`): `texture x (sun_color x N.L
///   + ambient_color x hemisphere)`, the hemisphere colour blending the ground colour below
///   into `dynamic_sky` by the normal's height, minus `hemi_lerp_bias`.
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq)]
#[serde(default)]
pub struct WorldLighting {
    /// Static objects: sky light (BF2 `Lightmanager.staticSkyColor`).
    pub static_sky: [f32; 3],
    /// Static objects: sunlight (`staticSunColor`); black on night levels.
    pub static_sun: [f32; 3],
    /// Static objects' highlights (`staticSpecularColor`).
    pub static_specular: [f32; 3],
    /// Lamps baked into static objects' lightmaps (`singlePointColor`).
    pub point: [f32; 3],
    /// Terrain sunlight (`terrain.sunColor`).
    pub terrain_sun: [f32; 3],
    /// Terrain sky light (`terrain.GIColor`).
    pub terrain_gi: [f32; 3],
    /// Sun visibility in the terrain lightmaps on open flat ground, relative to N.L: 1 for
    /// most levels, lower where they were baked with a weaker sun (overcast Operation
    /// Harvest: 0.5). Measured from the lightmaps.
    pub terrain_sun_scale: f32,
    /// `terrain.waterSunIntensity`.
    pub water_sun_intensity: f32,
    /// Trees: ambient light (`treeAmbientColor`).
    pub tree_ambient: [f32; 3],
    /// Trees: sunlight (`treeSunColor`).
    pub tree_sun: [f32; 3],
    /// Trees: sky colour (`treeSkyColor`).
    pub tree_sky: [f32; 3],
    /// Soldiers and vehicles: sky colour of the hemisphere light (`Lightmanager.skyColor`).
    pub dynamic_sky: [f32; 3],
    /// Soldiers and vehicles: how much the hemisphere light leans to the ground colour.
    pub hemi_lerp_bias: f32,
    /// Soldiers and vehicles: colour of lamps (`DynamicPointColor`).
    pub dynamic_point: [f32; 3],
    /// Particles in sunlight and in shadow (`effectSunColor`, `effectShadowColor`).
    pub effect_sun: [f32; 3],
    pub effect_shadow: [f32; 3],
    /// BF2's "faked HDR" (the Special Forces night levels): the colours once the eye has
    /// adapted to the dark (`*High`) and to bright light (`*Low`). The game blends between
    /// them by how bright the view is; the colours above are the level editor's.
    pub dark_adapted: Option<AdaptedLighting>,
    pub bright_adapted: Option<AdaptedLighting>,
    /// Seconds the faked HDR takes to adapt to bright light and to the dark.
    pub adaptation_seconds: [f32; 2],
}

impl Default for WorldLighting {
    fn default() -> Self {
        Self {
            static_sky: [0.5; 3],
            static_sun: [0.8; 3],
            static_specular: [0.5; 3],
            point: [0.0; 3],
            terrain_sun: [0.75; 3],
            terrain_gi: [0.7; 3],
            terrain_sun_scale: 1.0,
            water_sun_intensity: 0.8,
            tree_ambient: [0.5; 3],
            tree_sun: [0.8; 3],
            tree_sky: [0.8; 3],
            dynamic_sky: [0.8; 3],
            hemi_lerp_bias: 0.25,
            dynamic_point: [1.0; 3],
            effect_sun: [0.9; 3],
            effect_shadow: [0.3; 3],
            dark_adapted: None,
            bright_adapted: None,
            adaptation_seconds: [0.5, 2.0],
        }
    }
}

/// The colours BF2's faked HDR replaces (see [`WorldLighting`]).
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq)]
pub struct AdaptedLighting {
    pub static_sky: [f32; 3],
    pub terrain_gi: [f32; 3],
    pub point: [f32; 3],
    /// Soldiers' and vehicles' sunlight (moonlight).
    pub sun: [f32; 3],
    pub dynamic_sky: [f32; 3],
    pub dynamic_point: [f32; 3],
}

impl WorldLighting {
    /// With the dark-adapted colours of the faked HDR where the level has them: what BF2
    /// showed while the player looked at dark surroundings, the usual case on night levels.
    /// Also returns the dynamic sun colour to use (`sun_color` unless adapted).
    pub fn dark_adapted(&self, sun_color: [f32; 3]) -> (WorldLighting, [f32; 3]) {
        let mut out = self.clone();
        let mut sun = sun_color;
        if let Some(adapted) = &self.dark_adapted {
            out.static_sky = adapted.static_sky;
            out.terrain_gi = adapted.terrain_gi;
            out.point = adapted.point;
            out.dynamic_sky = adapted.dynamic_sky;
            out.dynamic_point = adapted.dynamic_point;
            sun = adapted.sun;
        }
        (out, sun)
    }
}

/// A textured sky dome drawn around the camera.
#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct SkyDesc {
    /// Dome `.glb` path relative to the imported root.
    pub mesh: String,
    /// Dome radius in its own units (used to scale it to the view distance).
    pub radius: f32,
    /// Texture path relative to the imported root.
    pub texture: String,
    /// Rotation around the vertical axis, degrees.
    #[serde(default)]
    pub rotation: f32,
}

impl Default for EnvironmentDesc {
    fn default() -> Self {
        Self {
            sun_direction: [-0.4, -0.8, -0.45],
            sun_color: [1.0, 0.95, 0.85],
            ambient_color: [0.45, 0.47, 0.5],
            sky_color: [0.55, 0.68, 0.85],
            fog_color: [0.7, 0.75, 0.8],
            fog_range: [300.0, 900.0],
            view_distance: 900.0,
            sky: None,
            lighting: None,
        }
    }
}

/// One placed instance of an object template.
#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct StaticInstance {
    /// Template name, resolved to `templates/<template>.ron`.
    pub template: String,
    #[serde(flatten)]
    pub placement: Placement,
}

/// A road decal mesh.
#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct RoadDesc {
    /// `.glb` path relative to the imported root.
    pub mesh: String,
    pub position: [f32; 3],
}

/// A game mode layout, e.g. conquest at 64 players.
#[derive(Serialize, Deserialize, Debug, Clone, Default)]
pub struct GameModeDesc {
    /// Mode id, e.g. `gpm_cq`.
    pub mode: String,
    /// Layout size (16, 32, 64).
    pub size: u32,
    #[serde(default)]
    pub control_points: Vec<ControlPointDesc>,
    #[serde(default)]
    pub spawn_points: Vec<SpawnPointDesc>,
    #[serde(default)]
    pub vehicle_spawners: Vec<VehicleSpawnerDesc>,
    /// Additional objects that only exist in this layout.
    #[serde(default)]
    pub statics: Vec<StaticInstance>,
}

/// The flag pole every control point has, and the flags that go up and down on it.
#[derive(Serialize, Deserialize, Debug, Clone, Default)]
pub struct FlagModels {
    /// Pole mesh (`.glb`), origin at its foot.
    #[serde(default)]
    pub pole: Option<String>,
    /// Pole height in meters.
    #[serde(default)]
    pub pole_height: f32,
    /// Flags of neutral, team 1 and team 2: skinned `.glb`s with a looping `idle` clip.
    #[serde(default)]
    pub flags: [Option<String>; 3],
    /// Looping sound of the flag flapping.
    #[serde(default)]
    pub sound: Option<SoundDesc>,
}

#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct ControlPointDesc {
    pub id: String,
    /// Display name.
    #[serde(default)]
    pub name: String,
    pub position: [f32; 3],
    /// 0 = neutral, 1/2 = team.
    pub initial_team: u8,
    pub radius: f32,
    /// Main bases can't be captured.
    #[serde(default)]
    pub uncapturable: bool,
    /// Worth of this point to team 1 and team 2 for ticket bleed (BF2 `areaValueTeam1/2`).
    #[serde(default)]
    pub area_value: [f32; 2],
    /// Seconds for one attacker to raise the flag, and to lower an enemy flag.
    #[serde(default = "default_capture_time")]
    pub time_to_get_control: f32,
    #[serde(default = "default_capture_time")]
    pub time_to_lose_control: f32,
    /// Only this team (1/2) may capture it; 0 = both.
    #[serde(default)]
    pub only_takeable_by_team: u8,
    /// Tickets the other team loses at once when this point is captured.
    #[serde(default)]
    pub enemy_ticket_loss_when_captured: f32,
}

#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct SpawnPointDesc {
    /// Control point this spawn belongs to.
    pub control_point: String,
    #[serde(flatten)]
    pub placement: Placement,
}

#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct VehicleSpawnerDesc {
    /// Control point this spawner belongs to (vehicles follow the owning team).
    #[serde(default)]
    pub control_point: Option<String>,
    /// Template spawned for team 1 and team 2.
    pub templates: [Option<String>; 2],
    #[serde(flatten)]
    pub placement: Placement,
    #[serde(default)]
    pub min_respawn_seconds: f32,
    #[serde(default)]
    pub max_respawn_seconds: f32,
}

fn default_capture_time() -> f32 {
    10.0
}

fn default_ticket_loss_at_end() -> f32 {
    200.0
}

fn one() -> u32 {
    1
}

fn default_water_color() -> [f32; 4] {
    [0.1, 0.25, 0.3, 0.8]
}

fn no_tint() -> [f32; 3] {
    [1.0; 3]
}

fn default_water_opaque_depth() -> f32 {
    4.0
}

fn default_water_specular() -> [f32; 4] {
    [1.0, 0.95, 0.85, 1.0]
}

fn default_water_specular_power() -> f32 {
    40.0
}

fn default_water_wave_speed() -> f32 {
    0.5
}
