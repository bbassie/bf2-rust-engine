//! Bot navigation: a layered walkability grid built from the level's own collision.
//!
//! When a level loads, its collision (the terrain heightfield and the static trimeshes on
//! [`GameLayer::World`]) is rasterized in the background into columns of cells, much like
//! Recast's voxel heightfield: every surface a soldier can stand on with room for a standing
//! soldier above it becomes a cell, so multi-storey buildings have several cells per column.
//! Cells link to the neighbours a soldier can walk, step or jump to. Paths are found with A*
//! on background tasks and shortened by walking straight lines over the grid (see
//! [`NavGrid::find_path`]). The grid covers the played layout's control points and spawns.
//! Grids are cached ([`game_shared::cache`], `nav/<level>/infantry-<key>.bin`), keyed by a
//! hash of the collision geometry, the movement limits and the play area, so layouts with the
//! same inputs share one.
//!
//! Levels with combat areas (BF2 `CombatArea`) confine their grids to the area that bounds
//! each kind of traveller, plus a margin ([`area`]): columns outside get no cells and the
//! grid is cropped to the area's bounds, so bots never pick goals out of bounds.
//!
//! Big, intricate statics (the aircraft carriers: a hangar, ramps, narrow doors and
//! catwalks, turned at any angle) get a **detail patch** ([`patch`]): a finer grid in the
//! object's own frame, so its corridors run along the cells, that takes the object's
//! footprint out of the level grid and joins it through portals along the edge. Patch
//! cells are ordinary [`NavCell`]s in the same arrays, so every query ([`NavGrid::find_path`],
//! [`NavGrid::locate`], [`NavGrid::cells_near`], regions) covers both without callers
//! knowing. Vehicles parked on the grid are [`obstacles`] paths go around.

pub mod area;
mod build;
mod cache;
pub mod obstacles;
mod patch;
mod path;
pub mod vehicle;

#[cfg(test)]
mod level_tests;

use std::{ops::Range, sync::Arc, time::Instant};

use avian3d::prelude::*;
use bevy::{
    platform::collections::HashMap,
    prelude::*,
    tasks::{AsyncComputeTaskPool, Task, futures::check_ready},
};
use bytemuck::{Pod, Zeroable};
use game_data::GameModeDesc;
use area::{COMBAT_AREA_MARGIN, KEEP_RADIUS, LADDER_REACH, PlayArea, Traveller};
use game_shared::{
    cache::Cache,
    ladder::{Ladder, LadderPart},
    level::{LevelEntity, LoadedLevel, Terrain},
    physics::GameLayer,
    protocol::MatchInfo,
    soldier::{SOLDIER_HEIGHT, SoldierTuning},
};

pub use obstacles::{NavBlocked, NavObstacles, StuckCells};
pub use path::{LadderStep, NavPath, Waypoint};

pub struct NavPlugin;

impl Plugin for NavPlugin {
    fn build(&self, app: &mut App) {
        app.add_systems(
            Update,
            (
                // Not on a client playing on someone else's server.
                start_build
                    .run_if(resource_exists_and_changed::<LoadedLevel>)
                    .run_if(in_state(bevy_replicon::prelude::ClientState::Disconnected)),
                finish_build.run_if(resource_exists::<NavBuild>),
                finish_vehicle_build.run_if(resource_exists::<VehicleNavBuild>),
                obstacles::update_obstacles.run_if(resource_exists::<Navigation>),
                forget_level
                    .run_if(not(resource_exists::<LoadedLevel>))
                    .run_if(
                        resource_exists::<Navigation>
                            .or_else(resource_exists::<NavBuild>)
                            .or_else(resource_exists::<vehicle::VehicleNavigation>)
                            .or_else(resource_exists::<VehicleNavBuild>),
                    ),
            )
                .chain(),
        );
    }
}

/// The navigation grid of the loaded level, once it is built.
#[derive(Resource, Clone)]
pub struct Navigation(pub Arc<NavGrid>);

#[derive(Resource)]
struct NavBuild {
    task: Task<NavGrid>,
    /// The layout's spawn points and control points, checked once the grid is there.
    spawns: SpawnCheck,
}

#[derive(Resource)]
struct VehicleNavBuild(Task<vehicle::VehicleNavGrid>);

/// How far around the control points and spawn points the grid reaches, meters.
const GAMEPLAY_MARGIN: f32 = 60.0;
/// The same for the vehicle grid, which also covers the vehicle spawners.
const VEHICLE_MARGIN: f32 = 120.0;

/// Movement limits the grid is built for, derived from [`SoldierTuning`].
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct NavParams {
    /// Horizontal cell size, meters.
    pub cell: f32,
    /// Vertical resolution of the rasterization, meters.
    pub voxel: f32,
    /// Free height a standing soldier needs above a surface.
    pub height: f32,
    /// Cosine of the steepest walkable slope.
    pub min_normal_y: f32,
    /// Highest ledge a soldier walks up without jumping.
    pub step: f32,
    /// Highest ledge a jump gets onto.
    pub jump: f32,
    /// Deepest ledge a soldier drops down from.
    pub drop: f32,
    /// The level's water surface, if it has water: cells well below it cost much more to
    /// cross (see [`path`]), so bots swim only when it's worth it. Not part of the cache
    /// key: loading a grid always overlays the level's current water height, cached or not.
    pub water_height: Option<f32>,
}

impl NavParams {
    /// See [`NavGrid::walk_climb`]; slopes as stored in [`NavCell::slope`].
    fn walk_climb(&self, slope_a: u8, slope_b: u8) -> f32 {
        self.step + 0.5 * self.cell * (slope_a as f32 + slope_b as f32) / SLOPE_SCALE
    }

    pub fn from_tuning(tuning: &SoldierTuning) -> Self {
        // The jump apex, with a margin for getting the capsule over the edge.
        let jump = 0.8 * tuning.jump_speed * tuning.jump_speed / (2.0 * tuning.gravity);
        Self {
            cell: 0.5,
            voxel: 0.05,
            height: SOLDIER_HEIGHT + 0.05,
            // A margin below the slope where soldiers start sliding.
            min_normal_y: (tuning.max_slope - 10f32.to_radians()).cos(),
            step: tuning.step_height,
            jump: jump.max(tuning.step_height),
            drop: 2.5,
            water_height: None,
        }
    }
}

/// A walkable surface in one grid column.
#[repr(C)]
#[derive(Clone, Copy, Debug, Pod, Zeroable)]
pub struct NavCell {
    /// Height of the surface.
    pub y: f32,
    /// Connected area the cell belongs to; cells of different regions can't reach each
    /// other. 0 for tiny islands (tops of walls, furniture), which are never used.
    pub region: u16,
    /// Per direction of [`NavGrid::DIRS`]: the linked cell's index within the neighbouring
    /// column, or [`NavGrid::NONE`].
    pub links: [u8; 4],
    /// Distance to the edge of the walkable area (walls, ledges) in half cells: 0 at the edge.
    pub dist: u8,
    /// Surface slope as `tan * SLOPE_SCALE`.
    pub slope: u8,
}

const SLOPE_SCALE: f32 = 32.0;

/// A cell and its column.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct CellRef {
    pub x: u32,
    pub z: u32,
    /// Index into the grid's cells.
    pub index: u32,
}

/// The walkable surfaces of a level. See the module docs.
pub struct NavGrid {
    pub params: NavParams,
    /// World XZ of the outer corner of column (0, 0) of the level grid.
    pub origin: Vec2,
    pub width: u32,
    pub depth: u32,
    /// The cells of level grid column `z * width + x` are `cells[columns[i]..columns[i + 1]]`,
    /// bottom to top. The columns of the patches follow (see [`NavPatch`]).
    columns: Vec<u32>,
    cells: Vec<NavCell>,
    ladders: Vec<NavLadder>,
    /// Ladders by the cells at their ends.
    ladder_ends: HashMap<u32, Vec<u16>>,
    /// Cells below this index are the level grid's; the patches' follow.
    base_cells: u32,
    patches: Vec<NavPatch>,
    /// Links between the level grid and its patches along their edges, by the cell they
    /// start from (one way: drops only go down).
    portals: HashMap<u32, Vec<CellRef>>,
}

/// A detail patch: a finer grid over one object, in the object's frame. Its cells'
/// [`CellRef::x`] and [`CellRef::z`] are columns of the patch.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct NavPatch {
    pub frame: build::Frame,
    /// Local XZ of the outer corner of column (0, 0).
    pub origin: Vec2,
    pub cell: f32,
    pub width: u32,
    pub depth: u32,
    /// Index into [`NavGrid::columns`] of column (0, 0).
    column_base: u32,
    /// Index of its first cell.
    first_cell: u32,
}

/// The columns a cell belongs to: the level grid or a patch.
#[derive(Clone, Copy, Debug, PartialEq)]
struct Space {
    origin: Vec2,
    cell: f32,
    width: u32,
    depth: u32,
    column_base: u32,
    /// `None` for the level grid (world axes).
    frame: Option<build::Frame>,
}

impl Space {
    fn to_local(&self, world: Vec2) -> Vec2 {
        self.frame.map_or(world, |f| f.to_local(world))
    }

    fn to_world(&self, local: Vec2) -> Vec2 {
        self.frame.map_or(local, |f| f.to_world(local))
    }
}

/// A ladder soldiers climb between two cells.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct NavLadder {
    /// The cell in front of its foot, and the one behind its top.
    pub bottom: CellRef,
    pub top: CellRef,
    /// Where to stand to get on at the bottom (in front of it), and where getting off at
    /// the top lands (behind it).
    pub foot: Vec3,
    pub head: Vec3,
    /// Horizontal, out of the wall: the side it is climbed from.
    pub front: Vec3,
    /// It can be climbed down too (its top is level with the floor behind it).
    pub down: bool,
}

impl NavGrid {
    pub const NONE: u8 = u8::MAX;
    /// Column offsets of the link directions: +X, +Z, -X, -Z.
    pub const DIRS: [(i32, i32); 4] = [(1, 0), (0, 1), (-1, 0), (0, -1)];

    pub fn ladders(&self) -> &[NavLadder] {
        &self.ladders
    }

    /// The ladders with an end at a cell (by index).
    pub fn ladders_at(&self, cell: u32) -> impl Iterator<Item = &NavLadder> + '_ {
        self.ladder_ends
            .get(&cell)
            .into_iter()
            .flatten()
            .map(|&i| &self.ladders[i as usize])
    }

    pub fn cell_count(&self) -> usize {
        self.cells.len()
    }

    /// Cells of the detail patches.
    pub fn patch_cell_count(&self) -> usize {
        self.cells.len() - self.base_cells as usize
    }

    pub fn patches(&self) -> &[NavPatch] {
        &self.patches
    }

    pub fn memory_bytes(&self) -> usize {
        let portals: usize = self.portals.values().map(|p| 16 + p.len() * size_of::<CellRef>()).sum();
        self.columns.len() * 4 + self.cells.len() * size_of::<NavCell>() + portals
    }

    fn base_space(&self) -> Space {
        Space {
            origin: self.origin,
            cell: self.params.cell,
            width: self.width,
            depth: self.depth,
            column_base: 0,
            frame: None,
        }
    }

    fn patch_space(p: &NavPatch) -> Space {
        Space {
            origin: p.origin,
            cell: p.cell,
            width: p.width,
            depth: p.depth,
            column_base: p.column_base,
            frame: Some(p.frame),
        }
    }

    /// The level grid, then every patch.
    fn spaces(&self) -> impl Iterator<Item = Space> + '_ {
        std::iter::once(self.base_space()).chain(self.patches.iter().map(Self::patch_space))
    }

    /// The columns the cell with this index belongs to.
    fn space(&self, index: u32) -> Space {
        if index < self.base_cells {
            return self.base_space();
        }
        self.patches
            .iter()
            .rev()
            .find(|p| index >= p.first_cell)
            .map_or_else(|| self.base_space(), Self::patch_space)
    }

    /// Which patch a cell is in (0 for the level grid, else 1 + the patch's index).
    fn space_id(&self, index: u32) -> usize {
        if index < self.base_cells {
            return 0;
        }
        self.patches.iter().rposition(|p| index >= p.first_cell).map_or(0, |i| i + 1)
    }

    /// Horizontal size of a cell, meters.
    pub fn cell_size(&self, c: CellRef) -> f32 {
        self.space(c.index).cell
    }

    fn space_column(&self, s: &Space, x: u32, z: u32) -> Range<u32> {
        let i = (s.column_base + z * s.width + x) as usize;
        self.columns[i]..self.columns[i + 1]
    }

    /// Calls `f` with every cell (of the level grid and the patches).
    pub fn for_each_cell(&self, mut f: impl FnMut(CellRef)) {
        for s in self.spaces() {
            for z in 0..s.depth {
                for x in 0..s.width {
                    for index in self.space_column(&s, x, z) {
                        f(CellRef { x, z, index });
                    }
                }
            }
        }
    }

    /// Portals from a cell into or out of a patch.
    pub fn portals_at(&self, cell: u32) -> &[CellRef] {
        self.portals.get(&cell).map_or(&[], |p| &p[..])
    }

    /// The column of the level grid containing a world position.
    pub fn column_at(&self, x: f32, z: f32) -> Option<(u32, u32)> {
        let cx = ((x - self.origin.x) / self.params.cell).floor();
        let cz = ((z - self.origin.y) / self.params.cell).floor();
        (cx >= 0.0 && cz >= 0.0 && cx < self.width as f32 && cz < self.depth as f32)
            .then_some((cx as u32, cz as u32))
    }

    /// Cell indices of a column of the level grid.
    pub fn column(&self, x: u32, z: u32) -> Range<u32> {
        let i = (z * self.width + x) as usize;
        self.columns[i]..self.columns[i + 1]
    }

    pub fn cell(&self, c: CellRef) -> &NavCell {
        &self.cells[c.index as usize]
    }

    /// World position of the middle of a cell's surface.
    pub fn position(&self, c: CellRef) -> Vec3 {
        let y = self.cells[c.index as usize].y;
        if c.index < self.base_cells {
            let cell = self.params.cell;
            return Vec3::new(self.origin.x + (c.x as f32 + 0.5) * cell, y, self.origin.y + (c.z as f32 + 0.5) * cell);
        }
        let s = self.space(c.index);
        let p = s.to_world(s.origin + (Vec2::new(c.x as f32, c.z as f32) + 0.5) * s.cell);
        Vec3::new(p.x, y, p.y)
    }

    /// The cell linked in direction `dir` (an index into [`Self::DIRS`]): in the columns of
    /// the cell's own grid (a patch's directions are the patch's axes).
    pub fn neighbour(&self, c: CellRef, dir: usize) -> Option<CellRef> {
        let link = self.cells[c.index as usize].links[dir];
        if link == Self::NONE {
            return None;
        }
        let (dx, dz) = Self::DIRS[dir];
        let (x, z) = (c.x.wrapping_add_signed(dx), c.z.wrapping_add_signed(dz));
        let start = if c.index < self.base_cells {
            self.columns[(z * self.width + x) as usize]
        } else {
            let s = self.space(c.index);
            self.columns[(s.column_base + z * s.width + x) as usize]
        };
        Some(CellRef {
            x,
            z,
            index: start + link as u32,
        })
    }

    /// Height difference between neighbours a soldier walks without jumping or dropping:
    /// a step, plus what the slopes of the two surfaces explain.
    pub fn walk_climb(&self, a: &NavCell, b: &NavCell) -> f32 {
        self.params.walk_climb(a.slope, b.slope)
    }

    /// Whether going from `a` to its neighbour `b` needs a jump.
    pub fn needs_jump(&self, a: &NavCell, b: &NavCell) -> bool {
        b.y - a.y > self.walk_climb(a, b)
    }

    /// The usable cell closest to `pos` (feet height) within `radius` meters, optionally
    /// only in `region`. Prefers surfaces at the same height.
    pub fn locate(&self, pos: Vec3, radius: f32, region: Option<u16>) -> Option<CellRef> {
        self.locate_where(pos, radius, region, |_| true)
    }

    /// [`Self::locate`] among the cells `wanted` accepts (by index).
    pub fn locate_where(&self, pos: Vec3, radius: f32, region: Option<u16>, wanted: impl Fn(u32) -> bool) -> Option<CellRef> {
        let mut best: Option<(f32, CellRef)> = None;
        for s in self.spaces() {
            // Distances are the same in a patch's frame.
            let local = s.to_local(pos.xz());
            let cell = s.cell;
            let r = (radius / cell).ceil() as i32;
            let cx = ((local.x - s.origin.x) / cell).floor() as i32;
            let cz = ((local.y - s.origin.y) / cell).floor() as i32;
            if cx + r < 0 || cz + r < 0 || cx - r >= s.width as i32 || cz - r >= s.depth as i32 {
                continue;
            }
            for z in (cz - r).max(0)..=(cz + r).min(s.depth as i32 - 1) {
                for x in (cx - r).max(0)..=(cx + r).min(s.width as i32 - 1) {
                    let (x, z) = (x as u32, z as u32);
                    let center = s.origin + (Vec2::new(x as f32, z as f32) + 0.5) * cell;
                    let d2 = center.distance_squared(local);
                    if d2 > (radius + cell) * (radius + cell) || best.is_some_and(|(b, _)| d2 >= b) {
                        continue;
                    }
                    for index in self.space_column(&s, x, z) {
                        let c = &self.cells[index as usize];
                        let dy = c.y - pos.y;
                        let ok = c.region != 0 && region.is_none_or(|r| r == c.region);
                        if !ok || !(-3.0..=1.0).contains(&dy) || !wanted(index) {
                            continue;
                        }
                        let edge = if c.dist == 0 { 0.2 } else { 0.0 };
                        let score = d2 + 4.0 * dy * dy + edge;
                        if best.is_none_or(|(b, _)| score < b) {
                            best = Some((score, CellRef { x, z, index }));
                        }
                    }
                }
            }
        }
        best.map(|(_, c)| c)
    }

    /// Usable cells in the columns within `radius` of `center` (XZ), of the level grid and
    /// the patches.
    pub fn cells_near(&self, center: Vec2, radius: f32) -> impl Iterator<Item = CellRef> + '_ {
        self.spaces().flat_map(move |s| {
            let local = s.to_local(center);
            let cell = s.cell;
            let lo = ((local - radius - s.origin) / cell).floor().max(Vec2::ZERO);
            let hi = ((local + radius - s.origin) / cell)
                .floor()
                .min(Vec2::new(s.width as f32 - 1.0, s.depth as f32 - 1.0));
            let (x0, z0, x1, z1) = (lo.x as i64, lo.y as i64, hi.x as i64, hi.y as i64);
            (z0..=z1).flat_map(move |z| {
                (x0..=x1).flat_map(move |x| {
                    let (x, z) = (x as u32, z as u32);
                    self.space_column(&s, x, z)
                        .filter(|&index| self.cells[index as usize].region != 0)
                        .map(move |index| CellRef { x, z, index })
                })
            })
        })
    }
}

/// What the grids are built from: the level's collision, split by layer.
struct Collected {
    geometry: build::LevelGeometry,
    vehicle_meshes: Vec<build::MeshInstance>,
}

/// Gathers the level's collision for the grids (from the level's entities; see
/// [`start_build`]).
fn collect_geometry<'a>(
    terrain: Option<Arc<game_shared::level::Heightmap>>,
    colliders: impl Iterator<Item = (&'a Collider, &'a Transform, &'a CollisionLayers)>,
    ladder_parts: impl Iterator<Item = (&'a Collider, &'a Transform)>,
    layout: Option<&GameModeDesc>,
    strategic: &[Vec3],
) -> Collected {
    let (mut meshes, mut vehicle_meshes) = (Vec::new(), Vec::new());
    for (collider, transform, layers) in colliders {
        let mesh = || build::MeshInstance {
            shape: collider.shape().clone(),
            transform: transform.compute_affine(),
        };
        if layers.memberships.has_all(GameLayer::World) {
            meshes.push(mesh());
        }
        // The vehicle grid sees only what actually blocks a vehicle (terrain and BF2's
        // vehicle-type collision): small plants that only carry `GameLayer::World` (soldier
        // or projectile collision) are left out, so vehicles can path straight through them
        // instead of routing around bushes they'd just drive over.
        if layers.memberships.has_all(GameLayer::VehicleGround) {
            vehicle_meshes.push(mesh());
        }
    }
    // The boxes movement climbs (see `game_shared::ladder`).
    let ladders = ladder_parts
        .map(|(collider, transform)| {
            let aabb = collider.aabb(Vec3::ZERO, Quat::IDENTITY);
            let half = aabb.size() * 0.5 * transform.scale.abs();
            Ladder::from_box(transform.transform_point(aabb.center()), transform.rotation, half)
        })
        .collect::<Vec<Ladder>>();
    let area = layout.and_then(|l| infantry_area(l, &ladders, strategic));
    let mut bounds = layout.and_then(gameplay_bounds);
    if let Some(area) = &area {
        let cropped = area.crop(bounds);
        info!(
            "nav: infantry grid kept to the combat area ({} polygons, {:.2} km2, margin {:.0} m{}): {:.0}x{:.0} m instead of {}",
            area.polygons.len(),
            area.polygon_area() / 1e6,
            area.margin,
            match area.keep.len() {
                0 => String::new(),
                n => format!(", {n} places outside kept"),
            },
            cropped.1.x - cropped.0.x,
            cropped.1.y - cropped.0.y,
            bounds.map_or("the whole level".into(), |(lo, hi)| format!("{:.0}x{:.0} m", hi.x - lo.x, hi.y - lo.y)),
        );
        bounds = Some(cropped);
    }
    Collected {
        geometry: build::LevelGeometry {
            terrain,
            meshes,
            ladders,
            bounds,
            area,
            ..default()
        },
        vehicle_meshes,
    }
}

/// The places of a layout gameplay needs to reach, by what they are: its control points,
/// spawn points and vehicle spawners.
fn gameplay_points(layout: &GameModeDesc) -> impl Iterator<Item = (String, Vec3)> + '_ {
    let at = |p: [f32; 3]| Vec3::from_array(p);
    layout
        .control_points
        .iter()
        .map(move |cp| (format!("control point {}", cp.id), at(cp.position)))
        .chain(layout.spawn_points.iter().map(move |sp| (format!("spawn of {}", sp.control_point), at(sp.placement.position))))
        .chain(layout.vehicle_spawners.iter().map(move |v| {
            let name = v.templates.iter().flatten().next().map_or("?", |t| t.as_str());
            (format!("{name} spawner"), at(v.placement.position))
        }))
}

/// Keeps `points` outside `area` (a circle and a corridor back each), logging them.
fn keep_points(area: &mut PlayArea, grid: &str, points: impl Iterator<Item = (String, Vec3)>) {
    let mut outside = Vec::new();
    for (what, p) in points {
        let distance = area.nearest_edge_point(p.xz()).1;
        if area.keep_point(p.xz(), KEEP_RADIUS) {
            outside.push(format!("{what} {distance:.0} m out at {:.0}, {:.0}", p.x, p.z));
        }
    }
    if !outside.is_empty() {
        info!(
            "nav: {} places the {grid} needs lie outside the combat area, kept with corridors: {}",
            outside.len(),
            outside.join("; ")
        );
    }
}

/// The soldiers' play area of a layout: its combat area (see [`area`]), with the control
/// points, spawns, vehicle spawners, strategic areas and routes (`strategic`) outside it
/// kept, and ladders just outside.
fn infantry_area(layout: &GameModeDesc, ladders: &[Ladder], strategic: &[Vec3]) -> Option<PlayArea> {
    let mut area = PlayArea::for_layout(layout, Traveller::Soldier, COMBAT_AREA_MARGIN)?;
    let strategic = strategic.iter().map(|p| ("strategic area point".to_string(), *p));
    keep_points(&mut area, "infantry grid", gameplay_points(layout).chain(strategic));
    for ladder in ladders {
        let p = ladder.center.xz();
        if !area.inside_polygons(p) && area.nearest_edge_point(p).1 <= area.margin + LADDER_REACH {
            area.keep_point(p, 6.0);
        }
    }
    Some(area)
}

/// The land vehicles' play area of a layout: its combat area, with the control points and
/// vehicle spawners outside it kept.
fn land_area(layout: &GameModeDesc) -> Option<PlayArea> {
    let mut area = PlayArea::for_layout(layout, Traveller::Land, COMBAT_AREA_MARGIN)?;
    let points = gameplay_points(layout).filter(|(what, _)| !what.starts_with("spawn of"));
    keep_points(&mut area, "vehicle grid", points);
    Some(area)
}

/// The boats' play area of a layout, with the vehicle spawners outside it kept.
fn boat_area(layout: &GameModeDesc) -> Option<PlayArea> {
    let mut area = PlayArea::for_layout(layout, Traveller::Boat, COMBAT_AREA_MARGIN)?;
    for v in &layout.vehicle_spawners {
        area.keep_point(Vec3::from_array(v.placement.position).xz(), KEEP_RADIUS);
    }
    Some(area)
}

/// The strategic areas' positions and route waypoints of the layout (BF2's AI hints in the
/// level's `ai.ron`).
fn strategic_points(level: &LoadedLevel, layout: Option<&GameModeDesc>) -> Vec<Vec3> {
    let (Some(dir), Some(layout)) = (&level.dir, layout) else {
        return Vec::new();
    };
    let path = dir.join("ai.ron");
    if !path.exists() {
        return Vec::new();
    }
    let Ok(ai) = game_data::read_ron::<game_data::LevelAiDesc>(&path) else {
        return Vec::new();
    };
    let Some(desc) = ai.layout(&layout.mode, layout.size) else {
        return Vec::new();
    };
    desc.areas
        .iter()
        .flat_map(|a| std::iter::once(a.position).chain(a.infantry_position))
        .chain(desc.routes.iter().flat_map(|r| r.waypoints.iter().copied()))
        .map(Vec3::from_array)
        .collect()
}

/// The detail patches of a level (see [`patch`]) that are within the grid's bounds.
fn detail_patches(level: &LoadedLevel, geometry: &build::LevelGeometry, paths: Option<&game_shared::config::GamePaths>) -> Vec<build::Rect> {
    let Some(paths) = paths else {
        return Vec::new();
    };
    let template = |name: &str| paths.read_ron::<game_data::ObjectDesc>(format!("templates/{name}.ron")).ok();
    patch::detail_objects(&level.desc.statics, template, &geometry.meshes)
        .into_iter()
        .filter(|rect| {
            geometry.bounds.is_none_or(|(lo, hi)| {
                let (a, b) = rect.world_aabb();
                a.x < hi.x && b.x > lo.x && a.y < hi.y && b.y > lo.y
            })
        })
        .collect()
}

/// Collects the level's collision and builds (or loads) its grid on a background task.
fn start_build(
    mut commands: Commands,
    level: Res<LoadedLevel>,
    match_info: Query<&MatchInfo>,
    tuning: Res<SoldierTuning>,
    terrain: Query<&Terrain, With<LevelEntity>>,
    colliders: Query<
        (&Collider, &Transform, &CollisionLayers),
        (With<LevelEntity>, Without<ColliderDisabled>),
    >,
    ladder_parts: Query<(&Collider, &Transform), (With<LadderPart>, Without<ColliderDisabled>)>,
    paths: Option<Res<game_shared::config::GamePaths>>,
    cache: Option<Res<Cache>>,
) {
    commands.remove_resource::<Navigation>();
    commands.remove_resource::<vehicle::VehicleNavigation>();
    commands.remove_resource::<NavObstacles>();
    let params = NavParams {
        water_height: level.desc.water.as_ref().map(|w| w.height),
        ..NavParams::from_tuning(&tuning)
    };
    // Only the layout being played: the 64 player layouts of the big maps would take a lot
    // of memory. Layouts made from another (Rush from conquest) share its grid.
    let layout = match_info
        .iter()
        .next()
        .and_then(|info| level.base_layout(&info.mode, info.size));
    let strategic = strategic_points(&level, layout);
    let Collected { geometry, vehicle_meshes } = collect_geometry(
        terrain.iter().next().map(|t| t.0.clone()),
        colliders.iter(),
        ladder_parts.iter(),
        layout,
        &strategic,
    );
    let patches = detail_patches(&level, &geometry, paths.as_deref());
    // Imported levels only: the built-in test range builds in no time.
    let cache = cache.filter(|_| level.dir.is_some()).map(|c| c.clone());
    start_vehicle_build(&mut commands, &level, layout, &geometry, vehicle_meshes, paths.as_deref(), cache.clone());
    let name = level.desc.name.clone();
    let spawns = SpawnCheck {
        spawns: layout.map_or_else(Vec::new, |l| {
            l.spawn_points
                .iter()
                .map(|sp| (sp.control_point.clone(), Vec3::from_array(sp.placement.position)))
                .collect()
        }),
        control_points: layout.map_or_else(Vec::new, |l| {
            l.control_points.iter().map(|cp| (cp.id.clone(), Vec3::from_array(cp.position))).collect()
        }),
    };
    let task = AsyncComputeTaskPool::get()
        .spawn(async move { load_or_build(&geometry, &patches, params, cache.as_ref(), &name) });
    commands.insert_resource(NavBuild { task, spawns });
}

/// The grid from the cache if it is there and up to date, else built (and cached).
fn load_or_build(
    geometry: &build::LevelGeometry,
    patches: &[build::Rect],
    params: NavParams,
    cache: Option<&Cache>,
    name: &str,
) -> NavGrid {
    let started = Instant::now();
    let key = build::geometry_key(geometry, &params) ^ build::words_key(&patch::hash_rects(patches));
    if let Some(cache) = cache
        && let Some(grid) = cache::load(cache, name, "infantry", key, params)
    {
        info!(
            "nav: loaded the cached grid of `{name}` ({key:016x}: {} cells, {} in {} detail patches, {} ladders, {:.1} MB) in {:.2} s",
            grid.cell_count(),
            grid.patch_cell_count(),
            grid.patches().len(),
            grid.ladders().len(),
            grid.memory_bytes() as f32 / 1e6,
            started.elapsed().as_secs_f32()
        );
        return grid;
    }
    let grid = patch::build_level(geometry, patches, params);
    info!(
        "nav: built {}x{} grid for `{name}` in {:.2} s: {} cells ({} in {} detail patches), {} ladders, {:.1} MB",
        grid.width,
        grid.depth,
        started.elapsed().as_secs_f32(),
        grid.cell_count(),
        grid.patch_cell_count(),
        grid.patches().len(),
        grid.ladders().len(),
        grid.memory_bytes() as f32 / 1e6
    );
    if let Some(cache) = cache
        && let Err(err) = cache::save(cache, name, "infantry", key, &grid)
    {
        warn!("nav: can't cache the grid of `{name}` in {}: {err:#}", cache.root().display());
    }
    grid
}

/// A layout's spawn points (by control point id) and control points.
#[derive(Default)]
struct SpawnCheck {
    spawns: Vec<(String, Vec3)>,
    control_points: Vec<(String, Vec3)>,
}

impl SpawnCheck {
    /// Warns about spawn points soldiers can't walk from to their control point's flag (or,
    /// when most can't reach the flag, to the spawn point most of the others can walk to):
    /// pockets like the catwalks 1.4 m below a carrier's deck without its ladders (dropping in
    /// is possible, so they share the deck's region). Runs in the background once the grid is
    /// there.
    fn report(&self, grid: &NavGrid) {
        let cut_off = self.cut_off(grid);
        if !cut_off.is_empty() {
            warn!(
                "nav: {} spawn points can't walk to their control point: {}",
                cut_off.len(),
                cut_off.join(", ")
            );
        }
    }

    /// The spawn points of [`Self::report`], as "control point at position".
    fn cut_off(&self, grid: &NavGrid) -> Vec<String> {
        let mut cut_off = Vec::new();
        for (id, flag) in &self.control_points {
            let spawns: Vec<Vec3> = self.spawns.iter().filter(|(cp, _)| cp == id).map(|(_, at)| *at).collect();
            // The flag, else one of the first spawn points (a carrier's flag is on top of its
            // island, out of reach).
            let mut best: Option<(usize, Vec<bool>)> = None;
            for anchor in std::iter::once(*flag).chain(spawns.iter().copied().take(3)) {
                let reaches: Vec<bool> = spawns
                    .iter()
                    .map(|at| at.distance(anchor) < 1.0 || grid.find_path(*at, anchor).is_some_and(|p| p.complete))
                    .collect();
                let reaching = reaches.iter().filter(|r| **r).count();
                let enough = reaching == spawns.len() || (anchor == *flag && reaching * 2 >= spawns.len());
                if enough || best.as_ref().is_none_or(|(b, _)| reaching > *b) {
                    best = Some((reaching, reaches));
                }
                if enough {
                    break;
                }
            }
            if let Some((reaching, reaches)) = best
                && reaching > 0
            {
                let missing = spawns.iter().zip(&reaches).filter(|(_, r)| !**r);
                cut_off.extend(missing.map(|(at, _)| format!("{id} at {at:.1}")));
            }
        }
        cut_off
    }
}

/// Builds (or loads) the vehicle grids on a background task: see [`vehicle`].
fn start_vehicle_build(
    commands: &mut Commands,
    level: &LoadedLevel,
    layout: Option<&GameModeDesc>,
    infantry: &build::LevelGeometry,
    vehicle_meshes: Vec<build::MeshInstance>,
    paths: Option<&game_shared::config::GamePaths>,
    cache: Option<Cache>,
) {
    let (geometry, areas) = vehicle_geometry(infantry, vehicle_meshes, layout);
    let water = level.desc.water.as_ref().map(|w| w.height);
    let roads_desc = level.desc.roads.clone();
    let paths = paths.cloned();
    let name = level.desc.name.clone();
    let task = AsyncComputeTaskPool::get().spawn(async move {
        let started = Instant::now();
        let roads = paths.as_ref().map_or_else(Vec::new, |p| vehicle::road_triangles(p, &roads_desc));
        let input = vehicle::VehicleGeometry {
            geometry,
            roads,
            water,
            areas,
        };
        let params = vehicle::land_params();
        let key = build::geometry_key(&input.geometry, &params);
        let cached = cache.as_ref().and_then(|c| cache::load(c, &name, "vehicle", key, params));
        let was_cached = cached.is_some();
        let grid = vehicle::build_all(&input, cached);
        if !was_cached
            && let Some(cache) = &cache
            && let Err(err) = cache::save(cache, &name, "vehicle", key, &grid.land)
        {
            warn!("nav: can't cache the vehicle grid of `{name}` in {}: {err:#}", cache.root().display());
        }
        info!(
            "nav: vehicle grids in {:.2} s ({}): land {}x{} ({} cells, {} road columns from {} road triangles),              water {}, air {}",
            started.elapsed().as_secs_f32(),
            if was_cached { "cached" } else { "built" },
            grid.land.width,
            grid.land.depth,
            grid.land.cell_count(),
            grid.road_columns(),
            input.roads.len(),
            grid.water.as_ref().map_or("none".to_string(), |w| format!("{}x{}", w.width, w.depth)),
            grid.air.as_ref().map_or("none".to_string(), |a| format!("{}x{}", a.width, a.depth)),
        );
        grid
    });
    commands.insert_resource(VehicleNavBuild(task));
}

/// What the vehicle grids are built from besides the roads and the water: the land grid's
/// collision and bounds, and the play areas of land vehicles, boats and aircraft.
fn vehicle_geometry(
    infantry: &build::LevelGeometry,
    vehicle_meshes: Vec<build::MeshInstance>,
    layout: Option<&GameModeDesc>,
) -> (build::LevelGeometry, vehicle::VehicleAreas) {
    let area = layout.and_then(land_area);
    let mut bounds = layout.and_then(vehicle_bounds);
    if let Some(area) = &area {
        bounds = Some(area.crop(bounds));
    }
    let geometry = build::LevelGeometry {
        terrain: infantry.terrain.clone(),
        meshes: vehicle_meshes,
        bounds,
        area,
        ..default()
    };
    let air = |traveller| layout.and_then(|l| PlayArea::for_layout(l, traveller, COMBAT_AREA_MARGIN));
    let areas = vehicle::VehicleAreas {
        boats: layout.and_then(boat_area),
        helicopters: air(Traveller::Helicopter),
        jets: air(Traveller::Jet),
    };
    (geometry, areas)
}

fn finish_vehicle_build(mut commands: Commands, mut build: ResMut<VehicleNavBuild>) {
    if let Some(grid) = check_ready(&mut build.0) {
        commands.insert_resource(vehicle::VehicleNavigation(Arc::new(grid)));
        commands.remove_resource::<VehicleNavBuild>();
    }
}

fn finish_build(mut commands: Commands, mut build: ResMut<NavBuild>) {
    if let Some(grid) = check_ready(&mut build.task) {
        let grid = Arc::new(grid);
        let check = std::mem::take(&mut build.spawns);
        let for_check = grid.clone();
        AsyncComputeTaskPool::get().spawn(async move { check.report(&for_check) }).detach();
        commands.insert_resource(Navigation(grid));
        commands.insert_resource(NavObstacles::default());
        commands.remove_resource::<NavBuild>();
    }
}

/// The match ended and the level is gone.
fn forget_level(mut commands: Commands) {
    commands.remove_resource::<Navigation>();
    commands.remove_resource::<NavObstacles>();
    commands.remove_resource::<NavBuild>();
    commands.remove_resource::<vehicle::VehicleNavigation>();
    commands.remove_resource::<VehicleNavBuild>();
}

/// The area around the control points and spawn points of a layout.
fn gameplay_bounds(layout: &GameModeDesc) -> Option<(Vec2, Vec2)> {
    let points = layout
        .control_points
        .iter()
        .map(|cp| cp.position)
        .chain(layout.spawn_points.iter().map(|sp| sp.placement.position));
    let (min, max) = points.fold((Vec2::MAX, Vec2::MIN), |(min, max), p| {
        let p = Vec2::new(p[0], p[2]);
        (min.min(p), max.max(p))
    });
    (min.x <= max.x).then(|| (min - GAMEPLAY_MARGIN, max + GAMEPLAY_MARGIN))
}

/// The area around the control points, spawn points and vehicle spawners of a layout.
fn vehicle_bounds(layout: &GameModeDesc) -> Option<(Vec2, Vec2)> {
    let points = layout
        .control_points
        .iter()
        .map(|cp| cp.position)
        .chain(layout.spawn_points.iter().map(|sp| sp.placement.position))
        .chain(layout.vehicle_spawners.iter().map(|v| v.placement.position));
    let (min, max) = points.fold((Vec2::MAX, Vec2::MIN), |(min, max), p| {
        let p = Vec2::new(p[0], p[2]);
        (min.min(p), max.max(p))
    });
    (min.x <= max.x).then(|| (min - VEHICLE_MARGIN, max + VEHICLE_MARGIN))
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use avian3d::parry::shape::SharedShape;
    use bevy::{math::Affine3A, prelude::*};
    use game_shared::{level::Heightmap, soldier::SoldierTuning};

    use super::{
        NavGrid, NavParams, NavPath,
        build::{self, LevelGeometry, MeshInstance},
    };

    fn cuboid(center: Vec3, size: Vec3) -> MeshInstance {
        MeshInstance {
            shape: SharedShape::cuboid(size.x / 2.0, size.y / 2.0, size.z / 2.0),
            transform: Affine3A::from_translation(center),
        }
    }

    /// Flat ground from -32 to 32 m plus `meshes`.
    fn grid(meshes: Vec<MeshInstance>) -> NavGrid {
        let terrain = Heightmap {
            resolution: 33,
            spacing: 2.0,
            origin: Vec3::new(-32.0, 0.0, -32.0),
            heights: vec![0.0; 33 * 33],
        };
        let geometry = LevelGeometry {
            terrain: Some(Arc::new(terrain)),
            meshes,
            ..default()
        };
        build::build(&geometry, NavParams::from_tuning(&SoldierTuning::default()))
    }

    /// A 3 m wall along z = 0 over the whole level, with a gap from `door.0` to `door.1`.
    fn wall(door: Option<(f32, f32)>) -> Vec<MeshInstance> {
        let segment = |x0: f32, x1: f32| {
            cuboid(
                Vec3::new((x0 + x1) / 2.0, 1.5, 0.0),
                Vec3::new(x1 - x0, 3.0, 0.3),
            )
        };
        match door {
            Some((a, b)) => vec![segment(-32.0, a), segment(b, 32.0)],
            None => vec![segment(-32.0, 32.0)],
        }
    }

    /// Where the path crosses z = 0.
    fn crossings(path: &NavPath) -> Vec<f32> {
        path.waypoints
            .windows(2)
            .filter(|w| w[0].position.z.signum() != w[1].position.z.signum())
            .map(|w| {
                let (a, b) = (w[0].position, w[1].position);
                a.x + (b.x - a.x) * a.z / (a.z - b.z)
            })
            .collect()
    }

    #[test]
    fn walks_through_a_door() {
        let grid = grid(wall(Some((4.1, 5.1))));
        let path = grid
            .find_path(Vec3::new(-10.0, 0.0, -10.0), Vec3::new(-10.0, 0.0, 10.0))
            .unwrap();
        assert!(path.complete);
        let crossings = crossings(&path);
        assert_eq!(crossings.len(), 1, "{path:?}");
        assert!(
            (4.1..=5.1).contains(&crossings[0]),
            "crossed the wall at x = {}",
            crossings[0]
        );
        assert!(path.waypoints.len() <= 6, "path not smoothed: {path:?}");
    }

    #[test]
    fn walks_straight_lines_in_the_open_only() {
        let grid = grid(wall(Some((4.1, 5.1))));
        let y = Vec3::ZERO;
        assert!(grid.walkable_line(y + Vec3::new(-10.0, 0.0, -5.0), y + Vec3::new(-2.0, 0.0, -3.0)));
        // Through the wall: the other side is reachable, but only through the door.
        assert!(!grid.walkable_line(y + Vec3::new(-10.0, 0.0, -5.0), y + Vec3::new(-10.0, 0.0, 5.0)));
    }

    #[test]
    fn climbs_ladders() {
        // A 4 m platform from z = 0 to 10 with a ladder up its south face (at z = 0), and
        // one that ends high above it (up only) and one starting in the air (useless).
        let platform = cuboid(Vec3::new(0.0, 2.0, 5.0), Vec3::new(8.0, 4.0, 10.0));
        let ladder = |x: f32, bottom: f32, top: f32| {
            game_shared::ladder::Ladder::from_box(
                Vec3::new(x, (bottom + top) / 2.0, -0.1),
                Quat::from_rotation_y(std::f32::consts::PI),
                Vec3::new(0.3, (top - bottom) / 2.0, 0.1),
            )
        };
        let terrain = Heightmap {
            resolution: 33,
            spacing: 2.0,
            origin: Vec3::new(-32.0, 0.0, -32.0),
            heights: vec![0.0; 33 * 33],
        };
        let geometry = LevelGeometry {
            terrain: Some(Arc::new(terrain)),
            meshes: vec![platform],
            ladders: vec![ladder(0.0, 0.0, 4.3), ladder(-3.0, 0.0, 6.0), ladder(3.0, 1.5, 4.3)],
            ..default()
        };
        let grid = build::build(&geometry, NavParams::from_tuning(&SoldierTuning::default()));
        let downs: Vec<bool> = grid.ladders().iter().map(|l| l.down).collect();
        assert_eq!(downs, [true, false], "{:?}", grid.ladders());
        let (ground, top) = (Vec3::new(0.0, 0.0, -10.0), Vec3::new(0.0, 4.0, 6.0));
        let up = grid.find_path(ground, top).unwrap();
        assert!(up.complete, "{up:?}");
        let steps: Vec<_> = up.waypoints.iter().filter_map(|w| w.ladder).collect();
        assert_eq!(steps.len(), 1, "{up:?}");
        assert!(steps[0].up && steps[0].front.z < -0.9, "{steps:?}");
        let down = grid.find_path(top, ground).unwrap();
        assert!(down.complete && down.waypoints.iter().any(|w| w.ladder.is_some_and(|s| !s.up)), "{down:?}");

        let loaded = super::cache::decode(&super::cache::encode(&grid), grid.params).unwrap();
        assert_eq!(loaded.ladders(), grid.ladders());
    }

    #[test]
    fn walls_separate_regions() {
        let grid = grid(wall(None));
        let path = grid
            .find_path(Vec3::new(-10.0, 0.0, -10.0), Vec3::new(-10.0, 0.0, 10.0))
            .unwrap();
        assert!(!path.complete);
        assert!(crossings(&path).is_empty());
    }

    #[test]
    fn jumps_onto_ledges() {
        let params = NavParams::from_tuning(&SoldierTuning::default());
        let height = (params.step + params.jump) / 2.0;
        let grid = grid(vec![cuboid(
            Vec3::new(0.0, height / 2.0, 10.0),
            Vec3::new(6.0, height, 6.0),
        )]);
        let path = grid
            .find_path(Vec3::new(0.0, 0.0, -10.0), Vec3::new(0.0, height, 10.0))
            .unwrap();
        assert!(path.complete);
        assert_eq!(
            path.waypoints.iter().filter(|w| w.jump).count(),
            1,
            "{path:?}"
        );
        assert!((path.waypoints.last().unwrap().position.y - height).abs() < 0.1);
    }

    #[test]
    fn keeps_to_the_combat_area() {
        let terrain = Arc::new(Heightmap {
            resolution: 33,
            spacing: 2.0,
            origin: Vec3::new(-32.0, 0.0, -32.0),
            heights: vec![0.0; 33 * 33],
        });
        let params = NavParams::from_tuning(&SoldierTuning::default());
        // A 20 m square left of the middle with a 4 m margin, and a flag at (24, 0) kept
        // with a corridor.
        let mut area = super::PlayArea {
            polygons: vec![vec![Vec2::new(-20.0, -10.0), Vec2::new(0.0, -10.0), Vec2::new(0.0, 10.0), Vec2::new(-20.0, 10.0)]],
            margin: 4.0,
            keep: Vec::new(),
        };
        let plain = LevelGeometry {
            terrain: Some(terrain.clone()),
            ..default()
        };
        let confined = |area: &super::PlayArea| LevelGeometry {
            terrain: Some(terrain.clone()),
            bounds: Some(area.crop(None)),
            area: Some(area.clone()),
            ..default()
        };
        let full = build::build(&plain, params);
        let grid = build::build(&confined(&area), params);
        // Cropped to the area's bounds (28 m square), no cell outside the area.
        assert_eq!((grid.width, grid.depth), (56, 56));
        assert!(grid.cell_count() < full.cell_count() / 4, "{} of {}", grid.cell_count(), full.cell_count());
        let mut cells = 0;
        grid.for_each_cell(|c| {
            cells += 1;
            assert!(area.contains(grid.position(c).xz()), "cell at {}", grid.position(c));
        });
        // The square and its margin with rounded corners: 28 m square less (4 - pi) * 16 m2.
        let expected = (28.0f32 * 28.0 - (4.0 - std::f32::consts::PI) * 16.0) / 0.25;
        assert!((cells as f32 - expected).abs() < 0.03 * expected, "{cells} cells, expected about {expected}");
        // Outside: no column, no cell, and paths there end at the edge.
        let (inside, outside) = (Vec3::new(-10.0, 0.0, 0.0), Vec3::new(20.0, 0.0, 0.0));
        assert!(grid.column_at(outside.x, outside.z).is_none());
        assert!(grid.locate(outside, 3.0, None).is_none());
        assert!(grid.locate(Vec3::new(-10.0, 0.0, 13.0), 0.3, None).is_some(), "the margin is walkable");
        assert!(grid.locate(Vec3::new(-10.0, 0.0, 15.0), 0.3, None).is_none());
        let path = grid.find_path(inside, outside).unwrap();
        assert!(!path.complete && path.waypoints.last().unwrap().position.x < 4.5, "{path:?}");
        // Another area, another cache key; levels without one keep theirs.
        assert_ne!(build::geometry_key(&confined(&area), &params), build::geometry_key(&plain, &params));

        // A flag outside: kept, and reachable along the corridor.
        let flag = Vec3::new(24.0, 0.0, 0.0);
        assert!(area.keep_point(flag.xz(), 5.0));
        let grid = build::build(&confined(&area), params);
        assert!(grid.width > 56);
        let path = grid.find_path(inside, flag).unwrap();
        assert!(path.complete, "{path:?}");
        assert!(grid.locate(Vec3::new(10.0, 0.0, 9.0), 0.3, None).is_none(), "only a corridor");
    }

    #[test]
    fn caches_grids() {
        let grid = grid(wall(Some((4.1, 5.1))));
        let root = std::env::temp_dir().join(format!("navgrid_test_{}", std::process::id()));
        let cache = game_shared::cache::Cache::new(&root, game_shared::cache::DEFAULT_LIMIT);
        super::cache::save(&cache, "x", "infantry", 42, &grid).unwrap();
        assert!(super::cache::load(&cache, "x", "infantry", 43, grid.params).is_none());
        assert!(super::cache::load(&cache, "x", "vehicle", 42, grid.params).is_none());
        let loaded = super::cache::load(&cache, "x", "infantry", 42, grid.params).unwrap();
        let _ = std::fs::remove_dir_all(&root);
        assert_eq!(
            (loaded.origin, loaded.width, loaded.depth),
            (grid.origin, grid.width, grid.depth)
        );
        assert_eq!(loaded.columns, grid.columns);
        assert_eq!(
            bytemuck::cast_slice::<_, u8>(&loaded.cells),
            bytemuck::cast_slice::<_, u8>(&grid.cells)
        );
    }

    #[test]
    fn drops_down_but_not_up() {
        let grid = grid(vec![cuboid(
            Vec3::new(0.0, 1.0, 0.0),
            Vec3::new(6.0, 2.0, 6.0),
        )]);
        let (top, ground) = (Vec3::new(0.0, 2.0, 0.0), Vec3::new(0.0, 0.0, 10.0));
        let down = grid.find_path(top, ground).unwrap();
        assert!(
            down.complete && down.waypoints.iter().all(|w| !w.jump),
            "{down:?}"
        );
        assert!(!grid.find_path(ground, top).unwrap().complete);
    }

    #[test]
    fn climbs_ramps_and_walks_under_bridges() {
        // A 3 m platform with a 30 degree ramp up from the south, and a bridge over the
        // ground next to it.
        let ramp = [
            Vec3::new(-2.0, 0.0, -0.2),
            Vec3::new(2.0, 0.0, -0.2),
            Vec3::new(-2.0, 0.0, 5.0),
            Vec3::new(2.0, 0.0, 5.0),
            Vec3::new(-2.0, 3.0, 5.0),
            Vec3::new(2.0, 3.0, 5.0),
        ];
        let ramp = MeshInstance {
            shape: SharedShape::convex_hull(&ramp).unwrap(),
            transform: Affine3A::IDENTITY,
        };
        let platform = cuboid(Vec3::new(0.0, 1.5, 10.0), Vec3::new(4.0, 3.0, 10.0));
        let bridge = cuboid(Vec3::new(10.0, 3.1, 10.0), Vec3::new(3.0, 0.2, 10.0));
        let grid = grid(vec![ramp, platform, bridge]);
        let path = grid
            .find_path(Vec3::new(0.0, 0.0, -10.0), Vec3::new(0.0, 3.0, 12.0))
            .unwrap();
        assert!(path.complete, "{path:?}");
        assert!(path.waypoints.iter().all(|w| !w.jump), "{path:?}");

        let (x, z) = grid.column_at(10.0, 10.0).unwrap();
        let heights: Vec<f32> = grid
            .column(x, z)
            .map(|i| grid.cells[i as usize].y)
            .collect();
        assert_eq!(heights.len(), 2, "{heights:?}");
        assert!(
            heights[0].abs() < 0.1 && (heights[1] - 3.2).abs() < 0.1,
            "{heights:?}"
        );
    }
}
