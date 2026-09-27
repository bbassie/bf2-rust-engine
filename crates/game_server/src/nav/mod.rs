//! Bot navigation: a layered walkability grid built from the level's own collision.
//!
//! When a level loads, its collision (the terrain heightfield and the static trimeshes on
//! [`GameLayer::World`]) is rasterized in the background into columns of cells, much like
//! Recast's voxel heightfield: every surface a soldier can stand on with room for a standing
//! soldier above it becomes a cell, so multi-storey buildings have several cells per column.
//! Cells link to the neighbours a soldier can walk, step or jump to. Paths are found with A*
//! on background tasks and shortened by walking straight lines over the grid (see
//! [`NavGrid::find_path`]). The grid covers the played layout's control points and spawns.
//! Grids are cached next to the level (`navgrid_<mode>_<size>.bin`), keyed by a hash of
//! the collision geometry and the movement limits.

mod build;
mod cache;
mod path;

use std::{ops::Range, sync::Arc, time::Instant};

use avian3d::prelude::*;
use bevy::{
    platform::collections::HashMap,
    prelude::*,
    tasks::{AsyncComputeTaskPool, Task, futures::check_ready},
};
use bytemuck::{Pod, Zeroable};
use game_data::GameModeDesc;
use game_shared::{
    ladder::{Ladder, LadderPart},
    level::{LevelEntity, LoadedLevel, Terrain},
    physics::GameLayer,
    protocol::MatchInfo,
    soldier::{SOLDIER_HEIGHT, SoldierTuning},
};

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
                forget_level
                    .run_if(not(resource_exists::<LoadedLevel>))
                    .run_if(resource_exists::<Navigation>.or_else(resource_exists::<NavBuild>)),
            )
                .chain(),
        );
    }
}

/// The navigation grid of the loaded level, once it is built.
#[derive(Resource, Clone)]
pub struct Navigation(pub Arc<NavGrid>);

#[derive(Resource)]
struct NavBuild(Task<NavGrid>);

/// How far around the control points and spawn points the grid reaches, meters.
const GAMEPLAY_MARGIN: f32 = 60.0;

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
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct CellRef {
    pub x: u32,
    pub z: u32,
    /// Index into the grid's cells.
    pub index: u32,
}

/// The walkable surfaces of a level. See the module docs.
pub struct NavGrid {
    pub params: NavParams,
    /// World XZ of the outer corner of column (0, 0).
    pub origin: Vec2,
    pub width: u32,
    pub depth: u32,
    /// The cells of column `z * width + x` are `cells[columns[i]..columns[i + 1]]`, bottom
    /// to top.
    columns: Vec<u32>,
    cells: Vec<NavCell>,
    ladders: Vec<NavLadder>,
    /// Ladders by the cells at their ends.
    ladder_ends: HashMap<u32, Vec<u16>>,
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

    pub fn memory_bytes(&self) -> usize {
        self.columns.len() * 4 + self.cells.len() * size_of::<NavCell>()
    }

    /// The column containing a world position.
    pub fn column_at(&self, x: f32, z: f32) -> Option<(u32, u32)> {
        let cx = ((x - self.origin.x) / self.params.cell).floor();
        let cz = ((z - self.origin.y) / self.params.cell).floor();
        (cx >= 0.0 && cz >= 0.0 && cx < self.width as f32 && cz < self.depth as f32)
            .then_some((cx as u32, cz as u32))
    }

    /// Cell indices of a column.
    pub fn column(&self, x: u32, z: u32) -> Range<u32> {
        let i = (z * self.width + x) as usize;
        self.columns[i]..self.columns[i + 1]
    }

    pub fn cell(&self, c: CellRef) -> &NavCell {
        &self.cells[c.index as usize]
    }

    /// World position of the middle of a cell's surface.
    pub fn position(&self, c: CellRef) -> Vec3 {
        let cell = self.params.cell;
        Vec3::new(
            self.origin.x + (c.x as f32 + 0.5) * cell,
            self.cells[c.index as usize].y,
            self.origin.y + (c.z as f32 + 0.5) * cell,
        )
    }

    /// The cell linked in direction `dir` (an index into [`Self::DIRS`]).
    pub fn neighbour(&self, c: CellRef, dir: usize) -> Option<CellRef> {
        let link = self.cells[c.index as usize].links[dir];
        if link == Self::NONE {
            return None;
        }
        let (dx, dz) = Self::DIRS[dir];
        let (x, z) = (c.x.wrapping_add_signed(dx), c.z.wrapping_add_signed(dz));
        Some(CellRef {
            x,
            z,
            index: self.column(x, z).start + link as u32,
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
        let cell = self.params.cell;
        let r = (radius / cell).ceil() as i32;
        let cx = ((pos.x - self.origin.x) / cell).floor() as i32;
        let cz = ((pos.z - self.origin.y) / cell).floor() as i32;
        let mut best: Option<(f32, CellRef)> = None;
        for z in (cz - r).max(0)..=(cz + r).min(self.depth as i32 - 1) {
            for x in (cx - r).max(0)..=(cx + r).min(self.width as i32 - 1) {
                let (x, z) = (x as u32, z as u32);
                let center = self.origin + (Vec2::new(x as f32, z as f32) + 0.5) * cell;
                let d2 = center.distance_squared(pos.xz());
                if d2 > (radius + cell) * (radius + cell) || best.is_some_and(|(b, _)| d2 >= b) {
                    continue;
                }
                for index in self.column(x, z) {
                    let c = &self.cells[index as usize];
                    let dy = c.y - pos.y;
                    let wanted = c.region != 0 && region.is_none_or(|r| r == c.region);
                    if !wanted || !(-3.0..=1.0).contains(&dy) {
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
        best.map(|(_, c)| c)
    }

    /// Usable cells in the columns within `radius` of `center` (XZ).
    pub fn cells_near(&self, center: Vec2, radius: f32) -> impl Iterator<Item = CellRef> + '_ {
        let cell = self.params.cell;
        let lo = ((center - radius - self.origin) / cell)
            .floor()
            .max(Vec2::ZERO);
        let hi = ((center + radius - self.origin) / cell)
            .floor()
            .min(Vec2::new(self.width as f32 - 1.0, self.depth as f32 - 1.0));
        let (x0, z0, x1, z1) = (lo.x as u32, lo.y as u32, hi.x as i64, hi.y as i64);
        (z0 as i64..=z1).flat_map(move |z| {
            (x0 as i64..=x1).flat_map(move |x| {
                let (x, z) = (x as u32, z as u32);
                self.column(x, z)
                    .filter(|&index| self.cells[index as usize].region != 0)
                    .map(move |index| CellRef { x, z, index })
            })
        })
    }
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
) {
    commands.remove_resource::<Navigation>();
    let params = NavParams::from_tuning(&tuning);
    let meshes = colliders
        .iter()
        .filter(|(_, _, layers)| layers.memberships.has_all(GameLayer::World))
        .map(|(collider, transform, _)| build::MeshInstance {
            shape: collider.shape().clone(),
            transform: transform.compute_affine(),
        })
        .collect();
    // Only the layout being played: the 64 player layouts of the big maps would take a lot
    // of memory.
    let layout = match_info
        .iter()
        .next()
        .and_then(|info| level.game_mode(&info.mode, info.size));
    // The boxes movement climbs (see `game_shared::ladder`).
    let ladders = ladder_parts
        .iter()
        .map(|(collider, transform)| {
            let aabb = collider.aabb(Vec3::ZERO, Quat::IDENTITY);
            let half = aabb.size() * 0.5 * transform.scale.abs();
            Ladder::from_box(transform.transform_point(aabb.center()), transform.rotation, half)
        })
        .collect();
    let geometry = build::LevelGeometry {
        terrain: terrain.iter().next().map(|t| t.0.clone()),
        meshes,
        ladders,
        bounds: layout.and_then(gameplay_bounds),
    };
    let cache_path = level.dir.as_ref().map(|dir| {
        dir.join(match layout {
            Some(l) => format!("navgrid_{}_{}.bin", l.mode, l.size),
            None => "navgrid.bin".into(),
        })
    });
    let name = level.desc.name.clone();
    let task = AsyncComputeTaskPool::get().spawn(async move {
        let started = Instant::now();
        let key = build::geometry_key(&geometry, &params);
        if let Some(path) = &cache_path
            && let Some(grid) = cache::load(path, key, params)
        {
            info!(
                "nav: loaded {} ({} cells) in {:.2} s",
                path.display(),
                grid.cell_count(),
                started.elapsed().as_secs_f32()
            );
            return grid;
        }
        let grid = build::build(&geometry, params);
        info!(
            "nav: built {}x{} grid for `{name}` in {:.2} s: {} cells, {:.1} MB",
            grid.width,
            grid.depth,
            started.elapsed().as_secs_f32(),
            grid.cell_count(),
            grid.memory_bytes() as f32 / 1e6
        );
        if let Some(path) = &cache_path
            && let Err(err) = cache::save(path, key, &grid)
        {
            warn!("nav: can't write {}: {err:#}", path.display());
        }
        grid
    });
    commands.insert_resource(NavBuild(task));
}

fn finish_build(mut commands: Commands, mut build: ResMut<NavBuild>) {
    if let Some(grid) = check_ready(&mut build.0) {
        commands.insert_resource(Navigation(Arc::new(grid)));
        commands.remove_resource::<NavBuild>();
    }
}

/// The match ended and the level is gone.
fn forget_level(mut commands: Commands) {
    commands.remove_resource::<Navigation>();
    commands.remove_resource::<NavBuild>();
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
            ladders: Vec::new(),
            bounds: None,
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
        // A 4 m platform from z = 0 to 10 with a ladder on its south face (at z = 0).
        let platform = cuboid(Vec3::new(0.0, 2.0, 5.0), Vec3::new(8.0, 4.0, 10.0));
        let ladder = game_shared::ladder::Ladder::from_box(
            Vec3::new(0.0, 2.5, -0.1),
            Quat::from_rotation_y(std::f32::consts::PI),
            Vec3::new(0.3, 2.5, 0.1),
        );
        let terrain = Heightmap {
            resolution: 33,
            spacing: 2.0,
            origin: Vec3::new(-32.0, 0.0, -32.0),
            heights: vec![0.0; 33 * 33],
        };
        let geometry = LevelGeometry {
            terrain: Some(Arc::new(terrain)),
            meshes: vec![platform],
            ladders: vec![ladder],
            bounds: None,
        };
        let grid = build::build(&geometry, NavParams::from_tuning(&SoldierTuning::default()));
        assert_eq!(grid.ladders().len(), 1, "ladder not placed");
        let (ground, top) = (Vec3::new(0.0, 0.0, -10.0), Vec3::new(0.0, 4.0, 6.0));
        let up = grid.find_path(ground, top).unwrap();
        assert!(up.complete, "{up:?}");
        let steps: Vec<_> = up.waypoints.iter().filter_map(|w| w.ladder).collect();
        assert_eq!(steps.len(), 1, "{up:?}");
        assert!(steps[0].up && steps[0].front.z < -0.9, "{steps:?}");
        let down = grid.find_path(top, ground).unwrap();
        assert!(down.complete && down.waypoints.iter().any(|w| w.ladder.is_some_and(|s| !s.up)), "{down:?}");

        let path = std::env::temp_dir().join(format!("navgrid_ladder_{}.bin", std::process::id()));
        super::cache::save(&path, 7, &grid).unwrap();
        let loaded = super::cache::load(&path, 7, grid.params).unwrap();
        std::fs::remove_file(&path).unwrap();
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
    fn caches_grids() {
        let grid = grid(wall(Some((4.1, 5.1))));
        let path = std::env::temp_dir().join(format!("navgrid_test_{}.bin", std::process::id()));
        super::cache::save(&path, 42, &grid).unwrap();
        assert!(super::cache::load(&path, 43, grid.params).is_none());
        let loaded = super::cache::load(&path, 42, grid.params).unwrap();
        std::fs::remove_file(&path).unwrap();
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
