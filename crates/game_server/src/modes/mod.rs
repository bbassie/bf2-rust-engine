//! Game modes on the server, and the round every mode is played in.
//!
//! A mode is a unit of its own:
//!
//! | part | where |
//! |---|---|
//! | id, name, layout data and generated layouts | `game_data::modes` |
//! | what clients see | `game_shared::conquest` (flags, tickets, round, deployment: every mode) and `game_shared::modes` (`ModeState` on the match, Rush's charges, Breakthrough's sectors) |
//! | its rules | a module here: a plugin whose systems run [`in_mode`], and a [`ServerMode`] in [`MODES`] with the round's setup and the bots' strategy |
//! | the bots | [`ServerMode::objectives`]: what each team's commander sends squads to attack and defend (`ai::strategy`); bots arm and defuse charges themselves (`bots`) |
//! | HUD, deploy screen, maps, menu | `game_client::conquest_hud`, `mode_hud`, `deploy`, `menu::levels` read the replicated state |
//!
//! Every round: the layout's control points are spawned (every mode has them: flags to take,
//! or where each side spawns), the mode's [`ServerMode::setup`] adjusts them and sets the
//! tickets, and systems in [`ModeSystems`] play it until one of them calls [`end_round`].
//! After a break the next round starts, or the rotation's next map.
//!
//! Adding a mode: an id and label in `game_data::modes::ModeKind` (and layout generation if
//! the levels have no layouts for it), a module here with its plugin and a [`ServerMode`]
//! entry, and its HUD in the client.

pub mod breakthrough;
pub mod deathmatch;
pub mod rush;
pub mod staged;

use bevy::prelude::*;
use bevy_replicon::prelude::*;
use game_data::{GameModeDesc, modes::ModeKind};
use game_shared::{
    conquest::{ControlPoint, DeployRequest, Deployment, FlagState, RoundState, Tickets, team_from_id},
    level::LoadedLevel,
    modes::{Charge, Locked, ModeState, Sector},
    protocol::{MatchInfo, Player, Score, Team},
    soldier::Soldier,
};

use crate::{
    ClientPlayer, Controls, HostPlayer, RespawnTimer, ServerSimSystems,
    ai::strategy::{Objective, PlanView, Posture},
    conquest::{self, ControlPointRules},
    sender_player,
};

/// Seconds between the end of a round and the next one.
const ROUND_BREAK: f32 = 20.0;

/// A game mode on the server: what it adds to the round every mode shares.
pub struct ServerMode {
    pub kind: ModeKind,
    /// Sets up a new round: the tickets, the control points' owners and locks, and anything
    /// else the mode spawns (Rush's charges, with the `Commands`).
    pub setup: fn(&mut Commands, &mut RoundSetup),
    /// The bots' strategy: what `team` should attack and defend, most valuable first.
    pub objectives: fn(&PlanView, Team) -> (Posture, Vec<Objective>),
}

/// Every mode the server plays.
pub const MODES: &[ServerMode] = &[
    ServerMode {
        kind: ModeKind::Conquest,
        setup: |_, setup| conquest::setup(setup),
        objectives: crate::ai::strategy::conquest_objectives,
    },
    ServerMode {
        kind: ModeKind::Coop,
        setup: |_, setup| conquest::setup(setup),
        objectives: crate::ai::strategy::conquest_objectives,
    },
    ServerMode {
        kind: ModeKind::Rush,
        setup: rush::setup,
        objectives: rush::objectives,
    },
    ServerMode {
        kind: ModeKind::Breakthrough,
        setup: breakthrough::setup,
        objectives: breakthrough::objectives,
    },
    ServerMode {
        kind: ModeKind::TeamDeathmatch,
        setup: deathmatch::setup,
        objectives: deathmatch::objectives,
    },
];

/// The server side of a mode.
pub fn server_mode(kind: ModeKind) -> &'static ServerMode {
    MODES.iter().find(|m| m.kind == kind).unwrap_or(&MODES[0])
}

/// The rules a match is played by: its mode's, or conquest's when the layout lacks what the
/// mode needs (a staged mode on a level it couldn't make a layout for plays its conquest one).
pub fn effective_kind(layout: Option<&GameModeDesc>, mode: &str) -> ModeKind {
    let kind = ModeKind::of(mode);
    let fits = layout.is_some_and(|l| ModeKind::of(&l.mode) == kind && (!kind.staged() || l.staged.is_some()));
    if fits || !kind.staged() { kind } else { ModeKind::Conquest }
}

/// Ordering of the mode systems in `FixedUpdate`, after inputs, combat and abilities.
#[derive(SystemSet, Debug, Clone, PartialEq, Eq, Hash)]
pub enum ModeSystems {
    /// Flags move, charges are armed and defused, stages are taken.
    Objectives,
    /// Tickets are counted and the round may end.
    Tickets,
    /// The break between rounds, and the next one.
    Round,
}

/// Runs a system only while a mode of `kinds` is played.
pub fn in_modes(kinds: &'static [ModeKind]) -> impl FnMut(Query<&ModeState>) -> bool + Clone {
    move |states: Query<&ModeState>| states.iter().next().is_some_and(|s| kinds.contains(&s.kind))
}

/// Runs a system only while `kind` is played.
pub fn in_mode(kind: ModeKind) -> impl FnMut(Query<&ModeState>) -> bool + Clone {
    move |states: Query<&ModeState>| states.iter().next().is_some_and(|s| s.kind == kind)
}

pub struct ModesPlugin;

impl Plugin for ModesPlugin {
    fn build(&self, app: &mut App) {
        app.init_resource::<RoundClock>()
            .add_message::<RoundReset>()
            .configure_sets(
                FixedUpdate,
                (ModeSystems::Objectives, ModeSystems::Tickets, ModeSystems::Round)
                    .chain()
                    .after(ServerSimSystems::ApplyInputs)
                    .after(crate::combat::CombatSystems)
                    .after(crate::abilities::AbilitySystems)
                    .run_if(resource_exists::<LoadedLevel>)
                    .run_if(in_state(ClientState::Disconnected)),
            )
            .add_plugins((
                conquest::ConquestPlugin,
                staged::StagedPlugin,
                rush::RushPlugin,
                breakthrough::BreakthroughPlugin,
            ))
            .add_systems(
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
            .add_systems(FixedUpdate, (tick_clock, next_round).chain().in_set(ModeSystems::Round));
    }
}

/// How long the round and its stage have been going, for the logs.
#[derive(Resource, Default, Debug)]
pub struct RoundClock {
    pub round: f32,
    pub stage: f32,
}

/// Sent once a round starts over on the same level: after the break, or a round ended early
/// by an admin's `/tickets` (unlike `/restart` and `/map`, which reload the level and go
/// through the level-teardown path below instead). Fresh flags, tickets and spawns.
///
/// This is the round half of the two lifecycle hooks modules clean up on: a full map change
/// or leaving to the menu instead despawns everything tagged [`game_shared::level::LevelEntity`]
/// (`game_shared::level::load_level_for_match`, and `net::leave_match` on the client), which
/// modules also see through `resource_exists_and_changed::<LoadedLevel>` /
/// `resource_removed::<LoadedLevel>`. A round reset keeps the level and its `LevelEntity`s, so
/// per-round state that isn't level-scoped (the commander's assets and recharge, per-round
/// `Local` caches keyed by entity) needs this event instead.
#[derive(Message, Clone, Copy, Debug)]
pub struct RoundReset;

fn tick_clock(time: Res<Time>, rounds: Query<&RoundState>, mut clock: ResMut<RoundClock>) {
    if rounds.iter().next() == Some(&RoundState::Playing) {
        clock.round += time.delta_secs();
        clock.stage += time.delta_secs();
    }
}

/// What a new round is set up from; a mode's [`ServerMode::setup`] adjusts it.
pub struct RoundSetup<'a> {
    pub level: &'a LoadedLevel,
    pub layout: Option<&'a GameModeDesc>,
    /// The size asked for (the layout may be of another).
    pub size: u32,
    pub match_entity: Entity,
    /// The layout's control points, in its order, to be spawned after the setup.
    pub points: Vec<PointSetup>,
    pub tickets: Tickets,
    pub state: ModeState,
}

/// A control point to spawn.
pub struct PointSetup {
    pub point: ControlPoint,
    pub flag: FlagState,
    pub rules: ControlPointRules,
    /// Breakthrough: its sector.
    pub sector: Option<u8>,
    /// Its flag can't move for now.
    pub locked: bool,
}

impl PointSetup {
    /// Gives the point to `owner` (its flag at the top), or makes it neutral.
    pub fn set_owner(&mut self, owner: Team) {
        self.flag = FlagState::held_by(owner);
    }
}

fn start_first_round(
    mut commands: Commands,
    level: Res<LoadedLevel>,
    match_info: Single<(Entity, &MatchInfo)>,
    old: Query<Entity, Or<(With<ControlPoint>, With<Charge>)>>,
    mut clock: ResMut<RoundClock>,
) {
    let (match_entity, match_info) = *match_info;
    start_round(&mut commands, &level, match_entity, match_info, &old, &mut clock);
}

/// Resets the control points and tickets to the layout's start, and lets the mode set up
/// the rest.
fn start_round(
    commands: &mut Commands,
    level: &LoadedLevel,
    match_entity: Entity,
    match_info: &MatchInfo,
    old: &Query<Entity, Or<(With<ControlPoint>, With<Charge>)>>,
    clock: &mut RoundClock,
) {
    for entity in old {
        commands.entity(entity).despawn();
    }
    let layout = level.game_mode(&match_info.mode, match_info.size);
    let kind = effective_kind(layout, &match_info.mode);
    let points = layout
        .iter()
        .flat_map(|l| l.control_points.iter())
        .enumerate()
        .map(|(index, cp)| PointSetup {
            point: ControlPoint {
                index: index as u8,
                name: cp.name.clone(),
                position: Vec3::from_array(cp.position),
                radius: cp.radius,
                uncapturable: cp.uncapturable,
            },
            flag: FlagState::held_by(team_from_id(cp.initial_team)),
            rules: ControlPointRules::from_desc(cp),
            sector: None,
            locked: false,
        })
        .collect();
    let mut setup = RoundSetup {
        level,
        layout,
        size: match_info.size,
        match_entity,
        points,
        tickets: Tickets::default(),
        state: ModeState {
            kind,
            ..default()
        },
    };
    (server_mode(kind).setup)(commands, &mut setup);
    for point in setup.points {
        let mut entity = commands.spawn((point.point, point.flag, point.rules, Replicated));
        if let Some(sector) = point.sector {
            entity.insert(Sector(sector));
        }
        if point.locked {
            entity.insert(Locked);
        }
    }
    commands.entity(match_entity).insert((setup.tickets, RoundState::Playing, setup.state));
    *clock = RoundClock::default();
    let tickets = setup.tickets.start;
    match setup.state.staged() {
        true => info!(
            "round started: {} {}, {} stages, {:?} attacking with {} tickets",
            match_info.level,
            kind.label(),
            setup.state.stages,
            setup.state.attacker,
            tickets.iter().copied().fold(0.0, f32::max)
        ),
        false => info!("round started: {} {}, tickets {} / {}", match_info.level, kind.label(), tickets[0], tickets[1]),
    }
}

/// Ends the round with `winner` (`Spectator`: a draw), logging why, after how long and how
/// far it got.
pub fn end_round(round: &mut RoundState, mode: &ModeState, clock: &RoundClock, winner: Team, why: &str) {
    let stage = if mode.staged() {
        format!(", {} of {} reached", mode.stage_label(), mode.stages)
    } else {
        String::new()
    };
    info!(
        "round over, winner: {winner:?} ({} {why} after {:.0} s{stage})",
        mode.kind.label(),
        clock.round
    );
    *round = RoundState::Ended {
        winner,
        restart_in: ROUND_BREAK,
    };
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
            // Replicated: only a real change goes out.
            let wanted = Deployment {
                kit: request.message.kit.min(15),
                control_point: request.message.control_point,
                on_squad_leader: request.message.on_squad_leader,
                ..*deployment
            };
            deployment.set_if_neq(wanted);
        }
    }
}

/// After the break: everyone back to the start, with fresh flags and tickets.
#[allow(clippy::too_many_arguments, clippy::type_complexity)]
fn next_round(
    mut commands: Commands,
    time: Res<Time>,
    level: Res<LoadedLevel>,
    match_state: Single<(Entity, &MatchInfo, &mut RoundState, Option<&ModeState>)>,
    old: Query<Entity, Or<(With<ControlPoint>, With<Charge>)>>,
    soldiers: Query<Entity, With<Soldier>>,
    mut players: Query<(Entity, &mut Score, &mut Deployment), With<Player>>,
    mut remaining: Local<Option<f32>>,
    mut clock: ResMut<RoundClock>,
    rotation: Res<crate::rotation::MapRotation>,
    mut reset: MessageWriter<RoundReset>,
) {
    let (match_entity, match_info, mut round, _) = match_state.into_inner();
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
    // A server with a map rotation plays the next map instead.
    if rotation.moves_on() {
        commands.queue(crate::rotation::advance);
        return;
    }
    for soldier in &soldiers {
        commands.entity(soldier).despawn();
    }
    for (player, mut score, mut deployment) in &mut players {
        commands.entity(player).remove::<(Controls, RespawnTimer)>();
        *score = Score::default();
        deployment.respawn_in = 0.0;
    }
    reset.write(RoundReset);
    start_round(&mut commands, &level, match_entity, match_info, &old, &mut clock);
}

#[cfg(test)]
mod tests {
    use super::*;
    use game_data::modes::{self, RUSH};

    #[test]
    fn staged_modes_need_their_layout() {
        let conquest = GameModeDesc {
            mode: modes::CONQUEST.into(),
            ..default()
        };
        // Asked for Rush, got the conquest layout (the level had nothing to make one from).
        assert_eq!(effective_kind(Some(&conquest), RUSH), ModeKind::Conquest);
        assert_eq!(effective_kind(Some(&conquest), "gpm_ctf"), ModeKind::Conquest);
        assert_eq!(effective_kind(Some(&conquest), "coop"), ModeKind::Coop);
        let rush = GameModeDesc {
            mode: RUSH.into(),
            staged: Some(modes::StagedDesc {
                attacker: 2,
                tickets: 10.0,
                stages: Vec::new(),
                arm_seconds: 1.0,
                defuse_seconds: 1.0,
                fuse_seconds: 1.0,
            }),
            ..default()
        };
        assert_eq!(effective_kind(Some(&rush), "rush"), ModeKind::Rush);
    }
}
