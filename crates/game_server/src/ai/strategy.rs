//! The team layer, after BF2's strategic AI (`AIDefaultStrategies.ai`, `StrategicAreas.ai`):
//! strategic areas around the flags, linked into lines of advance, and a commander per team
//! that weighs attacking and defending them and gives every squad an order.
//!
//! BF2's two conquest strategies are kept: *attack* while the enemy holds a flag or one is
//! neutral (hostile flags weigh 10, neutral ones 12) and *guard* once the team holds them
//! all (owned flags weigh 10). On top of that the commander weighs how exposed a flag is
//! (next to enemy ground), what the team has seen of the enemy around it, flags being taken
//! right now, distance, and ticket bleed.
//!
//! Other modes value their own objectives ([`crate::modes::ServerMode::objectives`]: Rush's
//! charges are areas too, Breakthrough only values the open sector's flags); the orders, the
//! squads and everything below work the same in every mode.

use std::{
    ops::Range,
    sync::atomic::{AtomicU32, Ordering},
};

use bevy::{platform::collections::HashMap, prelude::*};
use game_data::StrategicLayoutDesc;
use game_shared::{
    conquest::{ControlPoint, FlagState, Tickets, team_index},
    level::LoadedLevel,
    modes::{Charge, ChargeState, Locked, ModeState},
    protocol::{MatchInfo, Team},
};

use game_shared::{
    commander::{Commander, OrderKind as CommanderOrderKind, SquadOrder as CommanderOrder},
    protocol::Player,
    revive::Downed,
    soldier::SoldierMotion,
    vehicle::VehicleMotion,
};

use super::{AiData, squad::SquadSnapshot};
use crate::{conquest::ControlPointRules, radio::Spot};

/// Seconds between plans.
const PLAN_INTERVAL: f32 = 3.0;
/// Orders younger than this are kept while they still make sense.
const MIN_ORDER_AGE: f32 = 25.0;
/// How long the team remembers where an enemy was seen, seconds.
const INTEL_MAX_AGE: f32 = 20.0;
/// Radius of strategic areas that aren't control points, meters.
const AREA_RADIUS: f32 = 15.0;
/// Radius of a charge's area (Rush), meters.
const CHARGE_RADIUS: f32 = 6.0;
/// Enemies seen within this many seconds count as a threat to a flag the team holds (older
/// sightings are of enemies long gone, or of the ones the team just fought off there).
const FRESH_THREAT: f32 = 12.0;
/// In the attack posture a whole squad defends a flag the team holds only while the enemy
/// is taking it, or this many enemies gather at it; fewer get a guard ([`Guard`]).
const DEFEND_THREAT: f32 = 3.0;
/// Guards stay at most this long, seconds.
const GUARD_TIME: f32 = 90.0;
/// A squad defending a flag the enemy is taking is found among bot-led squads this close,
/// meters, whose orders are at least this old, seconds.
const RESCUE_DISTANCE: f32 = 300.0;
const RESCUE_MIN_AGE: f32 = 5.0;

/// An area the commanders reason about (a BF2 strategic area).
#[derive(Clone, Debug)]
pub struct Area {
    pub name: String,
    /// [`ControlPoint::index`] of the flag, if the area is one.
    pub control_point: Option<u8>,
    /// [`Charge::index`] of the charge, if the area is one (Rush).
    pub charge: Option<u8>,
    pub position: Vec3,
    /// Where infantry go when sent here.
    pub order_position: Vec3,
    pub radius: f32,
    pub uncapturable: bool,
    /// The team can spawn here while it holds it.
    pub has_spawns: bool,
    pub neighbours: Vec<usize>,
    /// Places to pass through on the way to a neighbour, as alternatives.
    pub routes: Vec<(usize, Vec<Vec3>)>,
}

/// The strategic areas of the layout being played, rebuilt every round (control points are
/// new entities then) and with every level.
#[derive(Resource, Default)]
pub struct StrategicMap {
    pub areas: Vec<Area>,
    /// Control point entities by index.
    pub control_points: Vec<Option<Entity>>,
    /// Charge entities by index (Rush).
    pub charges: Vec<Option<Entity>>,
    /// Changes with every rebuild (never 0 once built): area indices kept from an older
    /// map mean nothing.
    pub generation: u32,
    built_for: Vec<Entity>,
    /// Where the charges stood then (the server moves them onto walkable ground).
    built_charges: Vec<(Entity, Vec3)>,
    /// Per area, the walkable region of the navigation grid it lies in (areas in different
    /// regions can't be walked between: a carrier and the island), once the grid is built.
    pub walk_regions: Vec<Option<u16>>,
    /// The grid those regions are of.
    regions_of: Option<usize>,
}

/// Generations of maps ever built, so they differ even when the resource is replaced.
static GENERATION: AtomicU32 = AtomicU32::new(0);

impl StrategicMap {
    /// The area of a control point.
    pub fn area_of(&self, control_point: u8) -> Option<usize> {
        self.areas.iter().position(|a| a.control_point == Some(control_point))
    }

    /// The charge entity of an area, if it is one.
    pub fn charge_of(&self, area: usize) -> Option<Entity> {
        let index = self.areas.get(area)?.charge?;
        self.charges.get(index as usize).copied().flatten()
    }

    /// A map of just these areas, for tests.
    #[cfg(test)]
    pub fn of_areas(areas: Vec<Area>) -> Self {
        Self { areas, ..default() }
    }

    /// The closest area to a position.
    pub fn nearest(&self, position: Vec3) -> Option<usize> {
        (0..self.areas.len()).min_by(|&a, &b| {
            let d = |i: usize| self.areas[i].position.xz().distance_squared(position.xz());
            d(a).total_cmp(&d(b))
        })
    }

    /// A waypoint BF2 laid out between the area nearest `from` and `to`, to spread squads
    /// over several routes.
    pub fn route_waypoint(&self, from: Vec3, to: usize) -> Option<Vec3> {
        let here = self.nearest(from)?;
        let (_, waypoints) = self.areas[here].routes.iter().find(|(n, _)| *n == to)?;
        fastrand::choice(waypoints).copied()
    }
}

/// Information about a control point needed to build areas.
struct PointInfo<'a> {
    index: u8,
    id: &'a str,
    name: &'a str,
    position: Vec3,
    radius: f32,
    uncapturable: bool,
    has_spawns: bool,
}

/// Rebuilds the areas when the control points or charges change (new level or round).
pub fn update_map(
    mut map: ResMut<StrategicMap>,
    mut strategy: ResMut<Strategy>,
    data: Res<AiData>,
    level: Res<LoadedLevel>,
    match_info: Single<&MatchInfo>,
    control_points: Query<(Entity, &ControlPoint, &ControlPointRules)>,
    charges: Query<(Entity, &Charge)>,
) {
    let mut points: Vec<(Entity, &ControlPoint, &ControlPointRules)> = control_points.iter().collect();
    points.sort_by_key(|(_, cp, _)| cp.index);
    let entities: Vec<Entity> = points.iter().map(|(e, ..)| *e).collect();
    let mut charges: Vec<(Entity, &Charge)> = charges.iter().collect();
    charges.sort_by_key(|(_, c)| c.index);
    let charge_spots: Vec<(Entity, Vec3)> = charges.iter().map(|(e, c)| (*e, c.position)).collect();
    if entities == map.built_for
        && charge_spots == map.built_charges
        && map.generation != 0
        && !data.is_changed()
        && !level.is_changed()
    {
        return;
    }
    // Orders and objectives name areas of the old map: plan again right away.
    *strategy = Strategy::default();
    let layout = level.game_mode(&match_info.mode, match_info.size);
    let spawn_ids: Vec<&str> = layout
        .map(|l| l.spawn_points.iter().map(|s| s.control_point.as_str()).collect())
        .unwrap_or_default();
    let infos: Vec<PointInfo> = points
        .iter()
        .map(|(_, cp, rules)| PointInfo {
            index: cp.index,
            id: &rules.id,
            name: &cp.name,
            position: cp.position,
            radius: cp.radius,
            uncapturable: cp.uncapturable,
            has_spawns: spawn_ids.contains(&rules.id.as_str()),
        })
        .collect();
    // BF2's strategic areas are those of the layout this one is made from, if any.
    let desc = level
        .base_layout(&match_info.mode, match_info.size)
        .and_then(|l| data.level.layout(&l.mode, l.size));
    let mut areas = build_areas(desc, &infos);
    add_charges(&mut areas, &charges);
    let max_index = points.iter().map(|(_, cp, _)| cp.index as usize + 1).max().unwrap_or(0);
    let mut by_index = vec![None; max_index];
    for (entity, cp, _) in &points {
        by_index[cp.index as usize] = Some(*entity);
    }
    let charge_count = charges.iter().map(|(_, c)| c.index as usize + 1).max().unwrap_or(0);
    let mut charges_by_index = vec![None; charge_count];
    for (entity, charge) in &charges {
        charges_by_index[charge.index as usize] = Some(*entity);
    }
    if !areas.is_empty() {
        let links: Vec<String> = areas
            .iter()
            .enumerate()
            .flat_map(|(i, a)| a.neighbours.iter().filter(move |&&n| n > i).map(move |&n| (a, n)))
            .map(|(a, n)| format!("{}-{}", a.name, areas[n].name))
            .collect();
        info!(
            "ai: {} strategic areas ({}), links: {}",
            areas.len(),
            if desc.is_some() { "from the level" } else { "from the control points" },
            links.join(", ")
        );
    }
    *map = StrategicMap {
        areas,
        control_points: by_index,
        charges: charges_by_index,
        generation: GENERATION.fetch_add(1, Ordering::Relaxed) + 1,
        built_for: entities,
        built_charges: charge_spots,
        walk_regions: Vec::new(),
        regions_of: None,
    };
}

/// An area for every charge (Rush), linked to the nearest flag's area only: charges stand
/// right beside flags and would otherwise cut the lines of advance between them.
fn add_charges(areas: &mut Vec<Area>, charges: &[(Entity, &Charge)]) {
    let flags = areas.len();
    for (_, charge) in charges {
        let nearest = (0..flags).filter(|&a| areas[a].control_point.is_some()).min_by(|&a, &b| {
            let d = |i: usize| areas[i].position.xz().distance_squared(charge.position.xz());
            d(a).total_cmp(&d(b))
        });
        areas.push(Area {
            name: format!("charge {} (stage {})", charge.name, charge.stage + 1),
            control_point: None,
            charge: Some(charge.index),
            position: charge.position,
            order_position: charge.position,
            radius: CHARGE_RADIUS,
            uncapturable: false,
            has_spawns: false,
            neighbours: Vec::new(),
            routes: Vec::new(),
        });
        if let Some(flag) = nearest {
            let new = areas.len() - 1;
            link(areas, flag, new);
        }
    }
}

/// Finds the walkable region of every area once the navigation grid is there.
pub fn update_regions(mut map: ResMut<StrategicMap>, nav: Option<Res<crate::nav::Navigation>>) {
    let Some(nav) = nav else {
        return;
    };
    let grid: &crate::nav::NavGrid = &nav.0;
    let id = grid as *const _ as usize;
    if map.regions_of == Some(id) && map.walk_regions.len() == map.areas.len() {
        return;
    }
    let regions = map.areas.iter().map(|a| walk_region(grid, a.position)).collect();
    map.walk_regions = regions;
    map.regions_of = Some(id);
}

/// The region of the walkable surface nearest to `position` (above or below it: flags sit
/// on poles and towers), within 30 m.
pub fn walk_region(grid: &crate::nav::NavGrid, position: Vec3) -> Option<u16> {
    grid.cells_near(position.xz(), 30.0)
        .filter(|c| (c.x + c.z) % 2 == 0)
        .map(|c| (grid.position(c).distance_squared(position), grid.cell(c).region))
        .min_by(|a, b| a.0.total_cmp(&b.0))
        .map(|(_, region)| region)
}

/// Areas from the level's strategic areas, plus one per control point they miss, linked
/// to their neighbours (derived from the layout where the level doesn't say).
fn build_areas(desc: Option<&StrategicLayoutDesc>, points: &[PointInfo]) -> Vec<Area> {
    let area = |name: &str, point: Option<&PointInfo>, position: Vec3, order: Option<Vec3>| Area {
        name: name.to_string(),
        control_point: point.map(|p| p.index),
        charge: None,
        position,
        order_position: order.unwrap_or(position),
        radius: point.map_or(AREA_RADIUS, |p| p.radius),
        uncapturable: point.is_some_and(|p| p.uncapturable),
        has_spawns: point.is_some_and(|p| p.has_spawns),
        neighbours: Vec::new(),
        routes: Vec::new(),
    };
    let mut areas: Vec<Area> = Vec::new();
    let mut linked = false;
    if let Some(desc) = desc {
        // The level's names, for its links; flags are shown by their own names.
        let mut keys: Vec<&str> = Vec::new();
        for a in &desc.areas {
            let point = a.control_point.as_ref().and_then(|id| points.iter().find(|p| p.id == id));
            if point.is_some_and(|p| areas.iter().any(|x| x.control_point == Some(p.index))) {
                continue;
            }
            let position = point.map_or(Vec3::from_array(a.position), |p| p.position);
            let name = point.map_or(a.name.as_str(), |p| p.name);
            areas.push(area(name, point, position, a.infantry_position.map(Vec3::from_array)));
            keys.push(&a.name);
        }
        let index = |name: &str| keys.iter().position(|k| k.eq_ignore_ascii_case(name));
        let mut links = Vec::new();
        for a in &desc.areas {
            let Some(from) = index(&a.name) else {
                continue;
            };
            links.extend(a.neighbours.iter().filter_map(|n| index(n)).map(|to| (from, to)));
        }
        let mut routes = Vec::new();
        for route in &desc.routes {
            if let (Some(from), Some(to)) = (index(&route.from), index(&route.to)) {
                let waypoints: Vec<Vec3> = route.waypoints.iter().copied().map(Vec3::from_array).collect();
                routes.push((from, to, waypoints));
            }
        }
        linked = !links.is_empty();
        for (from, to) in links {
            link(&mut areas, from, to);
        }
        for (from, to, waypoints) in routes {
            areas[from].routes.push((to, waypoints.clone()));
            areas[to].routes.push((from, waypoints));
        }
    }
    let first_new = if linked { areas.len() } else { 0 };
    for point in points {
        if !areas.iter().any(|a| a.control_point == Some(point.index)) {
            areas.push(area(point.name, Some(point), point.position, None));
        }
    }
    let count = areas.len();
    derive_links(&mut areas, first_new..count);
    areas
}

fn link(areas: &mut [Area], a: usize, b: usize) {
    if a == b {
        return;
    }
    if !areas[a].neighbours.contains(&b) {
        areas[a].neighbours.push(b);
    }
    if !areas[b].neighbours.contains(&a) {
        areas[b].neighbours.push(a);
    }
}

/// Links the areas in `new` to their neighbours in the relative neighbourhood graph of all
/// areas: two areas are neighbours unless a third one is closer to both. That follows how
/// flags line up along roads and fronts, and keeps the graph connected.
fn derive_links(areas: &mut [Area], new: Range<usize>) {
    let n = areas.len();
    let distance = |areas: &[Area], a: usize, b: usize| areas[a].position.xz().distance(areas[b].position.xz());
    let mut links = Vec::new();
    for a in 0..n {
        for b in a + 1..n {
            if !new.contains(&a) && !new.contains(&b) {
                continue;
            }
            let ab = distance(areas, a, b);
            let blocked = (0..n)
                .any(|c| c != a && c != b && distance(areas, a, c).max(distance(areas, b, c)) < ab);
            if !blocked {
                links.push((a, b));
            }
        }
    }
    for (a, b) in links {
        link(areas, a, b);
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum OrderKind {
    /// Take the flag: stand inside its radius until it's ours.
    Attack,
    /// Hold the flag: take positions around it.
    Defend,
}

/// What the commander wants a squad to do.
#[derive(Clone, Copy, Debug)]
pub struct SquadOrder {
    pub kind: OrderKind,
    /// Index into [`StrategicMap::areas`].
    pub area: usize,
    /// Seconds since it was given.
    pub age: f32,
    /// The squad is led by a human: only a suggestion. Bots in it follow their leader.
    pub suggestion: bool,
    /// Given by the team's human commander, and kept while he keeps it.
    pub commanded: bool,
    /// A point to go to and hold instead of the area (a human commander's order away
    /// from the flags).
    pub point: Option<Vec3>,
}

/// Members of a squad left behind at a flag the squad took while the rest moves on: one or
/// two, and only while the flag is threatened (enemies seen at it lately) or the team can't
/// afford to lose it (a spawn by the enemy with few flags held).
#[derive(Clone, Debug)]
pub struct Guard {
    pub area: usize,
    /// Player entities.
    pub members: Vec<Entity>,
    /// Seconds since it was left.
    pub age: f32,
}

/// A flag worth attacking or defending, and how much.
#[derive(Clone, Copy, Debug)]
pub struct Objective {
    pub area: usize,
    pub kind: OrderKind,
    pub value: f32,
}

/// BF2's conquest strategies.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum Posture {
    /// The enemy holds a flag or one is neutral.
    #[default]
    Attack,
    /// We hold every flag we can: defend the ones next to the enemy.
    Guard,
}

/// The commanders' plans.
#[derive(Resource, Default)]
pub struct Strategy {
    /// By (team, squad).
    pub orders: HashMap<(Team, u8), SquadOrder>,
    /// Per team, most valuable first.
    pub objectives: [Vec<Objective>; 2],
    pub posture: [Posture; 2],
    /// By (team, squad): members left to guard a flag the squad took.
    pub guards: HashMap<(Team, u8), Guard>,
    /// Owner of every area's flag at the last look: a flag changing hands means a plan at once.
    owners: Vec<Option<Team>>,
    timer: f32,
}

impl Strategy {
    /// The flag `player` of squad `(team, squad)` guards, if it was left to guard one.
    pub fn guarding(&self, team: Team, squad: u8, player: Entity) -> Option<usize> {
        self.guards.get(&(team, squad)).filter(|g| g.members.contains(&player)).map(|g| g.area)
    }

    /// An objective for a bot of `team` without a squad order: the best one given the
    /// distance, varied a little per bot (`seed`).
    pub fn objective_for(&self, team: Team, position: Vec3, map: &StrategicMap, seed: u32) -> Option<Objective> {
        let objectives = team_index(team).map(|t| &self.objectives[t])?;
        objectives.iter().copied().filter(|o| o.area < map.areas.len()).max_by(|a, b| {
            let score = |o: &Objective| {
                let jitter = 1.0 + 0.3 * hash01(seed, o.area as u32);
                let distance = map.areas.get(o.area).map_or(f32::MAX, |a| a.position.distance(position));
                o.value * jitter / (1.0 + distance / 300.0)
            };
            score(a).total_cmp(&score(b))
        })
    }
}

/// A deterministic pseudo-random number in 0..1.
pub fn hash01(a: u32, b: u32) -> f32 {
    let mut h = a.wrapping_mul(0x9E37_79B9) ^ b.wrapping_mul(0x85EB_CA6B);
    h ^= h >> 15;
    h = h.wrapping_mul(0x2C1B_3C6D);
    h ^= h >> 12;
    (h & 0xFFFF) as f32 / 65535.0
}

/// Where the team has seen enemies lately (BF2's information grid, simplified): bots
/// report what they see, commanders plan with it.
#[derive(Resource, Default)]
pub struct TeamIntel {
    seen: [HashMap<Entity, (Vec3, f32)>; 2],
    clock: f32,
}

impl TeamIntel {
    pub fn report(&mut self, team: Team, enemy: Entity, position: Vec3) {
        if let Some(t) = team_index(team) {
            self.seen[t].insert(enemy, (position, self.clock));
        }
    }

    /// Enemies of `team` seen recently within `radius` of `position`.
    pub fn enemies_near(&self, team: Team, position: Vec3, radius: f32) -> usize {
        let Some(t) = team_index(team) else {
            return 0;
        };
        self.seen[t]
            .values()
            .filter(|(p, time)| self.clock - time < INTEL_MAX_AGE && p.distance_squared(position) < radius * radius)
            .count()
    }

    /// Enemies of `team` seen within `radius` of `position` in the last `max_age` seconds.
    pub fn enemies_near_since(&self, team: Team, position: Vec3, radius: f32, max_age: f32) -> usize {
        self.sightings_near(team, position, radius, max_age).count()
    }

    /// Forgets enemies that are gone (dead, down, or no more).
    pub fn forget_unless(&mut self, mut alive: impl FnMut(Entity) -> bool) {
        for seen in &mut self.seen {
            seen.retain(|enemy, _| alive(*enemy));
        }
    }

    /// Where enemies of `team` were seen in the last `max_age` seconds.
    pub fn recent(&self, team: Team, max_age: f32) -> Vec<Vec3> {
        let Some(t) = team_index(team) else {
            return Vec::new();
        };
        self.seen[t]
            .values()
            .filter(|(_, time)| self.clock - time < max_age)
            .map(|(p, _)| *p)
            .collect()
    }

    /// Enemies of `team` seen by anyone on it within `radius` of `position` in the last
    /// `max_age` seconds: who, where and how long ago.
    pub fn sightings_near(&self, team: Team, position: Vec3, radius: f32, max_age: f32) -> impl Iterator<Item = (Entity, Vec3, f32)> + '_ {
        let clock = self.clock;
        team_index(team)
            .into_iter()
            .flat_map(move |t| self.seen[t].iter())
            .filter(move |(_, (p, time))| clock - time < max_age && p.distance_squared(position) < radius * radius)
            .map(move |(enemy, (p, time))| (*enemy, *p, clock - time))
    }

    fn tick(&mut self, dt: f32) {
        self.clock += dt;
        let clock = self.clock;
        for seen in &mut self.seen {
            seen.retain(|_, (_, time)| clock - *time < INTEL_MAX_AGE);
        }
    }
}

/// What a commander plans with: the areas and what stands on them, and the mode.
pub struct PlanView<'a> {
    pub map: &'a StrategicMap,
    /// Per area, its control point's flag.
    pub flags: &'a [Option<FlagState>],
    /// Per area, its charge (Rush).
    pub charges: &'a [Option<ChargeState>],
    /// Per area, whether its control point is locked (Breakthrough's other sectors).
    pub locked: &'a [bool],
    pub intel: &'a TeamIntel,
    /// Per team, whether it is bleeding tickets.
    pub bleeding: [bool; 2],
    pub mode: ModeState,
}

/// BF2's conquest strategies (conquest and co-op): see [`value_areas`].
pub fn conquest_objectives(view: &PlanView, team: Team) -> (Posture, Vec<Objective>) {
    let bleeding = team_index(team).is_some_and(|t| view.bleeding[t]);
    value_areas(view.map, view.flags, team, view.intel, bleeding)
}

/// The commanders: every few seconds, value the objectives for both teams (the way the mode
/// being played says) and hand out orders.
#[allow(clippy::too_many_arguments)]
pub fn plan(
    time: Res<Time>,
    map: Res<StrategicMap>,
    mut strategy: ResMut<Strategy>,
    mut intel: ResMut<TeamIntel>,
    snapshot: Res<SquadSnapshot>,
    (flags, charges, locked, modes): (Query<&FlagState>, Query<&ChargeState>, Query<(), With<Locked>>, Query<&ModeState>),
    tickets: Query<&Tickets>,
    commanders: Query<(&Team, &Player), With<Commander>>,
    commander_orders: Query<&CommanderOrder>,
    mut spots: MessageReader<Spot>,
    spotted: Query<(Option<&SoldierMotion>, Option<&VehicleMotion>, Has<Downed>)>,
) {
    let dt = time.delta_secs();
    intel.tick(dt);
    // The dead and the downed are no threat (they were counted at the flag they fell at).
    intel.forget_unless(|enemy| spotted.get(enemy).is_ok_and(|(.., downed)| !downed));
    // What the commander's UAV and scans show, the team knows.
    for spot in spots.read() {
        if let Ok((soldier, vehicle, _)) = spotted.get(spot.target)
            && let Some(position) = soldier.map(|s| s.position + Vec3::Y).or(vehicle.map(|v| v.position))
        {
            intel.report(spot.team, spot.target, position);
        }
    }
    for order in strategy.orders.values_mut() {
        order.age += dt;
    }
    for guard in strategy.guards.values_mut() {
        guard.age += dt;
    }
    let states: Vec<Option<FlagState>> = map
        .areas
        .iter()
        .map(|a| {
            let entity = map.control_points.get(a.control_point? as usize).copied().flatten()?;
            flags.get(entity).ok().copied()
        })
        .collect();
    // A flag changed hands: the squads that took it move on (and the ones that lost it
    // react) now rather than at the next plan.
    let owners: Vec<Option<Team>> = states.iter().map(|s| s.map(|s| s.owner)).collect();
    if owners != strategy.owners {
        if !strategy.owners.is_empty() {
            strategy.timer = 0.0;
        }
        strategy.owners = owners;
    }
    strategy.timer -= dt;
    if strategy.timer > 0.0 {
        return;
    }
    strategy.timer = PLAN_INTERVAL;
    if map.areas.is_empty() {
        strategy.orders.clear();
        strategy.guards.clear();
        return;
    }
    let count = map.areas.len();
    strategy.orders.retain(|_, order| order.area < count);
    strategy.guards.retain(|_, guard| guard.area < count);
    let charge_states: Vec<Option<ChargeState>> = (0..map.areas.len())
        .map(|a| map.charge_of(a).and_then(|e| charges.get(e).ok().copied()))
        .collect();
    let locked_areas: Vec<bool> = map
        .areas
        .iter()
        .map(|a| {
            a.control_point
                .and_then(|i| map.control_points.get(i as usize).copied().flatten())
                .is_some_and(|e| locked.contains(e))
        })
        .collect();
    let mode = modes.iter().next().copied().unwrap_or_default();
    let tickets = tickets.iter().next().copied();

    for team in [Team::One, Team::Two] {
        let t = team_index(team).unwrap();
        let view = PlanView {
            map: &map,
            flags: &states,
            charges: &charge_states,
            locked: &locked_areas,
            intel: &intel,
            bleeding: std::array::from_fn(|i| tickets.is_some_and(|k| k.bleed[i] > 0.0)),
            mode,
        };
        let (posture, objectives) = (crate::modes::server_mode(mode.kind).objectives)(&view, team);
        strategy.posture[t] = posture;

        // Squads of the team, where they are, and who leads them.
        let mut squads: Vec<(u8, Option<Vec3>, bool)> = snapshot
            .squads
            .iter()
            .filter(|((squad_team, _), _)| *squad_team == team)
            .map(|((_, squad), info)| (*squad, info.centroid(), !info.leader_is_bot))
            .collect();
        squads.sort_by_key(|(squad, ..)| *squad);
        strategy
            .orders
            .retain(|(order_team, squad), _| *order_team != team || squads.iter().any(|(s, ..)| s == squad));
        // The orders before this plan: squads whose flag was just taken leave a guard.
        let previous: HashMap<u8, SquadOrder> = strategy
            .orders
            .iter()
            .filter(|((order_team, _), _)| *order_team == team)
            .map(|((_, squad), order)| (*squad, *order))
            .collect();
        // Flags of ours the enemy is taking right now.
        let under_attack = |a: usize| {
            states.get(a).copied().flatten().is_some_and(|s| {
                s.owner == team && (s.rate < 0.0 || (s.flag != team && s.height < 1.0))
            })
        };

        let mut load: HashMap<usize, f32> = HashMap::default();
        let weight = |human: bool| if human { 0.5 } else { 1.0 };
        // In the staged modes things change fast (a charge armed: 30 s to defuse it): an
        // order is only kept while its objective is worth a good part of the best one.
        let best_value = objectives.first().map_or(0.0, |o| o.value);
        // ... except the only squad defending an objective: it stays (see `cover_defences`).
        let mut defenders: HashMap<usize, u32> = HashMap::default();
        for ((order_team, _), order) in &strategy.orders {
            if *order_team == team && order.kind == OrderKind::Defend && !order.suggestion {
                *defenders.entry(order.area).or_default() += 1;
            }
        }
        let still_valid = |order: &SquadOrder| {
            let sole_defender = order.kind == OrderKind::Defend && !order.suggestion && defenders.get(&order.area) == Some(&1);
            objectives.iter().any(|o| {
                o.area == order.area
                    && o.kind == order.kind
                    && (!mode.staged() || o.value >= 0.4 * best_value || sole_defender)
            })
        };
        // A human commander's orders go to the bot-led squads as they are.
        let human_commander = commanders.iter().any(|(t, player)| *t == team && !player.is_bot);
        let mut open = Vec::new();
        for &(squad, position, human) in &squads {
            let given = commander_orders
                .iter()
                .find(|o| human_commander && !human && o.team == team && o.squad == squad)
                .and_then(|o| commanded_order(&map, &states, team, o));
            if let Some(mut order) = given {
                if let Some(current) = strategy.orders.get(&(team, squad))
                    && current.commanded
                    && current.area == order.area
                    && current.kind == order.kind
                    && current.point == order.point
                {
                    order.age = current.age;
                }
                *load.entry(order.area).or_default() += 1.0;
                strategy.orders.insert((team, squad), order);
                continue;
            }
            // Recent orders that still make sense stay; they count first.
            match strategy.orders.get(&(team, squad)) {
                Some(order) if order.commanded => open.push((squad, position, human)),
                Some(order) if order.age < MIN_ORDER_AGE && still_valid(order) => {
                    *load.entry(order.area).or_default() += weight(human);
                }
                _ => open.push((squad, position, human)),
            }
        }
        let fallback = snapshot.soldiers[t]
            .iter()
            .map(|s| s.position)
            .reduce(|a, b| a + b)
            .map(|sum| sum / snapshot.soldiers[t].len() as f32);
        for (squad, position, human) in open {
            let current = strategy.orders.get(&(team, squad)).copied();
            let from = position.or(fallback);
            // A squad that just took its flag moves on (leaving a guard if need be) unless
            // the enemy is taking it back already.
            let took = current
                .filter(|c| c.kind == OrderKind::Attack && !c.commanded)
                .map(|c| c.area)
                .filter(|&a| states.get(a).copied().flatten().is_some_and(|s| s.owner == team) && !under_attack(a));
            let best = objectives.iter().filter(|o| Some(o.area) != took).max_by(|a, b| {
                let score = |o: &Objective| {
                    let distance = from.map_or(0.0, |p| p.distance(map.areas[o.area].position));
                    let same = current.is_some_and(|c| c.area == o.area && c.kind == o.kind);
                    // Defences need one squad; attacks take several.
                    let crowding = match o.kind {
                        OrderKind::Attack => 0.6,
                        OrderKind::Defend => 2.0,
                    };
                    o.value / (1.0 + distance / 500.0)
                        / (1.0 + crowding * load.get(&o.area).copied().unwrap_or(0.0))
                        * if same { 1.25 } else { 1.0 }
                };
                score(a).total_cmp(&score(b))
            });
            match best {
                Some(best) => {
                    *load.entry(best.area).or_default() += weight(human);
                    let same = current.is_some_and(|c| c.area == best.area && c.kind == best.kind);
                    let order = SquadOrder {
                        kind: best.kind,
                        area: best.area,
                        age: if same { current.map_or(0.0, |c| c.age) } else { 0.0 },
                        suggestion: human,
                        commanded: false,
                        point: None,
                    };
                    strategy.orders.insert((team, squad), order);
                }
                None => {
                    strategy.orders.remove(&(team, squad));
                }
            }
        }
        // A flag the enemy is taking with nobody sent to it: the nearest bot-led squad that
        // isn't holding one itself comes back (conquest; the staged modes cover their
        // defences below).
        if !mode.staged() {
            for objective in objectives.iter().filter(|o| o.kind == OrderKind::Defend && under_attack(o.area)) {
                if load.get(&objective.area).copied().unwrap_or(0.0) > 0.0 {
                    continue;
                }
                let at = map.areas[objective.area].position;
                let rescuer = squads
                    .iter()
                    .filter(|(_, _, human)| !human)
                    .filter_map(|&(squad, position, _)| {
                        let order = strategy.orders.get(&(team, squad))?;
                        let free = !order.commanded && order.age > RESCUE_MIN_AGE && !under_attack(order.area);
                        let distance = position?.distance(at);
                        (free && distance < RESCUE_DISTANCE).then_some((squad, distance))
                    })
                    .min_by(|a, b| a.1.total_cmp(&b.1));
                if let Some((squad, _)) = rescuer {
                    let order = SquadOrder {
                        kind: OrderKind::Defend,
                        area: objective.area,
                        age: 0.0,
                        suggestion: false,
                        commanded: false,
                        point: None,
                    };
                    strategy.orders.insert((team, squad), order);
                    *load.entry(objective.area).or_default() += 1.0;
                }
            }
        }
        for ((order_team, squad), order) in strategy.orders.iter_mut() {
            if *order_team == team {
                order.suggestion = squads.iter().any(|(s, _, human)| s == squad && *human);
            }
        }
        if mode.staged() {
            cover_defences(&mut strategy.orders, team, &objectives, &squads, &map);
        }
        if mode.kind != game_data::modes::ModeKind::Rush {
            let needs: Vec<u8> = (0..map.areas.len())
                .map(|a| guard_need(&map, &states, &locked_areas, team, &intel, a))
                .collect();
            let strategy = &mut *strategy;
            update_guards(&mut strategy.guards, team, &strategy.orders, &previous, &snapshot.squads, &needs, &map);
        }
        strategy.objectives[t] = objectives;
    }
}

/// Staged modes (Rush, Breakthrough): a squad on every objective the team defends (both
/// charges of a stage, every flag of the open sector) while it has squads enough. Bot-led
/// squads move from objectives two or more squads were sent to onto uncovered ones, the
/// closest first; a human commander's orders stay.
fn cover_defences(
    orders: &mut HashMap<(Team, u8), SquadOrder>,
    team: Team,
    objectives: &[Objective],
    squads: &[(u8, Option<Vec3>, bool)],
    map: &StrategicMap,
) {
    loop {
        let mut count: HashMap<usize, u32> = HashMap::default();
        for ((order_team, _), order) in orders.iter() {
            if *order_team == team && !order.suggestion {
                *count.entry(order.area).or_default() += 1;
            }
        }
        let Some(uncovered) = objectives
            .iter()
            .find(|o| o.kind == OrderKind::Defend && !count.contains_key(&o.area))
        else {
            return;
        };
        let at = map.areas[uncovered.area].position;
        let donor = squads
            .iter()
            .filter(|(_, _, human)| !human)
            .filter_map(|&(squad, position, _)| {
                let order = orders.get(&(team, squad))?;
                let spare = !order.commanded && count.get(&order.area).is_some_and(|&n| n >= 2);
                spare.then(|| (squad, position.map_or(f32::MAX, |p| p.distance(at))))
            })
            .min_by(|a, b| a.1.total_cmp(&b.1));
        let Some((squad, _)) = donor else {
            return;
        };
        orders.insert(
            (team, squad),
            SquadOrder {
                kind: OrderKind::Defend,
                area: uncovered.area,
                age: 0.0,
                suggestion: false,
                commanded: false,
                point: None,
            },
        );
    }
}

/// Owners of the flags next to area `a`, looking through areas that aren't flags.
fn neighbour_owners(map: &StrategicMap, states: &[Option<FlagState>], a: usize) -> Vec<Team> {
    let owner = |a: usize| states.get(a).copied().flatten().map(|s| s.owner);
    let mut owners = Vec::new();
    for &n in &map.areas[a].neighbours {
        match owner(n) {
            Some(o) => owners.push(o),
            None => owners.extend(map.areas[n].neighbours.iter().filter(|&&m| m != a).filter_map(|&m| owner(m))),
        }
    }
    owners
}

/// How many guards a flag `team` holds needs when the squad that took it moves on: two with
/// enemies gathering, one with an enemy about or when it is a spawn by the enemy and the
/// team holds few flags, none behind the lines.
fn guard_need(
    map: &StrategicMap,
    states: &[Option<FlagState>],
    locked: &[bool],
    team: Team,
    intel: &TeamIntel,
    a: usize,
) -> u8 {
    let area = &map.areas[a];
    let held = states.get(a).copied().flatten().is_some_and(|s| s.owner == team);
    if !held || area.uncapturable || locked.get(a).copied().unwrap_or(false) {
        return 0;
    }
    if !neighbour_owners(map, states, a).iter().any(|&o| o != team) {
        return 0;
    }
    let threat = intel.enemies_near_since(team, area.position, area.radius + 35.0, FRESH_THREAT);
    let held_flags = (0..map.areas.len())
        .filter(|&i| !map.areas[i].uncapturable && states.get(i).copied().flatten().is_some_and(|s| s.owner == team))
        .count();
    match threat {
        t if t as f32 >= DEFEND_THREAT => 2,
        t if t >= 1 => 1,
        _ if area.has_spawns && held_flags <= 2 => 1,
        _ => 0,
    }
}

/// Keeps the guards of `team`'s squads up to date: a bot-led squad whose order moved on from
/// a flag the team holds leaves the members nearest to it there, as many as the flag needs
/// (`needs`, per area) and never more than leaves two moving on, unless another squad is
/// sent there or guards it already. Guards go back to their squad once the flag needs none,
/// the squad is sent back there, they are down, or after [`GUARD_TIME`].
fn update_guards(
    guards: &mut HashMap<(Team, u8), Guard>,
    team: Team,
    orders: &HashMap<(Team, u8), SquadOrder>,
    previous: &HashMap<u8, SquadOrder>,
    squads: &HashMap<(Team, u8), super::squad::SquadInfo>,
    needs: &[u8],
    map: &StrategicMap,
) {
    guards.retain(|&(guard_team, squad), guard| {
        if guard_team != team {
            return true;
        }
        let Some(info) = squads.get(&(team, squad)).filter(|s| s.leader_is_bot) else {
            return false;
        };
        guard.members.retain(|m| info.alive.iter().any(|s| s.player == *m) && info.leader != Some(*m));
        guard.members.truncate(needs.get(guard.area).copied().unwrap_or(0) as usize);
        let back = orders.get(&(team, squad)).is_none_or(|o| o.area == guard.area);
        !guard.members.is_empty() && !back && guard.age < GUARD_TIME
    });
    let mut keys: Vec<(Team, u8)> = orders.keys().copied().filter(|(t, _)| *t == team).collect();
    keys.sort_by_key(|(_, squad)| *squad);
    for key in keys {
        let order = orders[&key];
        let Some(before) = previous.get(&key.1) else {
            continue;
        };
        if order.area == before.area || guards.contains_key(&key) || order.suggestion {
            continue;
        }
        let need = needs.get(before.area).copied().unwrap_or(0) as usize;
        let taken = orders.iter().any(|(k, o)| k.0 == team && *k != key && o.area == before.area && !o.suggestion)
            || guards.iter().any(|(k, g)| k.0 == team && g.area == before.area);
        let Some(info) = squads.get(&key).filter(|s| s.leader_is_bot) else {
            continue;
        };
        if need == 0 || taken {
            continue;
        }
        let area = &map.areas[before.area];
        let mut near: Vec<(Entity, f32)> = info
            .alive
            .iter()
            .filter(|s| info.leader != Some(s.player))
            .map(|s| (s.player, s.position.xz().distance(area.position.xz())))
            .filter(|(_, d)| *d < area.radius + 40.0)
            .collect();
        near.sort_by(|a, b| a.1.total_cmp(&b.1));
        let count = need.min(info.alive.len().saturating_sub(2));
        if count == 0 || near.is_empty() {
            continue;
        }
        let members: Vec<Entity> = near.into_iter().take(count).map(|(m, _)| m).collect();
        guards.insert(key, Guard { area: before.area, members, age: 0.0 });
    }
}

/// A human commander's order as the bots take it: attack or defend the flag it is at, or
/// hold the point it names.
fn commanded_order(
    map: &StrategicMap,
    states: &[Option<FlagState>],
    team: Team,
    order: &CommanderOrder,
) -> Option<SquadOrder> {
    let area = map.nearest(order.position)?;
    let near = map.areas[area].position.xz().distance(order.position.xz()) < map.areas[area].radius + 30.0;
    let held = states.get(area).copied().flatten().is_some_and(|s| s.owner == team);
    let kind = match order.kind {
        CommanderOrderKind::Attack if near && !held => OrderKind::Attack,
        CommanderOrderKind::Move if near && !held => OrderKind::Attack,
        _ => OrderKind::Defend,
    };
    Some(SquadOrder {
        kind,
        area,
        age: 0.0,
        suggestion: false,
        commanded: true,
        point: (!near).then_some(order.position),
    })
}

/// How much each flag is worth attacking or defending for `team`.
fn value_areas(
    map: &StrategicMap,
    states: &[Option<FlagState>],
    team: Team,
    intel: &TeamIntel,
    bleeding: bool,
) -> (Posture, Vec<Objective>) {
    let owner = |a: usize| states[a].map(|s| s.owner);
    // Neighbours, looking through areas that aren't flags.
    let holds_any = (0..map.areas.len()).any(|a| owner(a) == Some(team));
    let contested = (0..map.areas.len())
        .any(|a| !map.areas[a].uncapturable && owner(a).is_some_and(|o| o != team));
    let posture = if contested { Posture::Attack } else { Posture::Guard };

    let mut objectives = Vec::new();
    for (a, area) in map.areas.iter().enumerate() {
        let (Some(state), false) = (states[a], area.uncapturable) else {
            continue;
        };
        let around = neighbour_owners(map, states, a);
        if state.owner != team {
            let threat = intel.enemies_near(team, area.position, area.radius + 35.0) as f32;
            let reachable = !holds_any || around.contains(&team);
            let mut value = if state.owner == Team::Spectator { 12.0 } else { 10.0 };
            // Flags behind enemy lines are rarely worth going round them for.
            value *= if reachable { 1.0 } else { 0.2 };
            if bleeding {
                value *= 1.4;
            }
            if state.flag == team && state.rate > 0.0 {
                // Our flag is going up: finish the job.
                value *= 1.3;
            }
            value /= 1.0 + 0.1 * threat;
            objectives.push(Objective { area: a, kind: OrderKind::Attack, value });
        } else {
            // Like BF2's attack strategy, which defends nothing, flags are only worth a
            // squad while they are being taken or the enemy is gathering next to them (seen
            // there lately: the ones who fell taking it are no threat). With fewer enemies
            // about, the squad that took it leaves a guard ([`Guard`]) and moves on.
            let threat = intel.enemies_near_since(team, area.position, area.radius + 35.0, FRESH_THREAT) as f32;
            let frontline = around.iter().any(|&o| o != team);
            let under_attack = state.rate < 0.0 || (state.flag != team && state.height < 1.0);
            let exposure = match (posture, frontline) {
                (Posture::Guard, true) => 10.0,
                // Defence in depth: squads beyond what the front needs hold the flags behind.
                (Posture::Guard, false) => 4.0,
                (Posture::Attack, true) => 3.0,
                (Posture::Attack, false) => 0.0,
            };
            let pressure = if frontline || under_attack { 1.5 * threat.min(4.0) } else { 0.0 };
            let value = exposure + pressure + if under_attack { 15.0 } else { 0.0 };
            let wanted = match posture {
                Posture::Guard => value >= 4.0,
                Posture::Attack => under_attack || (frontline && threat >= DEFEND_THREAT),
            };
            if wanted {
                objectives.push(Objective { area: a, kind: OrderKind::Defend, value });
            }
        }
    }
    objectives.sort_by(|a, b| b.value.total_cmp(&a.value));
    (posture, objectives)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn point(index: u8, x: f32, z: f32) -> PointInfo<'static> {
        PointInfo {
            index,
            id: ["a", "b", "c", "d", "e"][index as usize],
            name: ["A", "B", "C", "D", "E"][index as usize],
            position: Vec3::new(x, 0.0, z),
            radius: 10.0,
            uncapturable: index == 0,
            has_spawns: true,
        }
    }

    #[test]
    fn derives_lines_of_advance() {
        // A chain A - B - C with D off to the side of C.
        let points = [point(0, 0.0, 0.0), point(1, 0.0, 200.0), point(2, 0.0, 400.0), point(3, 150.0, 450.0)];
        let areas = build_areas(None, &points);
        let names = |a: &Area| {
            let mut n: Vec<&str> = a.neighbours.iter().map(|&i| areas[i].name.as_str()).collect();
            n.sort();
            n
        };
        assert_eq!(names(&areas[0]), ["B"]);
        assert_eq!(names(&areas[1]), ["A", "C"]);
        assert_eq!(names(&areas[2]), ["B", "D"]);
        assert_eq!(names(&areas[3]), ["C"]);
    }

    #[test]
    fn uses_the_levels_areas_and_adds_missing_flags() {
        use game_data::{StrategicAreaDesc, StrategicRouteDesc};
        let area = |name: &str, cp: Option<&str>, z: f32, neighbours: &[&str]| StrategicAreaDesc {
            name: name.into(),
            control_point: cp.map(Into::into),
            position: [0.0, 0.0, z],
            infantry_position: None,
            neighbours: neighbours.iter().map(|n| n.to_string()).collect(),
        };
        let desc = StrategicLayoutDesc {
            mode: "gpm_cq".into(),
            size: 16,
            areas: vec![
                area("CP_A", Some("a"), 0.0, &["CP_B"]),
                area("CP_B", Some("b"), 200.0, &["CP_A", "FLANK"]),
                area("FLANK", None, 300.0, &["CP_B"]),
            ],
            routes: vec![StrategicRouteDesc {
                from: "CP_A".into(),
                to: "CP_B".into(),
                waypoints: vec![[50.0, 0.0, 100.0]],
            }],
        };
        let points = [point(0, 0.0, 0.0), point(1, 0.0, 200.0), point(2, 0.0, 420.0)];
        let areas = build_areas(Some(&desc), &points);
        let names: Vec<&str> = areas.iter().map(|a| a.name.as_str()).collect();
        assert_eq!(names, ["A", "B", "FLANK", "C"]);
        assert_eq!(areas[0].neighbours, [1]);
        assert_eq!(areas[0].routes[0].0, 1);
        // C isn't in the level's areas: linked to its nearest neighbour.
        assert_eq!(areas[3].neighbours, [2]);
        assert!(areas[2].neighbours.contains(&3));
    }

    #[test]
    fn takes_a_human_commanders_orders() {
        let points = [point(0, 0.0, 0.0), point(1, 0.0, 200.0)];
        let map = StrategicMap {
            areas: build_areas(None, &points),
            ..default()
        };
        let state = |owner: Team| Some(FlagState { owner, flag: owner, height: 1.0, rate: 0.0 });
        let states = [state(Team::One), state(Team::Two)];
        let order = |kind, position| CommanderOrder { team: Team::One, squad: 1, kind, position };
        // At the enemy's flag: take it; at our own: hold it; anywhere else: hold that point.
        let at_b = commanded_order(&map, &states, Team::One, &order(CommanderOrderKind::Move, Vec3::new(5.0, 0.0, 195.0)))
            .unwrap();
        assert_eq!((at_b.kind, at_b.area, at_b.point), (OrderKind::Attack, 1, None));
        let at_a = commanded_order(&map, &states, Team::One, &order(CommanderOrderKind::Attack, Vec3::ZERO)).unwrap();
        assert_eq!((at_a.kind, at_a.area), (OrderKind::Defend, 0));
        let far = Vec3::new(120.0, 0.0, 100.0);
        let elsewhere = commanded_order(&map, &states, Team::One, &order(CommanderOrderKind::Defend, far)).unwrap();
        assert_eq!((elsewhere.kind, elsewhere.point), (OrderKind::Defend, Some(far)));
        assert!(elsewhere.commanded);
    }

    #[test]
    fn staged_defences_cover_every_objective() {
        let points = [point(0, 0.0, 0.0), point(1, 0.0, 200.0), point(2, 100.0, 200.0)];
        let map = StrategicMap {
            areas: build_areas(None, &points),
            ..default()
        };
        let order = |area| SquadOrder {
            kind: OrderKind::Defend,
            area,
            age: 0.0,
            suggestion: false,
            commanded: false,
            point: None,
        };
        // Three squads all sent to B (under attack); C is uncovered.
        let mut orders: HashMap<(Team, u8), SquadOrder> = HashMap::default();
        for squad in 1..=3 {
            orders.insert((Team::One, squad), order(1));
        }
        let objectives = [
            Objective { area: 1, kind: OrderKind::Defend, value: 23.0 },
            Objective { area: 2, kind: OrderKind::Defend, value: 8.0 },
        ];
        let squads = [
            (1, Some(Vec3::new(0.0, 0.0, 190.0)), false),
            (2, Some(Vec3::new(90.0, 0.0, 190.0)), false),
            (3, Some(Vec3::new(10.0, 0.0, 190.0)), false),
        ];
        cover_defences(&mut orders, Team::One, &objectives, &squads, &map);
        // The squad closest to C goes there; B keeps two.
        assert_eq!(orders[&(Team::One, 2)].area, 2);
        assert_eq!(orders.values().filter(|o| o.area == 1).count(), 2);
        // A lone squad isn't taken off its objective.
        let mut orders: HashMap<(Team, u8), SquadOrder> = HashMap::default();
        orders.insert((Team::One, 1), order(1));
        cover_defences(&mut orders, Team::One, &objectives, &squads[..1], &map);
        assert_eq!(orders[&(Team::One, 1)].area, 1);
    }

    #[test]
    fn values_flags_along_the_front() {
        let points = [point(0, 0.0, 0.0), point(1, 0.0, 200.0), point(2, 0.0, 400.0)];
        let map = StrategicMap {
            areas: build_areas(None, &points),
            ..default()
        };
        let state = |owner: Team| {
            Some(FlagState {
                owner,
                flag: owner,
                height: 1.0,
                rate: 0.0,
            })
        };
        // Team one holds its base and B; C is the enemy's.
        let states = [state(Team::One), state(Team::One), state(Team::Two)];
        let (posture, objectives) = value_areas(&map, &states, Team::One, &TeamIntel::default(), false);
        assert_eq!(posture, Posture::Attack);
        assert_eq!(objectives[0].area, 2);
        assert_eq!(objectives[0].kind, OrderKind::Attack);
        assert!(!objectives.iter().any(|o| o.kind == OrderKind::Defend));
        // B borders the enemy: worth a defence once enemies gather next to it.
        let mut intel = TeamIntel::default();
        let mut world = World::new();
        for i in 0..3 {
            intel.report(Team::One, world.spawn_empty().id(), Vec3::new(i as f32, 0.0, 230.0));
        }
        let (_, objectives) = value_areas(&map, &states, Team::One, &intel, false);
        assert!(objectives.iter().any(|o| o.area == 1 && o.kind == OrderKind::Defend));
        // For team two, B is next to C: attackable at full value.
        let (_, objectives) = value_areas(&map, &states, Team::Two, &TeamIntel::default(), false);
        let b = objectives.iter().find(|o| o.area == 1).unwrap();
        assert_eq!((b.kind, b.value), (OrderKind::Attack, 10.0));
    }

    fn held(owner: Team) -> Option<FlagState> {
        Some(FlagState::held_by(owner))
    }

    #[test]
    fn a_taken_flag_needs_a_squad_only_under_attack() {
        // Team one just took B (next to the enemy's C) in a fight.
        let points = [point(0, 0.0, 0.0), point(1, 0.0, 200.0), point(2, 0.0, 400.0)];
        let mut map = StrategicMap {
            areas: build_areas(None, &points),
            ..default()
        };
        // Its only flag, and one it spawns at: worth a guard in any case.
        let states = [held(Team::One), held(Team::One), held(Team::Two)];
        assert_eq!(guard_need(&map, &states, &[false; 3], Team::One, &TeamIntel::default(), 1), 1);
        map.areas[1].has_spawns = false;
        let states = [held(Team::One), held(Team::One), held(Team::Two)];
        let mut world = World::new();
        let mut intel = TeamIntel::default();
        let dead: Vec<Entity> = (0..3).map(|_| world.spawn_empty().id()).collect();
        for (i, enemy) in dead.iter().enumerate() {
            intel.report(Team::One, *enemy, Vec3::new(i as f32, 0.0, 210.0));
        }
        // Those who fell there are no threat: nothing to defend, B needs no guard.
        intel.forget_unless(|e| !dead.contains(&e));
        let (_, objectives) = value_areas(&map, &states, Team::One, &intel, false);
        assert!(!objectives.iter().any(|o| o.kind == OrderKind::Defend), "{objectives:?}");
        assert_eq!(guard_need(&map, &states, &[false; 3], Team::One, &intel, 1), 0);
        // One enemy about: a guard, not a squad.
        let scout = world.spawn_empty().id();
        intel.report(Team::One, scout, Vec3::new(0.0, 0.0, 230.0));
        let (_, objectives) = value_areas(&map, &states, Team::One, &intel, false);
        assert!(!objectives.iter().any(|o| o.kind == OrderKind::Defend));
        assert_eq!(guard_need(&map, &states, &[false; 3], Team::One, &intel, 1), 1);
        // Seen long ago: gone.
        intel.tick(FRESH_THREAT + 1.0);
        assert_eq!(guard_need(&map, &states, &[false; 3], Team::One, &intel, 1), 0);
        // The enemy taking it back: a squad.
        let mut taken = states;
        taken[1] = Some(FlagState { owner: Team::One, flag: Team::One, height: 0.6, rate: -0.05 });
        let (_, objectives) = value_areas(&map, &taken, Team::One, &intel, false);
        assert!(objectives.iter().any(|o| o.area == 1 && o.kind == OrderKind::Defend && o.value >= 15.0));
        // A flag behind the lines needs no guard.
        let states = [held(Team::One), held(Team::One), held(Team::One)];
        intel.report(Team::One, scout, Vec3::new(0.0, 0.0, 210.0));
        assert_eq!(guard_need(&map, &states, &[false; 3], Team::One, &intel, 1), 0);
    }

    #[test]
    fn a_squad_moving_on_leaves_a_guard() {
        use super::super::squad::{SoldierInfo, SquadInfo};
        let points = [point(0, 0.0, 0.0), point(1, 0.0, 200.0), point(2, 0.0, 400.0)];
        let map = StrategicMap {
            areas: build_areas(None, &points),
            ..default()
        };
        let mut world = World::new();
        let players: Vec<Entity> = (0..5).map(|_| world.spawn_empty().id()).collect();
        let soldier = |player: Entity, z: f32| SoldierInfo {
            player,
            position: Vec3::new(0.0, 0.0, z),
            yaw: 0.0,
            heading: 0.0,
            health: 1.0,
            busy: false,
        };
        // The leader and three members at B, one member further off.
        let alive: Vec<SoldierInfo> = players
            .iter()
            .zip([200.0, 205.0, 190.0, 212.0, 330.0])
            .map(|(p, z)| soldier(*p, z))
            .collect();
        let info = SquadInfo {
            leader: Some(players[0]),
            leader_is_bot: true,
            leader_soldier: Some(alive[0]),
            members: players[1..].to_vec(),
            alive,
        };
        let squads: HashMap<(Team, u8), SquadInfo> = [((Team::One, 1), info)].into_iter().collect();
        let order = |kind, area| SquadOrder { kind, area, age: 0.0, suggestion: false, commanded: false, point: None };
        let previous: HashMap<u8, SquadOrder> = [(1, order(OrderKind::Attack, 1))].into_iter().collect();
        let mut orders: HashMap<(Team, u8), SquadOrder> = [((Team::One, 1), order(OrderKind::Attack, 2))].into_iter().collect();
        let mut guards = HashMap::default();
        // B needs one guard: the member closest to the flag stays, not the leader.
        update_guards(&mut guards, Team::One, &orders, &previous, &squads, &[0, 1, 0], &map);
        assert_eq!(guards[&(Team::One, 1)].members, [players[1]]);
        assert_eq!(guards[&(Team::One, 1)].area, 1);
        // Two with enemies gathering, but never more than leaves two moving on.
        guards.clear();
        update_guards(&mut guards, Team::One, &orders, &previous, &squads, &[0, 2, 0], &map);
        assert_eq!(guards[&(Team::One, 1)].members, [players[1], players[2]]);
        // Once B needs none, they go back to the squad (so as when the squad is sent back).
        let previous: HashMap<u8, SquadOrder> = [(1, order(OrderKind::Attack, 2))].into_iter().collect();
        update_guards(&mut guards, Team::One, &orders, &previous, &squads, &[0, 0, 0], &map);
        assert!(guards.is_empty());
        // Another squad sent to B: no guard.
        let previous: HashMap<u8, SquadOrder> = [(1, order(OrderKind::Attack, 1))].into_iter().collect();
        orders.insert((Team::One, 2), order(OrderKind::Defend, 1));
        update_guards(&mut guards, Team::One, &orders, &previous, &squads, &[0, 1, 0], &map);
        assert!(guards.is_empty());
    }
}
