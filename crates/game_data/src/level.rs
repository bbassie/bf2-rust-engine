//! Level description: `levels/<name>/level.ron`.

use serde::{Deserialize, Serialize};

use crate::{Placement, TeamDesc};

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
    /// Conquest: tickets per minute a team loses once it holds no control point and has
    /// nobody alive.
    #[serde(default = "default_ticket_loss_at_end")]
    pub ticket_loss_at_end_per_minute: f32,
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
