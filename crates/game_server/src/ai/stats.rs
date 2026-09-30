//! Per-minute statistics of the team AI, logged next to the bots' movement statistics:
//! flags taken, kills, what the squads were ordered to do, how much of their time bots
//! spent at their objectives, and how often they used each behaviour.

use bevy::{platform::collections::HashMap, prelude::*};
use game_shared::{
    conquest::{ControlPoint, FlagState, team_index},
    protocol::{ControlledBy, Player, Team},
    squad::squad_name,
    weapons::Armory,
};

use super::{
    squad::{SquadSnapshot, SquadTactics},
    strategy::{OrderKind, StrategicMap, Strategy},
};
use crate::{
    bots::BotBrain,
    combat::{Died, SoldierHit, VehicleDestroyed},
};

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
    /// Rush: bot-seconds going to a charge to arm or defuse it and holding the use key there.
    pub charge_seconds: f32,
    /// Bot-seconds engaged (an enemy in sight or just lost, or just hurt), and of those in
    /// cover from the threat (a ray from its eye to the middle of the body is blocked);
    /// sampled every half second.
    pub engaged_seconds: f32,
    pub covered_seconds: f32,
    /// Bot-seconds fighting from a cover spot (tactical bots).
    pub cover_fights: f32,
    /// Runs to cover in a firefight, suppressive fire, bags run to squad mates, times
    /// suppressed (near misses after a quiet while), enemies spotted on the radio.
    pub takecovers: u32,
    pub suppressions: u32,
    pub supplies_run: u32,
    pub suppressed: u32,
    pub spots: u32,
    /// Squads' bounds begun and times pinned down.
    pub bounds: u32,
    pub pinned: u32,
    /// Firefights in detail (see [`CombatStats`]).
    pub combat: CombatStats,
}

/// What a bot on foot is doing in a firefight, for [`CombatStats`]: the same buckets for
/// both behaviours (tactical and legacy).
pub const FIGHT_STATES: [&str; FIGHT_N] = [
    "open", "cover up", "cover down", "to cover", "watch", "suppress", "overwatch", "moving", "search", "flank", "medic",
    "throw", "other", "vehicle", "bounding", "moving seen",
];
pub const FIGHT_N: usize = 16;

/// How bots fight: time with an enemy in sight and holding the trigger, rounds and hits, how
/// long from seeing an enemy to the first shot, where engaged time goes and what bots were
/// doing when they went down.
#[derive(Default, Clone, Copy, Debug)]
pub struct CombatStats {
    /// Bot-seconds with an enemy in sight (on foot).
    pub sight: f32,
    /// Bot-seconds holding the trigger at an enemy in sight, and in suppressive fire.
    pub trigger: f32,
    pub suppress_fire: f32,
    /// Rounds of the main weapon fired, and hits on enemy soldiers (all weapons) with their
    /// damage.
    pub rounds: u32,
    pub hits: u32,
    pub damage: f32,
    /// Enemies coming into sight (none in sight before), of those shot at before losing
    /// sight, and the seconds from sighting to the first shot.
    pub sightings: u32,
    pub first_shots: u32,
    pub first_shot_time: f32,
    /// Engaged bot-seconds (see [`TeamStats::engaged_seconds`]) by [`FIGHT_STATES`].
    pub engaged_by: [f32; FIGHT_N],
    /// Downs and deaths by [`FIGHT_STATES`], and of those with an enemy in sight.
    pub deaths_by: [u32; FIGHT_N],
    pub deaths_aware: [u32; FIGHT_N],
    /// Rounds and hits by [`FIGHT_STATES`], by the shooter's stance (standing, crouching,
    /// prone), and while suppressed (over 0.3) or not.
    pub rounds_by: [u32; FIGHT_N],
    pub hits_by: [u32; FIGHT_N],
    pub rounds_stance: [u32; 3],
    pub hits_stance: [u32; 3],
    pub rounds_suppressed: [u32; 2],
    pub hits_suppressed: [u32; 2],
    /// Hits taken from enemies by [`FIGHT_STATES`], by stance, and standing still or moving
    /// (over 1 m/s).
    pub taken_by: [u32; FIGHT_N],
    pub taken_stance: [u32; 3],
    pub taken_moving: [u32; 2],
    /// Engaged bot-seconds by stance, and standing still or moving.
    pub engaged_stance: [f32; 3],
    pub engaged_moving: [f32; 2],
    /// Deaths by what hit them last ([`DEATH_CAUSES`]), and bullet deaths by the shooter's
    /// distance ([`DISTANCES`]).
    pub death_cause: [u32; 5],
    pub death_dist: [u32; 4],
    /// Main-weapon rounds fired at an enemy in sight, and bullet hits scored on foot, by the
    /// distance ([`DISTANCES`]).
    pub rounds_dist: [u32; 4],
    pub hits_dist: [u32; 4],
    /// Bot-seconds on foot by [`FIGHT_STATES`], engaged or not (where the time goes).
    pub alive_by: [f32; FIGHT_N],
    /// Bot-seconds on foot by the distance to the area of its order: inside its radius, up to
    /// 30 m beyond, 100 m, 200 m, further; attacking (0..5) and defending (5..10).
    pub obj_dist: [f32; 10],
    /// Medics going to revive someone, and how that ended: back up, dead (bled out or
    /// finished off), out of time, something else came first.
    pub revive: [u32; 5],
}

/// What killed a soldier (the last hit before going down): bullets, a vehicle's guns or
/// wheels, a grenade, rocket or mine, the commander's artillery, nothing (a fall, water).
pub const DEATH_CAUSES: [&str; 5] = ["bullet", "vehicle", "explosive", "artillery", "none"];
/// Distance buckets of fights: under 10 m, 10-25, 25-50, beyond.
pub const DISTANCES: [&str; 4] = ["<10", "10-25", "25-50", "50+"];

/// The [`DISTANCES`] bucket of a distance.
pub fn distance_bucket(d: f32) -> usize {
    match d {
        d if d < 10.0 => 0,
        d if d < 25.0 => 1,
        d if d < 50.0 => 2,
        _ => 3,
    }
}

impl CombatStats {
    fn add(&mut self, o: &CombatStats) {
        self.sight += o.sight;
        self.trigger += o.trigger;
        self.suppress_fire += o.suppress_fire;
        self.rounds += o.rounds;
        self.hits += o.hits;
        self.damage += o.damage;
        self.sightings += o.sightings;
        self.first_shots += o.first_shots;
        self.first_shot_time += o.first_shot_time;
        for i in 0..FIGHT_STATES.len() {
            self.engaged_by[i] += o.engaged_by[i];
            self.deaths_by[i] += o.deaths_by[i];
            self.deaths_aware[i] += o.deaths_aware[i];
            self.rounds_by[i] += o.rounds_by[i];
            self.hits_by[i] += o.hits_by[i];
            self.taken_by[i] += o.taken_by[i];
            self.alive_by[i] += o.alive_by[i];
        }
        for i in 0..2 {
            self.taken_moving[i] += o.taken_moving[i];
            self.engaged_moving[i] += o.engaged_moving[i];
        }
        for i in 0..3 {
            self.rounds_stance[i] += o.rounds_stance[i];
            self.hits_stance[i] += o.hits_stance[i];
            self.taken_stance[i] += o.taken_stance[i];
            self.engaged_stance[i] += o.engaged_stance[i];
        }
        for i in 0..2 {
            self.rounds_suppressed[i] += o.rounds_suppressed[i];
            self.hits_suppressed[i] += o.hits_suppressed[i];
        }
        for i in 0..5 {
            self.death_cause[i] += o.death_cause[i];
        }
        for i in 0..10 {
            self.obj_dist[i] += o.obj_dist[i];
        }
        for i in 0..5 {
            self.revive[i] += o.revive[i];
        }
        for i in 0..4 {
            self.death_dist[i] += o.death_dist[i];
            self.rounds_dist[i] += o.rounds_dist[i];
            self.hits_dist[i] += o.hits_dist[i];
        }
    }

    /// The second log line: causes of death and fights by distance.
    fn describe_more(&self) -> String {
        let list = |names: &[&str], values: &[u32]| {
            names.iter().zip(values).map(|(n, v)| format!("{n} {v}")).collect::<Vec<_>>().join(", ")
        };
        format!(
            "deaths by cause: {}; bullet deaths by distance: {}; rounds by distance: {}; hits by distance: {}; time by state: {}; objective distance: {}; revives {}",
            list(&DEATH_CAUSES, &self.death_cause),
            list(&DISTANCES, &self.death_dist),
            list(&DISTANCES, &self.rounds_dist),
            list(&DISTANCES, &self.hits_dist),
            FIGHT_STATES
                .iter()
                .zip(&self.alive_by)
                .map(|(n, v)| format!("{n} {v:.0}"))
                .collect::<Vec<_>>()
                .join(", "),
            self.obj_dist.iter().map(|v| format!("{v:.0}")).collect::<Vec<_>>().join(" "),
            self.revive.iter().map(|v| v.to_string()).collect::<Vec<_>>().join(" "),
        )
    }

    /// One log line's worth.
    fn describe(&self) -> String {
        let engaged: f32 = self.engaged_by.iter().sum();
        let states = |values: &dyn Fn(usize) -> String| {
            (0..FIGHT_STATES.len())
                .map(|i| format!("{} {}", FIGHT_STATES[i], values(i)))
                .collect::<Vec<_>>()
                .join(", ")
        };
        format!(
            "{:.0} s in sight, {:.0} s trigger, {:.0} s suppressive; {} rounds, {} hits ({:.0} damage); {} sightings, {} first shots after {:.2} s; engaged {:.0} s: {}; deaths: {}; rounds/hits: {}; by stance {}/{} {}/{} {}/{}; suppressed {}/{}, not {}/{}; hits taken: {}; by stance {} {} {}; still {} moving {}; engaged by stance {:.0} {:.0} {:.0}; still {:.0} moving {:.0}",
            self.sight,
            self.trigger,
            self.suppress_fire,
            self.rounds,
            self.hits,
            self.damage,
            self.sightings,
            self.first_shots,
            self.first_shot_time / self.first_shots.max(1) as f32,
            engaged,
            states(&|i| format!("{:.0}", self.engaged_by[i])),
            states(&|i| format!("{}/{}", self.deaths_by[i], self.deaths_aware[i])),
            states(&|i| format!("{}/{}", self.rounds_by[i], self.hits_by[i])),
            self.rounds_stance[0],
            self.hits_stance[0],
            self.rounds_stance[1],
            self.hits_stance[1],
            self.rounds_stance[2],
            self.hits_stance[2],
            self.rounds_suppressed[1],
            self.hits_suppressed[1],
            self.rounds_suppressed[0],
            self.hits_suppressed[0],
            states(&|i| format!("{}", self.taken_by[i])),
            self.taken_stance[0],
            self.taken_stance[1],
            self.taken_stance[2],
            self.taken_moving[0],
            self.taken_moving[1],
            self.engaged_stance[0],
            self.engaged_stance[1],
            self.engaged_stance[2],
            self.engaged_moving[0],
            self.engaged_moving[1],
        )
    }
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
        self.charge_seconds += other.charge_seconds;
        self.engaged_seconds += other.engaged_seconds;
        self.covered_seconds += other.covered_seconds;
        self.cover_fights += other.cover_fights;
        self.takecovers += other.takecovers;
        self.suppressions += other.suppressions;
        self.supplies_run += other.supplies_run;
        self.suppressed += other.suppressed;
        self.spots += other.spots;
        self.bounds += other.bounds;
        self.pinned += other.pinned;
        self.combat.add(&other.combat);
    }
}

impl AiStats {
    pub fn team(&mut self, team: Team) -> Option<&mut TeamStats> {
        team_index(team).map(|t| &mut self.teams[t])
    }
}

/// Counts flags changing hands, deaths and vehicles bots destroyed.
#[allow(clippy::too_many_arguments)]
pub(crate) fn track_events(
    time: Res<Time>,
    mut stats: ResMut<AiStats>,
    mut deaths: MessageReader<Died>,
    mut destroyed: MessageReader<VehicleDestroyed>,
    mut hits: MessageReader<SoldierHit>,
    players: Query<(&Player, &Team, Option<&BotBrain>)>,
    controlled: Query<(&ControlledBy, &game_shared::soldier::SoldierMotion, Has<game_shared::vehicle::Seated>)>,
    flags: Query<(Entity, &FlagState), (With<ControlPoint>, Changed<FlagState>)>,
    control_points: Query<(), With<ControlPoint>>,
    armory: Res<Armory>,
    mut owners: Local<HashMap<Entity, Team>>,
    // The last hit on each player: cause, distance, when.
    mut last_hits: Local<HashMap<Entity, (usize, f32, f32)>>,
) {
    let now = time.elapsed_secs();
    for kill in destroyed.read() {
        if let Some((player, team, _)) = kill.by.and_then(|by| players.get(by).ok())
            && player.is_bot
            && let Some(t) = team_index(*team)
        {
            stats.teams[t].vehicle_kills += 1;
        }
    }
    for hit in hits.read() {
        let by = hit.attacker.player.and_then(|p| players.get(p).ok());
        let victim_soldier = controlled.get(hit.victim).ok();
        let victim = victim_soldier.and_then(|(c, ..)| players.get(c.0).ok());
        let shooter = hit.attacker.soldier.and_then(|s| controlled.get(s).ok());
        let distance = shooter.zip(victim_soldier).map_or(f32::MAX, |((_, a, _), (_, v, _))| a.position.distance(v.position));
        let bullet = armory.weapon(&hit.attacker.weapon).is_some_and(|w| w.fire.kind == game_data::FireKind::Gun);
        let cause = match () {
            _ if &*hit.attacker.weapon == "artillery" => 3,
            _ if shooter.is_some_and(|(_, _, seated)| seated) => 1,
            _ if bullet => 0,
            _ => 2,
        };
        if let Some((c, ..)) = victim_soldier {
            last_hits.insert(c.0, (cause, distance, now));
        }
        if cause == 0
            && let (Some((player, team, Some(_))), Some((_, victim_team, _))) = (by, victim)
            && player.is_bot
            && team != victim_team
            && let Some(t) = team_index(*team)
        {
            stats.teams[t].combat.hits_dist[distance_bucket(distance)] += 1;
        }
        if let (Some((_, team, _)), Some((_, victim_team, Some(brain))), Some((_, motion, _))) = (by, victim, victim_soldier)
            && team != victim_team
            && let Some(t) = team_index(*victim_team)
        {
            let combat = &mut stats.teams[t].combat;
            combat.taken_by[brain.state_before()] += 1;
            combat.taken_stance[match motion.stance {
                game_shared::soldier::Stance::Standing => 0,
                game_shared::soldier::Stance::Crouching => 1,
                game_shared::soldier::Stance::Prone => 2,
            }] += 1;
            combat.taken_moving[usize::from(motion.velocity.with_y(0.0).length() > 1.0)] += 1;
        }
        if let (Some((player, team, brain)), Some((_, victim_team, _))) = (by, victim)
            && player.is_bot
            && team != victim_team
            && let Some(t) = team_index(*team)
        {
            let combat = &mut stats.teams[t].combat;
            combat.hits += 1;
            combat.damage += hit.damage;
            if let Some(brain) = brain {
                let (state, stance, suppressed) = brain.shooting_state();
                combat.hits_by[state] += 1;
                combat.hits_stance[stance] += 1;
                combat.hits_suppressed[suppressed as usize] += 1;
            }
        }
    }
    for death in deaths.read() {
        if let Some(t) = team_index(death.team) {
            stats.teams[t].deaths += 1;
            stats.teams[1 - t].kills += 1;
            // Bleeding out after going down can take a while.
            let (cause, distance) = match last_hits.remove(&death.player) {
                Some((cause, distance, at)) if now - at < 40.0 => (cause, distance),
                _ => (4, f32::MAX),
            };
            let combat = &mut stats.teams[t].combat;
            combat.death_cause[cause] += 1;
            if cause == 0 {
                combat.death_dist[distance_bucket(distance)] += 1;
            }
        }
    }
    last_hits.retain(|_, (.., at)| now - *at < 60.0);
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
    mut tactics: ResMut<SquadTactics>,
    bots: Query<(&BotBrain, &Team)>,
) {
    stats.elapsed += time.delta_secs();
    if stats.elapsed < 60.0 {
        return;
    }
    stats.elapsed = 0.0;
    stats.minutes += 1;
    for t in 0..2 {
        stats.teams[t].bounds = std::mem::take(&mut tactics.bounds[t]);
        stats.teams[t].pinned = std::mem::take(&mut tactics.pins[t]);
    }
    if bots.is_empty() {
        stats.teams = default();
        return;
    }
    for t in 0..2 {
        let minute = stats.teams[t];
        stats.total[t].add(&minute);
        let total = stats.total[t];
        let team = t_team(t);
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
                let mut note = if order.suggestion { " (suggested)".to_string() } else { String::new() };
                if let Some(guard) = strategy.guards.get(&(team, *squad))
                    && let Some(at) = map.areas.get(guard.area)
                {
                    note += &format!(" ({} guarding {})", guard.members.len(), at.name);
                }
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
             {:.0}% of engaged time in cover ({:.0} s engaged), {:.0} s fighting from cover spots; \
             {} takecovers, {} suppressions, {} suppressed, {} spots, {} bounds, {} pinned, {} supply runs; \
             {} covers, {} flanks, {} grenades, {} reactions, {} revives, {} bags, {} launcher shots, \
             {} rockets, {} repairs, {} flashed, {:.0} s gassed; commander: {} orders, {} artillery, {} UAVs, \
             {} scans, {} supply drops; {} of {} spawns on the squad leader; \
             vehicles: {} entered ({} stationary), {:.2} km driven, {} stuck, {} shots, {} vehicle kills, \
             {} takeoffs, {} crashes, {} countermeasures, {} AT mines, {} C4 attacks;{} kits {}; {:?}, {} squads attacking, {} defending: {}",
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
            100.0 * minute.covered_seconds / minute.engaged_seconds.max(0.5),
            minute.engaged_seconds,
            minute.cover_fights,
            minute.takecovers,
            minute.suppressions,
            minute.suppressed,
            minute.spots,
            minute.bounds,
            minute.pinned,
            minute.supplies_run,
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
            if minute.charge_seconds > 0.0 { format!(" {:.0} s at charges;", minute.charge_seconds) } else { String::new() },
            kits.iter().map(|(k, n)| format!("{k} {n}")).collect::<Vec<_>>().join(", "),
            strategy.posture[t],
            attacking,
            strategy.orders.iter().filter(|((o, _), _)| *o == team).count() - attacking,
            orders.into_iter().map(|(_, s)| s).collect::<Vec<_>>().join(", "),
        );
        info!("ai combat team {}: {}", t + 1, minute.combat.describe());
        info!("ai combat2 team {}: {}", t + 1, minute.combat.describe_more());
        // Where the team is: soldiers on their feet at each objective, and bots sent there.
        let presence: Vec<String> = strategy.objectives[t]
            .iter()
            .filter_map(|o| {
                let area = map.areas.get(o.area)?;
                let at = snapshot.soldiers[t]
                    .iter()
                    .filter(|s| s.position.xz().distance(area.position.xz()) < area.radius + 40.0)
                    .count();
                let sent = bots.iter().filter(|(b, team)| **team == t_team(t) && b.order().is_some_and(|(_, a)| a == o.area)).count();
                let kind = match o.kind {
                    OrderKind::Attack => "attack",
                    OrderKind::Defend => "defend",
                };
                Some(format!("{kind} {} ({:.1}): {at} there, {sent} sent", area.name, o.value))
            })
            .collect();
        info!("ai objectives team {}: {}", t + 1, presence.join("; "));
    }
    stats.teams = default();
}

fn t_team(t: usize) -> Team {
    if t == 0 { Team::One } else { Team::Two }
}

/// Seconds a squad is watched after its team took the flag it was at.
const FLOW_WATCH: f32 = 120.0;

/// A squad that was at a flag when its team took it: how long until the commander gives it
/// something else to do, and until it leaves.
struct FlowWatch {
    team: Team,
    squad: u8,
    area: usize,
    since: f32,
    /// The first order other than the one it had (and what it was), then the first one for
    /// another objective.
    first: Option<(f32, String)>,
    away: Option<(f32, String)>,
    left: Option<f32>,
    /// Logged what keeps it there (15 s after an order elsewhere).
    probed: bool,
}

/// What happens after a capture ([`track_flow`]).
#[derive(Default)]
pub struct CaptureFlow {
    watches: Vec<FlowWatch>,
    owners: HashMap<usize, Team>,
    /// Orders at the last tick: the plan may already have moved a squad on in the tick its
    /// flag was taken.
    orders: HashMap<(Team, u8), (OrderKind, usize)>,
    generation: u32,
}

fn describe_order(map: &StrategicMap, order: Option<&super::strategy::SquadOrder>) -> String {
    match order {
        Some(o) => {
            let verb = match o.kind {
                OrderKind::Attack => "attack",
                OrderKind::Defend => "defend",
            };
            format!("{verb} {}", map.areas.get(o.area).map_or("?", |a| a.name.as_str()))
        }
        None => "no order".into(),
    }
}

/// Logs what squads do after their team took the flag they were at (`ai flow:` lines; squads
/// with at least half their living members or three of them there are followed): the time to their next order, the time to an order for another objective, and the time until
/// most of the squad left the flag (no more than a third of its living members within the
/// radius plus 40 m, not counting members left to guard it).
pub fn track_flow(
    time: Res<Time>,
    map: Res<StrategicMap>,
    strategy: Res<Strategy>,
    snapshot: Res<SquadSnapshot>,
    flags: Query<&FlagState>,
    brains: Query<&BotBrain>,
    seated: Query<(), With<game_shared::vehicle::Seated>>,
    controls: Query<&crate::Controls>,
    mut flow: Local<CaptureFlow>,
) {
    let now = time.elapsed_secs();
    if flow.generation != map.generation {
        *flow = CaptureFlow { generation: map.generation, ..default() };
    }
    let near = |info: &super::squad::SquadInfo, area: usize, skip: &[Entity]| {
        let a = &map.areas[area];
        let alive = || info.alive.iter().filter(|s| !skip.contains(&s.player));
        let there = alive().filter(|s| s.position.xz().distance(a.position.xz()) < a.radius + 40.0).count();
        (there, alive().count())
    };
    for (index, area) in map.areas.iter().enumerate() {
        let Some(state) = area
            .control_point
            .and_then(|i| map.control_points.get(i as usize).copied().flatten())
            .and_then(|e| flags.get(e).ok())
        else {
            continue;
        };
        let previous = flow.owners.insert(index, state.owner);
        if previous.is_none_or(|p| p == state.owner) || team_index(state.owner).is_none() {
            continue;
        }
        let team = state.owner;
        let mut there = Vec::new();
        for (&(squad_team, squad), info) in &snapshot.squads {
            if squad_team != team || !info.leader_is_bot {
                continue;
            }
            let (at, alive) = near(info, index, &[]);
            let before = flow.orders.get(&(team, squad)).copied();
            let ordered = before.is_some_and(|(_, area)| area == index);
            if at == 0 || !(ordered || at * 2 >= alive) {
                continue;
            }
            let was = match before {
                Some((OrderKind::Attack, a)) => format!("attack {}", map.areas.get(a).map_or("?", |a| a.name.as_str())),
                Some((OrderKind::Defend, a)) => format!("defend {}", map.areas.get(a).map_or("?", |a| a.name.as_str())),
                None => "no order".into(),
            };
            there.push(format!("{} ({at} of {alive}, {was})", squad_name(squad)));
            // Watched: squads mostly there, or three of it (for the others there is nothing
            // to leave).
            if at * 2 < alive && at < 3 {
                continue;
            }
            flow.watches.push(FlowWatch {
                team,
                squad,
                area: index,
                since: now,
                first: None,
                away: None,
                left: None,
                probed: false,
            });
        }
        info!(
            "ai flow: team {} took {}; squads there: {}",
            team_index(team).unwrap() + 1,
            area.name,
            if there.is_empty() { "none".into() } else { there.join(", ") }
        );
    }
    flow.watches.retain_mut(|watch| {
        let age = now - watch.since;
        let order = strategy.orders.get(&(watch.team, watch.squad));
        let info = snapshot.squads.get(&(watch.team, watch.squad));
        let guards = strategy
            .guards
            .get(&(watch.team, watch.squad))
            .filter(|g| g.area == watch.area)
            .map_or(&[][..], |g| g.members.as_slice());
        let (at, alive) = info.map_or((0, 0), |info| near(info, watch.area, guards));
        // The order it had at the capture counts as the old one while it stays.
        let changed = |o: Option<&super::strategy::SquadOrder>| match o {
            Some(o) => !(o.area == watch.area && o.kind == OrderKind::Attack),
            None => true,
        };
        let who = || {
            format!(
                "team {} {} ({} taken {age:.1} s ago)",
                team_index(watch.team).unwrap() + 1,
                squad_name(watch.squad),
                map.areas[watch.area].name
            )
        };
        if watch.first.is_none() && changed(order) {
            watch.first = Some((age, describe_order(&map, order)));
        }
        if watch.away.is_none() && order.is_some_and(|o| o.area != watch.area) {
            info!("ai flow: {}: new order {}", who(), describe_order(&map, order));
            watch.away = Some((age, describe_order(&map, order)));
        }
        if watch.left.is_none() && alive > 0 && at * 3 <= alive {
            info!("ai flow: {}: squad moved off ({at} of {alive} still there)", who());
            watch.left = Some(age);
        }
        // Still there a while after an order elsewhere: what keeps the squad?
        if !watch.probed
            && watch.left.is_none()
            && let Some((t, _)) = &watch.away
            && age > t + 15.0
            && let Some(info) = info
        {
            watch.probed = true;
            let a = &map.areas[watch.area];
            let describe = |s: &super::squad::SoldierInfo| {
                let riding = controls.get(s.player).is_ok_and(|c| seated.contains(c.0));
                format!(
                    "{} {:.0} m{}",
                    brains.get(s.player).map_or("human", |b| b.doing()),
                    s.position.xz().distance(a.position.xz()),
                    if riding { " seated" } else { "" }
                )
            };
            let leader = info.leader_soldier.as_ref().map_or("leader down".to_string(), |l| format!("leader {}", describe(l)));
            let members: Vec<String> = info
                .alive
                .iter()
                .filter(|s| Some(s.player) != info.leader)
                .map(|s| describe(s))
                .collect();
            info!("ai flow: {}: still there 15 s after its new order: {leader}; members {}", who(), members.join(", "));
        }
        let done = watch.away.is_some() && watch.left.is_some();
        if done || age > FLOW_WATCH || info.is_none() {
            let time = |t: &Option<(f32, String)>| match t {
                Some((s, what)) => format!("{what} after {s:.1} s"),
                None => format!("none in {age:.0} s"),
            };
            // What those still there are doing.
            let doing: Vec<&str> = info
                .map(|info| {
                    let a = &map.areas[watch.area];
                    info.alive
                        .iter()
                        .filter(|s| s.position.xz().distance(a.position.xz()) < a.radius + 40.0)
                        .filter_map(|s| brains.get(s.player).ok().map(|b| b.doing()))
                        .collect()
                })
                .unwrap_or_default();
            info!(
                "ai flow: team {} {} after taking {}: next order {}, away {}, left {} ({at} of {alive} there, now {}{})",
                team_index(watch.team).unwrap() + 1,
                squad_name(watch.squad),
                map.areas[watch.area].name,
                time(&watch.first),
                time(&watch.away),
                watch.left.map_or(format!("no, after {age:.0} s"), |s| format!("after {s:.1} s")),
                describe_order(&map, order),
                if doing.is_empty() { String::new() } else { format!("; doing: {}", doing.join(", ")) },
            );
            return false;
        }
        true
    });
    flow.orders = strategy.orders.iter().map(|(key, o)| (*key, (o.kind, o.area))).collect();
}
