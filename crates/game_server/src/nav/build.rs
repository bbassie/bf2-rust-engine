//! Building a [`NavGrid`] from collision geometry.
//!
//! 1. Rasterize: every triangle (and the terrain) is clipped to the grid columns it covers
//!    and added to that column as a solid span of heights. Overlapping spans merge; a span's
//!    top is walkable if the surface forming it is flat enough. Tiles of columns are
//!    rasterized in parallel.
//! 2. Every walkable span top with room for a standing soldier up to the next span becomes
//!    a cell.
//! 3. Cells link to the best cell in each neighbouring column that is within step, slope or
//!    jump height and leaves head room along the way.
//! 4. A distance field (to walls and ledges) lets paths keep off walls.
//! 5. Ladders link the cell at their foot to the one behind their top.
//! 6. Connected regions let path requests reject unreachable goals quickly.

use std::{
    collections::HashMap,
    sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    },
};

use avian3d::parry::shape::{SharedShape, TypedShape};
use bevy::{math::Affine3A, platform::collections::HashMap as Map, prelude::*};
use game_shared::{ladder::Ladder, level::Heightmap};

use super::{NavCell, NavGrid, NavLadder, NavParams, SLOPE_SCALE};

/// Bump when the build changes, to invalidate cached grids.
pub const VERSION: u32 = 6;

/// Columns per side of the tiles rasterized in parallel.
const TILE: u32 = 64;

/// Connected areas with fewer cells are dropped (tops of walls, furniture, roof ridges).
const MIN_REGION_CELLS: usize = 24;

/// Grids with more columns than this get bigger cells.
const MAX_COLUMNS: f32 = 4e6;

/// At most this many cells per column (links store the index in a `u8`).
const MAX_LAYERS: u16 = 250;

pub struct LevelGeometry {
    pub terrain: Option<Arc<Heightmap>>,
    pub meshes: Vec<MeshInstance>,
    pub ladders: Vec<Ladder>,
    /// XZ area to cover, clamped to the terrain. The whole level if `None`.
    pub bounds: Option<(Vec2, Vec2)>,
}

pub struct MeshInstance {
    pub shape: SharedShape,
    pub transform: Affine3A,
}

struct Instance<'a> {
    mesh: &'a MeshInstance,
    min: Vec3,
    max: Vec3,
}

/// A cell before linking.
#[derive(Clone, Copy)]
struct RawCell {
    y: f32,
    /// Bottom of the next solid span above.
    ceiling: f32,
    slope: u8,
}

pub fn build(geometry: &LevelGeometry, mut params: NavParams) -> NavGrid {
    let instances: Vec<Instance> = geometry
        .meshes
        .iter()
        .filter(|m| !matches!(m.shape.as_typed_shape(), TypedShape::HeightField(_)))
        .map(|mesh| {
            let (min, max) = world_aabb(mesh);
            Instance { mesh, min, max }
        })
        .collect();

    let terrain_area = geometry.terrain.as_ref().map(|t| {
        let origin = t.origin.xz();
        (origin, origin + t.world_size())
    });
    let (lo, hi) = match (geometry.bounds, terrain_area) {
        (Some((lo, hi)), Some((t_lo, t_hi))) => (lo.max(t_lo), hi.min(t_hi)),
        (Some(bounds), None) => bounds,
        (None, Some(area)) => area,
        (None, None) => instances
            .iter()
            .fold((Vec2::MAX, Vec2::MIN), |(lo, hi), i| {
                (lo.min(i.min.xz()), hi.max(i.max.xz()))
            }),
    };
    if !(lo.x < hi.x && lo.y < hi.y) {
        return NavGrid {
            params,
            origin: Vec2::ZERO,
            width: 0,
            depth: 0,
            columns: vec![0],
            cells: Vec::new(),
            ladders: Vec::new(),
            ladder_ends: Map::default(),
        };
    }
    // Huge areas get coarser cells to bound memory (about 16 bytes per column).
    let size = hi - lo;
    params.cell = params
        .cell
        .max(((size.x * size.y / MAX_COLUMNS).sqrt() * 4.0).ceil() / 4.0);
    let cell = params.cell;
    let origin = (lo / cell).floor() * cell;
    let width = ((hi.x - origin.x) / cell).ceil() as u32;
    let depth = ((hi.y - origin.y) / cell).ceil() as u32;
    let area_max = origin + Vec2::new(width as f32, depth as f32) * cell;

    // Heights are quantized relative to the lowest point.
    let (mut y_lo, mut y_hi) = (f32::MAX, f32::MIN);
    if let Some(t) = &geometry.terrain {
        for h in &t.heights {
            y_lo = y_lo.min(t.origin.y + h);
            y_hi = y_hi.max(t.origin.y + h);
        }
    }
    for i in &instances {
        if i.max.x >= origin.x
            && i.min.x <= area_max.x
            && i.max.z >= origin.y
            && i.min.z <= area_max.y
        {
            y_lo = y_lo.min(i.min.y);
            y_hi = y_hi.max(i.max.y);
        }
    }
    let y0 = y_lo - 1.0;
    let voxel = params.voxel.max((y_hi - y0 + 1.0) / 65000.0);

    // Which instances touch which tile.
    let tiles_x = width.div_ceil(TILE);
    let tiles_z = depth.div_ceil(TILE);
    let tile_size = cell * TILE as f32;
    let mut buckets = vec![Vec::new(); (tiles_x * tiles_z) as usize];
    for (index, i) in instances.iter().enumerate() {
        let t0 = ((i.min.xz() - origin) / tile_size).floor();
        let t1 = ((i.max.xz() - origin) / tile_size).floor();
        let (x0, z0) = ((t0.x as i64).max(0), (t0.y as i64).max(0));
        let (x1, z1) = (
            (t1.x as i64).min(tiles_x as i64 - 1),
            (t1.y as i64).min(tiles_z as i64 - 1),
        );
        for tz in z0..=z1 {
            for tx in x0..=x1 {
                buckets[(tz * tiles_x as i64 + tx) as usize].push(index as u32);
            }
        }
    }

    let ctx = Rasterizer {
        params,
        origin,
        width,
        depth,
        y0,
        voxel,
        terrain: geometry.terrain.as_deref(),
        instances: &instances,
        buckets: &buckets,
        tiles_x,
    };
    let tile_count = buckets.len();
    let next = AtomicUsize::new(0);
    // Half the cores, so the running game keeps its share.
    let threads = std::thread::available_parallelism()
        .map_or(1, |n| n.get() / 2)
        .clamp(1, 8);
    let mut tiles: Vec<Option<TileCells>> = (0..tile_count).map(|_| None).collect();
    std::thread::scope(|scope| {
        let workers: Vec<_> = (0..threads)
            .map(|_| {
                scope.spawn(|| {
                    let mut pool = SpanPool::default();
                    let mut done = Vec::new();
                    loop {
                        let tile = next.fetch_add(1, Ordering::Relaxed);
                        if tile >= tile_count {
                            break done;
                        }
                        done.push((tile, ctx.tile(tile, &mut pool)));
                    }
                })
            })
            .collect();
        for worker in workers {
            for (tile, cells) in worker.join().expect("nav tile worker panicked") {
                tiles[tile] = Some(cells);
            }
        }
    });

    // Gather the tiles' columns in grid order.
    let mut columns = Vec::with_capacity((width * depth + 1) as usize);
    let mut raw: Vec<RawCell> = Vec::new();
    columns.push(0);
    for z in 0..depth {
        for x in 0..width {
            let tile = tiles[((z / TILE) * tiles_x + x / TILE) as usize]
                .as_ref()
                .unwrap();
            let local = ((z % TILE) * tile.width + x % TILE) as usize;
            let (start, end) = (tile.starts[local] as usize, tile.starts[local + 1] as usize);
            raw.extend_from_slice(&tile.cells[start..end]);
            columns.push(raw.len() as u32);
        }
        if z % TILE == TILE - 1 {
            // Done with this row of tiles.
            let row = (z / TILE * tiles_x) as usize;
            tiles[row..row + tiles_x as usize]
                .iter_mut()
                .for_each(|t| *t = None);
        }
    }
    drop(tiles);

    let mut grid = NavGrid {
        params,
        origin,
        width,
        depth,
        columns,
        cells: raw
            .iter()
            .map(|c| NavCell {
                y: c.y,
                region: 0,
                links: [NavGrid::NONE; 4],
                dist: 0,
                slope: c.slope,
            })
            .collect(),
        ladders: Vec::new(),
        ladder_ends: Map::default(),
    };
    link(&mut grid, &raw);
    distance_field(&mut grid);
    place_ladders(&mut grid, &geometry.ladders);
    regions(&mut grid);
    grid
}

/// Links every cell to the best cell of each neighbouring column. Drops are one-way.
fn link(grid: &mut NavGrid, raw: &[RawCell]) {
    let params = grid.params;
    for z in 0..grid.depth {
        for x in 0..grid.width {
            for i in grid.column(x, z) {
                let a = raw[i as usize];
                let mut links = [NavGrid::NONE; 4];
                for (dir, (dx, dz)) in NavGrid::DIRS.into_iter().enumerate() {
                    let (nx, nz) = (x as i64 + dx as i64, z as i64 + dz as i64);
                    if nx < 0 || nz < 0 || nx >= grid.width as i64 || nz >= grid.depth as i64 {
                        continue;
                    }
                    let column = grid.column(nx as u32, nz as u32);
                    let mut best: Option<(f32, u32)> = None;
                    for j in column.clone() {
                        let b = raw[j as usize];
                        // Up by walking or jumping, down by walking or dropping.
                        let dy = b.y - a.y;
                        let reach = params.walk_climb(a.slope, b.slope).max(params.jump);
                        let head_room = a.ceiling.min(b.ceiling) - a.y.max(b.y);
                        if dy <= reach
                            && dy >= -reach.max(params.drop)
                            && head_room >= params.height
                            && best.is_none_or(|(d, _)| dy.abs() < d)
                        {
                            best = Some((dy.abs(), j - column.start));
                        }
                    }
                    if let Some((_, j)) = best {
                        links[dir] = j as u8;
                    }
                }
                grid.cells[i as usize].links = links;
            }
        }
    }
}

/// Recast-style chamfer distance to the edge of the walkable area (cells that can't walk
/// on in some direction: walls, ledges, jumps, drops): 2 per straight step, 3 per diagonal.
fn distance_field(grid: &mut NavGrid) {
    // The neighbour in a direction, if a soldier just walks there.
    let walk = |grid: &NavGrid, c: super::CellRef, dir: usize| {
        grid.neighbour(c, dir).filter(|n| {
            let (a, b) = (grid.cell(c), grid.cell(*n));
            (b.y - a.y).abs() <= grid.walk_climb(a, b)
        })
    };
    let mut dist = vec![u32::MAX / 2; grid.cells.len()];
    for z in 0..grid.depth {
        for x in 0..grid.width {
            for index in grid.column(x, z) {
                let c = super::CellRef { x, z, index };
                if (0..4).any(|dir| walk(grid, c, dir).is_none()) {
                    dist[index as usize] = 0;
                }
            }
        }
    }
    let relax =
        |grid: &NavGrid, dist: &mut [u32], c: super::CellRef, first: usize, second: usize| {
            let mut d = dist[c.index as usize];
            if let Some(a) = walk(grid, c, first) {
                d = d.min(dist[a.index as usize] + 2);
                if let Some(b) = walk(grid, a, second) {
                    d = d.min(dist[b.index as usize] + 3);
                }
            }
            dist[c.index as usize] = d;
        };
    for z in 0..grid.depth {
        for x in 0..grid.width {
            for index in grid.column(x, z) {
                let c = super::CellRef { x, z, index };
                relax(grid, &mut dist, c, 2, 3); // -X, then -Z
                relax(grid, &mut dist, c, 3, 0); // -Z, then +X
            }
        }
    }
    for z in (0..grid.depth).rev() {
        for x in (0..grid.width).rev() {
            for index in grid.column(x, z) {
                let c = super::CellRef { x, z, index };
                relax(grid, &mut dist, c, 0, 1); // +X, then +Z
                relax(grid, &mut dist, c, 1, 2); // +Z, then -X
            }
        }
    }
    for (cell, d) in grid.cells.iter_mut().zip(dist) {
        cell.dist = d.min(255) as u8;
    }
}

/// Links the cell in front of every ladder's foot with the one behind its top.
fn place_ladders(grid: &mut NavGrid, ladders: &[Ladder]) {
    for ladder in ladders {
        // Where soldiers get on in front, and land getting off over the top (see
        // `game_shared::soldier`).
        let hold = ladder.half.z + 0.35;
        let foot = ladder.world(Vec3::new(0.0, -ladder.top(), hold + 0.3));
        let head = ladder.world(Vec3::new(0.0, ladder.top(), -(hold + 0.4)));
        let (Some(bottom), Some(top)) = (nearest_cell(grid, foot, 1.2), nearest_cell(grid, head, 1.2)) else {
            continue;
        };
        let (low, high) = (grid.position(bottom), grid.position(top));
        if high.y - low.y < 1.5 || high.y > head.y + 0.5 {
            continue;
        }
        let index = grid.ladders.len() as u16;
        grid.ladders.push(NavLadder {
            bottom,
            top,
            foot: Vec3::new(foot.x, low.y, foot.z),
            head: Vec3::new(head.x, high.y, head.z),
            front: ladder.front,
        });
        grid.ladder_ends.entry(bottom.index).or_default().push(index);
        grid.ladder_ends.entry(top.index).or_default().push(index);
    }
}

/// Like [`NavGrid::locate`], before there are regions.
fn nearest_cell(grid: &NavGrid, pos: Vec3, radius: f32) -> Option<super::CellRef> {
    let (lo, hi) = (pos.xz() - radius, pos.xz() + radius);
    let (Some((x0, z0)), Some((x1, z1))) = (
        grid.column_at(lo.x.max(grid.origin.x), lo.y.max(grid.origin.y)),
        grid.column_at(
            hi.x.min(grid.origin.x + grid.width as f32 * grid.params.cell - 0.01),
            hi.y.min(grid.origin.y + grid.depth as f32 * grid.params.cell - 0.01),
        ),
    ) else {
        return None;
    };
    let mut best: Option<(f32, super::CellRef)> = None;
    for z in z0..=z1 {
        for x in x0..=x1 {
            for index in grid.column(x, z) {
                let c = super::CellRef { x, z, index };
                let p = grid.position(c);
                let dy = p.y - pos.y;
                if !(-3.0..=1.0).contains(&dy) || p.xz().distance(pos.xz()) > radius + grid.params.cell {
                    continue;
                }
                let score = p.xz().distance_squared(pos.xz()) + 4.0 * dy * dy;
                if best.is_none_or(|(b, _)| score < b) {
                    best = Some((score, c));
                }
            }
        }
    }
    best.map(|(_, c)| c)
}

/// Connected regions (ignoring which way drops go, and joined by ladders); tiny ones get
/// region 0.
fn regions(grid: &mut NavGrid) {
    let mut parent: Vec<u32> = (0..grid.cells.len() as u32).collect();
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
                let c = super::CellRef { x, z, index };
                for dir in 0..4 {
                    if let Some(n) = grid.neighbour(c, dir) {
                        let (a, b) = (root(&mut parent, index), root(&mut parent, n.index));
                        parent[a.max(b) as usize] = a.min(b);
                    }
                }
            }
        }
    }
    for ladder in &grid.ladders {
        let (a, b) = (root(&mut parent, ladder.bottom.index), root(&mut parent, ladder.top.index));
        parent[a.max(b) as usize] = a.min(b);
    }
    let mut sizes = vec![0usize; grid.cells.len()];
    for i in 0..grid.cells.len() as u32 {
        sizes[root(&mut parent, i) as usize] += 1;
    }
    let mut region_of = vec![0u16; grid.cells.len()];
    let mut next_region = 1u32;
    for i in 0..grid.cells.len() as u32 {
        let r = root(&mut parent, i) as usize;
        if r == i as usize && sizes[r] >= MIN_REGION_CELLS {
            region_of[r] = next_region.min(u16::MAX as u32) as u16;
            next_region += 1;
        }
        grid.cells[i as usize].region = region_of[r];
    }
}

struct Rasterizer<'a> {
    params: NavParams,
    origin: Vec2,
    width: u32,
    depth: u32,
    y0: f32,
    voxel: f32,
    terrain: Option<&'a Heightmap>,
    instances: &'a [Instance<'a>],
    buckets: &'a [Vec<u32>],
    tiles_x: u32,
}

/// The cells of one tile's columns.
struct TileCells {
    width: u32,
    /// Cells of local column `i` are `cells[starts[i]..starts[i + 1]]`.
    starts: Vec<u32>,
    cells: Vec<RawCell>,
}

impl Rasterizer<'_> {
    fn tile(&self, tile: usize, pool: &mut SpanPool) -> TileCells {
        let (tx, tz) = (tile as u32 % self.tiles_x, tile as u32 / self.tiles_x);
        let (x0, z0) = (tx * TILE, tz * TILE);
        let (w, d) = (TILE.min(self.width - x0), TILE.min(self.depth - z0));
        let cell = self.params.cell;
        let mut raster = TileRaster {
            pool,
            min: self.origin + Vec2::new(x0 as f32, z0 as f32) * cell,
            w,
            d,
            cell,
            y0: self.y0,
            voxel: self.voxel,
            // Tops this close together merge their walkability.
            merge: (self.params.step / self.voxel) as i32,
            min_normal_y: self.params.min_normal_y,
        };
        raster.pool.reset((w * d) as usize);
        if let Some(terrain) = self.terrain {
            raster.terrain(terrain);
        }
        for &i in &self.buckets[tile] {
            let instance = &self.instances[i as usize];
            for_each_triangle(&instance.mesh.shape, instance.mesh.transform, &mut |tri| {
                raster.triangle(tri)
            });
        }

        // Walkable tops with room above become cells.
        let mut out = TileCells {
            width: w,
            starts: Vec::with_capacity((w * d + 1) as usize),
            cells: Vec::new(),
        };
        out.starts.push(0);
        let pool = &*raster.pool;
        for &head in &pool.heads {
            let mut count = 0;
            let mut cur = head;
            while cur != END {
                let span = pool.spans[cur as usize];
                if span.walkable && count < MAX_LAYERS {
                    let y = self.y0 + span.max as f32 * self.voxel;
                    let ceiling = if span.next == END {
                        f32::MAX
                    } else {
                        self.y0 + pool.spans[span.next as usize].min as f32 * self.voxel
                    };
                    if ceiling - y >= self.params.height {
                        out.cells.push(RawCell {
                            y,
                            ceiling,
                            slope: span.slope,
                        });
                        count += 1;
                    }
                }
                cur = span.next;
            }
            out.starts.push(out.cells.len() as u32);
        }
        out
    }
}

const END: u32 = u32::MAX;

/// A solid vertical interval in a column, in voxels.
#[derive(Clone, Copy)]
struct Span {
    min: u16,
    max: u16,
    /// Whether the top is a walkable surface.
    walkable: bool,
    slope: u8,
    next: u32,
}

/// Per column a sorted linked list of disjoint spans.
#[derive(Default)]
struct SpanPool {
    heads: Vec<u32>,
    spans: Vec<Span>,
    free: u32,
}

impl SpanPool {
    fn reset(&mut self, columns: usize) {
        self.heads.clear();
        self.heads.resize(columns, END);
        self.spans.clear();
        self.free = END;
    }

    /// Adds a span, merging it with the spans it overlaps. The higher top decides whether
    /// the merged top is walkable; tops within `merge` voxels are walkable if either is.
    fn add(&mut self, column: usize, mut s: Span, merge: i32) {
        let mut prev = END;
        let mut cur = self.heads[column];
        while cur != END {
            let c = self.spans[cur as usize];
            if c.min > s.max {
                break;
            }
            if c.max < s.min {
                prev = cur;
                cur = c.next;
                continue;
            }
            let rise = c.max as i32 - s.max as i32;
            if rise > merge {
                s.walkable = c.walkable;
                s.slope = c.slope;
            } else if rise >= -merge && c.walkable {
                s.slope = if s.walkable {
                    s.slope.max(c.slope)
                } else {
                    c.slope
                };
                s.walkable = true;
            }
            s.min = s.min.min(c.min);
            s.max = s.max.max(c.max);
            let next = c.next;
            self.spans[cur as usize].next = self.free;
            self.free = cur;
            cur = next;
        }
        s.next = cur;
        let index = if self.free != END {
            let index = self.free;
            self.free = self.spans[index as usize].next;
            self.spans[index as usize] = s;
            index
        } else {
            self.spans.push(s);
            (self.spans.len() - 1) as u32
        };
        if prev == END {
            self.heads[column] = index;
        } else {
            self.spans[prev as usize].next = index;
        }
    }
}

struct TileRaster<'a> {
    pool: &'a mut SpanPool,
    /// World XZ of the tile's corner.
    min: Vec2,
    w: u32,
    d: u32,
    cell: f32,
    y0: f32,
    voxel: f32,
    merge: i32,
    min_normal_y: f32,
}

impl TileRaster<'_> {
    fn add(&mut self, x: u32, z: u32, y_min: f32, y_max: f32, walkable: bool, slope: u8) {
        let min = ((y_min - self.y0) / self.voxel).floor().clamp(0.0, 65535.0) as u16;
        let max = ((y_max - self.y0) / self.voxel).ceil().clamp(0.0, 65535.0) as u16;
        let span = Span {
            min,
            max: max.max(min),
            walkable,
            slope,
            next: END,
        };
        self.pool.add((z * self.w + x) as usize, span, self.merge);
    }

    /// The terrain is solid from the bottom of the grid up to its surface.
    fn terrain(&mut self, terrain: &Heightmap) {
        let (w, d, cell) = (self.w, self.d, self.cell);
        let stride = (w + 1) as usize;
        let mut corners = Vec::with_capacity(stride * (d + 1) as usize);
        for z in 0..=d {
            for x in 0..=w {
                let p = self.min + Vec2::new(x as f32, z as f32) * cell;
                corners.push(terrain.height_at(p.x, p.y));
            }
        }
        let max_tan = (1.0 - self.min_normal_y * self.min_normal_y)
            .max(0.0)
            .sqrt()
            / self.min_normal_y;
        for z in 0..d {
            for x in 0..w {
                let i = z as usize * stride + x as usize;
                let (h00, h10, h01, h11) = (
                    corners[i],
                    corners[i + 1],
                    corners[i + stride],
                    corners[i + stride + 1],
                );
                // The two triangles of the terrain's diagonal split (see `Heightmap::height_at`).
                let tan_a = Vec2::new(h10 - h00, h11 - h10).length() / cell;
                let tan_b = Vec2::new(h11 - h01, h01 - h00).length() / cell;
                let tan = tan_a.max(tan_b);
                let top = h00.max(h10).max(h01).max(h11);
                let slope = (tan * SLOPE_SCALE).ceil().min(255.0) as u8;
                self.add(x, z, self.y0, top, tan <= max_tan, slope);
            }
        }
    }

    fn triangle(&mut self, tri: [Vec3; 3]) {
        let t_min = tri[0].min(tri[1]).min(tri[2]);
        let t_max = tri[0].max(tri[1]).max(tri[2]);
        let (w, d, cell) = (self.w, self.d, self.cell);
        let tile_max = self.min + Vec2::new(w as f32, d as f32) * cell;
        if t_max.x < self.min.x
            || t_min.x > tile_max.x
            || t_max.z < self.min.y
            || t_min.z > tile_max.y
        {
            return;
        }
        let normal = (tri[1] - tri[0]).cross(tri[2] - tri[0]);
        let area = normal.length();
        if area < 1e-9 {
            return;
        }
        // Either winding: collision meshes aren't always consistent.
        let ny = normal.y.abs() / area;
        let walkable = ny >= self.min_normal_y;
        let slope = if walkable {
            ((1.0 - ny * ny).max(0.0).sqrt() / ny * SLOPE_SCALE)
                .ceil()
                .min(255.0) as u8
        } else {
            0
        };

        let vertical = ny < 0.01;
        let min_area = 1e-4 * cell * cell;
        let (mut row, mut tmp, mut poly) = ([Vec3::ZERO; 12], [Vec3::ZERO; 12], [Vec3::ZERO; 12]);
        let z0 = (((t_min.z - self.min.y) / cell).floor() as i32).max(0);
        let z1 = (((t_max.z - self.min.y) / cell).floor() as i32).min(d as i32 - 1);
        for z in z0..=z1 {
            let lo = self.min.y + z as f32 * cell;
            let n = clip(&tri, &mut tmp, 2, lo, true);
            let n_row = clip(&tmp[..n], &mut row, 2, lo + cell, false);
            if n_row < 3 {
                continue;
            }
            let (row_min, row_max) = row[..n_row]
                .iter()
                .fold((f32::MAX, f32::MIN), |(lo, hi), p| {
                    (lo.min(p.x), hi.max(p.x))
                });
            let x0 = (((row_min - self.min.x) / cell).floor() as i32).max(0);
            let x1 = (((row_max - self.min.x) / cell).floor() as i32).min(w as i32 - 1);
            for x in x0..=x1 {
                let lo = self.min.x + x as f32 * cell;
                let n = clip(&row[..n_row], &mut tmp, 0, lo, true);
                let n_poly = clip(&tmp[..n], &mut poly, 0, lo + cell, false);
                // Walls count wherever they touch a column, floors only where they cover
                // some of it (not along an edge that lies on the column boundary).
                if n_poly < 3 || (!vertical && projected_area(&poly[..n_poly]) < min_area) {
                    continue;
                }
                let (y_min, y_max) = poly[..n_poly]
                    .iter()
                    .fold((f32::MAX, f32::MIN), |(lo, hi), p| {
                        (lo.min(p.y), hi.max(p.y))
                    });
                self.add(x as u32, z as u32, y_min, y_max, walkable, slope);
            }
        }
    }
}

/// Clips a convex polygon to the half space `p[axis] >= bound` (or `<=`), returning the
/// number of vertices written to `out`.
fn clip(poly: &[Vec3], out: &mut [Vec3; 12], axis: usize, bound: f32, keep_above: bool) -> usize {
    let side = |p: Vec3| {
        if keep_above {
            p[axis] - bound
        } else {
            bound - p[axis]
        }
    };
    let mut n = 0;
    for i in 0..poly.len() {
        let (a, b) = (poly[i], poly[(i + 1) % poly.len()]);
        let (da, db) = (side(a), side(b));
        if da >= 0.0 && n < out.len() {
            out[n] = a;
            n += 1;
        }
        if (da >= 0.0) != (db >= 0.0) && n < out.len() {
            out[n] = a + (b - a) * (da / (da - db));
            n += 1;
        }
    }
    n
}

/// Area of a polygon seen from above.
fn projected_area(poly: &[Vec3]) -> f32 {
    let twice: f32 = (0..poly.len())
        .map(|i| {
            let (a, b) = (poly[i], poly[(i + 1) % poly.len()]);
            a.x * b.z - b.x * a.z
        })
        .sum();
    twice.abs() * 0.5
}

/// Calls `f` with every triangle of a shape, in world space.
fn for_each_triangle(shape: &SharedShape, transform: Affine3A, f: &mut impl FnMut([Vec3; 3])) {
    let owned;
    let (vertices, indices): (&[Vec3], &[[u32; 3]]) = match shape.as_typed_shape() {
        TypedShape::TriMesh(mesh) => (mesh.vertices(), mesh.indices()),
        TypedShape::Compound(compound) => {
            for (pose, part) in compound.shapes() {
                let local = Affine3A::from_rotation_translation(pose.rotation, pose.translation);
                for_each_triangle(part, transform * local, f);
            }
            return;
        }
        TypedShape::Cuboid(cuboid) => {
            owned = cuboid.to_trimesh();
            (&owned.0, &owned.1)
        }
        TypedShape::ConvexPolyhedron(polyhedron) => {
            owned = polyhedron.to_trimesh();
            (&owned.0, &owned.1)
        }
        // The terrain is rasterized from its heightmap; other shapes don't occur in levels.
        _ => return,
    };
    for [a, b, c] in indices {
        f([
            transform.transform_point3(vertices[*a as usize]),
            transform.transform_point3(vertices[*b as usize]),
            transform.transform_point3(vertices[*c as usize]),
        ]);
    }
}

fn world_aabb(mesh: &MeshInstance) -> (Vec3, Vec3) {
    let aabb = mesh.shape.compute_local_aabb();
    let (lo, hi) = (aabb.mins, aabb.maxs);
    (0..8).fold((Vec3::MAX, Vec3::MIN), |(min, max), i| {
        let corner = Vec3::new(
            if i & 1 == 0 { lo.x } else { hi.x },
            if i & 2 == 0 { lo.y } else { hi.y },
            if i & 4 == 0 { lo.z } else { hi.z },
        );
        let p = mesh.transform.transform_point3(corner);
        (min.min(p), max.max(p))
    })
}

/// Identifies the grid a geometry and parameter set produce, for the cache.
pub fn geometry_key(geometry: &LevelGeometry, params: &NavParams) -> u64 {
    let mut h = Fnv::default();
    h.bytes(&VERSION.to_le_bytes());
    for v in [
        params.cell,
        params.voxel,
        params.height,
        params.min_normal_y,
        params.step,
        params.jump,
        params.drop,
    ] {
        h.f32(v);
    }
    if let Some((lo, hi)) = geometry.bounds {
        [lo.x, lo.y, hi.x, hi.y].into_iter().for_each(|v| h.f32(v));
    }
    for ladder in &geometry.ladders {
        for v in [ladder.center, ladder.up, ladder.front, ladder.half] {
            v.to_array().into_iter().for_each(|v| h.f32(v));
        }
    }
    if let Some(t) = &geometry.terrain {
        h.bytes(&t.resolution.to_le_bytes());
        h.f32(t.spacing);
        t.origin.to_array().into_iter().for_each(|v| h.f32(v));
        h.bytes(bytemuck::cast_slice(&t.heights));
    }
    // Shapes are shared between instances: hash each once. Instances are summed so their
    // order doesn't matter.
    let mut shapes: HashMap<usize, u64> = HashMap::new();
    let mut instances = 0u64;
    for mesh in &geometry.meshes {
        let ptr = Arc::as_ptr(&mesh.shape.0) as *const () as usize;
        let shape = *shapes.entry(ptr).or_insert_with(|| {
            let mut h = Fnv::default();
            for_each_triangle(&mesh.shape, Affine3A::IDENTITY, &mut |tri| {
                tri.iter().flat_map(|p| p.to_array()).for_each(|v| h.f32(v))
            });
            h.0
        });
        let mut instance = Fnv::default();
        instance.bytes(&shape.to_le_bytes());
        mesh.transform
            .to_cols_array()
            .into_iter()
            .for_each(|v| instance.f32(v));
        instances = instances.wrapping_add(instance.0);
    }
    h.bytes(&instances.to_le_bytes());
    h.bytes(&(geometry.meshes.len() as u64).to_le_bytes());
    h.0
}

/// FNV-1a, stable across runs and platforms.
struct Fnv(u64);

impl Default for Fnv {
    fn default() -> Self {
        Self(0xcbf2_9ce4_8422_2325)
    }
}

impl Fnv {
    fn bytes(&mut self, bytes: &[u8]) {
        for &b in bytes {
            self.0 = (self.0 ^ b as u64).wrapping_mul(0x0100_0000_01b3);
        }
    }

    fn f32(&mut self, v: f32) {
        self.bytes(&v.to_le_bytes());
    }
}
