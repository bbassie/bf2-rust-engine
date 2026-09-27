//! Per-minute statistics of the team AI, logged next to the bots' movement statistics:
//! flags taken, kills, what the squads were ordered to do, how much of their time bots
//! spent at their objectives, and how often they used each behaviour.

use bevy::{platform::collections::HashMap, prelude::*};
use game_shared::{
    conquest::{ControlPoint, FlagState, team_index},
    protocol::Team,
    squad::squad_name,
};

use game_shared::weapons::Armory;

use super::{
    squad::SquadSnapshot,
    strategy::{OrderKind, StrategicMap, Strategy},
};
use crate::{bots::BotBrain, combat::Died};

#[derive(Resource, Default)]
pub struct AiStats {
    elapsed: f32,
    minutes: u32,
    pub teams: [TeamStats; 2],
    /// Since the server started.
    pub total: [TeamStats; 2],
}

#[derive(Default, Clone, Copy, Debug)]
pub struct TeamStats {
    pub captures: u32,
    pub neutralized: u32,
    /// Enemy soldiers that died.
    pub kills: u32,
    pub deaths: u32,
    /// Bot-seconds alive.
    pub alive: f32,
    /// Bot-seconds within reach of their objective (the area plus 30 m).
    pub at_objective: f32,
    /// Bot-seconds shooting at someone.
    pub fighting: f32,
    pub covers: u32,
    pub flanks: u32,
    pub grenades: u32,
    /// Times a bot hurt by someone it didn't see turned to find him.
    pub reactions: u32,
    /// Medics going to revive someone.
    pub revives: u32,
    pub spawns: u32,
    /// Spawns on the squad leader.
    pub leader_spawns: u32,
}

impl TeamStats {
    fn add(&mut self, other: &TeamStats) {
        self.captures += other.captures;
        self.neutralized += other.neutralized;
        self.kills += other.kills;
        self.deaths += other.deaths;
        self.alive += other.alive;
        self.at_objective += other.at_objective;
        self.fighting += other.fighting;
        self.covers += other.covers;
        self.flanks += other.flanks;
        self.grenades += other.grenades;
        self.reactions += other.reactions;
        self.revives += other.revives;
        self.spawns += other.spawns;
        self.leader_spawns += other.leader_spawns;
    }
}

impl AiStats {
    pub fn team(&mut self, team: Team) -> Option<&mut TeamStats> {
        team_index(team).map(|t| &mut self.teams[t])
    }
}

/// Counts flags changing hands and deaths.
pub fn track_events(
    mut stats: ResMut<AiStats>,
    mut deaths: MessageReader<Died>,
    flags: Query<(Entity, &FlagState), (With<ControlPoint>, Changed<FlagState>)>,
    control_points: Query<(), With<ControlPoint>>,
    mut owners: Local<HashMap<Entity, Team>>,
) {
    for death in deaths.read() {
        if let Some(t) = team_index(death.team) {
            stats.teams[t].deaths += 1;
            stats.teams[1 - t].kills += 1;
        }
    }
    for (entity, state) in &flags {
        let Some(previous) = owners.insert(entity, state.owner) else {
            continue;
        };
        if previous == state.owner {
            continue;
        }
        match team_index(state.owner) {
            Some(t) => stats.teams[t].captures += 1,
            None => {
                if let Some(t) = team_index(previous.opponent()) {
                    stats.teams[t].neutralized += 1;
                }
            }
        }
    }
    // Control points of past rounds.
    owners.retain(|entity, _| control_points.contains(*entity));
}

pub fn log_stats(
    time: Res<Time>,
    mut stats: ResMut<AiStats>,
    strategy: Res<Strategy>,
    map: Res<StrategicMap>,
    snapshot: Res<SquadSnapshot>,
    armory: Res<Armory>,
    bots: Query<(), With<BotBrain>>,
) {
    stats.elapsed += time.delta_secs();
    if stats.elapsed < 60.0 {
        return;
    }
    stats.elapsed = 0.0;
    stats.minutes += 1;
    if bots.is_empty() {
        stats.teams = default();
        return;
    }
    for t in 0..2 {
        let minute = stats.teams[t];
        stats.total[t].add(&minute);
        let total = stats.total[t];
        let team = if t == 0 { Team::One } else { Team::Two };
        let mut orders: Vec<(u8, String)> = strategy
            .orders
            .iter()
            .filter(|((order_team, _), _)| *order_team == team)
            .filter_map(|((_, squad), order)| {
                let area = map.areas.get(order.area)?;
                let verb = match order.kind {
                    OrderKind::Attack => "attack",
                    OrderKind::Defend => "defend",
                };
                let note = if order.suggestion { " (suggested)" } else { "" };
                Some((*squad, format!("{} {verb} {}{note}", squad_name(*squad), area.name)))
            })
            .collect();
        orders.sort();
        let attacking = strategy.orders.iter().filter(|((o, _), s)| *o == team && s.kind == OrderKind::Attack).count();
        let mut kits: Vec<(&str, usize)> = Vec::new();
        for (_, slot) in &snapshot.kits[t] {
            let kind = armory
                .team_kits[t]
                .get(*slot as usize)
                .and_then(|name| armory.kits.get(name))
                .map_or("?", |k| k.kind.as_str());
            match kits.iter_mut().find(|(k, _)| *k == kind) {
                Some((_, n)) => *n += 1,
                None => kits.push((kind, 1)),
            }
        }
        kits.sort_by(|a, b| b.1.cmp(&a.1));
        info!(
            "ai team {}: {} captured, {} neutralized, {} kills, {} deaths in the last minute \
             ({} / {} / {} / {} in {} min); {:.0}% of bot time at objectives, {:.0}% fighting; \
             {} covers, {} flanks, {} grenades, {} reactions, {} revives; {} of {} spawns on the squad leader; \
             kits {}; {:?}, {} squads attacking, {} defending: {}",
            t + 1,
            minute.captures,
            minute.neutralized,
            minute.kills,
            minute.deaths,
            total.captures,
            total.neutralized,
            total.kills,
            total.deaths,
            stats.minutes,
            100.0 * minute.at_objective / minute.alive.max(1.0),
            100.0 * minute.fighting / minute.alive.max(1.0),
            minute.covers,
            minute.flanks,
            minute.grenades,
            minute.reactions,
            minute.revives,
            minute.leader_spawns,
            minute.spawns,
            kits.iter().map(|(k, n)| format!("{k} {n}")).collect::<Vec<_>>().join(", "),
            strategy.posture[t],
            attacking,
            strategy.orders.iter().filter(|((o, _), _)| *o == team).count() - attacking,
            orders.into_iter().map(|(_, s)| s).collect::<Vec<_>>().join(", "),
        );
    }
    stats.teams = default();
}
