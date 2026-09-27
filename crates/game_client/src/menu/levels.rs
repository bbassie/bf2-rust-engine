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
        let test_range = game_shared::level::test_range().desc;
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
}

/// The parts of a `level.ron` the menu needs; the rest is skipped.
#[derive(Deserialize)]
struct LevelSummary {
    display_name: String,
    #[serde(default)]
    minimap: Option<String>,
    #[serde(default)]
    game_modes: Vec<ModeSummary>,
    #[serde(default)]
    teams: Vec<TeamSummary>,
}

#[derive(Deserialize)]
struct ModeSummary {
    mode: String,
    size: u32,
}

#[derive(Deserialize)]
struct TeamSummary {
    name: String,
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
            levels.push(LevelInfo {
                name: name.clone(),
                display_name: summary.display_name.clone(),
                minimap: summary.minimap.clone(),
                layouts,
                teams: [team(0), team(1)],
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
