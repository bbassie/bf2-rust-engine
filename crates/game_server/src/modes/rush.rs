//! Rush: each stage has a pair of charges ("M-COM stations"). An attacker arms one by holding
//! the use key at it for the layout's `arm_seconds`; a defender defuses an armed one the same
//! way (`defuse_seconds`); an armed charge goes off after `fuse_seconds`, hurting everyone
//! around it. Once both of a stage's charges are destroyed the front moves: the attackers get
//! their tickets back and both sides spawn at the next stage's control points.
//!
//! Nothing is captured: control points only say which side holds them during a stage (the
//! layout's `attacker_spawns` and `defender_spawns`), and so whose vehicles spawn there. Of
//! those, each side spawns only at the ones near the stage's charges (`staged::front_open`).

use std::sync::Arc;

use bevy::prelude::*;
use bevy_replicon::prelude::*;
use game_data::{
    GameModeDesc,
    modes::{ModeKind, StageDesc},
};
use game_shared::{
    conquest::{FlagState, RoundState, Tickets, team_from_id},
    effects::PlayEffect,
    input::Buttons,
    level::LoadedLevel,
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
    nav::{CellRef, NavGrid, Navigation},
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
    /// Placed by the layout generator near `near` (its control point, see [`flag_ground`]):
    /// still to be moved onto walkable ground.
    approximate: bool,
    near: Option<Vec3>,
}

/// Where the ground at a layout's control point `id` is: the flag, at the height its spawn
/// points are at mostly (a flag may stand on a roof or be sunk into the ground; soldiers
/// spawn on the ground around it).
pub fn flag_ground(layout: &GameModeDesc, id: &str) -> Option<Vec3> {
    let cp = layout.control_points.iter().find(|cp| cp.id == id)?;
    let mut heights: Vec<f32> = layout
        .spawn_points
        .iter()
        .filter(|sp| sp.control_point == id)
        .map(|sp| sp.placement.position[1])
        .collect();
    heights.sort_by(f32::total_cmp);
    let y = heights.get(heights.len() / 2).copied().unwrap_or(cp.position[1]);
    Some(Vec3::new(cp.position[0], y, cp.position[2]))
}

/// The round: the attackers' tickets, every stage's charges (the first stage's in play), and
/// the first stage's spawns.
pub(crate) fn setup(commands: &mut Commands, setup: &mut RoundSetup) {
    let Some(layout) = setup.layout else {
        return;
    };
    let Some(staged) = layout.staged.as_ref() else {
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
            let near = charge.control_point.as_deref().and_then(|id| flag_ground(layout, id));
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
/// [`place_all`]), logging every one whose spot from the layout needed correcting.
fn place_charges(
    nav: Res<Navigation>,
    level: Res<LoadedLevel>,
    matches: Query<&MatchInfo>,
    mut charges: Query<(&mut Charge, &mut ChargeRules)>,
) {
    if !charges.iter().any(|(_, rules)| rules.approximate) {
        return;
    }
    let spawns: Vec<Vec3> = matches
        .iter()
        .next()
        .and_then(|info| level.game_mode(&info.mode, info.size))
        .map(|layout| layout.spawn_points.iter().map(|sp| Vec3::from_array(sp.placement.position)).collect())
        .unwrap_or_default();
    let mut list: Vec<_> = charges.iter_mut().collect();
    list.sort_by_key(|(charge, _)| (charge.stage, charge.index));
    let wanted: Vec<ChargeToPlace> = list
        .iter()
        .map(|(charge, rules)| ChargeToPlace {
            stage: charge.stage,
            wanted: charge.position,
            near: rules.near,
            approximate: rules.approximate,
        })
        .collect();
    let placed = place_all(&nav.0, &wanted, &spawns);
    let (mut count, mut moved, mut corrected, mut stuck) = (0, 0.0, 0, 0);
    for ((charge, rules), placed) in list.iter_mut().zip(placed) {
        if !rules.approximate {
            continue;
        }
        rules.approximate = false;
        let Some(spot) = placed.spot else {
            warn!(
                "rush: charge {} of stage {}: no walkable ground near {:.0} ({}), left where the layout put it",
                charge.name,
                charge.stage + 1,
                charge.position,
                placed.problems.join(", ")
            );
            stuck += 1;
            continue;
        };
        let distance = spot.distance(charge.position);
        if placed.problems.is_empty() {
            debug!("rush: charge {} of stage {} at {spot:.1} ({distance:.1} m from where the layout put it)", charge.name, charge.stage + 1);
        } else {
            info!(
                "rush: charge {} of stage {} moved {distance:.1} m to {spot:.0}: the layout's spot was {}{}",
                charge.name,
                charge.stage + 1,
                placed.problems.join(", "),
                if placed.remaining.is_empty() {
                    String::new()
                } else {
                    format!(" (the new one still {}: nothing better near)", placed.remaining.join(", "))
                }
            );
            corrected += 1;
        }
        count += 1;
        moved += distance;
        charge.position = spot;
    }
    info!(
        "rush: {count} charges placed on walkable ground ({:.1} m from the layout's spots on average, {corrected} corrected), {stuck} without any near",
        moved / count.max(1) as f32
    );
}

/// A charge to place (see [`place_all`]).
#[derive(Clone, Debug)]
pub struct ChargeToPlace {
    pub stage: u8,
    /// Where the layout put it.
    pub wanted: Vec3,
    /// The ground at its control point ([`flag_ground`]): the level it goes at.
    pub near: Option<Vec3>,
    /// Generated: to be moved. Hand-placed charges stay where they are.
    pub approximate: bool,
}

/// Where a charge went.
#[derive(Clone, Debug, Default)]
pub struct PlacedCharge {
    /// `None`: no walkable ground near (or hand-placed: where the layout put it).
    pub spot: Option<Vec3>,
    /// What was wrong with the layout's spot and isn't with the one it went to, if anything:
    /// what the placement corrected.
    pub problems: Vec<&'static str>,
    /// What is still wrong with the spot it went to (nothing better near).
    pub remaining: Vec<&'static str>,
}

/// Charges of a stage at least this far apart, when there's room.
pub const CHARGES_APART: f32 = 20.0;
/// How far from the layout's spot a charge may go: first within the near radius, else the
/// far one.
const PLACE_NEAR: f32 = 16.0;
const PLACE_FAR: f32 = 40.0;
/// What a spot above its flag's level (on a roof, a wall), below it or indoors costs, in
/// meters of going further.
const RAISED_COST: f32 = 12.0;
const SUNKEN_COST: f32 = 6.0;
const INDOORS_COST: f32 = 6.0;

/// Spots for generated charges ([`charge_spot`]), stage by stage so that a charge keeps its
/// distance from the ones of its stage placed before it, on ground soldiers can walk to from
/// the layout's `spawns` (the walkable regions of the grid they are in). Hand-placed charges
/// keep their spot.
pub fn place_all(grid: &NavGrid, charges: &[ChargeToPlace], spawns: &[Vec3]) -> Vec<PlacedCharge> {
    let mut regions: Vec<u16> = spawns
        .iter()
        .filter_map(|sp| grid.locate(*sp, 3.0, None))
        .map(|c| grid.cell(c).region)
        .collect();
    regions.sort_unstable();
    regions.dedup();
    let mut placed: Vec<PlacedCharge> = Vec::with_capacity(charges.len());
    for (i, charge) in charges.iter().enumerate() {
        let partners: Vec<Vec3> = charges[..i]
            .iter()
            .zip(&placed)
            .filter(|(other, _)| other.stage == charge.stage)
            .map(|(other, p)| p.spot.unwrap_or(other.wanted))
            .collect();
        placed.push(if charge.approximate {
            charge_spot(grid, charge.wanted, charge.near, &partners, &regions)
        } else {
            PlacedCharge::default()
        });
    }
    placed
}

/// What makes a cell a poor spot for a charge, or one it can't go on at all.
struct SpotCheck<'a> {
    grid: &'a NavGrid,
    /// The walkable regions soldiers spawn in (else its flag's), and the level of the ground
    /// at its flag.
    regions: Vec<u16>,
    level: f32,
    partners: &'a [Vec3],
}

impl SpotCheck<'_> {
    /// The problems of a cell as a spot, each with what it costs (`None`: it can't go there).
    fn issues(&self, cell: CellRef) -> Vec<(&'static str, Option<f32>)> {
        let spot = self.grid.position(cell);
        let mut issues = Vec::new();
        // The seabed under a pier is walkable, but nobody arms a charge there.
        if self.grid.params.water_height.is_some_and(|w| spot.y < w + 0.5) {
            issues.push(("under water", None));
        }
        if !self.regions.is_empty() && !self.regions.contains(&self.grid.cell(cell).region) {
            issues.push(("not reachable from the spawns", None));
        }
        // By the flag's level rather than the terrain's: a pier's deck or a carrier's is the
        // ground there.
        if spot.y > self.level + 2.0 {
            issues.push(("above its flag's level (a roof or a wall)", Some(RAISED_COST)));
        } else if spot.y < self.level - 3.0 {
            issues.push(("below its flag's level", Some(SUNKEN_COST)));
        }
        if self.covered(spot) {
            issues.push(("indoors", Some(INDOORS_COST)));
        }
        let crowding: f32 = self
            .partners
            .iter()
            .map(|p| (CHARGES_APART - p.xz().distance(spot.xz())).max(0.0))
            .sum();
        if crowding > 0.0 {
            issues.push(("next to the other charge", Some(2.0 * crowding)));
        }
        issues
    }

    fn problems(&self, cell: CellRef) -> Vec<&'static str> {
        self.issues(cell).into_iter().map(|(name, _)| name).collect()
    }

    /// Whether walkable ground of the level grid is above `spot` (a floor or a roof over it).
    fn covered(&self, spot: Vec3) -> bool {
        let Some((x, z)) = self.grid.column_at(spot.x, spot.z) else {
            return false;
        };
        self.grid.column(x, z).any(|index| self.grid.cell(CellRef { x, z, index }).y > spot.y + 2.0)
    }

    /// How poor a spot the cell is (lower is better), `wanted` being the layout's spot: its
    /// distance from it, too little room around (1.5 m from walls and ledges) and what its
    /// problems cost. `None` where it can't go.
    fn score(&self, cell: CellRef, wanted: Vec3) -> Option<f32> {
        let mut score = self.grid.position(cell).xz().distance(wanted.xz());
        // `dist` is in half cells.
        let room = self.grid.cell(cell).dist as f32 * 0.5 * self.grid.cell_size(cell);
        score += 4.0 * (1.5 - room).max(0.0);
        for (_, cost) in self.issues(cell) {
            score += cost?;
        }
        Some(score)
    }
}

/// A spot for a generated charge near `wanted` (at the level of the ground at its flag,
/// `near`): dry walkable ground in the `regions` of the navigation grid soldiers spawn in (or
/// else the one its flag is in), so both sides can walk to it; with room around it and at its flag's level rather than on a roof, a wall or
/// indoors; at least [`CHARGES_APART`] from the other charges of its stage (`partners`) where
/// that's possible; as close to `wanted` as all that allows, within 16 m (40 m if nothing
/// nearer will do). The problems are those of the layout's spot (of the walkable ground right
/// there) that the new spot doesn't have: empty if it was as good as any near.
pub fn charge_spot(grid: &NavGrid, wanted: Vec3, near: Option<Vec3>, partners: &[Vec3], regions: &[u16]) -> PlacedCharge {
    let flag = near.unwrap_or(wanted);
    let check = SpotCheck {
        grid,
        regions: if regions.is_empty() { walk_region(grid, flag).into_iter().collect() } else { regions.to_vec() },
        level: flag.y,
        partners,
    };
    // The layout's spot: the walkable ground nearest to it (within 2 m), if any.
    let at_wanted = grid
        .cells_near(wanted.xz(), 2.0)
        .map(|c| (grid.position(c).xz().distance(wanted.xz()), check.problems(c)))
        .min_by(|a, b| a.1.len().cmp(&b.1.len()).then(a.0.total_cmp(&b.0)));
    let problems = match at_wanted {
        Some((_, problems)) => problems,
        None => vec!["no walkable ground"],
    };
    let best = |radius: f32| {
        grid.cells_near(wanted.xz(), radius)
            .filter(|&c| grid.position(c).xz().distance(wanted.xz()) <= radius)
            .filter_map(|c| Some((check.score(c, wanted)?, c)))
            .min_by(|a, b| a.0.total_cmp(&b.0))
    };
    let cell = best(PLACE_NEAR)
        // Nothing but poor spots near: further out.
        .filter(|(score, _)| *score < PLACE_NEAR + INDOORS_COST)
        .or_else(|| best(PLACE_FAR))
        .map(|(_, c)| c);
    let remaining = cell.map_or_else(Vec::new, |c| check.problems(c));
    PlacedCharge {
        spot: cell.map(|c| grid.position(c)),
        // A charge by a flag indoors stays indoors: that's no correction.
        problems: problems.into_iter().filter(|p| !remaining.contains(p)).collect(),
        remaining,
    }
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
