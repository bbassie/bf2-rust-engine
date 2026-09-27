//! Conquest rules, after BF2's `gpm_cq`: whichever team has more soldiers inside a control
//! point's radius lowers the enemy flag and raises its own, a team bleeds tickets while the
//! enemy holds more area value, every death costs a ticket, and the round ends when a team
//! runs out. A new round starts after a short break.

use bevy::prelude::*;
use bevy_replicon::prelude::*;
use game_shared::{
    conquest::{
        ControlPoint, DeployRequest, Deployment, FlagEvent, FlagEventKind, FlagState, RoundState,
        Tickets, team_from_id, team_index,
    },
    level::LoadedLevel,
    protocol::{ControlledBy, MatchInfo, Player, Score, Team},
    soldier::{Soldier, SoldierMotion},
};

use crate::{
    ClientPlayer, Controls, HostPlayer, RespawnTimer, ServerSimSystems, combat::Died,
    sender_player,
};

/// Seconds between the end of a round and the next one.
const ROUND_BREAK: f32 = 20.0;
/// Tickets per team when the level doesn't say.
const DEFAULT_TICKETS: f32 = 100.0;
const DEFAULT_TICKET_LOSS_PER_MINUTE: f32 = 10.0;
/// Score for everyone of the team inside when a flag is captured or neutralized.
const SCORE_CAPTURE: i32 = 2;

pub struct ConquestPlugin;

impl Plugin for ConquestPlugin {
    fn build(&self, app: &mut App) {
        app.add_systems(
            PreUpdate,
            receive_deploy_requests
                .after(ServerSystems::Receive)
                .run_if(in_state(ClientState::Disconnected)),
        )
        .add_systems(
            Update,
            start_first_round
                .run_if(resource_exists_and_changed::<LoadedLevel>)
                .run_if(in_state(ClientState::Disconnected)),
        )
        .add_systems(
            FixedUpdate,
            (update_flags, update_tickets, next_round)
                .chain()
                .after(ServerSimSystems::ApplyInputs)
                .after(crate::combat::CombatSystems)
                .run_if(resource_exists::<LoadedLevel>)
                .run_if(in_state(ClientState::Disconnected)),
        );
    }
}

/// Server-only rules of a control point, next to its [`ControlPoint`].
#[derive(Component, Clone, Debug)]
pub struct ControlPointRules {
    /// Layout id; spawn points refer to it.
    pub id: String,
    area_value: [f32; 2],
    time_to_get_control: f32,
    time_to_lose_control: f32,
    only_takeable_by: Team,
    enemy_ticket_loss_when_captured: f32,
}

fn start_first_round(
    mut commands: Commands,
    level: Res<LoadedLevel>,
    match_info: Single<(Entity, &MatchInfo)>,
    control_points: Query<Entity, With<ControlPoint>>,
) {
    let (match_entity, match_info) = *match_info;
    start_round(&mut commands, &level, match_entity, match_info, &control_points);
}

/// Resets control points and tickets to the layout's starting state.
fn start_round(
    commands: &mut Commands,
    level: &LoadedLevel,
    match_entity: Entity,
    match_info: &MatchInfo,
    control_points: &Query<Entity, With<ControlPoint>>,
) {
    for entity in control_points {
        commands.entity(entity).despawn();
    }
    let layout = level.game_mode(&match_info.mode, match_info.size);
    for (index, cp) in layout.iter().flat_map(|l| l.control_points.iter()).enumerate() {
        let owner = team_from_id(cp.initial_team);
        commands.spawn((
            ControlPoint {
                index: index as u8,
                name: cp.name.clone(),
                position: Vec3::from_array(cp.position),
                radius: cp.radius,
                uncapturable: cp.uncapturable,
            },
            FlagState {
                owner,
                flag: owner,
                height: if owner == Team::Spectator { 0.0 } else { 1.0 },
                rate: 0.0,
            },
            ControlPointRules {
                id: cp.id.clone(),
                area_value: cp.area_value,
                time_to_get_control: cp.time_to_get_control,
                time_to_lose_control: cp.time_to_lose_control,
                only_takeable_by: team_from_id(cp.only_takeable_by_team),
                enemy_ticket_loss_when_captured: cp.enemy_ticket_loss_when_captured,
            },
            Replicated,
        ));
    }

    let start = std::array::from_fn(|team| {
        level
            .desc
            .teams
            .get(team)
            .and_then(|t| t.tickets.iter().min_by_key(|(size, _)| size.abs_diff(match_info.size)))
            .map_or(DEFAULT_TICKETS, |(_, tickets)| *tickets as f32)
    });
    commands.entity(match_entity).insert((
        Tickets {
            remaining: start,
            start,
            bleed: [0.0; 2],
        },
        RoundState::Playing,
    ));
    info!("round started: tickets {} / {}", start[0], start[1]);
}

fn receive_deploy_requests(
    mut requests: MessageReader<FromClient<DeployRequest>>,
    clients: Query<&ClientPlayer>,
    host: Option<Res<HostPlayer>>,
    mut players: Query<&mut Deployment>,
) {
    for request in requests.read() {
        let Some(player) = sender_player(request.client_id, &clients, host.as_deref()) else {
            continue;
        };
        if let Ok(mut deployment) = players.get_mut(player) {
            deployment.kit = request.message.kit.min(15);
            deployment.control_point = request.message.control_point;
        }
    }
}

/// Moves every flag according to who stands around it (BF2 `onCPTrigger` and
/// `onCPStatusChange`, evaluated continuously instead of on trigger events).
#[allow(clippy::too_many_arguments)]
fn update_flags(
    time: Res<Time>,
    round: Single<&RoundState>,
    mut tickets: Single<&mut Tickets>,
    mut control_points: Query<(Entity, &ControlPoint, &mut FlagState, &ControlPointRules)>,
    soldiers: Query<(&SoldierMotion, &ControlledBy), With<Soldier>>,
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

/// Deaths and area-value bleed (BF2 `updateTicketLoss`), and the end of the round.
#[allow(clippy::too_many_arguments)]
fn update_tickets(
    time: Res<Time>,
    level: Res<LoadedLevel>,
    mut deaths: MessageReader<Died>,
    match_state: Single<(&mut Tickets, &mut RoundState)>,
    control_points: Query<(&FlagState, &ControlPointRules)>,
    soldiers: Query<&ControlledBy, With<Soldier>>,
    teams: Query<&Team>,
) {
    let (mut tickets, mut round) = match_state.into_inner();
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
        info!("round over, winner: {winner:?}");
        *round = RoundState::Ended {
            winner,
            restart_in: ROUND_BREAK,
        };
    }
}

/// After the break: everyone back to the start, with fresh flags and tickets.
#[allow(clippy::too_many_arguments, clippy::type_complexity)]
fn next_round(
    mut commands: Commands,
    time: Res<Time>,
    level: Res<LoadedLevel>,
    match_state: Single<(Entity, &MatchInfo, &mut RoundState)>,
    control_points: Query<Entity, With<ControlPoint>>,
    soldiers: Query<Entity, With<Soldier>>,
    mut players: Query<(Entity, &mut Score, &mut Deployment), With<Player>>,
    mut remaining: Local<Option<f32>>,
) {
    let (match_entity, match_info, mut round) = match_state.into_inner();
    let RoundState::Ended { winner, restart_in } = *round else {
        *remaining = None;
        return;
    };
    let left = remaining.get_or_insert(restart_in);
    *left -= time.delta_secs();
    if *left > 0.0 {
        // Replicate whole seconds only.
        if left.ceil() != restart_in {
            *round = RoundState::Ended {
                winner,
                restart_in: left.ceil(),
            };
        }
        return;
    }
    *remaining = None;
    for soldier in &soldiers {
        commands.entity(soldier).despawn();
    }
    for (player, mut score, mut deployment) in &mut players {
        commands.entity(player).remove::<(Controls, RespawnTimer)>();
        *score = Score::default();
        deployment.respawn_in = 0.0;
    }
    start_round(&mut commands, &level, match_entity, match_info, &control_points);
}
