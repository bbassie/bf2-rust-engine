//! What Rush and Breakthrough share: only the attackers have tickets, one per death. Taking a
//! stage moves the front and refills them. The round ends when they take the last stage
//! (they win) or run out of tickets (the defenders win), unless an armed charge keeps it going
//! until it goes off or is defused (overtime). Nobody spawns at a flag with enemies at it
//! ([`SpawnBlocked`]): the defenders' flags at the front would otherwise put them right back
//! where the attackers are, and the front would never move.

use bevy::prelude::*;
use bevy_replicon::prelude::*;
use game_data::modes::ModeKind;
use game_shared::{
    conquest::{ControlPoint, FlagState, RoundState, Tickets, team_index},
    modes::{ChargeState, ModeState, ObjectiveEvent, ObjectiveEventKind, SpawnBlocked},
    protocol::{ControlledBy, Team},
    revive::Downed,
    soldier::{Soldier, SoldierMotion},
};

use super::{ModeSystems, RoundClock, end_round, in_modes};
use crate::combat::Died;

/// Enemies this close to a flag (beyond its radius, at least this far) close it for spawning.
const CONTESTED_MARGIN: f32 = 15.0;
const CONTESTED_MIN: f32 = 30.0;

pub struct StagedPlugin;

impl Plugin for StagedPlugin {
    fn build(&self, app: &mut App) {
        app.add_systems(
            FixedUpdate,
            (
                block_contested_spawns.in_set(ModeSystems::Objectives),
                attacker_tickets.in_set(ModeSystems::Tickets),
            )
                .run_if(in_modes(&[ModeKind::Rush, ModeKind::Breakthrough])),
        );
    }
}

/// Closes flags with enemies at them for spawning, twice a second.
#[allow(clippy::type_complexity)]
fn block_contested_spawns(
    mut commands: Commands,
    time: Res<Time>,
    mut timer: Local<f32>,
    points: Query<(Entity, &ControlPoint, &FlagState, Option<&SpawnBlocked>)>,
    soldiers: Query<(&SoldierMotion, &ControlledBy), (With<Soldier>, Without<Downed>)>,
    teams: Query<&Team>,
) {
    *timer -= time.delta_secs();
    if *timer > 0.0 {
        return;
    }
    *timer = 0.5;
    let soldiers: Vec<(Vec3, Team)> = soldiers
        .iter()
        .filter_map(|(motion, controlled_by)| Some((motion.position, *teams.get(controlled_by.0).ok()?)))
        .collect();
    for (entity, cp, flag, blocked) in &points {
        let owner = flag.owner;
        let reach = (cp.radius + CONTESTED_MARGIN).max(CONTESTED_MIN);
        let contested = owner != Team::Spectator
            && soldiers
                .iter()
                .any(|(at, team)| *team == owner.opponent() && at.xz().distance(cp.position.xz()) < reach);
        match (contested, blocked) {
            (true, Some(b)) if b.0 == owner => {}
            (true, _) => {
                commands.entity(entity).insert(SpawnBlocked(owner));
            }
            (false, Some(_)) => {
                commands.entity(entity).remove::<SpawnBlocked>();
            }
            (false, None) => {}
        }
    }
}

/// The attackers' `tickets`; the defenders have none (and need none).
pub fn attacker_tickets_at_start(attacker: Team, tickets: f32) -> Tickets {
    let mut start = [0.0; 2];
    if let Some(a) = team_index(attacker) {
        start[a] = tickets.max(1.0);
    }
    Tickets {
        remaining: start,
        start,
        bleed: [0.0; 2],
    }
}

/// A ticket per attacker death; out of tickets, the round is the defenders' (not while a
/// charge is armed).
fn attacker_tickets(
    mut deaths: MessageReader<Died>,
    match_state: Single<(&mut Tickets, &mut RoundState, &mut ModeState)>,
    charges: Query<&ChargeState>,
    clock: Res<RoundClock>,
) {
    let (mut tickets, mut round, mut mode) = match_state.into_inner();
    let Some(a) = team_index(mode.attacker) else {
        deaths.clear();
        return;
    };
    if *round != RoundState::Playing {
        deaths.clear();
        return;
    }
    let mut next = *tickets;
    for death in deaths.read() {
        if death.team == mode.attacker {
            next.remaining[a] = (next.remaining[a] - 1.0).max(0.0);
        }
    }
    tickets.set_if_neq(next);
    let out = next.remaining[a] <= 0.0;
    let armed = charges.iter().any(|c| c.armed());
    let overtime = out && armed;
    if mode.overtime != overtime {
        mode.overtime = overtime;
        if overtime {
            info!("{}: attackers out of tickets with a charge armed: overtime", mode.kind.label());
        }
    }
    if out && !armed {
        let state = *mode;
        end_round(&mut round, &state, &clock, state.defender(), "attackers out of tickets");
    }
}

/// The attackers took the current stage. After the last the round is theirs; otherwise the
/// next stage begins with their tickets refilled. Returns whether there is a next stage (the
/// caller then brings its objectives into play and moves the spawns).
pub fn take_stage(
    mode: &mut ModeState,
    tickets: &mut Tickets,
    round: &mut RoundState,
    clock: &mut RoundClock,
    events: &mut MessageWriter<ToClients<ObjectiveEvent>>,
) -> bool {
    events.write(ToClients {
        targets: SendTargets::All,
        message: ObjectiveEvent {
            kind: ObjectiveEventKind::StageTaken,
            charge: None,
            team: mode.attacker,
            stage: mode.stage,
        },
    });
    let left = team_index(mode.attacker).map_or(0.0, |a| tickets.remaining[a]);
    info!(
        "{}: {} taken after {:.0} s ({:.0} s into the round, {left:.0} tickets left)",
        mode.kind.label(),
        mode.stage_label(),
        clock.stage,
        clock.round
    );
    if mode.stage + 1 >= mode.stages {
        let state = *mode;
        end_round(round, &state, clock, state.attacker, "attackers took the last stage");
        return false;
    }
    mode.stage += 1;
    mode.overtime = false;
    clock.stage = 0.0;
    if let Some(a) = team_index(mode.attacker) {
        tickets.remaining[a] = tickets.remaining[a].max(tickets.start[a]);
    }
    info!("{}: {} of {} begins, tickets refilled", mode.kind.label(), mode.stage_label(), mode.stages);
    true
}

#[cfg(test)]
mod tests {
    use bevy::ecs::system::RunSystemOnce;

    use super::*;

    #[test]
    fn taking_stages() {
        let mut app = App::new();
        app.add_message::<ToClients<ObjectiveEvent>>();
        let mut tickets = attacker_tickets_at_start(Team::Two, 100.0);
        assert_eq!(tickets.start, [0.0, 100.0]);
        tickets.remaining[1] = 20.0;
        let mut mode = ModeState {
            kind: ModeKind::Rush,
            attacker: Team::Two,
            stage: 0,
            stages: 2,
            overtime: false,
        };
        let mut round = RoundState::Playing;
        let mut clock = RoundClock::default();
        app.world_mut()
            .run_system_once(move |mut events: MessageWriter<ToClients<ObjectiveEvent>>| {
                // The first stage: on to the second, tickets refilled.
                assert!(take_stage(&mut mode, &mut tickets, &mut round, &mut clock, &mut events));
                assert_eq!((mode.stage, tickets.remaining[1]), (1, 100.0));
                // The last: the attackers win.
                assert!(!take_stage(&mut mode, &mut tickets, &mut round, &mut clock, &mut events));
                assert!(matches!(round, RoundState::Ended { winner: Team::Two, .. }));
            })
            .unwrap();
    }
}
