//! The levels the menu offers: the built-in test range and the levels of `imported/` and
//! the mods.

use super::*;

/// Levels to offer: the built-in test range, then the imported and mod levels.
#[derive(Resource)]
pub struct LevelCatalog {
    pub levels: Vec<LevelInfo>,
    pub(super) scan: Option<Task<Vec<LevelInfo>>>,
    /// Bumped when the list changes.
    pub(super) version: u32,
}

impl Default for LevelCatalog {
    fn default() -> Self {
        let test_range = game_shared::level::test_range();
        let map_area = test_range.heightmap.as_ref().map(|h| {
            let size = h.world_size().max(1.0);
            (Vec2::new(h.origin.x, h.origin.z), size)
        });
        let test_range = test_range.desc;
        Self {
            levels: vec![LevelInfo {
                name: TEST_RANGE.into(),
                display_name: test_range.display_name,
                minimap: None,
                layouts: test_range
                    .game_modes
                    .iter()
                    .map(|g| (g.mode.clone(), g.size))
                    .collect(),
                teams: ["Team 1".into(), "Team 2".into()],
                map_area,
                icons: Default::default(),
                previews: test_range
                    .game_modes
                    .iter()
                    .map(|g| LayoutPreview {
                        mode: g.mode.clone(),
                        size: g.size,
                        control_points: g.control_points.clone(),
                        vehicles: g.vehicle_spawners.clone(),
                    })
                    .collect(),
                vehicle_icons: Default::default(),
            }],
            scan: None,
            version: 0,
        }
    }
}

impl LevelCatalog {
    pub fn get(&self, name: &str) -> Option<&LevelInfo> {
        self.levels.iter().find(|l| l.name == name)
    }
}

#[derive(Clone, Debug)]
pub struct LevelInfo {
    /// Folder name.
    pub name: String,
    pub display_name: String,
    /// Map image, relative to the imported root.
    pub minimap: Option<String>,
    /// Game mode layouts: `(mode, size)`.
    pub layouts: Vec<(String, u32)>,
    pub teams: [String; 2],
    /// The map's north-west corner (world X, Z) and width in meters.
    pub map_area: Option<(Vec2, f32)>,
    /// Flag icons of neutral, team 1 and team 2.
    pub icons: [game_data::TeamIcons; 3],
    /// What each layout puts on the map.
    pub previews: Vec<LayoutPreview>,
    pub vehicle_icons: std::collections::BTreeMap<String, game_data::VehicleIcon>,
}

/// A layout's control points and vehicle spawners, for the map preview.
#[derive(Clone, Debug)]
pub struct LayoutPreview {
    pub mode: String,
    pub size: u32,
    pub control_points: Vec<game_data::ControlPointDesc>,
    pub vehicles: Vec<game_data::VehicleSpawnerDesc>,
}

impl LevelInfo {
    /// The preview of a layout (the closest size of the mode).
    pub fn preview(&self, mode: &str, size: u32) -> Option<&LayoutPreview> {
        self.previews
            .iter()
            .filter(|p| p.mode == mode)
            .min_by_key(|p| p.size.abs_diff(size))
    }
}

/// The parts of a `level.ron` the menu needs; the rest is skipped.
#[derive(Deserialize)]
struct LevelSummary {
    display_name: String,
    #[serde(default)]
    minimap: Option<String>,
    #[serde(default)]
    terrain: Option<TerrainSummary>,
    #[serde(default)]
    game_modes: Vec<ModeSummary>,
    #[serde(default)]
    teams: Vec<TeamSummary>,
    #[serde(default)]
    neutral_icons: game_data::TeamIcons,
    #[serde(default)]
    vehicle_icons: std::collections::BTreeMap<String, game_data::VehicleIcon>,
}

#[derive(Deserialize)]
struct TerrainSummary {
    resolution: u32,
    spacing: f32,
    origin: [f32; 3],
}

#[derive(Deserialize)]
struct ModeSummary {
    mode: String,
    size: u32,
    #[serde(default)]
    control_points: Vec<game_data::ControlPointDesc>,
    #[serde(default)]
    vehicle_spawners: Vec<game_data::VehicleSpawnerDesc>,
}

#[derive(Deserialize)]
struct TeamSummary {
    name: String,
    #[serde(default)]
    icons: game_data::TeamIcons,
}

/// `gpm_cq` -> `Conquest`.
pub(super) fn mode_label(mode: &str) -> String {
    match mode {
        "gpm_cq" => "Conquest".into(),
        "gpm_coop" => "Co-op".into(),
        "gpm_ctf" => "Capture the Flag".into(),
        "sp1" | "sp2" | "sp3" => "Singleplayer".into(),
        other => other.trim_start_matches("gpm_").to_uppercase(),
    }
}

pub(super) fn scan_levels(mut catalog: ResMut<LevelCatalog>, paths: Res<GamePaths>) {
    let paths = paths.clone();
    catalog.scan = Some(AsyncComputeTaskPool::get().spawn(async move {
        let mut levels = Vec::new();
        for name in paths.level_names() {
            let path = paths.level_dir(&name).join("level.ron");
            let Ok(text) = std::fs::read_to_string(&path) else {
                continue;
            };
            let summary: LevelSummary = match ron::from_str(&text) {
                Ok(summary) => summary,
                Err(err) => {
                    warn!("{}: {err}", path.display());
                    continue;
                }
            };
            let team = |i: usize| {
                summary
                    .teams
                    .get(i)
                    .map(|t| t.name.clone())
                    .filter(|n| !n.is_empty())
                    .unwrap_or_else(|| format!("Team {}", i + 1))
            };
            let mut layouts: Vec<(String, u32)> = summary
                .game_modes
                .iter()
                .map(|g| (g.mode.clone(), g.size))
                .collect();
            layouts.sort_by(|a, b| (a.0 != "gpm_cq", &a.0, a.1).cmp(&(b.0 != "gpm_cq", &b.0, b.1)));
            layouts.dedup();
            let icons = |i: usize| summary.teams.get(i).map(|t| t.icons.clone()).unwrap_or_default();
            let map_area = summary.terrain.as_ref().map(|t| {
                let size = (t.resolution.saturating_sub(1)) as f32 * t.spacing;
                (Vec2::new(t.origin[0], t.origin[2]), size.max(1.0))
            });
            let icons = [summary.neutral_icons.clone(), icons(0), icons(1)];
            let previews = summary
                .game_modes
                .into_iter()
                .map(|g| LayoutPreview {
                    mode: g.mode,
                    size: g.size,
                    control_points: g.control_points,
                    vehicles: g.vehicle_spawners,
                })
                .collect();
            levels.push(LevelInfo {
                name: name.clone(),
                display_name: summary.display_name.clone(),
                minimap: summary.minimap.clone(),
                layouts,
                teams: [team(0), team(1)],
                map_area,
                icons,
                previews,
                vehicle_icons: summary.vehicle_icons,
            });
        }
        levels.sort_by_key(|l| l.display_name.to_lowercase());
        levels
    }));
}

pub(super) fn collect_levels(mut catalog: ResMut<LevelCatalog>) {
    let Some(task) = catalog.scan.as_mut() else {
        return;
    };
    let Some(levels) = check_ready(task) else {
        return;
    };
    info!("menu: {} imported levels", levels.len());
    catalog.scan = None;
    catalog.levels.truncate(1);
    catalog.levels.extend(levels);
    catalog.version += 1;
}

/// The layout to use on `level`: the current one if it has it, else the same mode at the
/// closest size, else its first.
pub(super) fn pick_layout(level: &LevelInfo, mode: &str, size: u32) -> Option<(String, u32)> {
    level
        .layouts
        .iter()
        .filter(|(m, _)| m == mode)
        .min_by_key(|(_, s)| s.abs_diff(size))
        .or_else(|| level.layouts.first())
        .cloned()
}
