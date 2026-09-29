//! Vehicle navigation: where land vehicles can drive and boats can sail, with path costs per
//! vehicle class, and how high aircraft must stay.
//!
//! BF2 ships two navmeshes per level (`AIPathFinding/Infantry.qtr` and `Vehicle.qtr`, built by
//! its editor with the GTS library for `maxSlope 20`, `radius 3.5`, `headClearance 3.5`), per
//! vehicle class material costs for ground, road, shallows and deep water
//! (`setVehicleMaterialCost` in `AIBehaviours.ai`) and a flight altitude map
//! (`AerialHeighMap.ahm`, 16 m cells). The runtime files are raw struct dumps and Wake Island
//! has no AI data at all, so like the infantry grid this is built from our own collision:
//!
//! - **land**: a [`NavGrid`] rasterized with vehicle limits (1 m cells, 3 m head room, 0.5 m
//!   steps, 40° slopes, 1.2 m drops). Its distance field is the clearance a vehicle's width
//!   needs, the level's road meshes (BF2's compiled road decals) mark road columns, and the
//!   level's water height gives the depth of every cell.
//! - **water**: a 4 m grid of where boats float: deep enough, and nothing of the level's
//!   collision within a couple of meters of the surface (piers, hulls, bridge pillars).
//! - **air**: the highest point of the terrain and the statics per 16 m cell, like BF2's aerial
//!   height map.
//!
//! [`NavClass`] weighs cells like BF2's material costs: wheeled vehicles prefer roads and stay
//! out of water deeper than their wheels, tracked ones climb steeper slopes and care less for
//! roads, amphibious ones swim, boats need water.

use std::{cmp::Ordering, collections::BinaryHeap, f32::consts::SQRT_2, sync::Arc};

use bevy::{platform::collections::HashMap, prelude::*};

use super::{
    CellRef, NavCell, NavGrid, NavParams, SLOPE_SCALE,
    build::{self, LevelGeometry},
};

/// Land grid cell size, meters.
pub const LAND_CELL: f32 = 1.0;
/// Water grid cell size, meters.
pub const WATER_CELL: f32 = 4.0;
/// Air map cell size, meters (BF2's aerial height map uses 16 m too).
pub const AIR_CELL: f32 = 16.0;

/// Nodes A* may expand on the land grid before settling for the best partial path.
const MAX_EXPANDED: usize = 250_000;
/// The same on the water grid.
const MAX_EXPANDED_WATER: usize = 60_000;
/// How far string pulling looks ahead, in path cells.
const MAX_LOOKAHEAD: usize = 160;
/// Weight of the A* heuristic over the cheapest possible cost: paths cost at most this much
/// more than the best.
const HEURISTIC_WEIGHT: f32 = 1.5;
/// Costs of driving down a ledge, in meters of driving.
const DROP_COST: f32 = 6.0;

/// The limits the land grid is built for.
pub fn land_params() -> NavParams {
    NavParams {
        cell: LAND_CELL,
        voxel: 0.05,
        height: 3.0,
        min_normal_y: 40f32.to_radians().cos(),
        step: 0.5,
        jump: 0.5,
        drop: 1.2,
    }
}

/// How a vehicle gets around, for path costs (BF2's `Tank`, `Car`, `LandingCraft`, `Boat`
/// navigation classes).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum NavClass {
    Wheeled,
    Tracked,
    /// Drives on land and swims (APCs with floaters).
    Amphibious,
    Boat,
}

/// What a path is planned for.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct DriveSpec {
    pub class: NavClass,
    /// Half the vehicle's width, meters.
    pub half_width: f32,
    /// Land vehicles: the deepest water they drive through; boats: their draft.
    pub depth: f32,
}

impl DriveSpec {
    /// Steepest slope (tangent) the class drives up, and where slopes start to cost.
    fn slopes(&self) -> (f32, f32) {
        match self.class {
            NavClass::Tracked => (0.8, 0.35),
            _ => (0.55, 0.2),
        }
    }

    /// Cost factor of road cells (BF2 gives roads a lower material cost for cars).
    fn road_factor(&self) -> f32 {
        match self.class {
            NavClass::Wheeled => 0.5,
            NavClass::Amphibious => 0.6,
            NavClass::Tracked => 0.8,
            NavClass::Boat => 1.0,
        }
    }
}

/// Something to drive around that the grid doesn't know: another vehicle, as a rectangle in
/// XZ (already grown by the driving vehicle's half width).
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Obstacle {
    pub center: Vec2,
    /// Unit vector along its length.
    pub forward: Vec2,
    /// Half its width and half its length.
    pub half: Vec2,
}

impl Obstacle {
    pub fn contains(&self, p: Vec2) -> bool {
        let d = p - self.center;
        d.dot(self.forward).abs() < self.half.y && d.perp_dot(self.forward).abs() < self.half.x
    }
}

/// A path for a vehicle: corners to drive through, starting where it is.
#[derive(Clone, Debug, Default)]
pub struct VehiclePath {
    pub points: Vec<Vec3>,
    /// Whether it reaches the goal; otherwise it gets as close as it could.
    pub complete: bool,
    /// Nodes the search expanded.
    pub expanded: usize,
}

/// Where boats float: a coarse grid over the level's water.
pub struct WaterGrid {
    pub origin: Vec2,
    pub width: u32,
    pub depth: u32,
    pub surface: f32,
    /// Water depth per cell, meters (negative on land): the shallowest point of the cell.
    depths: Vec<f32>,
    /// Something of the level's collision is at the surface.
    blocked: Vec<bool>,
    /// Cells to the nearest cell a 1 m draft can't float in, capped at 255.
    shore: Vec<u8>,
}

/// The highest point of the terrain and statics per [`AIR_CELL`] cell.
pub struct AirMap {
    pub origin: Vec2,
    pub width: u32,
    pub depth: u32,
    heights: Vec<f32>,
}

/// Everything vehicles navigate by. See the module docs.
pub struct VehicleNavGrid {
    pub land: NavGrid,
    /// A bit per land column: covered by a road.
    roads: Vec<u64>,
    pub water_height: Option<f32>,
    pub water: Option<WaterGrid>,
    pub air: Option<AirMap>,
    /// Cell costs and connected areas per kind of vehicle (see [`Self::class_costs`]), made
    /// when first needed.
    classes: std::sync::Mutex<HashMap<(NavClass, u16, u16), Arc<ClassCosts>>>,
}

/// What a kind of vehicle (a [`DriveSpec`]) makes of every land cell.
struct ClassCosts {
    /// Cost factor times [`FACTOR_SCALE`] (0: it can't go there).
    factors: Vec<u8>,
    /// Connected area it can drive around in, [`NO_REGION`] for none.
    regions: Vec<u16>,
}

/// Fixed point scale of [`ClassCosts::factors`].
const FACTOR_SCALE: f32 = 24.0;

/// Cells in no area a vehicle can drive around in.
const NO_REGION: u16 = u16::MAX;
/// Areas of fewer cells (square meters) don't count.
const MIN_DRIVE_REGION: u32 = 200;

/// The vehicle navigation of the loaded level, once built.
#[derive(Resource, Clone)]
pub struct VehicleNavigation(pub Arc<VehicleNavGrid>);

/// What the vehicle grids are built from.
pub struct VehicleGeometry {
    /// Collision, with the area the land grid covers.
    pub geometry: LevelGeometry,
    /// Road triangles, world space.
    pub roads: Vec<[Vec3; 3]>,
    pub water: Option<f32>,
}

/// Builds the land grid (or takes a cached one) and the water grid and air map.
pub fn build_all(input: &VehicleGeometry, land: Option<NavGrid>) -> VehicleNavGrid {
    let land = land.unwrap_or_else(|| build::build(&input.geometry, land_params()));
    let roads = rasterize_roads(&land, &input.roads);
    let water = input.water.and_then(|surface| build_water(&input.geometry, surface));
    let air = build_air(&input.geometry);
    VehicleNavGrid {
        land,
        roads,
        water_height: input.water,
        water,
        air,
        classes: default(),
    }
}

/// Marks the land columns whose middle a road triangle covers.
fn rasterize_roads(grid: &NavGrid, triangles: &[[Vec3; 3]]) -> Vec<u64> {
    let columns = (grid.width * grid.depth) as usize;
    let mut bits = vec![0u64; columns.div_ceil(64)];
    let cell = grid.params.cell;
    for tri in triangles {
        let [a, b, c] = tri.map(|p| p.xz());
        let (lo, hi) = (a.min(b).min(c), a.max(b).max(c));
        let x0 = (((lo.x - grid.origin.x) / cell).floor() as i64).max(0);
        let z0 = (((lo.y - grid.origin.y) / cell).floor() as i64).max(0);
        let x1 = (((hi.x - grid.origin.x) / cell).floor() as i64).min(grid.width as i64 - 1);
        let z1 = (((hi.y - grid.origin.y) / cell).floor() as i64).min(grid.depth as i64 - 1);
        let area = (b - a).perp_dot(c - a);
        if area.abs() < 1e-6 {
            continue;
        }
        for z in z0..=z1 {
            for x in x0..=x1 {
                let p = grid.origin + (Vec2::new(x as f32, z as f32) + 0.5) * cell;
                let u = (b - p).perp_dot(c - p) / area;
                let v = (c - p).perp_dot(a - p) / area;
                let w = 1.0 - u - v;
                // A little outside counts: decals are drawn up to their edges.
                if u > -0.15 && v > -0.15 && w > -0.15 {
                    let i = (z as u32 * grid.width + x as u32) as usize;
                    bits[i / 64] |= 1 << (i % 64);
                }
            }
        }
    }
    bits
}

/// The terrain area (or the grid's bounds without terrain), clamped to `bounds` grown by
/// `margin`.
fn area(geometry: &LevelGeometry, margin: f32) -> Option<(Vec2, Vec2)> {
    let terrain = geometry.terrain.as_ref().map(|t| (t.origin.xz(), t.origin.xz() + t.world_size()));
    match (geometry.bounds, terrain) {
        (Some((lo, hi)), Some((t_lo, t_hi))) => Some(((lo - margin).max(t_lo), (hi + margin).min(t_hi))),
        (Some((lo, hi)), None) => Some((lo - margin, hi + margin)),
        (None, t) => t,
    }
    .filter(|(lo, hi)| lo.x < hi.x && lo.y < hi.y)
}

fn build_water(geometry: &LevelGeometry, surface: f32) -> Option<WaterGrid> {
    let terrain = geometry.terrain.as_ref()?;
    let (lo, hi) = area(geometry, 400.0)?;
    let origin = (lo / WATER_CELL).floor() * WATER_CELL;
    let width = ((hi.x - origin.x) / WATER_CELL).ceil() as u32;
    let depth = ((hi.y - origin.y) / WATER_CELL).ceil() as u32;
    let count = (width * depth) as usize;
    let mut depths = vec![0.0; count];
    for z in 0..depth {
        for x in 0..width {
            let corner = origin + Vec2::new(x as f32, z as f32) * WATER_CELL;
            let mut top = f32::MIN;
            for (u, v) in [(0.0, 0.0), (1.0, 0.0), (0.0, 1.0), (1.0, 1.0), (0.5, 0.5)] {
                let p = corner + Vec2::new(u, v) * WATER_CELL;
                top = top.max(terrain.height_at(p.x, p.y));
            }
            depths[(z * width + x) as usize] = surface - top;
        }
    }
    // Anything solid from a little under the surface to above a boat's deck blocks.
    let (band_lo, band_hi) = (surface - 2.0, surface + 2.5);
    let mut blocked = vec![false; count];
    for mesh in &geometry.meshes {
        let (min, max) = build::world_aabb(mesh);
        if max.y < band_lo || min.y > band_hi || max.x < origin.x || max.z < origin.y {
            continue;
        }
        build::for_each_triangle(&mesh.shape, mesh.transform, &mut |tri| {
            let (mut a, mut b) = ([Vec3::ZERO; 12], [Vec3::ZERO; 12]);
            let n = build::clip(&tri, &mut a, 1, band_lo, true);
            let n = build::clip(&a[..n], &mut b, 1, band_hi, false);
            if n < 2 {
                return;
            }
            let (lo, hi) = b[..n].iter().fold((Vec2::MAX, Vec2::MIN), |(lo, hi), p| (lo.min(p.xz()), hi.max(p.xz())));
            let x0 = (((lo.x - origin.x) / WATER_CELL).floor() as i64).max(0);
            let z0 = (((lo.y - origin.y) / WATER_CELL).floor() as i64).max(0);
            let x1 = (((hi.x - origin.x) / WATER_CELL).floor() as i64).min(width as i64 - 1);
            let z1 = (((hi.y - origin.y) / WATER_CELL).floor() as i64).min(depth as i64 - 1);
            for z in z0..=z1 {
                for x in x0..=x1 {
                    blocked[(z as u32 * width + x as u32) as usize] = true;
                }
            }
        });
    }
    let mut grid = WaterGrid {
        origin,
        width,
        depth,
        surface,
        depths,
        blocked,
        shore: Vec::new(),
    };
    grid.shore = distance_field(width, depth, |i| grid.navigable_index(i, 1.0));
    grid.depths.iter().any(|d| *d > 1.0).then_some(grid)
}

fn build_air(geometry: &LevelGeometry) -> Option<AirMap> {
    let (lo, hi) = area(geometry, 1.0e5)?;
    let origin = (lo / AIR_CELL).floor() * AIR_CELL;
    let width = ((hi.x - origin.x) / AIR_CELL).ceil() as u32;
    let depth = ((hi.y - origin.y) / AIR_CELL).ceil() as u32;
    let mut heights = vec![f32::MIN; (width * depth) as usize];
    if let Some(terrain) = &geometry.terrain {
        let step = terrain.spacing.clamp(1.0, AIR_CELL / 2.0);
        let per = (AIR_CELL / step).ceil() as u32;
        for z in 0..depth {
            for x in 0..width {
                let corner = origin + Vec2::new(x as f32, z as f32) * AIR_CELL;
                let mut top = f32::MIN;
                for j in 0..=per {
                    for i in 0..=per {
                        let p = corner + Vec2::new(i as f32, j as f32) * (AIR_CELL / per as f32);
                        top = top.max(terrain.height_at(p.x, p.y));
                    }
                }
                heights[(z * width + x) as usize] = top;
            }
        }
    }
    for mesh in &geometry.meshes {
        build::for_each_triangle(&mesh.shape, mesh.transform, &mut |tri| {
            let top = tri[0].y.max(tri[1].y).max(tri[2].y);
            let lo = tri[0].xz().min(tri[1].xz()).min(tri[2].xz());
            let hi = tri[0].xz().max(tri[1].xz()).max(tri[2].xz());
            let x0 = (((lo.x - origin.x) / AIR_CELL).floor() as i64).max(0);
            let z0 = (((lo.y - origin.y) / AIR_CELL).floor() as i64).max(0);
            let x1 = (((hi.x - origin.x) / AIR_CELL).floor() as i64).min(width as i64 - 1);
            let z1 = (((hi.y - origin.y) / AIR_CELL).floor() as i64).min(depth as i64 - 1);
            for z in z0..=z1 {
                for x in x0..=x1 {
                    let h = &mut heights[(z as u32 * width + x as u32) as usize];
                    *h = h.max(top);
                }
            }
        });
    }
    Some(AirMap {
        origin,
        width,
        depth,
        heights,
    })
}

/// Chessboard distance (in cells, capped at 255) of every cell to the nearest cell that
/// isn't `open`.
fn distance_field(width: u32, depth: u32, open: impl Fn(usize) -> bool) -> Vec<u8> {
    let count = (width * depth) as usize;
    let mut dist: Vec<u8> = (0..count).map(|i| if open(i) { 255 } else { 0 }).collect();
    let mut queue: std::collections::VecDeque<usize> = (0..count).filter(|&i| dist[i] == 0).collect();
    while let Some(i) = queue.pop_front() {
        let (x, z) = ((i as u32 % width) as i64, (i as u32 / width) as i64);
        let next = dist[i].saturating_add(1);
        for (dx, dz) in [(1, 0), (-1, 0), (0, 1), (0, -1), (1, 1), (1, -1), (-1, 1), (-1, -1)] {
            let (nx, nz) = (x + dx, z + dz);
            if nx < 0 || nz < 0 || nx >= width as i64 || nz >= depth as i64 {
                continue;
            }
            let j = (nz as u32 * width + nx as u32) as usize;
            if dist[j] > next {
                dist[j] = next;
                queue.push_back(j);
            }
        }
    }
    dist
}

impl WaterGrid {
    fn index(&self, p: Vec3) -> Option<usize> {
        let c = ((p.xz() - self.origin) / WATER_CELL).floor();
        (c.x >= 0.0 && c.y >= 0.0 && c.x < self.width as f32 && c.y < self.depth as f32)
            .then(|| (c.y as u32 * self.width + c.x as u32) as usize)
    }

    fn position(&self, i: usize) -> Vec3 {
        let (x, z) = (i as u32 % self.width, i as u32 / self.width);
        let p = self.origin + (Vec2::new(x as f32, z as f32) + 0.5) * WATER_CELL;
        Vec3::new(p.x, self.surface, p.y)
    }

    fn navigable_index(&self, i: usize, draft: f32) -> bool {
        !self.blocked[i] && self.depths[i] >= draft
    }

    /// Whether a boat with this draft floats at `p`.
    pub fn navigable(&self, p: Vec3, draft: f32) -> bool {
        self.index(p).is_some_and(|i| self.navigable_index(i, draft))
    }

    /// Water depth at `p` (negative on land).
    pub fn depth_at(&self, p: Vec3) -> Option<f32> {
        self.index(p).map(|i| self.depths[i])
    }

    /// Meters to the nearest shore or obstacle (for a 1 m draft), roughly.
    pub fn shore_distance(&self, p: Vec3) -> f32 {
        self.index(p).map_or(0.0, |i| self.shore[i] as f32 * WATER_CELL)
    }

    /// The navigable cell nearest to `p` within `radius`, preferring open water.
    fn nearest(&self, p: Vec3, radius: f32, draft: f32) -> Option<usize> {
        let r = (radius / WATER_CELL).ceil() as i64;
        let c = ((p.xz() - self.origin) / WATER_CELL).floor();
        let (cx, cz) = (c.x as i64, c.y as i64);
        let mut best: Option<(f32, usize)> = None;
        for z in (cz - r).max(0)..=(cz + r).min(self.depth as i64 - 1) {
            for x in (cx - r).max(0)..=(cx + r).min(self.width as i64 - 1) {
                let i = (z as u32 * self.width + x as u32) as usize;
                if !self.navigable_index(i, draft) {
                    continue;
                }
                let d = self.position(i).xz().distance(p.xz());
                let score = d + if self.shore[i] < 2 { 10.0 } else { 0.0 };
                if d <= radius && best.is_none_or(|(b, _)| score < b) {
                    best = Some((score, i));
                }
            }
        }
        best.map(|(_, i)| i)
    }

    /// Where a boat gets closest to `target` (on land or water), within `radius`.
    pub fn landing(&self, target: Vec3, radius: f32, draft: f32) -> Option<Vec3> {
        self.nearest(target, radius, draft).map(|i| self.position(i))
    }

    fn line_clear(&self, a: usize, b: usize, draft: f32) -> bool {
        let (pa, pb) = (self.position(a), self.position(b));
        let steps = (pa.xz().distance(pb.xz()) / (WATER_CELL * 0.5)).ceil().max(1.0) as usize;
        (0..=steps).all(|s| {
            let p = pa.lerp(pb, s as f32 / steps as f32);
            self.index(p).is_some_and(|i| self.navigable_index(i, draft) && self.shore[i] >= 1)
        })
    }

    /// A* over the water grid for a boat with this draft.
    pub fn find_path(&self, from: Vec3, to: Vec3, draft: f32) -> Option<VehiclePath> {
        let start = self.nearest(from, 60.0, draft)?;
        let goal = self.nearest(to, 30.0, draft);
        let target = goal.map_or(to, |g| self.position(g));
        let heuristic = |i: usize| self.position(i).xz().distance(target.xz());
        let cost = |i: usize| match self.shore[i] {
            0..=1 => 3.0,
            2..=3 => 1.4,
            _ => 1.0,
        };
        let (cells, complete, expanded) = astar(
            start,
            goal,
            heuristic,
            |i, out: &mut Vec<(usize, f32)>| {
                let (x, z) = ((i as u32 % self.width) as i64, (i as u32 / self.width) as i64);
                for (dx, dz) in [(1, 0), (-1, 0), (0, 1), (0, -1), (1, 1), (1, -1), (-1, 1), (-1, -1)] {
                    let (nx, nz) = (x + dx, z + dz);
                    if nx < 0 || nz < 0 || nx >= self.width as i64 || nz >= self.depth as i64 {
                        continue;
                    }
                    let j = (nz as u32 * self.width + nx as u32) as usize;
                    if !self.navigable_index(j, draft) {
                        continue;
                    }
                    let diagonal = dx != 0 && dz != 0;
                    if diagonal {
                        let side_a = (z as u32 * self.width + nx as u32) as usize;
                        let side_b = (nz as u32 * self.width + x as u32) as usize;
                        if !self.navigable_index(side_a, draft) || !self.navigable_index(side_b, draft) {
                            continue;
                        }
                    }
                    let length = if diagonal { SQRT_2 } else { 1.0 } * WATER_CELL;
                    out.push((j, length * cost(j)));
                }
            },
            MAX_EXPANDED_WATER,
        );
        let mut points = vec![self.position(cells[0])];
        let mut i = 0;
        while i + 1 < cells.len() {
            let mut best = i + 1;
            for k in i + 2..cells.len().min(i + MAX_LOOKAHEAD) {
                if self.line_clear(cells[i], cells[k], draft) {
                    best = k;
                }
            }
            points.push(self.position(cells[best]));
            i = best;
        }
        Some(VehiclePath { points, complete, expanded })
    }
}

impl AirMap {
    /// The highest point within `radius` of `p` (terrain and statics).
    pub fn floor(&self, p: Vec3, radius: f32) -> f32 {
        let lo = ((p.xz() - radius - self.origin) / AIR_CELL).floor();
        let hi = ((p.xz() + radius - self.origin) / AIR_CELL).floor();
        let mut top = f32::MIN;
        for z in (lo.y.max(0.0) as u32)..=(hi.y.min(self.depth as f32 - 1.0).max(0.0) as u32) {
            for x in (lo.x.max(0.0) as u32)..=(hi.x.min(self.width as f32 - 1.0).max(0.0) as u32) {
                top = top.max(self.heights[(z * self.width + x) as usize]);
            }
        }
        top
    }
}

/// A* over nodes of any kind: `neighbours(n, out)` adds `(next, cost)`. Returns the nodes from
/// `start` to the goal (or to the node closest to it by the heuristic) and whether it got
/// there.
fn astar<K: Copy + Eq + std::hash::Hash>(
    start: K,
    goal: Option<K>,
    heuristic: impl Fn(K) -> f32,
    mut neighbours: impl FnMut(K, &mut Vec<(K, f32)>),
    budget: usize,
) -> (Vec<K>, bool, usize) {
    struct Open<K>(f32, K);
    impl<K> PartialEq for Open<K> {
        fn eq(&self, other: &Self) -> bool {
            self.0 == other.0
        }
    }
    impl<K> Eq for Open<K> {}
    impl<K> Ord for Open<K> {
        fn cmp(&self, other: &Self) -> Ordering {
            other.0.total_cmp(&self.0)
        }
    }
    impl<K> PartialOrd for Open<K> {
        fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
            Some(self.cmp(other))
        }
    }
    // g, parent, closed
    let mut nodes: HashMap<K, (f32, Option<K>, bool)> = HashMap::with_capacity(4096);
    let mut open = BinaryHeap::new();
    nodes.insert(start, (0.0, None, false));
    open.push(Open(heuristic(start), start));
    let mut closest = (heuristic(start), start);
    let mut end = None;
    let mut expanded = 0;
    let mut next = Vec::with_capacity(8);
    while let Some(Open(_, key)) = open.pop() {
        let node = nodes[&key];
        if node.2 {
            continue;
        }
        nodes.get_mut(&key).unwrap().2 = true;
        if goal == Some(key) {
            end = Some(key);
            break;
        }
        let h = heuristic(key);
        if h < closest.0 {
            closest = (h, key);
        }
        expanded += 1;
        if expanded > budget {
            break;
        }
        next.clear();
        neighbours(key, &mut next);
        for &(to, cost) in &next {
            let g = node.0 + cost;
            if nodes.get(&to).is_none_or(|n| !n.2 && g < n.0) {
                nodes.insert(to, (g, Some(key), false));
                open.push(Open(g + heuristic(to), to));
            }
        }
    }
    let complete = end.is_some();
    let mut key = Some(end.unwrap_or(closest.1));
    let mut out = Vec::new();
    while let Some(k) = key {
        out.push(k);
        key = nodes[&k].1;
    }
    out.reverse();
    (out, complete, expanded)
}

impl VehicleNavGrid {
    fn road_index(&self, c: CellRef) -> bool {
        let i = (c.z * self.land.width + c.x) as usize;
        self.roads.get(i / 64).is_some_and(|w| w & (1 << (i % 64)) != 0)
    }

    /// Whether `p` is on a road.
    pub fn on_road(&self, p: Vec3) -> bool {
        self.land
            .column_at(p.x, p.z)
            .is_some_and(|(x, z)| self.road_index(CellRef { x, z, index: 0 }))
    }

    /// Road columns, for statistics.
    pub fn road_columns(&self) -> usize {
        self.roads.iter().map(|w| w.count_ones() as usize).sum()
    }

    /// Water depth over a land cell (negative when dry).
    fn depth(&self, cell: &NavCell) -> f32 {
        self.water_height.map_or(-100.0, |w| w - cell.y)
    }

    /// Meters from a cell's middle to the nearest obstacle or ledge, roughly.
    fn clearance(&self, cell: &NavCell) -> f32 {
        (cell.dist as f32 * 0.5 + 0.5) * self.land.params.cell
    }

    /// The cost factor of driving onto a land cell (1 = open ground), `None` where the class
    /// can't go (or something is in the way: `obstacles`, circles in XZ).
    fn factor(&self, spec: &DriveSpec, obstacles: &[Obstacle], c: CellRef) -> Option<f32> {
        let cell = self.land.cell(c);
        if cell.region == 0 {
            return None;
        }
        if !obstacles.is_empty() {
            let at = self.land.position(c).xz();
            if obstacles.iter().any(|o| o.contains(at)) {
                return None;
            }
        }
        let need = spec.half_width + 0.25;
        let clearance = self.clearance(cell);
        if clearance < need {
            return None;
        }
        let mut factor = if clearance < need + 1.5 { 1.8 } else { 1.0 };
        let tan = cell.slope as f32 / SLOPE_SCALE;
        let (max, soft) = spec.slopes();
        if tan > max {
            return None;
        }
        factor *= 1.0 + 3.0 * (tan - soft).max(0.0);
        let depth = self.depth(cell);
        match spec.class {
            NavClass::Boat => return None,
            NavClass::Amphibious => {
                if depth > 0.4 {
                    factor *= 1.6;
                }
            }
            _ => {
                if depth > spec.depth {
                    return None;
                }
                if depth > 0.1 {
                    factor *= 2.5;
                }
            }
        }
        if self.road_index(c) {
            factor *= spec.road_factor();
        }
        Some(factor)
    }

    /// Whether a vehicle can be at `p`.
    pub fn passable(&self, spec: &DriveSpec, p: Vec3) -> bool {
        if spec.class == NavClass::Boat {
            return self.water.as_ref().is_some_and(|w| w.navigable(p, spec.depth));
        }
        self.land.locate(p, 1.5, None).is_some_and(|c| self.factor(spec, &[], c).is_some())
    }

    /// The land cell under a vehicle whose hull origin is at `p`.
    fn locate_vehicle(&self, p: Vec3) -> Option<CellRef> {
        // The hull origin is somewhere above the ground.
        let feet = p - Vec3::Y * 1.0;
        self.land.locate(feet, 3.0, None).or_else(|| self.land.locate(feet, 8.0, None))
    }

    /// Cell costs and connected areas for a kind of vehicle: cells it fits on, joined where
    /// it can drive from one to the other (either way); areas too small to matter get
    /// [`NO_REGION`]. Made once per kind of vehicle (a few hundred ms), then path requests
    /// only snap to goals they can reach and read costs from a byte per cell.
    fn class_costs(&self, spec: &DriveSpec) -> Arc<ClassCosts> {
        let key = (spec.class, (spec.half_width * 10.0).round() as u16, (spec.depth * 10.0).round() as u16);
        if let Some(costs) = self.classes.lock().unwrap().get(&key) {
            return costs.clone();
        }
        let grid = &self.land;
        let count = grid.cells.len();
        let mut factors = vec![0u8; count];
        for z in 0..grid.depth {
            for x in 0..grid.width {
                for index in grid.column(x, z) {
                    if let Some(f) = self.factor(spec, &[], CellRef { x, z, index }) {
                        factors[index as usize] = (f * FACTOR_SCALE).round().clamp(1.0, 255.0) as u8;
                    }
                }
            }
        }
        let mut parent: Vec<u32> = (0..count as u32).collect();
        fn root(parent: &mut [u32], mut i: u32) -> u32 {
            while parent[i as usize] != i {
                parent[i as usize] = parent[parent[i as usize] as usize];
                i = parent[i as usize];
            }
            i
        }
        for z in 0..grid.depth {
            for x in 0..grid.width {
                for index in grid.column(x, z) {
                    if factors[index as usize] == 0 {
                        continue;
                    }
                    let c = CellRef { x, z, index };
                    // +X and +Z: every pair once.
                    for dir in 0..2 {
                        if let Some(n) = grid.neighbour(c, dir)
                            && factors[n.index as usize] != 0
                        {
                            let (a, b) = (root(&mut parent, index), root(&mut parent, n.index));
                            parent[a.max(b) as usize] = a.min(b);
                        }
                    }
                }
            }
        }
        let mut sizes = vec![0u32; count];
        for i in 0..count as u32 {
            if factors[i as usize] != 0 {
                sizes[root(&mut parent, i) as usize] += 1;
            }
        }
        let mut ids: HashMap<u32, u16> = HashMap::default();
        let mut regions = vec![NO_REGION; count];
        for i in 0..count as u32 {
            if factors[i as usize] == 0 {
                continue;
            }
            let r = root(&mut parent, i);
            if sizes[r as usize] < MIN_DRIVE_REGION {
                continue;
            }
            let next = ids.len().min(NO_REGION as usize - 1) as u16;
            regions[i as usize] = *ids.entry(r).or_insert(next);
        }
        let costs = Arc::new(ClassCosts { factors, regions });
        self.classes.lock().unwrap().insert(key, costs.clone());
        costs
    }

    /// A cell's cost factor from the class costs, `None` where it can't go or an obstacle is.
    fn cost(&self, costs: &ClassCosts, obstacles: &[Obstacle], c: CellRef) -> Option<f32> {
        let q = costs.factors[c.index as usize];
        if q == 0 {
            return None;
        }
        if !obstacles.is_empty() {
            let at = self.land.position(c).xz();
            if obstacles.iter().any(|o| o.contains(at)) {
                return None;
            }
        }
        Some(q as f32 / FACTOR_SCALE)
    }

    /// The passable cell nearest to `p` within `radius`, in `region`.
    fn locate_passable(&self, costs: &ClassCosts, obstacles: &[Obstacle], p: Vec3, radius: f32, region: u16) -> Option<CellRef> {
        let mut best: Option<(f32, CellRef)> = None;
        for c in self.land.cells_near(p.xz(), radius) {
            if costs.regions[c.index as usize] != region {
                continue;
            }
            let at = self.land.position(c);
            let dy = at.y - p.y;
            let score = at.xz().distance_squared(p.xz()) + 2.0 * dy * dy;
            if best.is_some_and(|(b, _)| score >= b) || self.cost(costs, obstacles, c).is_none() {
                continue;
            }
            best = Some((score, c));
        }
        best.map(|(_, c)| c)
    }

    /// A path for a vehicle from `from` (its hull origin) towards `to`. The goal snaps to the
    /// nearest place the vehicle can get to within `snap` meters (looking further out if there
    /// is none: as close as it gets). `obstacles` (other vehicles) are driven around, unless
    /// the vehicle is in one already. `None` if the vehicle isn't anywhere on the grid.
    pub fn find_path(
        &self,
        spec: &DriveSpec,
        from: Vec3,
        to: Vec3,
        snap: f32,
        obstacles: &[Obstacle],
    ) -> Option<VehiclePath> {
        if spec.class == NavClass::Boat {
            return self.water.as_ref()?.find_path(from, to, spec.depth);
        }
        let obstacles: Vec<Obstacle> = obstacles.iter().copied().filter(|o| !o.contains(from.xz())).collect();
        let obstacles = &obstacles[..];
        let start = self.locate_vehicle(from)?;
        let costs = self.class_costs(spec);
        // The area it can drive around in: the start's, or (wedged somewhere it doesn't fit)
        // the nearest one around it.
        let region = match costs.regions[start.index as usize] {
            NO_REGION => self
                .land
                .cells_near(from.xz(), 6.0)
                .filter(|c| costs.regions[c.index as usize] != NO_REGION)
                .min_by(|a, b| {
                    let d = |c: &CellRef| self.land.position(*c).distance_squared(from);
                    d(a).total_cmp(&d(b))
                })
                .map_or(NO_REGION, |c| costs.regions[c.index as usize]),
            region => region,
        };
        let goal = (region != NO_REGION)
            .then(|| {
                [snap, snap * 2.5, snap * 6.0]
                    .into_iter()
                    .find_map(|radius| self.locate_passable(&costs, obstacles, to, radius, region))
            })
            .flatten();
        let target = goal.map_or(to, |g| self.land.position(g));
        let budget = if goal.is_some() { MAX_EXPANDED } else { MAX_EXPANDED / 4 };
        let (cells, reached, expanded) = self.search(spec, &costs, obstacles, start, goal, target, budget);
        // Reaching a goal that stood in for an unreachable one is as good as it gets.
        let complete = reached && goal.is_some_and(|g| self.land.position(g).xz().distance(to.xz()) <= snap + 1.0);
        Some(VehiclePath {
            points: self.smooth(&costs, obstacles, &cells),
            complete,
            expanded,
        })
    }

    /// Weighted A* over the land grid with dense arrays (no hashing: the grids have millions
    /// of cells, paths expand tens of thousands). Returns the cells from `start` to the goal
    /// (or to the one closest to `target`), whether it got there, and the nodes expanded.
    #[allow(clippy::too_many_arguments)]
    fn search(
        &self,
        spec: &DriveSpec,
        costs: &ClassCosts,
        obstacles: &[Obstacle],
        start: CellRef,
        goal: Option<CellRef>,
        target: Vec3,
        budget: usize,
    ) -> (Vec<CellRef>, bool, usize) {
        #[derive(PartialEq)]
        struct Open(f32, CellRef);
        impl Eq for Open {}
        impl Ord for Open {
            fn cmp(&self, other: &Self) -> Ordering {
                other.0.total_cmp(&self.0)
            }
        }
        impl PartialOrd for Open {
            fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
                Some(self.cmp(other))
            }
        }
        let grid = &self.land;
        let cell = grid.params.cell;
        let count = grid.cells.len();
        let (tx, tz) = grid
            .column_at(target.x, target.z)
            .map_or((-1.0, -1.0), |(x, z)| (x as f32, z as f32));
        // 1.5 times the cheapest possible cost (all road), so paths still find roads that are
        // worth a detour without searching the whole map.
        let weight = (spec.road_factor() * HEURISTIC_WEIGHT).min(1.0) * cell;
        let heuristic = |c: CellRef| {
            let (dx, dz) = ((c.x as f32 - tx).abs(), (c.z as f32 - tz).abs());
            (dx.max(dz) + (SQRT_2 - 1.0) * dx.min(dz)) * weight
        };
        // Zeroed allocations cost nothing until touched. `parent` is the parent's index plus
        // one (0: not reached yet); `column` packs the cell's column (x, z).
        let mut g = vec![0.0f32; count];
        let mut parent = vec![0u32; count];
        let mut column = vec![0u32; count];
        let mut closed = vec![false; count];
        let pack = |c: CellRef| c.x | (c.z << 16);
        let unpack = |index: u32, packed: u32| CellRef {
            x: packed & 0xFFFF,
            z: packed >> 16,
            index,
        };
        let drop_cost = match spec.class {
            NavClass::Tracked => DROP_COST * 0.5,
            _ => DROP_COST,
        } / cell;
        let step = |from: &NavCell, to: CellRef| -> Option<f32> {
            let b = grid.cell(to);
            let dy = b.y - from.y;
            let climb = grid.walk_climb(from, b);
            if dy > climb {
                return None;
            }
            let factor = self.cost(costs, obstacles, to)?;
            Some(if dy < -climb { factor + drop_cost } else { factor })
        };
        let s = start.index as usize;
        parent[s] = u32::MAX;
        column[s] = pack(start);
        let mut open = BinaryHeap::new();
        open.push(Open(heuristic(start), start));
        let mut closest = (heuristic(start), start);
        let mut end = None;
        let mut expanded = 0;
        let mut moves: Vec<(CellRef, f32)> = Vec::with_capacity(8);
        while let Some(Open(_, here)) = open.pop() {
            let i = here.index as usize;
            if closed[i] {
                continue;
            }
            closed[i] = true;
            if goal == Some(here) {
                end = Some(here);
                break;
            }
            let h = heuristic(here);
            if h < closest.0 {
                closest = (h, here);
            }
            expanded += 1;
            if expanded > budget {
                break;
            }
            moves.clear();
            let a = *grid.cell(here);
            let straight: [Option<CellRef>; 4] = std::array::from_fn(|dir| grid.neighbour(here, dir));
            for dir in 0..4 {
                let Some(n) = straight[dir] else {
                    continue;
                };
                let direct = step(&a, n);
                if let Some(f) = direct {
                    moves.push((n, f * cell));
                }
                let next = (dir + 1) % 4;
                if let (Some(_), Some(p), Some(q)) = (direct, straight[dir], straight[next])
                    && let (Some(d1), Some(d2)) = (grid.neighbour(p, next), grid.neighbour(q, dir))
                    && d1.index == d2.index
                    && step(&a, q).is_some()
                    && let Some(f) = step(grid.cell(p), d1)
                {
                    moves.push((d1, f * cell * SQRT_2));
                }
            }
            let base = g[i];
            for &(to, cost) in &moves {
                let j = to.index as usize;
                if closed[j] {
                    continue;
                }
                let next = base + cost;
                if parent[j] == 0 || next < g[j] {
                    g[j] = next;
                    parent[j] = here.index + 1;
                    column[j] = pack(to);
                    open.push(Open(next + heuristic(to), to));
                }
            }
        }
        let reached = end.is_some();
        let mut at = end.unwrap_or(closest.1);
        let mut out = vec![at];
        loop {
            let p = parent[at.index as usize];
            if p == u32::MAX || p == 0 {
                break;
            }
            let index = p - 1;
            at = unpack(index, column[index as usize]);
            out.push(at);
        }
        out.reverse();
        (out, reached, expanded)
    }

    /// String pulling: keeps the cells where the path has to turn, where a straight drive to
    /// a later cell would leave passable ground or cross worse ground than the path does.
    fn smooth(&self, costs: &ClassCosts, obstacles: &[Obstacle], cells: &[CellRef]) -> Vec<Vec3> {
        let mut points = vec![self.land.position(cells[0])];
        let factors: Vec<f32> = cells.iter().map(|&c| self.cost(costs, obstacles, c).unwrap_or(4.0)).collect();
        let mut i = 0;
        while i + 1 < cells.len() {
            let mut best = i + 1;
            let mut worst = factors[i + 1];
            for k in i + 2..cells.len().min(i + MAX_LOOKAHEAD) {
                worst = worst.max(factors[k]);
                if !self.straight_drive(costs, obstacles, cells[i], cells[k], worst * 1.05) {
                    break;
                }
                best = k;
            }
            points.push(self.land.position(cells[best]));
            i = best;
        }
        points
    }

    /// Whether driving the straight line between the middles of `a` and `b` stays on linked
    /// cells the vehicle fits on without a drop, none costlier than `max_factor`.
    fn straight_drive(&self, costs: &ClassCosts, obstacles: &[Obstacle], a: CellRef, b: CellRef, max_factor: f32) -> bool {
        let grid = &self.land;
        let (dx, dz) = (b.x as f32 - a.x as f32, b.z as f32 - a.z as f32);
        let (step_x, step_z) = (if dx > 0.0 { 0 } else { 2 }, if dz > 0.0 { 1 } else { 3 });
        let delta_x = if dx != 0.0 { 1.0 / dx.abs() } else { f32::INFINITY };
        let delta_z = if dz != 0.0 { 1.0 / dz.abs() } else { f32::INFINITY };
        let (mut t_x, mut t_z) = (0.5 * delta_x, 0.5 * delta_z);
        let step = |c: CellRef, dir: usize| {
            let n = grid.neighbour(c, dir)?;
            let (from, to) = (grid.cell(c), grid.cell(n));
            let ok = (to.y - from.y).abs() <= grid.walk_climb(from, to)
                && self.cost(costs, obstacles, n).is_some_and(|f| f <= max_factor);
            ok.then_some(n)
        };
        let mut c = a;
        for _ in 0..(dx.abs() + dz.abs()) as usize + 2 {
            if c.x == b.x && c.z == b.z {
                return c.index == b.index;
            }
            if (t_x - t_z).abs() < 1e-5 {
                let via_x = step(c, step_x).and_then(|n| step(n, step_z));
                let via_z = step(c, step_z).and_then(|n| step(n, step_x));
                match (via_x, via_z) {
                    (Some(p), Some(q)) if p.index == q.index => c = p,
                    _ => return false,
                }
                t_x += delta_x;
                t_z += delta_z;
            } else if t_x < t_z {
                let Some(n) = step(c, step_x) else {
                    return false;
                };
                c = n;
                t_x += delta_x;
            } else {
                let Some(n) = step(c, step_z) else {
                    return false;
                };
                c = n;
                t_z += delta_z;
            }
        }
        false
    }

    /// A flat, open, dry spot near `near` (within `radius`) with `size` meters of room all
    /// around and nothing overhead: where a helicopter can land.
    pub fn landing_spot(&self, near: Vec3, radius: f32, size: f32) -> Option<Vec3> {
        let mut best: Option<(f32, Vec3)> = None;
        // Every other column is plenty.
        for c in self.land.cells_near(near.xz(), radius).filter(|c| (c.x + c.z) % 2 == 0) {
            let cell = self.land.cell(c);
            let at = self.land.position(c);
            if self.clearance(cell) < size || cell.slope as f32 / SLOPE_SCALE > 0.15 || self.depth(cell) > -0.3 {
                continue;
            }
            // Nothing much taller than the ground in the air map cells around it.
            if self.air.as_ref().is_some_and(|air| air.floor(at, size) > at.y + 6.0) {
                continue;
            }
            let d = at.xz().distance(near.xz());
            if best.is_none_or(|(b, _)| d < b) {
                best = Some((d, at));
            }
        }
        best.map(|(_, at)| at)
    }

    /// The highest obstacle within `radius` of `p`, for aircraft.
    pub fn flight_floor(&self, p: Vec3, radius: f32) -> Option<f32> {
        self.air.as_ref().map(|air| air.floor(p, radius))
    }
}

/// Road triangles of the level (its road decal meshes), world space.
pub fn road_triangles(paths: &game_shared::config::GamePaths, roads: &[game_data::RoadDesc]) -> Vec<[Vec3; 3]> {
    let mut out = Vec::new();
    for road in roads {
        let offset = Vec3::from_array(road.position);
        match mesh_triangles(&paths.find(&road.mesh)) {
            Ok(triangles) => out.extend(triangles.into_iter().map(|t| t.map(|p| p + offset))),
            Err(err) => debug!("nav: road {}: {err:#}", road.mesh),
        }
    }
    out
}

/// Every triangle of a `.glb`.
fn mesh_triangles(path: &std::path::Path) -> anyhow::Result<Vec<[Vec3; 3]>> {
    let bytes = std::fs::read(path)?;
    let gltf = gltf::Gltf::from_slice(&bytes)?;
    let blob = gltf.blob.as_deref().unwrap_or_default();
    let mut out = Vec::new();
    for mesh in gltf.meshes() {
        for primitive in mesh.primitives() {
            let reader = primitive.reader(|_| Some(blob));
            let Some(positions) = reader.read_positions() else {
                continue;
            };
            let positions: Vec<Vec3> = positions.map(Vec3::from_array).collect();
            let indices: Vec<u32> = match reader.read_indices() {
                Some(indices) => indices.into_u32().collect(),
                None => (0..positions.len() as u32).collect(),
            };
            for t in indices.chunks_exact(3) {
                let get = |i: u32| positions.get(i as usize).copied();
                if let (Some(a), Some(b), Some(c)) = (get(t[0]), get(t[1]), get(t[2])) {
                    out.push([a, b, c]);
                }
            }
        }
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use avian3d::parry::shape::SharedShape;
    use bevy::math::Affine3A;
    use game_shared::level::Heightmap;

    use super::*;
    use crate::nav::build::MeshInstance;

    fn cuboid(center: Vec3, size: Vec3) -> MeshInstance {
        MeshInstance {
            shape: SharedShape::cuboid(size.x / 2.0, size.y / 2.0, size.z / 2.0),
            transform: Affine3A::from_translation(center),
        }
    }

    /// Ground from -64 to 64 m at height 0, with `heights` overriding it where given (a
    /// function of x, z), plus `meshes`.
    fn level(meshes: Vec<MeshInstance>, height: impl Fn(f32, f32) -> f32, roads: Vec<[Vec3; 3]>, water: Option<f32>) -> VehicleNavGrid {
        let resolution = 65;
        let spacing = 2.0;
        let origin = Vec3::new(-64.0, 0.0, -64.0);
        let mut heights = Vec::new();
        for z in 0..resolution {
            for x in 0..resolution {
                heights.push(height(origin.x + x as f32 * spacing, origin.z + z as f32 * spacing));
            }
        }
        let terrain = Heightmap {
            resolution,
            spacing,
            origin,
            heights,
        };
        let geometry = VehicleGeometry {
            geometry: LevelGeometry {
                terrain: Some(Arc::new(terrain)),
                meshes,
                ..default()
            },
            roads,
            water,
        };
        build_all(&geometry, None)
    }

    const JEEP: DriveSpec = DriveSpec {
        class: NavClass::Wheeled,
        half_width: 1.2,
        depth: 0.6,
    };

    fn crosses_x(path: &VehiclePath) -> Vec<f32> {
        path.points
            .windows(2)
            .filter(|w| w[0].x.signum() != w[1].x.signum())
            .map(|w| {
                let (a, b) = (w[0], w[1]);
                a.z + (b.z - a.z) * a.x / (a.x - b.x)
            })
            .collect()
    }

    #[test]
    fn drives_through_gaps_it_fits() {
        // A wall along x = 0 with a 2 m gap at z = -20 and a 6 m one at z = 20.
        let wall = |z0: f32, z1: f32| cuboid(Vec3::new(0.0, 2.0, (z0 + z1) / 2.0), Vec3::new(1.0, 4.0, z1 - z0));
        let nav = level(vec![wall(-64.0, -21.0), wall(-19.0, 17.0), wall(23.0, 64.0)], |_, _| 0.0, Vec::new(), None);
        let path = nav.find_path(&JEEP, Vec3::new(-30.0, 1.0, -20.0), Vec3::new(30.0, 0.0, -20.0), 10.0, &[]).unwrap();
        assert!(path.complete, "{path:?}");
        let crossings = crosses_x(&path);
        assert_eq!(crossings.len(), 1, "{path:?}");
        assert!((17.0..23.0).contains(&crossings[0]), "crossed at z = {}", crossings[0]);
        // A parked vehicle in the wide gap: round it or through the narrow one? Neither fits
        // a jeep, so the path ends before the wall.
        let parked = [Obstacle {
            center: Vec2::new(0.0, 20.0),
            forward: Vec2::Y,
            half: Vec2::new(3.0, 4.0),
        }];
        let path = nav.find_path(&JEEP, Vec3::new(-30.0, 1.0, -20.0), Vec3::new(30.0, 0.0, -20.0), 10.0, &parked).unwrap();
        assert!(!path.complete, "{path:?}");
    }

    #[test]
    fn prefers_roads() {
        // A road loops north of the straight line; driving it is cheaper than off-road.
        let road = |a: Vec2, b: Vec2| {
            let side = (b - a).perp().normalize() * 4.0;
            let p = |v: Vec2| Vec3::new(v.x, 0.0, v.y);
            [[p(a - side), p(b - side), p(b + side)], [p(a - side), p(b + side), p(a + side)]]
        };
        let (s, e) = (Vec2::new(-40.0, 0.0), Vec2::new(40.0, 0.0));
        let (n1, n2) = (Vec2::new(-30.0, -20.0), Vec2::new(30.0, -20.0));
        let roads: Vec<[Vec3; 3]> = [road(s, n1), road(n1, n2), road(n2, e)].into_iter().flatten().collect();
        let nav = level(Vec::new(), |_, _| 0.0, roads, None);
        assert!(nav.road_columns() > 400, "{} road columns", nav.road_columns());
        let path = nav.find_path(&JEEP, Vec3::new(-40.0, 1.0, 0.0), Vec3::new(40.0, 0.0, 0.0), 5.0, &[]).unwrap();
        assert!(path.complete);
        let north = path.points.iter().map(|p| p.z).fold(0.0f32, f32::min);
        assert!(north < -15.0, "stayed off the road: {path:?}");
        // Tanks care less for roads and go straight.
        let tank = DriveSpec { class: NavClass::Tracked, half_width: 1.8, depth: 1.0 };
        let path = nav.find_path(&tank, Vec3::new(-40.0, 1.0, 0.0), Vec3::new(40.0, 0.0, 0.0), 5.0, &[]).unwrap();
        let north = path.points.iter().map(|p| p.z).fold(0.0f32, f32::min);
        assert!(north > -5.0, "tank took the road: {path:?}");
    }

    #[test]
    fn wheels_avoid_deep_water_amphibians_swim_boats_sail() {
        // A lake (ground 3 m under the water at 0, gentle banks) across the middle, from x = -10
        // to 10, with a ford at z > 50.
        let height = |x: f32, z: f32| if z < 50.0 { ((x.abs() - 10.0) * 0.3 - 3.0).clamp(-3.0, 0.5) } else { 0.5 };
        let nav = level(Vec::new(), height, Vec::new(), Some(0.0));
        let (a, b) = (Vec3::new(-30.0, 1.5, 0.0), Vec3::new(30.0, 1.5, 0.0));
        let jeep = nav.find_path(&JEEP, a, b, 5.0, &[]).unwrap();
        assert!(jeep.complete);
        assert!(jeep.points.iter().any(|p| p.z > 45.0), "jeep swam: {jeep:?}");
        let apc = DriveSpec { class: NavClass::Amphibious, half_width: 1.3, depth: 0.6 };
        let swim = nav.find_path(&apc, a, b, 5.0, &[]).unwrap();
        assert!(swim.complete && swim.points.iter().all(|p| p.z.abs() < 20.0), "{swim:?}");
        let boat = DriveSpec { class: NavClass::Boat, half_width: 1.3, depth: 1.0 };
        let water = nav.water.as_ref().expect("a water grid");
        assert!(water.navigable(Vec3::new(0.0, 0.0, -30.0), 1.0));
        assert!(!water.navigable(Vec3::new(30.0, 0.0, 0.0), 1.0));
        let sail = nav.find_path(&boat, Vec3::new(0.0, 0.0, -50.0), Vec3::new(0.0, 0.0, 30.0), 5.0, &[]).unwrap();
        assert!(sail.complete, "{sail:?}");
    }

    /// Paths from the vehicle spawners to the flags of imported levels (their cached grids):
    /// `cargo test -p game_server --lib vehicle_paths_on_levels -- --ignored --nocapture`.
    #[test]
    #[ignore]
    fn vehicle_paths_on_levels() {
        let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../imported");
        for level in ["strike_at_karkand", "dalian_plant", "gulf_of_oman", "wake_island_2007"] {
            let dir = root.join("levels").join(level);
            let Ok(desc) = game_data::read_ron::<game_data::LevelDesc>(&dir.join("level.ron")) else {
                println!("{level}: not imported");
                continue;
            };
            let Some(layout) = desc.game_modes.iter().find(|l| l.mode == "gpm_cq" && l.size == 64) else {
                continue;
            };
            let Some(land) = super::super::cache::load_unchecked(&dir.join("navgrid_vehicle_gpm_cq_64.bin"), land_params())
            else {
                println!("{level}: no cached vehicle grid");
                continue;
            };
            let nav = VehicleNavGrid {
                roads: rasterize_roads(&land, &[]),
                land,
                water_height: desc.water.as_ref().map(|w| w.height),
                water: None,
                air: None,
                classes: default(),
            };
            let land_vehicle = |t: &str| {
                ["jep", "jeep", "apc", "tnk", "aav", "usaav"].iter().any(|k| t.starts_with(k))
            };
            let (mut total, mut complete, mut ms, mut max_ms) = (0, 0, 0.0f32, 0.0f32);
            let mut complete_ms = 0.0f32;
            let mut expanded = 0usize;
            let mut misses = Vec::new();
            for spawner in &layout.vehicle_spawners {
                let Some(template) = spawner.templates.iter().flatten().find(|t| land_vehicle(t)) else {
                    continue;
                };
                let spec = DriveSpec {
                    class: if template.contains("tnk") { NavClass::Tracked } else { NavClass::Wheeled },
                    half_width: if template.contains("tnk") { 1.9 } else { 1.25 },
                    depth: 0.7,
                };
                let from = Vec3::from_array(spawner.placement.position) + Vec3::Y;
                for cp in &layout.control_points {
                    let to = Vec3::from_array(cp.position);
                    let started = std::time::Instant::now();
                    let path = nav.find_path(&spec, from, to, 30.0, &[]);
                    let took = started.elapsed().as_secs_f32() * 1000.0;
                    total += 1;
                    ms += took;
                    max_ms = max_ms.max(took);
                    match path {
                        Some(p) if p.complete => {
                            complete += 1;
                            complete_ms += took;
                            expanded += p.expanded;
                        }
                        Some(p) => misses.push(format!(
                            "{template} {from:.0} -> {} ({:.0} m): ends {:.0} m short at {:.0}",
                            cp.name,
                            from.distance(to),
                            p.points.last().unwrap().xz().distance(to.xz()),
                            p.points.last().unwrap()
                        )),
                        None => misses.push(format!("{template} {from:.0}: not on the grid")),
                    }
                }
            }
            println!(
                "{level}: {complete} of {total} paths complete, {:.1} ms avg ({:.1} ms, {} nodes for complete ones), {max_ms:.0} ms max",
                ms / total.max(1) as f32,
                complete_ms / complete.max(1) as f32,
                expanded / complete.max(1),
            );
            for miss in misses.iter().take(12) {
                println!("  {miss}");
            }
        }
    }

    #[test]
    fn keeps_off_steep_slopes_and_knows_the_skyline() {
        // A 45 degree ridge along x = 0 (too steep), 60 m long, and a tower.
        let height = |x: f32, z: f32| if z.abs() < 30.0 { (12.0 - x.abs()).max(0.0) } else { 0.0 };
        let tower = cuboid(Vec3::new(40.0, 25.0, 40.0), Vec3::new(4.0, 50.0, 4.0));
        let nav = level(vec![tower], height, Vec::new(), None);
        let path = nav.find_path(&JEEP, Vec3::new(-30.0, 1.0, 0.0), Vec3::new(30.0, 0.0, 0.0), 5.0, &[]).unwrap();
        assert!(path.complete);
        assert!(path.points.iter().any(|p| p.z.abs() > 29.0), "drove over the ridge: {path:?}");
        let floor = nav.flight_floor(Vec3::new(40.0, 0.0, 40.0), 5.0).unwrap();
        assert!((49.0..51.0).contains(&floor), "{floor}");
        assert!(nav.landing_spot(Vec3::new(-40.0, 0.0, -45.0), 20.0, 6.0).is_some());
    }
}
