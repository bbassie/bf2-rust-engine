//! The squad layer: bot squad leaders take their squad where the commander sends it, the
//! members follow in a loose wedge and wait for each other, and dead bots pick a kit the
//! team is short of and spawn on their leader when that gets them closer to the fight.
//! Bots in a human's squad follow the human; the commander's order is only a suggestion.
//!
//! Near the fight a bot-led squad works as two fire teams ([`fire_team`]; the leader's is
//! team 0) coordinated by [`coordinate`]:
//!
//! - **Bounding overwatch** on the way to an objective in contact (close to it, or enemies
//!   seen near the squad): one fire team moves while the other holds and covers, then they
//!   swap: the leader's team moves [`BOUND_DISTANCE`] meters with him, then the other team
//!   bounds past him to [`bound_slot`]s while his team covers.
//! - **Pinned down** (members fighting and under fire for a while, the enemy in one place):
//!   team 0 keeps his head down with suppressive fire while team 1 goes round him.

use bevy::{platform::collections::HashMap, prelude::*};
use game_shared::{
    conquest::{Deployment, team_index},
    protocol::{Player, Team},
    revive::Downed,
    soldier::{Health, SoldierMotion},
    squad::SquadMember,
    weapons::Armory,
};

use super::{
    skill::Personality,
    strategy::{StrategicMap, Strategy, TeamIntel},
};
use crate::{Controls, bots::BotBrain};

/// A living soldier, as squads and commanders see it.
#[derive(Clone, Copy, Debug)]
pub struct SoldierInfo {
    pub player: Entity,
    pub position: Vec3,
    pub yaw: f32,
    /// Direction of travel (yaw) while moving, otherwise where it looks.
    pub heading: f32,
    /// Health left, 0..1.
    pub health: f32,
    /// Just hurt (bots only).
    pub busy: bool,
}

#[derive(Default, Debug)]
pub struct SquadInfo {
    pub leader: Option<Entity>,
    pub leader_is_bot: bool,
    /// The leader's soldier while he is alive and not down.
    pub leader_soldier: Option<SoldierInfo>,
    /// Members other than the leader, in a stable order: their places in the formation.
    pub members: Vec<Entity>,
    /// Soldiers of the squad on their feet, the leader's included.
    pub alive: Vec<SoldierInfo>,
}

impl SquadInfo {
    /// Where the squad is: its leader, or the middle of its living members.
    pub fn centroid(&self) -> Option<Vec3> {
        if let Some(leader) = &self.leader_soldier {
            return Some(leader.position);
        }
        let sum = self.alive.iter().map(|s| s.position).reduce(|a, b| a + b)?;
        Some(sum / self.alive.len() as f32)
    }

    /// A member's place in the formation.
    pub fn slot(&self, player: Entity) -> Option<usize> {
        self.members.iter().position(|m| *m == player)
    }

    /// Average distance of the living members from the leader.
    pub fn spread(&self) -> Option<f32> {
        let leader = self.leader_soldier?;
        let members: Vec<f32> = self
            .alive
            .iter()
            .filter(|s| s.player != leader.player)
            .map(|s| s.position.distance(leader.position))
            .collect();
        (!members.is_empty()).then(|| members.iter().sum::<f32>() / members.len() as f32)
    }
}

/// Squads, kits and soldiers of both teams, gathered once per tick for the commanders and
/// the bots.
#[derive(Resource, Default)]
pub struct SquadSnapshot {
    pub squads: HashMap<(Team, u8), SquadInfo>,
    /// Kit slot of every player, per team.
    pub kits: [Vec<(Entity, u8)>; 2],
    /// Soldiers on their feet, per team.
    pub soldiers: [Vec<SoldierInfo>; 2],
}

pub fn snapshot(
    mut snapshot: ResMut<SquadSnapshot>,
    players: Query<
        (Entity, &Team, Option<&SquadMember>, Option<&Controls>, Option<&BotBrain>, &Deployment),
        With<Player>,
    >,
    soldiers: Query<(&SoldierMotion, Option<&Health>), Without<Downed>>,
) {
    let snapshot = &mut *snapshot;
    snapshot.squads.clear();
    for list in &mut snapshot.kits {
        list.clear();
    }
    for list in &mut snapshot.soldiers {
        list.clear();
    }
    let mut players: Vec<_> = players.iter().collect();
    players.sort_by_key(|(entity, ..)| *entity);
    for (player, team, member, controls, brain, deployment) in players {
        let Some(t) = team_index(*team) else {
            continue;
        };
        snapshot.kits[t].push((player, deployment.kit));
        let soldier = controls.and_then(|c| soldiers.get(c.0).ok()).map(|(motion, health)| SoldierInfo {
            player,
            position: motion.position,
            yaw: motion.yaw,
            heading: match motion.velocity.with_y(0.0) {
                v if v.length() > 1.0 => (-v.x).atan2(-v.z),
                _ => motion.yaw,
            },
            health: health.map_or(1.0, |h| (h.current / h.max.max(1.0)).clamp(0.0, 1.0)),
            busy: brain.is_some_and(|b| b.busy()),
        });
        if let Some(soldier) = soldier {
            snapshot.soldiers[t].push(soldier);
        }
        let Some(member) = member else {
            continue;
        };
        let squad = snapshot.squads.entry((*team, member.squad)).or_default();
        if member.leader {
            squad.leader = Some(player);
            squad.leader_is_bot = brain.is_some();
            squad.leader_soldier = soldier;
        } else {
            squad.members.push(player);
        }
        if let Some(soldier) = soldier {
            squad.alive.push(soldier);
        }
    }
}

/// Which fire team a squad member is in: 0 for the leader (`slot` `None`) and every other
/// member, 1 for the rest.
pub fn fire_team(slot: Option<usize>) -> u8 {
    match slot {
        None => 0,
        Some(s) => (s % 2 == 0) as u8,
    }
}

/// How far each fire team moves per bound, meters.
pub const BOUND_DISTANCE: f32 = 25.0;
/// How long a bound lasts at most, seconds.
const BOUND_SECONDS: f32 = 12.0;
/// Squads bound within this distance of their objective (or with enemies seen near them).
const CONTACT_DISTANCE: f32 = 220.0;
/// ... and run in a wedge once this close (they spread out over the objective there).
const ARRIVED_DISTANCE: f32 = 60.0;

/// What the bots of a squad reported this tick (written by [`crate::bots`], read by
/// [`coordinate`] next tick).
#[derive(Default, Clone, Copy, Debug)]
pub struct SquadReport {
    /// Members fighting (a target in sight, or one just lost).
    pub engaged: u32,
    /// Members under fire (suppressed).
    pub suppressed: u32,
    /// Sum and count of the enemy positions members are fighting.
    pub contact_sum: Vec3,
    pub contacts: u32,
}

/// Per squad: [`SquadReport`]s of this tick.
#[derive(Resource, Default)]
pub struct SquadReports(pub HashMap<(Team, u8), SquadReport>);

/// What a squad's fire teams do (see the module docs).
#[derive(Default, Clone, Copy, Debug)]
pub struct SquadTactic {
    /// Bounding overwatch on: `moving` is the fire team on the move.
    pub bounding: bool,
    pub moving: u8,
    /// Counts bounds: members note where they hold when it changes.
    pub phase: u32,
    /// Seconds into this bound.
    pub phase_time: f32,
    /// Where the leader was when the bound began, and the way forward (XZ unit vector).
    pub anchor: Vec3,
    pub axis: Vec3,
    /// Pinned down by enemies around here: team 0 suppresses, team 1 flanks on `flank_side`
    /// (+1 or -1) for `pin_time` more seconds.
    pub pinned: Option<Vec3>,
    pub flank_side: f32,
    pub pin_time: f32,
    /// Seconds the squad has been fighting under fire (towards pinned), and until it may be
    /// pinned again.
    pub pressure: f32,
    pub pin_cooldown: f32,
    /// Seconds without contacts while pinned.
    pub quiet: f32,
    /// Counts pinned episodes (members roll whether they join in once per episode).
    pub episode: u32,
}

/// Per squad (bot-led ones): what its fire teams do.
#[derive(Resource, Default)]
pub struct SquadTactics {
    pub squads: HashMap<(Team, u8), SquadTactic>,
    /// Per team, bounds begun and squads pinned down (for the statistics).
    pub bounds: [u32; 2],
    pub pins: [u32; 2],
}

/// Where member `slot` of fire team 1 goes on its bound: past the leader along the way
/// forward, spread out sideways.
pub fn bound_slot(tactic: &SquadTactic, leader: Vec3, slot: usize) -> Vec3 {
    let side = Vec3::new(-tactic.axis.z, 0.0, tactic.axis.x);
    let (right, forward) = match slot / 2 {
        0 => (-6.0, 14.0),
        1 => (6.0, 14.0),
        _ => (0.0, 18.0),
    };
    leader + tactic.axis * forward + side * right
}

/// Whether bots of `team` play with the tactics from before squad coordination (testing,
/// `--bot-legacy-team`).
pub fn legacy(settings: &crate::ServerSettings, team: Team) -> bool {
    match settings.bot_legacy_team {
        0 => false,
        3 => true,
        n => team_index(team) == Some(n as usize - 1),
    }
}

/// Updates every bot-led squad's fire teams from its members' reports (see the module docs).
#[allow(clippy::too_many_arguments)]
pub fn coordinate(
    time: Res<Time>,
    snapshot: Res<SquadSnapshot>,
    strategy: Res<Strategy>,
    map: Res<StrategicMap>,
    intel: Res<TeamIntel>,
    settings: Res<crate::ServerSettings>,
    mut reports: ResMut<SquadReports>,
    mut tactics: ResMut<SquadTactics>,
) {
    let dt = time.delta_secs();
    let tactics = &mut *tactics;
    tactics.squads.retain(|key, _| snapshot.squads.get(key).is_some_and(|s| s.leader_is_bot));
    for (&key, squad) in &snapshot.squads {
        let (team, _) = key;
        if !squad.leader_is_bot || legacy(&settings, team) {
            continue;
        }
        let Some(t) = team_index(team) else {
            continue;
        };
        let report = reports.0.get(&key).copied().unwrap_or_default();
        let tactic = tactics.squads.entry(key).or_default();
        let Some(leader) = squad.leader_soldier else {
            tactic.bounding = false;
            tactic.pinned = None;
            continue;
        };
        let members = squad.alive.len();

        // Pinned down: fighting under fire in one place for a while.
        tactic.pin_cooldown -= dt;
        let pressed = report.engaged >= 2 || (report.engaged >= 1 && report.suppressed >= 1);
        tactic.pressure = if pressed { tactic.pressure + dt } else { (tactic.pressure - 2.0 * dt).max(0.0) };
        let contact = (report.contacts > 0).then(|| report.contact_sum / report.contacts as f32);
        match (tactic.pinned, contact) {
            (None, Some(at)) if tactic.pressure > 4.0 && tactic.pin_cooldown <= 0.0 && members >= 3 => {
                tactic.pinned = Some(at);
                tactic.pin_time = 25.0;
                tactic.flank_side = if fastrand::bool() { 1.0 } else { -1.0 };
                tactic.quiet = 0.0;
                tactic.episode = tactic.episode.wrapping_add(1);
                tactics.pins[t] += 1;
            }
            (Some(pinned), _) => {
                tactic.pin_time -= dt;
                match contact {
                    // Follow the enemy slowly (the flankers aim for where he is).
                    Some(at) => {
                        tactic.pinned = Some(pinned.lerp(at, (dt * 0.2).min(1.0)));
                        tactic.quiet = 0.0;
                    }
                    None => tactic.quiet += dt,
                }
                if tactic.pin_time <= 0.0 || tactic.quiet > 6.0 || members < 2 {
                    tactic.pinned = None;
                    tactic.pin_cooldown = 20.0;
                    tactic.pressure = 0.0;
                }
            }
            _ => {}
        }

        // Bounding overwatch on the way to an objective in contact.
        let objective = strategy
            .orders
            .get(&key)
            .and_then(|o| o.point.or_else(|| map.areas.get(o.area).map(|a| a.order_position)));
        let enemies_near = intel.enemies_near(team, leader.position, super::tune::knob("bound_near", 110.0)) > 0;
        let wanted = objective.is_some_and(|at| {
            let d = at.distance(leader.position);
            d > ARRIVED_DISTANCE && (d < super::tune::knob("contact_dist", CONTACT_DISTANCE) || enemies_near)
        }) && members >= 3
            && tactic.pinned.is_none();
        let Some(objective) = objective.filter(|_| wanted) else {
            tactic.bounding = false;
            continue;
        };
        let axis = (objective - leader.position).with_y(0.0).normalize_or(Vec3::NEG_Z);
        let start = !tactic.bounding;
        tactic.phase_time += dt;
        let done = !start
            && match tactic.moving {
                0 => leader.position.distance(tactic.anchor) > BOUND_DISTANCE,
                _ => squad
                    .members
                    .iter()
                    .enumerate()
                    .filter(|(slot, _)| fire_team(Some(*slot)) == 1)
                    .filter_map(|(slot, player)| squad.alive.iter().find(|s| s.player == *player).map(|s| (slot, s)))
                    .all(|(slot, s)| s.position.distance(bound_slot(tactic, tactic.anchor, slot)) < 5.0),
            };
        if start || done || tactic.phase_time > super::tune::knob("bound_secs", BOUND_SECONDS) {
            tactic.moving = if start { 0 } else { 1 - tactic.moving };
            tactic.bounding = true;
            tactic.phase = tactic.phase.wrapping_add(1);
            tactic.phase_time = 0.0;
            tactic.anchor = leader.position;
            tactic.axis = axis;
            tactics.bounds[t] += 1;
        }
    }
    reports.0.clear();
}

/// Where member `slot` walks: a loose wedge behind and beside the leader, `spacing` meters
/// apart.
pub fn formation_slot(leader: &SoldierInfo, slot: usize, spacing: f32) -> Vec3 {
    // (right, forward) in units of `spacing`.
    const WEDGE: [(f32, f32); 5] = [(-1.0, -1.0), (1.0, -1.0), (-2.0, -2.0), (2.0, -2.0), (0.0, -2.5)];
    let (right, forward) = WEDGE[slot % WEDGE.len()];
    leader.position + Quat::from_rotation_y(leader.heading) * Vec3::new(right, 0.0, -forward) * spacing
}

/// The share of each kit kind a team should have. Medics and support are worth more once
/// healing and resupply work; anti-tank once bots meet vehicles.
const KIT_SHARES: [(&str, f32); 7] = [
    ("Assault", 0.26),
    ("Support", 0.16),
    ("Medic", 0.14),
    ("Specops", 0.12),
    ("Engineer", 0.10),
    ("Sniper", 0.10),
    ("AT", 0.12),
];

/// A kit slot for a bot of team `t`: the kind the team is shortest of, leaning towards the
/// bot's favourite and the kit it has. `enemy_vehicles`: the enemy drives vehicles, so
/// anti-tank kits are in demand.
pub fn choose_kit(
    armory: &Armory,
    t: usize,
    team_kits: &[(Entity, u8)],
    me: Entity,
    current: u8,
    personality: &Personality,
    enemy_vehicles: usize,
) -> u8 {
    let kind_of = |slot: u8| -> Option<&str> {
        let name = armory.team_kits.get(t)?.get(slot as usize)?;
        armory.kits.get(name).map(|k| k.kind.as_str())
    };
    let others: Vec<&str> = team_kits
        .iter()
        .filter(|(player, _)| *player != me)
        .filter_map(|(_, slot)| kind_of(*slot))
        .collect();
    let total = others.len().max(1) as f32;
    let slots = armory.team_kits.get(t).map_or(0, |k| k.len()) as u8;
    (0..slots)
        .filter_map(|slot| kind_of(slot).map(|kind| (slot, kind)))
        .map(|(slot, kind)| {
            let mut share = KIT_SHARES
                .iter()
                .find(|(k, _)| k.eq_ignore_ascii_case(kind))
                .map_or(0.05, |(_, s)| *s);
            if kind.eq_ignore_ascii_case("AT") {
                share += 0.08 * enemy_vehicles.min(3) as f32;
            }
            let have = others.iter().filter(|k| k.eq_ignore_ascii_case(kind)).count() as f32 / total;
            let mut score = share - have + 0.02 * fastrand::f32();
            if personality.favourite_kit.is_some_and(|f| f.eq_ignore_ascii_case(kind)) {
                score += 0.08;
            }
            if slot == current {
                score += 0.03;
            }
            (slot, score)
        })
        .max_by(|a, b| a.1.total_cmp(&b.1))
        .map_or(current, |(slot, _)| slot)
}

/// Where a dead bot spawns next: on its squad leader when he is alive, not under fire, and
/// closer to where the squad is going than any spawn the team holds; otherwise at the held
/// control point (with spawn points) closest to the objective. Returns the control point
/// and whether to spawn on the leader.
pub fn choose_spawn(objective: Option<Vec3>, leader: Option<&SoldierInfo>, spawns: &[(u8, Vec3)]) -> (Option<u8>, bool) {
    let Some(objective) = objective else {
        return (None, leader.is_some_and(|l| !l.busy));
    };
    let best = spawns
        .iter()
        .map(|(index, position)| (*index, position.distance(objective)))
        .min_by(|a, b| a.1.total_cmp(&b.1));
    let on_leader = leader.is_some_and(|l| {
        !l.busy && l.health > 0.3 && best.is_none_or(|(_, d)| l.position.distance(objective) < d - 30.0)
    });
    (best.map(|(index, _)| index), on_leader)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn soldier(position: Vec3, busy: bool) -> SoldierInfo {
        SoldierInfo {
            player: Entity::PLACEHOLDER,
            position,
            yaw: 0.0,
            heading: 0.0,
            health: 1.0,
            busy,
        }
    }

    #[test]
    fn wedge_trails_the_leader() {
        // Facing -Z: the first two slots are behind (+Z) to the left and right.
        let leader = soldier(Vec3::ZERO, false);
        let left = formation_slot(&leader, 0, 4.0);
        let right = formation_slot(&leader, 1, 4.0);
        assert!(left.x < 0.0 && right.x > 0.0 && left.z > 0.0 && right.z > 0.0, "{left} {right}");
    }

    #[test]
    fn fire_teams_leapfrog() {
        // The leader and every other member form team 0, the rest team 1.
        assert_eq!(fire_team(None), 0);
        assert_eq!([0, 1, 2, 3, 4].map(|s| fire_team(Some(s))), [1, 0, 1, 0, 1]);
        // Team 1 bounds past the leader along the way forward, spread out sideways.
        let tactic = SquadTactic {
            axis: Vec3::NEG_Z,
            ..default()
        };
        let slots = [0, 2, 4].map(|s| bound_slot(&tactic, Vec3::ZERO, s));
        assert!(slots.iter().all(|p| p.z < -10.0), "{slots:?}");
        assert!(slots[0].x * slots[1].x < 0.0, "{slots:?}");
    }

    #[test]
    fn legacy_teams() {
        let mut settings = crate::ServerSettings::default();
        assert!(!legacy(&settings, Team::One) && !legacy(&settings, Team::Two));
        settings.bot_legacy_team = 1;
        assert!(legacy(&settings, Team::One) && !legacy(&settings, Team::Two));
        settings.bot_legacy_team = 3;
        assert!(legacy(&settings, Team::One) && legacy(&settings, Team::Two));
    }

    #[test]
    fn spawns_on_the_leader_only_when_it_helps() {
        let objective = Vec3::new(0.0, 0.0, 300.0);
        let spawns = [(1, Vec3::ZERO), (2, Vec3::new(0.0, 0.0, 150.0))];
        let near = soldier(Vec3::new(0.0, 0.0, 250.0), false);
        assert_eq!(choose_spawn(Some(objective), Some(&near), &spawns), (Some(2), true));
        let fighting = soldier(Vec3::new(0.0, 0.0, 250.0), true);
        assert_eq!(choose_spawn(Some(objective), Some(&fighting), &spawns), (Some(2), false));
        let behind = soldier(Vec3::new(0.0, 0.0, 140.0), false);
        assert_eq!(choose_spawn(Some(objective), Some(&behind), &spawns), (Some(2), false));
    }
}
