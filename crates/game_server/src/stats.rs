//! Player stats: kills, deaths, score, captures, time played and favourite kit and weapon,
//! per round and over all rounds. Career stats are kept by player name (no accounts yet) in
//! a RON file (`stats_file` in the server config). When a round ends every player gets a
//! [`RoundSummary`] with the best players and their own numbers.

use std::{collections::BTreeMap, path::PathBuf};

use bevy::{ecs::schedule::common_conditions::on_message, platform::collections::HashMap, prelude::*};
use bevy_replicon::prelude::*;
use game_shared::{
    config::GamePaths,
    conquest::{ControlPoint, FlagEvent, FlagEventKind, RoundState},
    level::LoadedLevel,
    protocol::{ControlledBy, Player, Score, Team},
    soldier::{Soldier, SoldierMotion},
    summary::{PersonalSummary, RoundSummary, StatLine, SummaryRow},
    weapons::{Armory, Loadout},
};
use serde::{Deserialize, Serialize};

use crate::{
    Controls, HostPlayer, ServerSettings,
    abilities::KillScored,
    admin::bans::unix_now,
    combat::Died,
    chat::{announce, client_of},
    rotation::{MapRotation, level_display_name, mode_label},
};

pub struct StatsPlugin;

impl Plugin for StatsPlugin {
    fn build(&self, app: &mut App) {
        app.init_resource::<StatsDb>()
            .add_systems(
                Update,
                (add_round_stats, track_score, track_time, round_transitions, save_now_and_then)
                    .chain()
                    .run_if(in_state(ClientState::Disconnected)),
            )
            // After everything in `FixedUpdate` that reports kills and captures, and before
            // replicon sends (and drains) those messages.
            .add_systems(
                FixedPostUpdate,
                (track_kills, track_deaths, track_captures).run_if(in_state(ClientState::Disconnected)),
            )
            .add_systems(Last, save_on_exit.run_if(on_message::<AppExit>));
    }
}

/// Seconds between saves while players play.
const SAVE_INTERVAL: f32 = 60.0;
/// Players listed in the end-of-round summary.
const TOP_PLAYERS: usize = 8;

/// Career stats of one player.
#[derive(Serialize, Deserialize, Clone, Debug, Default)]
#[serde(default)]
pub struct CareerStats {
    pub rounds: u32,
    pub score: i64,
    pub kills: u32,
    pub deaths: u32,
    pub captures: u32,
    pub seconds_played: f64,
    /// Seconds played per kit type.
    pub kit_seconds: BTreeMap<String, f64>,
    pub weapon_kills: BTreeMap<String, u32>,
    /// Unix time.
    pub last_played: u64,
}

/// Career stats by player name, and where they are saved.
#[derive(Resource, Default)]
pub struct StatsDb {
    pub players: BTreeMap<String, CareerStats>,
    file: Option<PathBuf>,
    dirty: bool,
    since_save: f32,
}

impl StatsDb {
    fn load(file: Option<PathBuf>) -> Self {
        let players = match &file {
            Some(path) if path.exists() => game_data::read_ron(path).unwrap_or_else(|err| {
                warn!("{err}; starting with empty stats");
                BTreeMap::new()
            }),
            _ => BTreeMap::new(),
        };
        if let Some(path) = &file {
            info!("stats of {} players in {}", players.len(), path.display());
        }
        Self {
            players,
            file,
            ..default()
        }
    }

    pub fn save(&mut self) {
        if !std::mem::take(&mut self.dirty) {
            return;
        }
        self.since_save = 0.0;
        if let Some(path) = &self.file
            && let Err(err) = game_data::write_ron(path, &self.players)
        {
            warn!("can't save stats: {err}");
        }
    }

    /// The career of a player whose stats are kept (humans who told us their name).
    fn career(&mut self, player: &Player, identified: bool) -> Option<&mut CareerStats> {
        if player.is_bot || !identified {
            return None;
        }
        self.dirty = true;
        let career = self.players.entry(player.name.clone()).or_default();
        career.last_played = unix_now();
        Some(career)
    }
}

/// Server-side: a human whose name we know, so their stats are kept (the host, and clients
/// once they said hello: see `receive_hello`).
#[derive(Component)]
pub struct Identified;

/// Server-side: what a player did this round beyond the replicated [`Score`].
#[derive(Component, Default, Clone)]
pub struct RoundStats {
    pub captures: u32,
    pub seconds: f32,
    pub kit_seconds: HashMap<String, f32>,
    pub weapon_kills: HashMap<String, u32>,
    /// Seconds in each vehicle (by template), for ranked servers' reports (`accounts`).
    pub vehicle_seconds: HashMap<String, f32>,
    /// Score already added to the career.
    last_score: i32,
}

/// Loads the stats file.
pub fn start(world: &mut World) {
    let file = world.resource::<ServerSettings>().admin.stats_file.clone();
    world.insert_resource(StatsDb::load(file));
}

/// Saves the stats file.
pub fn stop(world: &mut World) {
    world.resource_mut::<StatsDb>().save();
}

fn add_round_stats(
    mut commands: Commands,
    host: Option<Res<HostPlayer>>,
    players: Query<Entity, (With<Player>, Without<RoundStats>)>,
) {
    for player in &players {
        let mut entity = commands.entity(player);
        entity.insert(RoundStats::default());
        if host.as_ref().is_some_and(|h| h.0 == player) {
            entity.insert(Identified);
        }
    }
}

fn track_score(
    mut db: ResMut<StatsDb>,
    mut players: Query<(&Player, &Score, &mut RoundStats, Has<Identified>), Changed<Score>>,
) {
    for (player, score, mut round, identified) in &mut players {
        // All zeros: a new round or map.
        if *score == Score::default() {
            round.last_score = 0;
            continue;
        }
        let delta = score.score - round.last_score;
        round.last_score = score.score;
        if delta != 0
            && let Some(career) = db.career(player, identified)
        {
            career.score += delta as i64;
        }
    }
}

fn add_time(map: &mut HashMap<String, f32>, key: &str, seconds: f32) {
    match map.get_mut(key) {
        Some(total) => *total += seconds,
        None => {
            map.insert(key.to_string(), seconds);
        }
    }
}

#[allow(clippy::type_complexity)]
fn track_time(
    time: Res<Time>,
    armory: Res<Armory>,
    rounds: Query<&RoundState>,
    mut db: ResMut<StatsDb>,
    mut players: Query<(&Player, &Team, &mut RoundStats, Option<&Controls>, Has<Identified>)>,
    loadouts: Query<&Loadout>,
    seated: Query<&game_shared::vehicle::Seated>,
    vehicles: Query<&game_shared::vehicle::Vehicle>,
) {
    if rounds.single().ok() != Some(&RoundState::Playing) {
        return;
    }
    let dt = time.delta_secs();
    for (player, team, mut round, controls, identified) in &mut players {
        if *team == Team::Spectator {
            continue;
        }
        round.seconds += dt;
        let kit = controls
            .and_then(|c| loadouts.get(c.0).ok())
            .map(|l| armory.kits.get(&l.kit).map_or(l.kit.as_str(), |k| k.kind.as_str()));
        if let Some(kit) = kit {
            add_time(&mut round.kit_seconds, kit, dt);
        }
        let vehicle = controls
            .and_then(|c| seated.get(c.0).ok())
            .and_then(|s| vehicles.get(s.vehicle).ok());
        if let Some(vehicle) = vehicle {
            add_time(&mut round.vehicle_seconds, &vehicle.template, dt);
        }
        if let Some(career) = db.career(player, identified) {
            career.seconds_played += dt as f64;
            if let Some(kit) = kit {
                *career.kit_seconds.entry(kit.to_string()).or_default() += dt as f64;
            }
        }
    }
}

/// `usrif_m16a2` -> `M16A2`, like the kill feed.
fn weapon_label(name: &str) -> String {
    let name = name.trim_start_matches("KILLMESSAGE_WEAPON_");
    let name = match name.split_once('_') {
        Some((prefix, rest)) if prefix.len() <= 6 && !rest.is_empty() => rest,
        _ => name,
    };
    name.replace('_', " ").to_uppercase()
}

/// Kills as BF2 scores them: when the victim goes down (the kill feed). Reads [`KillScored`]
/// (the server-only counterpart of the replicated `KillFeed`, which carries the killer's name
/// rather than its entity: see `game_shared::protocol::KillFeed`) rather than the outgoing
/// message itself, so this doesn't depend on how or whether that message gets mapped for
/// clients.
fn track_kills(
    mut scored: MessageReader<KillScored>,
    mut db: ResMut<StatsDb>,
    mut players: Query<(&Player, &mut RoundStats, Has<Identified>)>,
) {
    for kill in scored.read() {
        if let Ok((player, mut round, identified)) = players.get_mut(kill.killer) {
            let weapon = weapon_label(&kill.weapon);
            *round.weapon_kills.entry(weapon.clone()).or_default() += 1;
            if let Some(career) = db.career(player, identified) {
                career.kills += 1;
                *career.weapon_kills.entry(weapon).or_default() += 1;
            }
        }
    }
}

/// Deaths only when the soldier really dies: a revived soldier didn't.
fn track_deaths(mut deaths: MessageReader<Died>, mut db: ResMut<StatsDb>, players: Query<(&Player, Has<Identified>)>) {
    for death in deaths.read() {
        if let Ok((player, identified)) = players.get(death.player)
            && let Some(career) = db.career(player, identified)
        {
            career.deaths += 1;
        }
    }
}

/// Everyone of the capturing team inside the flag's radius gets a capture.
fn track_captures(
    mut events: MessageReader<ToClients<FlagEvent>>,
    mut db: ResMut<StatsDb>,
    control_points: Query<&ControlPoint>,
    soldiers: Query<(&SoldierMotion, &ControlledBy), With<Soldier>>,
    mut players: Query<(&Player, &Team, &mut RoundStats, Has<Identified>)>,
) {
    for ToClients { message: event, .. } in events.read() {
        if event.kind != FlagEventKind::Captured {
            continue;
        }
        let Ok(cp) = control_points.get(event.control_point) else {
            continue;
        };
        for (motion, controlled_by) in &soldiers {
            if !cp.contains(motion.position) {
                continue;
            }
            let Ok((player, team, mut round, identified)) = players.get_mut(controlled_by.0) else {
                continue;
            };
            if *team != event.team {
                continue;
            }
            round.captures += 1;
            if let Some(career) = db.career(player, identified) {
                career.captures += 1;
            }
        }
    }
}

/// Starts every round with fresh round stats, and sends the summaries when one ends.
fn round_transitions(
    mut commands: Commands,
    rounds: Query<(Entity, &RoundState)>,
    mut stats: Query<&mut RoundStats>,
    mut last: Local<Option<(Entity, bool)>>,
) {
    let Ok((entity, state)) = rounds.single() else {
        return;
    };
    let ended = matches!(state, RoundState::Ended { .. });
    let previous = last.replace((entity, ended));
    let new_round = !ended && previous.is_none_or(|(e, was_ended)| e != entity || was_ended);
    if new_round {
        for mut round in &mut stats {
            *round = RoundStats {
                last_score: round.last_score,
                ..default()
            };
        }
    }
    if let RoundState::Ended { winner, .. } = *state
        && previous != Some((entity, true))
    {
        commands.queue(move |world: &mut World| finish_round(world, winner));
    }
}

/// The favourite of a tally: the most seconds, kills, ...
fn favourite<'a, V: PartialOrd + Copy + 'a>(tally: impl Iterator<Item = (&'a String, &'a V)>) -> Option<String> {
    tally
        .max_by(|a, b| a.1.partial_cmp(b.1).unwrap_or(std::cmp::Ordering::Equal))
        .map(|(name, _)| name.clone())
}

fn career_line(career: &CareerStats) -> StatLine {
    StatLine {
        rounds: career.rounds,
        score: career.score.clamp(i32::MIN as i64, i32::MAX as i64) as i32,
        kills: career.kills,
        deaths: career.deaths,
        captures: career.captures,
        seconds_played: career.seconds_played as f32,
        favourite_kit: favourite(career.kit_seconds.iter()),
        favourite_weapon: favourite(career.weapon_kills.iter()),
    }
}

/// Counts the round for everyone who played it, tells everyone how it went, and saves.
fn finish_round(world: &mut World, winner: Team) {
    // Ranked servers: the players with an account, to the master server.
    crate::accounts::report_round(world, winner);
    let mut rows: Vec<(Entity, SummaryRow, bool)> = world
        .query::<(Entity, &Player, &Team, &Score, Has<Identified>)>()
        .iter(world)
        .filter(|(_, _, team, ..)| **team != Team::Spectator)
        .map(|(entity, player, team, score, identified)| {
            let row = SummaryRow {
                name: player.name.clone(),
                team: *team,
                score: score.score,
                kills: score.kills,
                deaths: score.deaths,
                is_bot: player.is_bot,
            };
            (entity, row, identified)
        })
        .collect();
    rows.sort_by(|(_, a, _), (_, b, _)| {
        b.score.cmp(&a.score).then(b.kills.cmp(&a.kills)).then(a.deaths.cmp(&b.deaths))
    });
    let top: Vec<SummaryRow> = rows.iter().take(TOP_PLAYERS).map(|(_, row, _)| row.clone()).collect();

    let settings = world.resource::<ServerSettings>();
    let rotation = world.resource::<MapRotation>();
    let (level, mode, size) = match rotation.next().filter(|_| rotation.moves_on()) {
        Some(map) => (map.level.clone(), map.mode.clone(), map.size),
        None => (settings.level.clone(), settings.mode.clone(), settings.size),
    };
    let next_name = level_display_name(world.resource::<GamePaths>(), &level);
    let next_map = Some((next_name.clone(), mode_label(&mode), size));

    let team_name = |team: usize| {
        world
            .get_resource::<LoadedLevel>()
            .and_then(|l| l.desc.teams.get(team))
            .map(|t| t.name.clone())
            .filter(|n| !n.is_empty())
            .unwrap_or_else(|| format!("Team {}", team + 1))
    };
    let headline = match winner {
        Team::One => format!("{} wins the round", team_name(0)),
        Team::Two => format!("{} wins the round", team_name(1)),
        Team::Spectator => "The round is a draw".into(),
    };

    for (rank, (entity, row, identified)) in rows.iter().enumerate() {
        if row.is_bot {
            continue;
        }
        let Some(round) = world.get::<RoundStats>(*entity) else {
            continue;
        };
        let round_line = StatLine {
            rounds: 1,
            score: row.score,
            kills: row.kills,
            deaths: row.deaths,
            captures: round.captures,
            seconds_played: round.seconds,
            favourite_kit: favourite(round.kit_seconds.iter()),
            favourite_weapon: favourite(round.weapon_kills.iter()),
        };
        let played = round.seconds > 0.0;
        let career = if *identified {
            let mut db = world.resource_mut::<StatsDb>();
            let career = db.players.entry(row.name.clone()).or_default();
            if played {
                career.rounds += 1;
            }
            let line = career_line(career);
            db.dirty = true;
            Some(line)
        } else {
            None
        };
        let Some(target) = client_of(world, *entity) else {
            continue;
        };
        world.write_message(ToClients {
            targets: SendTargets::Single(target),
            message: RoundSummary {
                winner,
                top: top.clone(),
                you: Some(PersonalSummary {
                    rank: rank as u32 + 1,
                    round: round_line,
                    career,
                }),
                next_map: next_map.clone(),
            },
        });
    }
    // A spectating host still sees the round's best.
    if !world.contains_resource::<HostPlayer>() {
        world.write_message(ToClients {
            targets: SendTargets::Single(ClientId::Server),
            message: RoundSummary {
                winner,
                top,
                you: None,
                next_map,
            },
        });
    }
    announce(world, format!("{headline}. Next map: {next_name}"));
    world.resource_mut::<StatsDb>().save();
}

fn save_now_and_then(time: Res<Time<Real>>, mut db: ResMut<StatsDb>) {
    db.since_save += time.delta_secs();
    if db.since_save >= SAVE_INTERVAL {
        db.since_save = 0.0;
        db.save();
    }
}

fn save_on_exit(db: Option<ResMut<StatsDb>>) {
    if let Some(mut db) = db {
        db.save();
    }
}
