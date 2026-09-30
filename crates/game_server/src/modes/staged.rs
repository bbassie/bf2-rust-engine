//! What Rush and Breakthrough share: only the attackers have tickets, one per death. Taking a
//! stage moves the front and refills them. The round ends when they take the last stage
//! (they win) or run out of tickets (the defenders win), unless an armed charge keeps it going
//! until it goes off or is defused (overtime).
//!
//! Spawns follow the front ([`SpawnBlocked`]): each side spawns only at the points it holds
//! near the stage being fought over ([`front_open`]), so the defenders don't spawn at a base
//! 500 m behind the charges and the attackers move up to the stage they took instead of
//! walking from their start every time. And nobody spawns at a point with enemies at it: the
//! defenders' points at the front would otherwise put them right back where the attackers
//! are, and the front would never move.

use bevy::prelude::*;
use bevy_replicon::prelude::*;
use game_data::{GameModeDesc, modes::ModeKind};
use game_shared::{
    conquest::{ControlPoint, FlagState, RoundState, Tickets, team_index},
    modes::{Charge, ChargeState, ModeState, ObjectiveEvent, ObjectiveEventKind, Sector, SpawnBlockReason, SpawnBlocked},
    protocol::{ControlledBy, Team},
    revive::Downed,
    soldier::{Soldier, SoldierMotion},
};

use super::{ModeSystems, RoundClock, RoundSetup, end_round, in_modes};
use crate::combat::Died;

/// Enemies this close to a flag (beyond its radius, at least this far) close it for spawning.
const CONTESTED_MARGIN: f32 = 15.0;
const CONTESTED_MIN: f32 = 30.0;

/// Spawns follow the front: a side spawns at the points it holds within this distance of the
/// stage's objectives (its charges, or its sector's flags)...
pub const FRONT_RANGE: f32 = 150.0;
/// ... and, further away, at the nearest point that isn't at an objective itself (where it
/// falls back to when enemies block those) and any up to this much farther than that one.
pub const FRONT_SLACK: f32 = 30.0;
/// Points this close to an objective are at it.
pub const AT_OBJECTIVE: f32 = 40.0;
/// The front check in the log warns about a side whose nearest spawn is farther than this
/// from the objectives (the attackers only once they took a stage).
const FAR_SPAWN: f32 = 250.0;

pub struct StagedPlugin;

impl Plugin for StagedPlugin {
    fn build(&self, app: &mut App) {
        app.add_systems(
            FixedUpdate,
            (
                // After the objectives: a stage taken this tick moves the spawns at once.
                update_spawn_blocks.in_set(ModeSystems::Tickets),
                attacker_tickets.in_set(ModeSystems::Tickets),
            )
                .run_if(in_modes(&[ModeKind::Rush, ModeKind::Breakthrough])),
        );
    }
}

/// What the front rule needs to know of a control point besides who holds it, set when the
/// round starts.
#[derive(Component, Clone, Debug, Default, PartialEq)]
pub struct FrontPoint {
    /// Where its spawn points are on average (XZ); `None` without spawn points.
    pub spawns: Option<Vec2>,
    /// Per team: open for it wherever the front is. The attackers' start points with vehicles
    /// for them, so their vehicles aren't lost once they spawn further on.
    pub keep: [bool; 2],
}

/// A point as [`front_open`] sees it.
#[derive(Clone, Copy, Debug)]
pub struct FrontCandidate {
    pub owner: Team,
    /// Where its spawn points are (XZ); `None` without any (never closed: nobody spawns there).
    pub spawns: Option<Vec2>,
    /// Open wherever the front is.
    pub keep: bool,
}

/// How far `at` is from the nearest of `objectives`.
fn objective_distance(at: Vec2, objectives: &[Vec2]) -> f32 {
    objectives.iter().map(|o| o.distance(at)).fold(f32::INFINITY, f32::min)
}

/// Per point, whether its owner may spawn there with the stage's objectives at `objectives`:
/// the points within [`FRONT_RANGE`] of them, the nearest point beyond [`AT_OBJECTIVE`] (so a
/// side whose points at the objectives are blocked still has one to fall back to, like the
/// attackers' start in the first stage) and those up to [`FRONT_SLACK`] farther than that
/// one, and the points to keep. Without objectives everything is open.
pub fn front_open(points: &[FrontCandidate], objectives: &[Vec2]) -> Vec<bool> {
    let mut open = vec![true; points.len()];
    if objectives.is_empty() {
        return open;
    }
    for team in [Team::One, Team::Two] {
        let held: Vec<(usize, f32)> = points
            .iter()
            .enumerate()
            .filter(|(_, p)| p.owner == team)
            .filter_map(|(i, p)| Some((i, objective_distance(p.spawns?, objectives))))
            .collect();
        let fallback = held
            .iter()
            .map(|(_, d)| *d)
            .filter(|d| *d > AT_OBJECTIVE)
            .fold(f32::INFINITY, f32::min);
        let reach = if fallback.is_finite() { FRONT_RANGE.max(fallback + FRONT_SLACK) } else { FRONT_RANGE };
        for (i, distance) in held {
            open[i] = distance <= reach || points[i].keep;
        }
    }
    open
}

/// The objectives of a layout's `stage` (XZ): Rush's charges, Breakthrough's flags.
pub fn layout_objectives(layout: &GameModeDesc, kind: ModeKind, stage: usize) -> Vec<Vec2> {
    let Some(stage) = layout.staged.as_ref().and_then(|s| s.stages.get(stage)) else {
        return Vec::new();
    };
    match kind {
        ModeKind::Rush => stage.charges.iter().map(|c| Vec2::new(c.position[0], c.position[2])).collect(),
        _ => layout
            .control_points
            .iter()
            .filter(|cp| stage.control_points.contains(&cp.id))
            .map(|cp| Vec2::new(cp.position[0], cp.position[2]))
            .collect(),
    }
}

/// A new round of a staged mode: every point's [`FrontPoint`], and the first stage's spawns
/// (closed where they don't follow the front), for the points as the mode set them up. The
/// round starts with them, before anyone spawns.
pub(crate) fn front_setup(setup: &RoundSetup) -> Vec<(FrontPoint, Option<SpawnBlocked>)> {
    let Some(layout) = setup.layout else {
        return Vec::new();
    };
    let attacker = setup.state.attacker;
    let fronts: Vec<FrontPoint> = setup
        .points
        .iter()
        .map(|point| {
            let id = point.rules.id.as_str();
            let spawns: Vec<Vec2> = layout
                .spawn_points
                .iter()
                .filter(|sp| sp.control_point == id)
                .map(|sp| Vec2::new(sp.placement.position[0], sp.placement.position[2]))
                .collect();
            let mut keep = [false; 2];
            if let Some(a) = team_index(attacker)
                && point.flag.owner == attacker
            {
                keep[a] = layout
                    .vehicle_spawners
                    .iter()
                    .any(|v| v.control_point.as_deref() == Some(id) && v.templates[a].is_some());
            }
            FrontPoint {
                spawns: (!spawns.is_empty()).then(|| spawns.iter().sum::<Vec2>() / spawns.len() as f32),
                keep,
            }
        })
        .collect();
    let candidates: Vec<FrontCandidate> = setup
        .points
        .iter()
        .zip(&fronts)
        .map(|(point, front)| candidate(point.flag.owner, front))
        .collect();
    let open = front_open(&candidates, &layout_objectives(layout, setup.state.kind, 0));
    fronts
        .into_iter()
        .zip(setup.points.iter().zip(open))
        .map(|(front, (point, open))| {
            let blocked = (!open).then_some(SpawnBlocked(point.flag.owner, SpawnBlockReason::Front));
            (front, blocked)
        })
        .collect()
}

fn candidate(owner: Team, front: &FrontPoint) -> FrontCandidate {
    FrontCandidate {
        owner,
        spawns: front.spawns,
        keep: team_index(owner).is_some_and(|t| front.keep[t]),
    }
}

type PointView<'a> = (Entity, &'a ControlPoint, &'a FlagState, Option<&'a Sector>, Option<&'a FrontPoint>, Option<&'a SpawnBlocked>);

/// The points nobody may spawn at, every tick: those their owner holds away from the front
/// ([`front_open`]: the stage's charges or sector decide), and, checked twice a second, those
/// with enemies at them. Logs the spawns each side has whenever they change.
#[allow(clippy::type_complexity, clippy::too_many_arguments)]
fn update_spawn_blocks(
    mut commands: Commands,
    time: Res<Time>,
    mut timer: Local<f32>,
    // Points with enemies of their owner at them, and that owner.
    mut contested: Local<Vec<(Entity, Team)>>,
    mut logged: Local<Option<(u8, Vec<(Entity, Option<usize>, bool)>)>>,
    mode: Single<&ModeState>,
    points: Query<(Entity, &ControlPoint, &FlagState, Option<&Sector>, Option<&FrontPoint>, Option<&SpawnBlocked>)>,
    charges: Query<&Charge>,
    soldiers: Query<(&SoldierMotion, &ControlledBy), (With<Soldier>, Without<Downed>)>,
    teams: Query<&Team>,
) {
    let mode = **mode;
    *timer -= time.delta_secs();
    if *timer <= 0.0 {
        *timer = 0.5;
        let soldiers: Vec<(Vec3, Team)> = soldiers
            .iter()
            .filter_map(|(motion, controlled_by)| Some((motion.position, *teams.get(controlled_by.0).ok()?)))
            .collect();
        contested.clear();
        for (entity, cp, flag, ..) in &points {
            let owner = flag.owner;
            let reach = (cp.radius + CONTESTED_MARGIN).max(CONTESTED_MIN);
            if owner != Team::Spectator
                && soldiers
                    .iter()
                    .any(|(at, team)| *team == owner.opponent() && at.xz().distance(cp.position.xz()) < reach)
            {
                contested.push((entity, owner));
            }
        }
    }
    let objectives: Vec<Vec2> = match mode.kind {
        ModeKind::Rush => charges.iter().filter(|c| c.stage == mode.stage).map(|c| c.position.xz()).collect(),
        _ => points
            .iter()
            .filter(|(_, _, _, sector, ..)| sector.is_some_and(|s| s.0 == mode.stage))
            .map(|(_, cp, ..)| cp.position.xz())
            .collect(),
    };
    let list: Vec<PointView> = points.iter().collect();
    let default_front = FrontPoint::default();
    let candidates: Vec<FrontCandidate> = list
        .iter()
        .map(|(_, _, flag, _, front, _)| candidate(flag.owner, front.unwrap_or(&default_front)))
        .collect();
    let open = front_open(&candidates, &objectives);
    for ((entity, _, flag, _, _, blocked), open) in list.iter().zip(&open) {
        let reason = if flag.owner == Team::Spectator {
            None
        } else if !open {
            Some(SpawnBlockReason::Front)
        } else if contested.contains(&(*entity, flag.owner)) {
            Some(SpawnBlockReason::Enemies)
        } else {
            None
        };
        let wanted = reason.map(|reason| SpawnBlocked(flag.owner, reason));
        if wanted.as_ref() != *blocked {
            match wanted {
                Some(b) => commands.entity(*entity).insert(b),
                None => commands.entity(*entity).remove::<SpawnBlocked>(),
            };
        }
    }
    // The log, whenever a side's spawns change.
    let mut state: Vec<(Entity, Option<usize>, bool)> = list
        .iter()
        .zip(&open)
        .filter(|((_, _, flag, _, front, _), _)| flag.owner != Team::Spectator && front.is_some_and(|f| f.spawns.is_some()))
        .map(|((entity, _, flag, ..), open)| (*entity, team_index(flag.owner), *open))
        .collect();
    state.sort();
    if objectives.is_empty() || logged.as_ref().is_some_and(|(stage, s)| *stage == mode.stage && *s == state) {
        return;
    }
    *logged = Some((mode.stage, state));
    log_front(&mode, &list, &open, &objectives);
}

/// Logs each side's spawns and how far they are from the objectives, and the front check: a
/// warning when a side's nearest spawn is far from them.
fn log_front(mode: &ModeState, points: &[PointView], open: &[bool], objectives: &[Vec2]) {
    let stage = mode.stage_label();
    let mut check = Vec::new();
    let mut far = Vec::new();
    for (team, side) in [(mode.attacker, "attackers"), (mode.defender(), "defenders")] {
        // Distance, name, open, kept for its vehicles.
        let mut spawns: Vec<(f32, &str, bool, bool)> = points
            .iter()
            .zip(open)
            .filter(|((_, _, flag, ..), _)| flag.owner == team)
            .filter_map(|((_, cp, _, _, front, _), open)| {
                let front = front.as_ref()?;
                let distance = objective_distance(front.spawns?, objectives);
                let keep = team_index(team).is_some_and(|t| front.keep[t]);
                Some((distance, cp.name.as_str(), *open, keep))
            })
            .collect();
        spawns.sort_by(|a, b| a.0.total_cmp(&b.0));
        let describe = |list: Vec<String>| if list.is_empty() { "none".to_string() } else { list.join(", ") };
        let opened = describe(
            spawns
                .iter()
                .filter(|s| s.2)
                .map(|(d, name, _, keep)| format!("{name} {d:.0} m{}", if *keep { " (vehicles)" } else { "" }))
                .collect(),
        );
        let closed = describe(spawns.iter().filter(|s| !s.2).map(|(d, name, ..)| format!("{name} {d:.0} m")).collect());
        info!("{} {stage}: {side} spawn at {opened}; closed: {closed}", mode.kind.label());
        // The nearest open spawn, the start kept for its vehicles last.
        let nearest = spawns
            .iter()
            .filter(|s| s.2)
            .min_by(|a, b| a.3.cmp(&b.3).then(a.0.total_cmp(&b.0)));
        let farthest = spawns.iter().filter(|s| s.2).map(|s| s.0).fold(0.0, f32::max);
        let held_farthest = spawns.iter().map(|s| s.0).fold(0.0, f32::max);
        match nearest {
            Some((d, name, ..)) => {
                check.push(format!(
                    "{side}' nearest spawn {name} {d:.0} m, farthest open {farthest:.0} m (farthest held {held_farthest:.0} m)"
                ));
                if *d > FAR_SPAWN && (team == mode.defender() || mode.stage > 0) {
                    far.push(side);
                }
            }
            None => check.push(format!("{side} have no spawn open")),
        }
    }
    if far.is_empty() {
        info!("front check {stage}: {} - ok", check.join("; "));
    } else {
        warn!("front check {stage}: {} - {} spawn FAR from the objectives", check.join("; "), far.join(" and "));
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

    fn point(owner: Team, x: f32, z: f32, keep: bool) -> FrontCandidate {
        FrontCandidate {
            owner,
            spawns: Some(Vec2::new(x, z)),
            keep,
        }
    }

    #[test]
    fn spawns_follow_the_front() {
        // Team 2 attacks northwards (-Z) from a start 400 m south of the first stage's charges
        // (its vehicles there); team 1 holds the stage's flag, flags 80 m, 170 m and 250 m
        // behind it and a base 500 m back.
        let mut points = vec![
            point(Team::Two, 0.0, 400.0, true),
            point(Team::One, 0.0, 10.0, false),
            point(Team::One, 0.0, -80.0, false),
            point(Team::One, 0.0, -170.0, false),
            point(Team::One, 0.0, -250.0, false),
            point(Team::One, 0.0, -500.0, false),
        ];
        let first = [Vec2::new(-15.0, 0.0), Vec2::new(15.0, 0.0)];
        // The defenders spawn at the stage and the flag behind it, not 170 m and more back;
        // the attackers at their start, far as it is.
        assert_eq!(front_open(&points, &first), [true, true, true, false, false, false]);
        // The stage falls: the attackers hold its flag and the one behind it now, the charges
        // are at the flag 170 m north. The attackers spawn 90 m from them (and at their start
        // for its vehicles), not at the flag they took first (180 m); the defenders at the
        // stage's flag and the one 80 m behind it.
        points[1].owner = Team::Two;
        points[2].owner = Team::Two;
        let second = [Vec2::new(-15.0, -170.0), Vec2::new(15.0, -170.0)];
        assert_eq!(front_open(&points, &second), [true, false, true, true, true, false]);
        // Without the vehicles the start is closed too.
        points[0].keep = false;
        assert!(!front_open(&points, &second)[0]);
        // The last stage, at the flag 250 m north: the defenders have nothing left but the base,
        // however far (to fall back to while the attackers are at the flag).
        points[3].owner = Team::Two;
        let last = [Vec2::new(-15.0, -250.0), Vec2::new(15.0, -250.0)];
        assert_eq!(front_open(&points, &last), [false, false, false, true, true, true]);
    }

    #[test]
    fn open_without_objectives_or_spawns() {
        let far = point(Team::One, 0.0, 900.0, false);
        assert_eq!(front_open(&[far], &[]), [true]);
        let no_spawns = FrontCandidate {
            spawns: None,
            ..far
        };
        assert_eq!(front_open(&[no_spawns], &[Vec2::ZERO]), [true]);
    }

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
