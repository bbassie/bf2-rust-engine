//! Per-minute statistics of the team AI, logged next to the bots' movement statistics:
//! flags taken, kills, what the squads were ordered to do, how much of their time bots
//! spent at their objectives, and how often they used each behaviour.

use bevy::{platform::collections::HashMap, prelude::*};
use game_shared::{
    conquest::{ControlPoint, FlagState, team_index},
    protocol::Team,
    squad::squad_name,
};

use game_shared::{vehicle::VehicleHealth, weapons::Armory};

use super::{
    squad::SquadSnapshot,
    strategy::{OrderKind, StrategicMap, Strategy},
    vehicles::VehicleClaims,
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
    /// Bags thrown to teammates, launcher shots, rockets at vehicles, repairs begun.
    pub bags: u32,
    pub launches: u32,
    pub rockets: u32,
    pub repairs: u32,
    /// Bots blinded by flashbangs, and seconds spent in tear gas without a mask.
    pub flashed: u32,
    pub gassed: f32,
    /// The AI commander: orders given, artillery strikes, UAVs, scans, supply drops.
    pub orders: u32,
    pub artillery: u32,
    pub uavs: u32,
    pub scans: u32,
    pub supplies: u32,
    pub spawns: u32,
    /// Spawns on the squad leader.
    pub leader_spawns: u32,
    /// Vehicles: seats taken (and of those stationary weapons), meters driven by bot drivers,
    /// their stuck events, rounds fired from vehicle guns, enemy vehicles wrecked while bots
    /// fired at them, aircraft taking off and crashing.
    pub mounts: u32,
    pub stationary: u32,
    pub driven: f32,
    pub vehicle_stuck: u32,
    pub vehicle_shots: u32,
    pub vehicle_kills: u32,
    pub flights: u32,
    pub crashes: u32,
    /// Anti-tank mines laid and C4 attacks on vehicles.
    pub mines: u32,
    pub demolitions: u32,
    /// Flares and smoke fired at incoming missiles.
    pub countermeasures: u32,
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
        self.bags += other.bags;
        self.launches += other.launches;
        self.rockets += other.rockets;
        self.repairs += other.repairs;
        self.flashed += other.flashed;
        self.gassed += other.gassed;
        self.orders += other.orders;
        self.artillery += other.artillery;
        self.uavs += other.uavs;
        self.scans += other.scans;
        self.supplies += other.supplies;
        self.spawns += other.spawns;
        self.leader_spawns += other.leader_spawns;
        self.mounts += other.mounts;
        self.stationary += other.stationary;
        self.driven += other.driven;
        self.vehicle_stuck += other.vehicle_stuck;
        self.vehicle_shots += other.vehicle_shots;
        self.vehicle_kills += other.vehicle_kills;
        self.flights += other.flights;
        self.crashes += other.crashes;
        self.mines += other.mines;
        self.demolitions += other.demolitions;
        self.countermeasures += other.countermeasures;
    }
}

impl AiStats {
    pub fn team(&mut self, team: Team) -> Option<&mut TeamStats> {
        team_index(team).map(|t| &mut self.teams[t])
    }
}

/// Counts flags changing hands, deaths and vehicles bots destroyed.
#[allow(clippy::too_many_arguments)]
pub fn track_events(
    mut stats: ResMut<AiStats>,
    mut deaths: MessageReader<Died>,
    flags: Query<(Entity, &FlagState), (With<ControlPoint>, Changed<FlagState>)>,
    control_points: Query<(), With<ControlPoint>>,
    mut owners: Local<HashMap<Entity, Team>>,
    vehicles: Query<(Entity, &VehicleHealth), Changed<VehicleHealth>>,
    mut wrecks: Local<bevy::platform::collections::HashSet<Entity>>,
    claims: Res<VehicleClaims>,
) {
    for (vehicle, health) in &vehicles {
        if health.wrecked()
            && wrecks.insert(vehicle)
            && let Some((team, at)) = claims.engaged.get(&vehicle)
            && claims.now() - at < 6.0
            && let Some(t) = team_index(*team)
        {
            stats.teams[t].vehicle_kills += 1;
        }
    }
    wrecks.retain(|v| vehicles.contains(*v) || claims.engaged.contains_key(v));
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
             {} covers, {} flanks, {} grenades, {} reactions, {} revives, {} bags, {} launcher shots, \
             {} rockets, {} repairs, {} flashed, {:.0} s gassed; commander: {} orders, {} artillery, {} UAVs, \
             {} scans, {} supply drops; {} of {} spawns on the squad leader; \
             vehicles: {} entered ({} stationary), {:.2} km driven, {} stuck, {} shots, {} vehicle kills, \
             {} takeoffs, {} crashes, {} countermeasures, {} AT mines, {} C4 attacks; kits {}; {:?}, {} squads attacking, {} defending: {}",
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
            minute.bags,
            minute.launches,
            minute.rockets,
            minute.repairs,
            minute.flashed,
            minute.gassed,
            minute.orders,
            minute.artillery,
            minute.uavs,
            minute.scans,
            minute.supplies,
            minute.leader_spawns,
            minute.spawns,
            minute.mounts,
            minute.stationary,
            minute.driven / 1000.0,
            minute.vehicle_stuck,
            minute.vehicle_shots,
            minute.vehicle_kills,
            minute.flights,
            minute.crashes,
            minute.countermeasures,
            minute.mines,
            minute.demolitions,
            kits.iter().map(|(k, n)| format!("{k} {n}")).collect::<Vec<_>>().join(", "),
            strategy.posture[t],
            attacking,
            strategy.orders.iter().filter(|((o, _), _)| *o == team).count() - attacking,
            orders.into_iter().map(|(_, s)| s).collect::<Vec<_>>().join(", "),
        );
    }
    stats.teams = default();
}
