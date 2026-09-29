//! Breakthrough: the control points come in sectors. Only the current sector's flags move
//! (conquest's flag rules); the others are `Locked`. Once the attackers hold every flag of
//! the sector it falls: its flags stay theirs, the next sector opens, the defenders fall back
//! (they spawn at the flags they still hold) and the attackers get their tickets back.

use bevy::prelude::*;
use bevy_replicon::prelude::*;
use game_data::modes::ModeKind;
use game_shared::{
    conquest::{FlagState, RoundState, Tickets, team_from_id},
    modes::{Locked, ModeState, ObjectiveEvent, Sector},
    protocol::Team,
};

use super::{ModeSystems, RoundClock, RoundSetup, in_mode, staged};
use crate::ai::strategy::{Objective, OrderKind, PlanView, Posture};

pub struct BreakthroughPlugin;

impl Plugin for BreakthroughPlugin {
    fn build(&self, app: &mut App) {
        app.add_systems(
            FixedUpdate,
            advance
                .in_set(ModeSystems::Objectives)
                // After the flags moved this tick.
                .after(crate::conquest::ConquestSystems)
                .run_if(in_mode(ModeKind::Breakthrough)),
        );
    }
}

/// The round: the attackers' tickets; every sector's flags the defenders', only the first
/// sector's open. Points in no sector (the bases, where the attackers start) stay as the
/// layout has them, locked.
pub(crate) fn setup(_: &mut Commands, setup: &mut RoundSetup) {
    let Some(staged) = setup.layout.and_then(|l| l.staged.as_ref()) else {
        return;
    };
    let attacker = team_from_id(staged.attacker);
    setup.state.attacker = attacker;
    setup.state.stages = staged.stages.len() as u8;
    setup.tickets = staged::attacker_tickets_at_start(attacker, staged.tickets);
    for point in &mut setup.points {
        let sector = staged.stages.iter().position(|s| s.control_points.contains(&point.rules.id));
        point.sector = sector.map(|s| s as u8);
        match sector {
            Some(s) => {
                point.set_owner(attacker.opponent());
                point.locked = s != 0;
            }
            None => point.locked = true,
        }
    }
}

/// The sector falls once the attackers hold all of its flags.
#[allow(clippy::type_complexity)]
fn advance(
    mut commands: Commands,
    match_state: Single<(&mut ModeState, &mut Tickets, &mut RoundState)>,
    points: Query<(Entity, &Sector, &FlagState)>,
    mut clock: ResMut<RoundClock>,
    mut events: MessageWriter<ToClients<ObjectiveEvent>>,
) {
    let (mut mode, mut tickets, mut round) = match_state.into_inner();
    if *round != RoundState::Playing {
        return;
    }
    let mut current = points.iter().filter(|(_, sector, _)| sector.0 == mode.stage).peekable();
    // A sector without flags (a hand-made layout naming no points it has) falls at once.
    let empty = current.peek().is_none();
    if !empty && current.any(|(_, _, flag)| flag.owner != mode.attacker) {
        return;
    }
    for (entity, sector, _) in &points {
        if sector.0 == mode.stage {
            commands.entity(entity).insert(Locked);
        }
    }
    if !staged::take_stage(&mut mode, &mut tickets, &mut round, &mut clock, &mut events) {
        return;
    }
    for (entity, sector, _) in &points {
        if sector.0 == mode.stage {
            commands.entity(entity).remove::<Locked>();
        }
    }
}

/// The bots' Breakthrough strategy, over the open sector's flags only: the attackers take the
/// ones they don't hold (they need them all) and hold the ones being taken back; the
/// defenders hold theirs, more so under attack, and take back what they lost.
pub(crate) fn objectives(view: &PlanView, team: Team) -> (Posture, Vec<Objective>) {
    let attacking = team == view.mode.attacker;
    let mut objectives = Vec::new();
    for (a, area) in view.map.areas.iter().enumerate() {
        let Some(flag) = view.flags.get(a).copied().flatten() else {
            continue;
        };
        if area.uncapturable || view.locked.get(a).copied().unwrap_or(true) {
            continue;
        }
        let threat = view.intel.enemies_near(team, area.position, area.radius + 35.0) as f32;
        let under_attack = flag.rate < 0.0 || (flag.flag != team && flag.height < 1.0);
        if flag.owner != team {
            let mut value = if flag.owner == Team::Spectator { 12.0 } else { 10.0 };
            if flag.flag == team && flag.rate > 0.0 {
                // Our flag is going up: finish the job.
                value *= 1.3;
            }
            if attacking {
                value *= 1.2;
            }
            value /= 1.0 + 0.1 * threat;
            objectives.push(Objective { area: a, kind: OrderKind::Attack, value });
        } else {
            let hold = if attacking { 3.0 } else { 8.0 };
            let value = hold + 1.5 * threat.min(4.0) + if under_attack { 15.0 } else { 0.0 };
            objectives.push(Objective { area: a, kind: OrderKind::Defend, value });
        }
    }
    objectives.sort_by(|a, b| b.value.total_cmp(&a.value));
    (if attacking { Posture::Attack } else { Posture::Guard }, objectives)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ai::strategy::{Area, StrategicMap, TeamIntel};

    fn flag(name: &str, index: u8) -> Area {
        Area {
            name: name.into(),
            control_point: Some(index),
            charge: None,
            position: Vec3::new(0.0, 0.0, index as f32 * 100.0),
            order_position: Vec3::ZERO,
            radius: 10.0,
            uncapturable: false,
            has_spawns: true,
            neighbours: Vec::new(),
            routes: Vec::new(),
        }
    }

    #[test]
    fn only_the_open_sector_counts() {
        let map = StrategicMap::of_areas(vec![flag("hotel", 0), flag("square", 1), flag("market", 2)]);
        // The attackers (team 2) took the hotel; the square is still the defenders'; the market
        // is in the next sector.
        let flags = [
            Some(FlagState::held_by(Team::Two)),
            Some(FlagState::held_by(Team::One)),
            Some(FlagState::held_by(Team::One)),
        ];
        let view = PlanView {
            map: &map,
            flags: &flags,
            charges: &[None; 3],
            locked: &[false, false, true],
            intel: &TeamIntel::default(),
            bleeding: [false; 2],
            mode: ModeState {
                kind: ModeKind::Breakthrough,
                attacker: Team::Two,
                stages: 2,
                ..default()
            },
        };
        let (_, attack) = objectives(&view, Team::Two);
        assert_eq!((attack[0].area, attack[0].kind), (1, OrderKind::Attack));
        assert!(attack.iter().all(|o| o.area != 2));
        let (_, defend) = objectives(&view, Team::One);
        // Hold the square, take the hotel back.
        assert!(defend.iter().any(|o| o.area == 1 && o.kind == OrderKind::Defend));
        assert!(defend.iter().any(|o| o.area == 0 && o.kind == OrderKind::Attack));
    }
}
