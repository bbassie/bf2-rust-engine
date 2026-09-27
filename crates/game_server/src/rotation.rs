//! Map rotation and live level changes: when a round ends the server moves on to the next
//! map of its list (see `conquest::next_round`), and admins can change the map at any time.
//! Connected players stay: their clients load the new level behind the loading screen.

use bevy::prelude::*;
use bevy_replicon::prelude::*;
use game_shared::{
    chat::ChatLine,
    config::GamePaths,
    conquest::{Deployment, Tickets},
    level::{LevelEntity, LoadedLevel, TEST_RANGE},
    protocol::{MatchInfo, Player, Score},
    weapons::Armory,
};
use serde::{Deserialize, Serialize};

use crate::{
    Controls, RespawnTimer, ServerSettings,
    ai::{
        squad::SquadSnapshot,
        strategy::{StrategicMap, Strategy, TeamIntel},
    },
    bots::BotBrain,
    nav,
};

pub struct RotationPlugin;

impl Plugin for RotationPlugin {
    fn build(&self, app: &mut App) {
        app.init_resource::<MapRotation>().add_observer(apply_ticket_ratio);
    }
}

/// One map of the rotation.
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
#[serde(default)]
pub struct MapEntry {
    /// Level folder name under `imported/levels`, or `test_range`.
    pub level: String,
    pub mode: String,
    /// Layout size: 16, 32 or 64 (the closest available is used).
    pub size: u32,
    /// Bots on this map; the server's number if unset.
    pub bots: Option<u32>,
}

impl Default for MapEntry {
    fn default() -> Self {
        Self {
            level: TEST_RANGE.into(),
            mode: "gpm_cq".into(),
            size: 64,
            bots: None,
        }
    }
}

/// The maps the server plays in turn.
#[derive(Resource, Default, Debug)]
pub struct MapRotation {
    pub maps: Vec<MapEntry>,
    /// The map being played, if it is one of `maps`.
    pub current: Option<usize>,
    /// Bots on maps that don't say.
    pub default_bots: u32,
}

impl MapRotation {
    /// The rotation of `settings`, positioned at its map (whose bots it applies).
    pub fn new(settings: &mut ServerSettings) -> Self {
        let maps = settings.rotation.clone();
        let current = maps
            .iter()
            .position(|m| m.level == settings.level && m.mode == settings.mode && m.size == settings.size)
            .or_else(|| maps.iter().position(|m| m.level == settings.level));
        let default_bots = settings.bots;
        if let Some(bots) = current.and_then(|i| maps[i].bots) {
            settings.bots = bots;
        }
        Self {
            maps,
            current,
            default_bots,
        }
    }

    /// Index of the map after the current one.
    fn next_index(&self) -> Option<usize> {
        if self.maps.is_empty() {
            return None;
        }
        Some(self.current.map_or(0, |i| (i + 1) % self.maps.len()))
    }

    pub fn next(&self) -> Option<&MapEntry> {
        self.maps.get(self.next_index()?)
    }

    /// Whether the end of a round moves to another map rather than restarting this one.
    pub fn moves_on(&self) -> bool {
        self.next_index().is_some_and(|i| Some(i) != self.current)
    }
}

/// Plays the next map of the rotation (the current one again without a rotation).
pub fn advance(world: &mut World) {
    let next = world.get_resource::<MapRotation>().and_then(|r| r.next().cloned());
    let map = next.unwrap_or_else(|| current_map(world));
    change_map(world, &map);
}

/// The map being played, as a rotation entry.
pub fn current_map(world: &World) -> MapEntry {
    let settings = world.resource::<ServerSettings>();
    MapEntry {
        level: settings.level.clone(),
        mode: settings.mode.clone(),
        size: settings.size,
        bots: Some(settings.bots),
    }
}

/// Switches to `map` right away, with everyone connected staying on. Everything of the old
/// map goes (soldiers, vehicles, flags, projectiles, bots, the match and its level); human
/// players keep their teams and start the new map with fresh scores. Spawning the new
/// match loads the level, and clients do the same when it replicates to them.
pub fn change_map(world: &mut World, map: &MapEntry) {
    let name = level_display_name(world.resource::<GamePaths>(), &map.level);
    info!("changing map to {} ({} {})", map.level, map.mode, map.size);
    world.write_message(ToClients {
        targets: SendTargets::All,
        message: ChatLine::server(format!("Loading {name} ({} {})", mode_label(&map.mode), map.size)),
    });
    let mut bots = map.bots.unwrap_or(world.resource::<ServerSettings>().bots);
    if let Some(mut rotation) = world.get_resource_mut::<MapRotation>() {
        rotation.current = rotation
            .maps
            .iter()
            .position(|m| m.level == map.level && m.mode == map.mode && m.size == map.size)
            .or_else(|| rotation.maps.iter().position(|m| m.level == map.level));
        bots = map.bots.unwrap_or(rotation.default_bots);
    }
    let mut settings = world.resource_mut::<ServerSettings>();
    settings.bots = bots;
    settings.level = map.level.clone();
    settings.mode = map.mode.clone();
    settings.size = map.size;

    let doomed: Vec<Entity> = world
        .query_filtered::<(Entity, Has<Player>, Has<BotBrain>), Or<(With<Replicated>, With<LevelEntity>)>>()
        .iter(world)
        .filter(|(_, player, bot)| !player || *bot)
        .map(|(entity, ..)| entity)
        .collect();
    for entity in doomed {
        // Children went with their parents.
        let _ = world.try_despawn(entity);
    }
    let humans: Vec<Entity> = world
        .query_filtered::<Entity, With<Player>>()
        .iter(world)
        .collect();
    for player in humans {
        let mut entity = world.entity_mut(player);
        entity.remove::<(Controls, RespawnTimer)>();
        entity.insert(Score::default());
        if let Some(mut deployment) = entity.get_mut::<Deployment>() {
            // Control point numbers mean something else on the new map.
            deployment.control_point = None;
            deployment.respawn_in = 0.0;
        }
    }
    forget_level(world);
    world.spawn((
        MatchInfo {
            level: map.level.clone(),
            mode: map.mode.clone(),
            size: map.size,
        },
        Replicated,
    ));
}

/// Drops what the server keeps about the loaded level: the level itself, navigation, the
/// armory and the bots' plans (which refer to its areas).
pub fn forget_level(world: &mut World) {
    world.remove_resource::<LoadedLevel>();
    world.remove_resource::<nav::Navigation>();
    world.insert_resource(Armory::default());
    world.insert_resource(StrategicMap::default());
    world.insert_resource(Strategy::default());
    world.insert_resource(TeamIntel::default());
    world.insert_resource(SquadSnapshot::default());
}

/// Whether `level` can be played: the built-in test range or an imported level.
pub fn level_exists(paths: &GamePaths, level: &str) -> bool {
    level == TEST_RANGE || paths.level_dir(level).join("level.ron").is_file()
}

/// Imported level folders, sorted.
pub fn available_levels(paths: &GamePaths) -> Vec<String> {
    let mut levels: Vec<String> = std::fs::read_dir(paths.imported.join("levels"))
        .into_iter()
        .flatten()
        .flatten()
        .filter(|e| e.path().join("level.ron").is_file())
        .map(|e| e.file_name().to_string_lossy().into_owned())
        .collect();
    levels.sort();
    levels.insert(0, TEST_RANGE.into());
    levels
}

/// A level's display name, read from the top of its `level.ron` without parsing all of it.
pub fn level_display_name(paths: &GamePaths, level: &str) -> String {
    if level == TEST_RANGE {
        return "Test Range".into();
    }
    let head = std::fs::File::open(paths.level_dir(level).join("level.ron")).ok().and_then(|file| {
        use std::io::Read;
        let mut head = String::new();
        file.take(4096).read_to_string(&mut head).ok()?;
        let start = head.find("display_name:")?;
        let rest = &head[start..];
        let open = rest.find('"')? + 1;
        let close = rest[open..].find('"')? + open;
        Some(rest[open..close].to_string())
    });
    head.unwrap_or_else(|| {
        level
            .split('_')
            .map(|word| {
                let mut chars = word.chars();
                chars.next().map_or(String::new(), |c| c.to_uppercase().chain(chars).collect())
            })
            .collect::<Vec<_>>()
            .join(" ")
    })
}

/// `gpm_cq` -> `Conquest`.
pub fn mode_label(mode: &str) -> String {
    match mode {
        "gpm_cq" => "Conquest".into(),
        "gpm_coop" => "Co-op".into(),
        "gpm_ctf" => "Capture the Flag".into(),
        other => other.trim_start_matches("gpm_").to_uppercase(),
    }
}

/// Scales the tickets of every new round by the server's ticket ratio.
fn apply_ticket_ratio(
    insert: On<Insert, Tickets>,
    state: Res<State<ClientState>>,
    settings: Res<ServerSettings>,
    mut tickets: Query<&mut Tickets>,
) {
    let ratio = settings.ticket_ratio / 100.0;
    if *state.get() != ClientState::Disconnected || ratio == 1.0 {
        return;
    }
    if let Ok(mut tickets) = tickets.get_mut(insert.entity) {
        for team in 0..2 {
            tickets.start[team] = (tickets.start[team] * ratio).round().max(1.0);
            tickets.remaining[team] = tickets.start[team];
        }
        info!("ticket ratio {}%: {} / {}", settings.ticket_ratio, tickets.start[0], tickets.start[1]);
    }
}
