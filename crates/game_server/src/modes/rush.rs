//! Rush: each stage has a pair of charges ("M-COM stations"). An attacker arms one by holding
//! the use key at it for the layout's `arm_seconds`; a defender defuses an armed one the same
//! way (`defuse_seconds`); an armed charge goes off after `fuse_seconds`, hurting everyone
//! around it. Once both of a stage's charges are destroyed the front moves: the attackers get
//! their tickets back and both sides spawn at the next stage's control points.
//!
//! Nothing is captured: control points only say where each side spawns during a stage (the
//! layout's `attacker_spawns` and `defender_spawns`).

use std::sync::Arc;

use bevy::prelude::*;
use bevy_replicon::prelude::*;
use game_data::modes::{ModeKind, StageDesc};
use game_shared::{
    conquest::{FlagState, RoundState, Tickets, team_from_id},
    effects::PlayEffect,
    input::Buttons,
    level::{Heightmap, LoadedLevel},
    modes::{Charge, ChargeState, ChargeTimes, ModeState, ObjectiveEvent, ObjectiveEventKind},
    protocol::{ControlledBy, MatchInfo, Score, Team},
    revive::Downed,
    soldier::{Soldier, SoldierMotion},
    vehicle::Seated,
};

use super::{ModeSystems, RoundClock, RoundSetup, in_mode, staged};
use crate::{
    AppliedInput,
    ai::strategy::{Objective, OrderKind, PlanView, Posture, walk_region},
    combat::{Attacker, Explosion},
    conquest::ControlPointRules,
    nav::{NavGrid, Navigation},
};

/// Score for arming or defusing a charge, and for the one who armed it when it goes off.
const SCORE_ARM: i32 = 2;
const SCORE_DEFUSE: i32 = 2;
const SCORE_DESTROY: i32 = 4;
/// The blast of a charge going off: damage at its middle and radius.
const BLAST_DAMAGE: f32 = 300.0;
const BLAST_RADIUS: f32 = 12.0;
const BLAST_EFFECT: &str = "e_exp_xlarge";

pub struct RushPlugin;

impl Plugin for RushPlugin {
    fn build(&self, app: &mut App) {
        app.add_systems(
            FixedUpdate,
            (update_charges, advance)
                .chain()
                .in_set(ModeSystems::Objectives)
                .run_if(in_mode(ModeKind::Rush)),
        )
        .add_systems(
            Update,
            place_charges
                .run_if(resource_exists::<Navigation>)
                .run_if(in_state(ClientState::Disconnected)),
        );
    }
}

/// Server-side state of a charge, next to its [`Charge`].
#[derive(Component, Debug, Default)]
pub struct ChargeRules {
    /// Arming or defusing progress and the fuse, unrounded (the replicated state is rounded).
    progress: f32,
    fuse: f32,
    /// The attacker who armed it, for the score.
    armed_by: Option<Entity>,
    /// Placed by the layout generator near `near` (its control point): still to be moved
    /// onto walkable ground.
    approximate: bool,
    near: Option<Vec3>,
}

/// The round: the attackers' tickets, every stage's charges (the first stage's in play), and
/// the first stage's spawns.
pub(crate) fn setup(commands: &mut Commands, setup: &mut RoundSetup) {
    let Some(staged) = setup.layout.and_then(|l| l.staged.as_ref()) else {
        return;
    };
    let attacker = team_from_id(staged.attacker);
    setup.state.attacker = attacker;
    setup.state.stages = staged.stages.len() as u8;
    setup.tickets = staged::attacker_tickets_at_start(attacker, staged.tickets);
    commands.entity(setup.match_entity).insert(ChargeTimes {
        arm: staged.arm_seconds.max(0.1),
        defuse: staged.defuse_seconds.max(0.1),
        fuse: staged.fuse_seconds.max(1.0),
    });
    let mut index = 0u8;
    for (s, stage) in staged.stages.iter().enumerate() {
        for charge in &stage.charges {
            let near = charge
                .control_point
                .as_ref()
                .and_then(|id| setup.points.iter().find(|p| &p.rules.id == id))
                .map(|p| p.point.position);
            commands.spawn((
                Charge {
                    index,
                    stage: s as u8,
                    name: charge.name.clone(),
                    position: Vec3::from_array(charge.position),
                    yaw: charge.yaw.to_radians(),
                    template: charge.template.clone(),
                },
                if s == 0 { ChargeState::Active { progress: 0.0 } } else { ChargeState::Waiting },
                ChargeRules {
                    approximate: charge.approximate,
                    near,
                    ..default()
                },
                Replicated,
            ));
            index = index.saturating_add(1);
        }
    }
    for point in &mut setup.points {
        point.point.uncapturable = true;
        if let Some(stage) = staged.stages.first() {
            let owner = spawn_owner(stage, attacker, &point.rules.id, point.flag.owner);
            point.set_owner(owner);
        }
    }
}

/// Who spawns at control point `id` during `stage`: listed for the attackers or the
/// defenders, else nobody. A stage that lists nobody keeps the points' owners.
fn spawn_owner(stage: &StageDesc, attacker: Team, id: &str, current: Team) -> Team {
    let listed = |list: &[String]| list.iter().any(|s| s == id);
    if stage.attacker_spawns.is_empty() && stage.defender_spawns.is_empty() {
        current
    } else if listed(&stage.attacker_spawns) {
        attacker
    } else if listed(&stage.defender_spawns) {
        attacker.opponent()
    } else {
        Team::Spectator
    }
}

/// Rounded for replication: progress in steps of 2 %, the fuse in tenths of a second.
fn rounded_progress(progress: f32) -> f32 {
    (progress * 50.0).floor() / 50.0
}

fn rounded_fuse(fuse: f32) -> f32 {
    (fuse * 10.0).ceil() / 10.0
}

/// Arming, defusing and fuses: soldiers on foot holding the use key within reach of a charge
/// of the current stage work on it.
#[allow(clippy::too_many_arguments, clippy::type_complexity)]
fn update_charges(
    time: Res<Time>,
    match_state: Single<(&ModeState, &RoundState, &ChargeTimes)>,
    mut charges: Query<(Entity, &Charge, &mut ChargeState, &mut ChargeRules)>,
    soldiers: Query<(&SoldierMotion, &ControlledBy, &AppliedInput), (With<Soldier>, Without<Downed>, Without<Seated>)>,
    teams: Query<&Team>,
    mut scores: Query<&mut Score>,
    mut events: MessageWriter<ToClients<ObjectiveEvent>>,
    mut explosions: MessageWriter<Explosion>,
    mut effects: MessageWriter<ToClients<PlayEffect>>,
) {
    let (mode, round, times) = *match_state;
    if *round != RoundState::Playing {
        return;
    }
    let dt = time.delta_secs();
    let mut event = |kind: ObjectiveEventKind, charge: Entity, team: Team| {
        events.write(ToClients {
            targets: SendTargets::All,
            message: ObjectiveEvent {
                kind,
                charge: Some(charge),
                team,
                stage: mode.stage,
            },
        });
    };
    let mut score = |player: Option<Entity>, points: i32| {
        if let Some(mut score) = player.and_then(|p| scores.get_mut(p).ok()) {
            score.score += points;
        }
    };
    for (entity, charge, mut state, mut rules) in &mut charges {
        if charge.stage != mode.stage || !state.in_play() {
            continue;
        }
        // A player of `team` holding the use key at it.
        let worker = |team: Team| {
            soldiers
                .iter()
                .filter(|(motion, _, applied)| applied.0.pressed(Buttons::USE) && charge.in_reach(motion.position))
                .map(|(_, controlled_by, _)| controlled_by.0)
                .find(|player| teams.get(*player).is_ok_and(|t| *t == team))
        };
        let next = match *state {
            ChargeState::Active { .. } => {
                match worker(mode.attacker) {
                    Some(player) => {
                        if rules.progress == 0.0 {
                            info!("rush: {:?} arming charge {} of {}", mode.attacker, charge.name, mode.stage_label());
                        }
                        rules.progress += dt / times.arm;
                        rules.armed_by = Some(player);
                    }
                    None => rules.progress = (rules.progress - dt / times.arm).max(0.0),
                }
                if rules.progress >= 1.0 {
                    rules.progress = 0.0;
                    rules.fuse = times.fuse;
                    score(rules.armed_by, SCORE_ARM);
                    event(ObjectiveEventKind::Armed, entity, mode.attacker);
                    info!("rush: charge {} of {} armed", charge.name, mode.stage_label());
                    ChargeState::Armed {
                        fuse: rounded_fuse(rules.fuse),
                        progress: 0.0,
                    }
                } else {
                    ChargeState::Active {
                        progress: rounded_progress(rules.progress),
                    }
                }
            }
            ChargeState::Armed { .. } => {
                rules.fuse -= dt;
                let defuser = worker(mode.defender());
                match defuser {
                    Some(_) => {
                        if rules.progress == 0.0 {
                            info!("rush: {:?} defusing charge {} of {} ({:.0} s left)", mode.defender(), charge.name, mode.stage_label(), rules.fuse);
                        }
                        rules.progress += dt / times.defuse;
                    }
                    None => rules.progress = (rules.progress - dt / times.defuse).max(0.0),
                }
                if rules.progress >= 1.0 {
                    rules.progress = 0.0;
                    score(defuser, SCORE_DEFUSE);
                    event(ObjectiveEventKind::Defused, entity, mode.defender());
                    info!("rush: charge {} of {} defused", charge.name, mode.stage_label());
                    ChargeState::Active { progress: 0.0 }
                } else if rules.fuse <= 0.0 {
                    score(rules.armed_by, SCORE_DESTROY);
                    event(ObjectiveEventKind::Destroyed, entity, mode.attacker);
                    info!("rush: charge {} of {} destroyed", charge.name, mode.stage_label());
                    let at = charge.position + Vec3::Y * 0.6;
                    explosions.write(Explosion {
                        position: at,
                        damage: BLAST_DAMAGE,
                        radius: BLAST_RADIUS,
                        material: 0,
                        attacker: Attacker {
                            player: rules.armed_by,
                            soldier: None,
                            weapon: Arc::from("M-COM charge"),
                        },
                        cone: None,
                    });
                    effects.write(ToClients {
                        targets: SendTargets::All,
                        message: PlayEffect::new(BLAST_EFFECT, charge.position),
                    });
                    ChargeState::Destroyed
                } else {
                    ChargeState::Armed {
                        fuse: rounded_fuse(rules.fuse),
                        progress: rounded_progress(rules.progress),
                    }
                }
            }
            other => other,
        };
        state.set_if_neq(next);
    }
}

/// Once every charge of the stage is destroyed: the next stage's charges come into play and
/// both sides spawn further on (or the round is over).
#[allow(clippy::too_many_arguments, clippy::type_complexity)]
fn advance(
    level: Res<LoadedLevel>,
    match_state: Single<(&MatchInfo, &mut ModeState, &mut Tickets, &mut RoundState)>,
    mut charges: Query<(&Charge, &mut ChargeState, &mut ChargeRules)>,
    mut points: Query<(&ControlPointRules, &mut FlagState)>,
    mut clock: ResMut<RoundClock>,
    mut events: MessageWriter<ToClients<ObjectiveEvent>>,
) {
    let (info, mut mode, mut tickets, mut round) = match_state.into_inner();
    if *round != RoundState::Playing {
        return;
    }
    // A stage with no charges of its own (a hand-written layout that skips a pair) counts as
    // already taken instead of stalling the round forever.
    let mut current = charges.iter().filter(|(c, ..)| c.stage == mode.stage).peekable();
    if current.peek().is_some() && current.any(|(_, state, _)| *state != ChargeState::Destroyed) {
        return;
    }
    if !staged::take_stage(&mut mode, &mut tickets, &mut round, &mut clock, &mut events) {
        return;
    }
    for (charge, mut state, mut rules) in &mut charges {
        if charge.stage == mode.stage {
            *state = ChargeState::Active { progress: 0.0 };
            rules.progress = 0.0;
        }
    }
    let stage = level
        .game_mode(&info.mode, info.size)
        .and_then(|l| l.staged.as_ref())
        .and_then(|s| s.stages.get(mode.stage as usize));
    if let Some(stage) = stage {
        for (rules, mut flag) in &mut points {
            let owner = spawn_owner(stage, mode.attacker, &rules.id, flag.owner);
            flag.set_if_neq(FlagState::held_by(owner));
        }
    }
}

/// Moves generated charges onto walkable ground once the navigation grid is there (see
/// [`walkable_spot`]).
fn place_charges(nav: Res<Navigation>, level: Res<LoadedLevel>, mut charges: Query<(&mut Charge, &mut ChargeRules)>) {
    let (mut placed, mut moved, mut stuck) = (0, 0.0, 0);
    for (mut charge, mut rules) in &mut charges {
        if !rules.approximate {
            continue;
        }
        rules.approximate = false;
        match walkable_spot(&nav.0, level.heightmap.as_deref(), charge.position, rules.near) {
            Some(spot) => {
                debug!(
                    "rush: charge {} of stage {} at {spot:.1} ({:.1} m from where the layout put it)",
                    charge.name,
                    charge.stage + 1,
                    spot.distance(charge.position)
                );
                placed += 1;
                moved += spot.distance(charge.position);
                charge.position = spot;
            }
            None => stuck += 1,
        }
    }
    if placed + stuck > 0 {
        info!(
            "rush: {placed} charges placed on walkable ground ({:.1} m from the layout's spots on average), {stuck} without any near",
            moved / placed.max(1) as f32
        );
    }
}

/// A spot for a charge near `wanted`: on walkable ground in the region of the navigation grid
/// its control point (`near`) is in, so both sides can walk to it; with room around it (1.5 m
/// from walls and ledges) and at ground level rather than on a roof or a wall; as close to
/// `wanted` as that allows, within 16 m.
pub fn walkable_spot(grid: &NavGrid, heightmap: Option<&Heightmap>, wanted: Vec3, near: Option<Vec3>) -> Option<Vec3> {
    let region = walk_region(grid, near.unwrap_or(wanted));
    let mut best: Option<(f32, Vec3)> = None;
    for cell in grid.cells_near(wanted.xz(), 16.0) {
        let c = grid.cell(cell);
        if region.is_some_and(|r| r != c.region) {
            continue;
        }
        let spot = grid.position(cell);
        let mut score = spot.xz().distance(wanted.xz());
        // `dist` is in quarter meters.
        score += 1.5 * (6.0 - c.dist as f32).max(0.0);
        if let Some(heightmap) = heightmap
            && spot.y - heightmap.height_at(spot.x, spot.z) > 1.5
        {
            score += 12.0;
        }
        if best.is_none_or(|(b, _)| score < b) {
            best = Some((score, spot));
        }
    }
    best.map(|(_, spot)| spot)
}

/// The bots' Rush strategy: the attackers go for the charges to arm (both at once, spread by
/// the commander's crowding rule) and guard the armed ones; the defenders guard both, and
/// every armed one comes first (to defuse it).
pub(crate) fn objectives(view: &PlanView, team: Team) -> (Posture, Vec<Objective>) {
    let attacking = team == view.mode.attacker;
    let mut objectives = Vec::new();
    for (a, area) in view.map.areas.iter().enumerate() {
        let Some(state) = view.charges.get(a).copied().flatten() else {
            continue;
        };
        let threat = view.intel.enemies_near(team, area.position, 40.0) as f32;
        let (kind, value) = match (attacking, state) {
            (true, ChargeState::Active { progress }) => (OrderKind::Attack, 12.0 * (1.0 + progress) / (1.0 + 0.1 * threat)),
            (true, ChargeState::Armed { .. }) => (OrderKind::Defend, 16.0),
            (false, ChargeState::Active { progress }) => {
                (OrderKind::Defend, 10.0 + 1.5 * threat.min(4.0) + if progress > 0.0 { 12.0 } else { 0.0 })
            }
            (false, ChargeState::Armed { .. }) => (OrderKind::Defend, 25.0),
            _ => continue,
        };
        objectives.push(Objective { area: a, kind, value });
    }
    objectives.sort_by(|a, b| b.value.total_cmp(&a.value));
    (if attacking { Posture::Attack } else { Posture::Guard }, objectives)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ai::strategy::{Area, StrategicMap, TeamIntel};

    fn area(name: &str, charge: Option<u8>) -> Area {
        Area {
            name: name.into(),
            control_point: None,
            charge,
            position: Vec3::ZERO,
            order_position: Vec3::ZERO,
            radius: 6.0,
            uncapturable: false,
            has_spawns: false,
            neighbours: Vec::new(),
            routes: Vec::new(),
        }
    }

    #[test]
    fn spawns_follow_the_stage() {
        let stage = StageDesc {
            attacker_spawns: vec!["base".into(), "hotel".into()],
            defender_spawns: vec!["market".into()],
            ..default()
        };
        assert_eq!(spawn_owner(&stage, Team::Two, "hotel", Team::One), Team::Two);
        assert_eq!(spawn_owner(&stage, Team::Two, "market", Team::Spectator), Team::One);
        assert_eq!(spawn_owner(&stage, Team::Two, "square", Team::One), Team::Spectator);
        // A stage listing nobody keeps the points as they are.
        assert_eq!(spawn_owner(&StageDesc::default(), Team::Two, "square", Team::One), Team::One);
    }

    #[test]
    fn attackers_arm_and_defenders_defuse_first() {
        let map = StrategicMap::of_areas(vec![area("flag", None), area("A", Some(0)), area("B", Some(1))]);
        let charges = [
            None,
            Some(ChargeState::Active { progress: 0.0 }),
            Some(ChargeState::Armed { fuse: 20.0, progress: 0.0 }),
        ];
        let view = PlanView {
            map: &map,
            flags: &[None, None, None],
            charges: &charges,
            locked: &[false; 3],
            intel: &TeamIntel::default(),
            bleeding: [false; 2],
            mode: ModeState {
                kind: ModeKind::Rush,
                attacker: Team::Two,
                stages: 2,
                ..default()
            },
        };
        let (posture, attack) = objectives(&view, Team::Two);
        assert_eq!(posture, Posture::Attack);
        // Arm A, guard the armed B.
        assert!(attack.iter().any(|o| o.area == 1 && o.kind == OrderKind::Attack));
        assert!(attack.iter().any(|o| o.area == 2 && o.kind == OrderKind::Defend));
        let (posture, defend) = objectives(&view, Team::One);
        assert_eq!(posture, Posture::Guard);
        // The armed charge first: it has to be defused.
        assert_eq!((defend[0].area, defend[0].kind), (2, OrderKind::Defend));
        assert!(defend.iter().all(|o| o.kind == OrderKind::Defend && o.area != 0));
    }

    #[test]
    fn rounded_for_replication() {
        assert_eq!(rounded_progress(0.019), 0.0);
        assert_eq!(rounded_progress(0.5), 0.5);
        assert_eq!(rounded_fuse(29.91), 30.0);
    }
}
