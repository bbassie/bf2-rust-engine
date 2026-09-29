//! Conquest rules, after BF2's `gpm_cq`: whichever team has more soldiers inside a control
//! point's radius lowers the enemy flag and raises its own, a team bleeds tickets while the
//! enemy holds more area value, every death costs a ticket, and the round ends when a team
//! runs out. Co-op plays by the same rules.
//!
//! The flags are shared with the other modes that have them (Breakthrough's sectors are
//! conquest flags, `Locked` outside the sector being fought over); rounds are common to all
//! modes (see [`crate::modes`]).

use bevy::prelude::*;
use bevy_replicon::prelude::*;
use game_data::{ControlPointDesc, modes::ModeKind};
use game_shared::{
    conquest::{ControlPoint, FlagEvent, FlagEventKind, FlagState, RoundState, Tickets, team_from_id, team_index},
    level::LoadedLevel,
    modes::{Locked, ModeState},
    protocol::{ControlledBy, Score, Team},
    revive::Downed,
    soldier::{Soldier, SoldierMotion},
};

use crate::{
    combat::Died,
    modes::{ModeSystems, RoundClock, RoundSetup, end_round, in_modes},
};

/// Tickets per team when the level doesn't say.
const DEFAULT_TICKETS: f32 = 100.0;
const DEFAULT_TICKET_LOSS_PER_MINUTE: f32 = 10.0;
/// Score for everyone of the team inside when a flag is captured or neutralized.
const SCORE_CAPTURE: i32 = 2;

pub struct ConquestPlugin;

impl Plugin for ConquestPlugin {
    fn build(&self, app: &mut App) {
        app.add_systems(
            FixedUpdate,
            (
                update_flags.in_set(ModeSystems::Objectives).in_set(ConquestSystems),
                update_tickets
                    .in_set(ModeSystems::Tickets)
                    .run_if(in_modes(&[ModeKind::Conquest, ModeKind::Coop, ModeKind::TeamDeathmatch])),
            ),
        );
    }
}

/// The flags moving (in [`ModeSystems::Objectives`]).
#[derive(SystemSet, Debug, Clone, PartialEq, Eq, Hash)]
pub struct ConquestSystems;

/// Server-only rules of a control point, next to its [`ControlPoint`].
#[derive(Component, Clone, Debug)]
pub struct ControlPointRules {
    /// Layout id; spawn points refer to it.
    pub id: String,
    pub(crate) area_value: [f32; 2],
    pub(crate) time_to_get_control: f32,
    pub(crate) time_to_lose_control: f32,
    pub(crate) only_takeable_by: Team,
    pub(crate) enemy_ticket_loss_when_captured: f32,
}

impl ControlPointRules {
    pub fn from_desc(cp: &ControlPointDesc) -> Self {
        Self {
            id: cp.id.clone(),
            area_value: cp.area_value,
            time_to_get_control: cp.time_to_get_control,
            time_to_lose_control: cp.time_to_lose_control,
            only_takeable_by: team_from_id(cp.only_takeable_by_team),
            enemy_ticket_loss_when_captured: cp.enemy_ticket_loss_when_captured,
        }
    }
}

/// Conquest's round: each team's tickets from the level (the closest layout size).
pub(crate) fn setup(setup: &mut RoundSetup) {
    let start = std::array::from_fn(|team| {
        setup
            .level
            .desc
            .teams
            .get(team)
            .and_then(|t| t.tickets.iter().min_by_key(|(size, _)| size.abs_diff(setup.size)))
            .map_or(DEFAULT_TICKETS, |(_, tickets)| *tickets as f32)
    });
    setup.tickets = Tickets {
        remaining: start,
        start,
        bleed: [0.0; 2],
    };
}

/// Moves every flag according to who stands around it (BF2 `onCPTrigger` and
/// `onCPStatusChange`, evaluated continuously instead of on trigger events). Locked flags
/// (Breakthrough, outside the sector being fought over) stay as they are.
#[allow(clippy::too_many_arguments, clippy::type_complexity)]
fn update_flags(
    time: Res<Time>,
    round: Single<&RoundState>,
    mut tickets: Single<&mut Tickets>,
    mut control_points: Query<(Entity, &ControlPoint, &mut FlagState, &ControlPointRules), Without<Locked>>,
    // The critically wounded don't hold flags (BF2 `onPlayerKilledCQ`).
    soldiers: Query<(&SoldierMotion, &ControlledBy), (With<Soldier>, Without<Downed>)>,
    teams: Query<&Team>,
    mut scores: Query<&mut Score>,
    mut events: MessageWriter<ToClients<FlagEvent>>,
) {
    if **round != RoundState::Playing {
        return;
    }
    let dt = time.delta_secs();
    let occupants: Vec<(Vec3, Team, Entity)> = soldiers
        .iter()
        .filter_map(|(motion, controlled_by)| {
            let team = *teams.get(controlled_by.0).ok()?;
            Some((motion.position, team, controlled_by.0))
        })
        .collect();

    for (entity, cp, mut state, rules) in &mut control_points {
        if cp.uncapturable {
            continue;
        }
        let inside: Vec<&(Vec3, Team, Entity)> =
            occupants.iter().filter(|(feet, ..)| cp.contains(*feet)).collect();
        let count = |team: Team| inside.iter().filter(|(_, t, _)| *t == team).count() as f32;
        let (one, two) = (count(Team::One), count(Team::Two));
        let attacking = match one - two {
            d if d > 0.0 => Team::One,
            d if d < 0.0 => Team::Two,
            _ => Team::Spectator,
        };

        let mut next = *state;
        let (overweight, time_to_change) = if one == 0.0 && two == 0.0 {
            // Nobody here: slowly go back to the owner.
            let drift = if state.owner == Team::Spectator { -0.5 } else { 0.5 };
            (drift, rules.time_to_lose_control)
        } else if state.flag == attacking
            || (state.height <= 0.0 && state.owner == Team::Spectator)
        {
            ((one - two).abs(), rules.time_to_get_control)
        } else {
            // Someone else's flag is up: lower it first.
            (-(one - two).abs(), rules.time_to_lose_control)
        };
        let blocked = rules.only_takeable_by != Team::Spectator
            && attacking != Team::Spectator
            && attacking != rules.only_takeable_by;
        // The flag on the pole can only change while it is at the bottom.
        if state.height <= 0.0 && !blocked {
            next.flag = attacking;
        }
        next.rate = if time_to_change > 0.0 && !blocked { overweight / time_to_change } else { 0.0 };
        if (state.height >= 1.0 && next.rate > 0.0) || (state.height <= 0.0 && next.rate < 0.0) {
            next.rate = 0.0;
        }
        next.height = (state.height + next.rate * dt).clamp(0.0, 1.0);

        let reached_top = state.height < 1.0 && next.height >= 1.0;
        let reached_bottom = state.height > 0.0 && next.height <= 0.0;
        let mut takeover = None;
        if reached_bottom && state.owner != Team::Spectator {
            takeover = Some((FlagEventKind::Neutralized, state.owner.opponent()));
            next.owner = Team::Spectator;
        } else if reached_top && state.owner == Team::Spectator && next.flag != Team::Spectator {
            takeover = Some((FlagEventKind::Captured, next.flag));
            next.owner = next.flag;
        }
        if let Some((kind, team)) = takeover {
            for (_, _, player) in inside.iter().filter(|(_, t, _)| *t == team) {
                if let Ok(mut score) = scores.get_mut(*player) {
                    score.score += SCORE_CAPTURE;
                }
            }
            if kind == FlagEventKind::Captured
                && rules.enemy_ticket_loss_when_captured > 0.0
                && let Some(enemy) = team_index(team.opponent())
            {
                tickets.remaining[enemy] -= rules.enemy_ticket_loss_when_captured;
            }
            info!("{team:?} {kind:?} {}", cp.name);
            events.write(ToClients {
                targets: SendTargets::All,
                message: FlagEvent {
                    control_point: entity,
                    kind,
                    team,
                },
            });
        }
        state.set_if_neq(next);
    }
}

/// Deaths and area-value bleed (BF2 `updateTicketLoss`; none in team deathmatch), and the end
/// of the round.
#[allow(clippy::too_many_arguments)]
fn update_tickets(
    time: Res<Time>,
    level: Res<LoadedLevel>,
    mut deaths: MessageReader<Died>,
    match_state: Single<(&mut Tickets, &mut RoundState, &ModeState)>,
    control_points: Query<(&FlagState, &ControlPointRules)>,
    soldiers: Query<&ControlledBy, With<Soldier>>,
    teams: Query<&Team>,
    clock: Res<RoundClock>,
) {
    let (mut tickets, mut round, mode) = match_state.into_inner();
    if *round != RoundState::Playing {
        deaths.clear();
        return;
    }
    let mut next = *tickets;
    for death in deaths.read() {
        if let Some(team) = team_index(death.team) {
            next.remaining[team] -= 1.0;
        }
    }

    let mut area_value = [0.0f32; 2];
    let mut held = [0u32; 2];
    for (state, rules) in &control_points {
        if let Some(team) = team_index(state.owner) {
            area_value[team] += rules.area_value[team];
            held[team] += 1;
        }
    }
    let loss_per_minute = |team: usize| {
        level
            .desc
            .teams
            .get(team)
            .map(|t| t.ticket_loss_per_minute)
            .filter(|loss| *loss > 0.0)
            .unwrap_or(DEFAULT_TICKET_LOSS_PER_MINUTE)
    };
    let alive = |team: usize| {
        soldiers
            .iter()
            .any(|c| teams.get(c.0).ok().and_then(|t| team_index(*t)) == Some(team))
    };
    let has_points = !control_points.is_empty();
    let wiped_out = (0..2).find(|&team| has_points && held[team] == 0 && !alive(team));
    next.bleed = match wiped_out {
        _ if mode.kind == ModeKind::TeamDeathmatch => [0.0; 2],
        // No flags and nobody alive: the round is lost. (BF2 applies its "per minute"
        // end-of-round rate per second.)
        Some(team) => {
            let mut bleed = [0.0; 2];
            bleed[team] = level.desc.ticket_loss_at_end_per_minute;
            bleed
        }
        None => std::array::from_fn(|team| {
            let other = 1 - team;
            let overweight = area_value[other] - area_value[team];
            if area_value[other] >= 100.0 && overweight > 0.0 {
                loss_per_minute(team) / 60.0 * overweight / 100.0
            } else {
                0.0
            }
        }),
    };
    let dt = time.delta_secs();
    for team in 0..2 {
        next.remaining[team] = (next.remaining[team] - next.bleed[team] * dt).max(0.0);
    }
    tickets.set_if_neq(next);

    let out = [next.remaining[0] <= 0.0, next.remaining[1] <= 0.0];
    if out[0] || out[1] {
        let winner = match out {
            [true, true] => Team::Spectator,
            [true, false] => Team::Two,
            _ => Team::One,
        };
        end_round(&mut round, mode, &clock, winner, "out of tickets");
    }
}
