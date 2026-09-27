//! The squad layer: bot squad leaders take their squad where the commander sends it, the
//! members follow in a loose wedge and wait for each other, and dead bots pick a kit the
//! team is short of and spawn on their leader when that gets them closer to the fight.
//! Bots in a human's squad follow the human; the commander's order is only a suggestion.

use bevy::{platform::collections::HashMap, prelude::*};
use game_shared::{
    conquest::{Deployment, team_index},
    protocol::{Player, Team},
    revive::Downed,
    soldier::{Health, SoldierMotion},
    squad::SquadMember,
    weapons::Armory,
};

use super::skill::Personality;
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
