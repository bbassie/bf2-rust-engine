//! A generated "tactical" map of the loaded level: a modern, Battlefield 6-style look offered
//! next to BF2's own minimap image. Dark slate ground with hillshade and faint contour lines,
//! buildings as lighter blocks (brighter the taller) with a soft drop shadow, roads as light
//! bands, water in a muted blue, trees as dark teal blobs, and outside the local team's combat
//! area a darker, hatched ground behind a thin light border.
//!
//! The picture covers exactly the terrain square, north up, in the UV space of
//! [`crate::map_icons::map_uv`] and BF2's minimap image: texel `(i, j)` of an `n x n` map is
//! centred on `map_point((i + 0.5) / n, (j + 0.5) / n)`, so every marker lines up.
//!
//! How it is made (on [`AsyncComputeTaskPool`], never on the frame):
//! - **Base picture** (terrain, water, roads, trees, buildings, shadows), per level and
//!   layout. Buildings come from the static objects' colliders the level already spawned:
//!   their triangles are rasterized top down into a height buffer, and whatever stands more
//!   than [`STRUCTURE_MIN_HEIGHT`] above the terrain is a structure. Trees are the static
//!   objects with vegetation meshes and the level's overgrowth, drawn as blobs sized by
//!   their meshes. Cached ([`game_shared::cache`], `tactical/<level>/base-<key>.bin`, RGBA)
//!   under a hash of the inputs and [`GENERATOR_VERSION`], so the next load of the level only
//!   reads it back, and the layouts of a level share one picture unless their statics differ.
//! - **Composite** for the local player's team: the base with the combat area's outside
//!   darkened and hatched and its border drawn ([`TacticalMap::image`]), plus a mask of the
//!   combat area ([`TacticalMap::bounds`]). Redone (a fraction of a second) when the team
//!   changes. Levels imported before combat areas were get the base picture and no mask.
//!
//! `BF2_TACTICAL_MAP_DUMP=<dir>` writes each finished map to `<dir>/<level>.png` (plus
//! `<level>_bounds.png`), for checking it; the scenarios in `scenarios/tactical_map/` do so
//! into `target/scenarios/tactical_map/` without it.

use std::{
    path::{Path, PathBuf},
    sync::{Arc, Mutex},
    time::Instant,
};

use avian3d::{
    parry::shape::{SharedShape, TypedShape},
    prelude::{Collider, ColliderDisabled, CollisionLayers},
};
use bevy::{
    asset::RenderAssetUsages,
    image::{ImageAddressMode, ImageFilterMode, ImageSampler, ImageSamplerDescriptor},
    math::Affine3A,
    platform::collections::HashMap,
    prelude::*,
    render::render_resource::{Extent3d, TextureDimension, TextureFormat},
    tasks::{AsyncComputeTaskPool, Task, futures::check_ready},
};
use game_data::VegetationDesc;
use game_shared::{
    cache::Cache,
    physics::GameLayer,
    config::GamePaths,
    level::{Heightmap, LevelEntity, LoadedLevel},
    protocol::{MatchInfo, Team},
    statics::{Inactive, StaticMesh},
};

use crate::net::LocalPlayer;

/// The generated map for the current level.
#[derive(Resource, Default)]
pub struct TacticalMap {
    /// The generated map for the current level, same UV space as the BF2 minimap image /
    /// `map_icons::map_uv`. None until ready.
    pub image: Option<Handle<Image>>,
    /// 1 inside the local team's combat area, 0 outside (same UV space). None for levels
    /// without combat areas (imported before they were).
    pub bounds: Option<Handle<Image>>,
    /// Bumped whenever image/bounds change, so consumers can refresh materials.
    pub version: u32,
}

/// Bump when the generated picture changes, to invalidate cached maps.
const GENERATOR_VERSION: u32 = 3;

// ---------------------------------------------------------------------------------------------
// Palette (sRGB) and look. Tune here.
// ---------------------------------------------------------------------------------------------

/// Ground at the level's lowest and highest points.
const GROUND_LOW: [f32; 3] = rgb(0x28323e);
const GROUND_HIGH: [f32; 3] = rgb(0x3a4756);
/// How much slopes facing the light (north-west) brighten and the others darken.
const HILLSHADE_STRENGTH: f32 = 0.9;
/// Vertical exaggeration of the terrain for the hillshade.
const HILLSHADE_EXAGGERATION: f32 = 2.0;
/// Contour lines every this many meters, darkening by this much.
const CONTOUR_INTERVAL: f32 = 10.0;
const CONTOUR_STRENGTH: f32 = 0.07;
/// Every fifth contour line.
const CONTOUR_MAJOR_STRENGTH: f32 = 0.12;

const WATER_SHALLOW: [f32; 3] = rgb(0x36526a);
const WATER_DEEP: [f32; 3] = rgb(0x263c50);
/// Depth (m) at which the water reaches [`WATER_DEEP`].
const WATER_DEEP_DEPTH: f32 = 10.0;
/// The shore line.
const WATER_EDGE: [f32; 3] = rgb(0x6e8ea3);

const ROAD: [f32; 3] = rgb(0x8f9aa6);
/// Unpaved roads are drawn this much weaker.
const DIRT_ROAD_OPACITY: f32 = 0.5;
const ROAD_OPACITY: f32 = 0.85;
/// Flat objects just above the ground (runways, platforms, pavements) look like roads, this
/// strongly.
const PAVEMENT_OPACITY: f32 = 0.6;
/// Tops between these heights (m) above the terrain are pavement.
const PAVEMENT_HEIGHTS: [f32; 2] = [0.03, 0.6];

const TREE: [f32; 3] = rgb(0x1f3d42);
/// The lit (north-west) side of a tree.
const TREE_LIGHT: [f32; 3] = rgb(0x2e5a5c);
const TREE_OPACITY: f32 = 0.9;
/// Darkening of the ground under a tree's shadow.
const TREE_SHADOW: f32 = 0.35;

/// Structures: the lowest ones (just above [`STRUCTURE_MIN_HEIGHT`]) and those
/// [`BUILDING_TALL`] meters high or more.
const BUILDING_LOW: [f32; 3] = rgb(0x596c82);
const BUILDING_HIGH: [f32; 3] = rgb(0xa6b7c9);
const BUILDING_TALL: f32 = 25.0;
/// The light (north-west) edge of a roof.
const BUILDING_EDGE: [f32; 3] = rgb(0xc9d4de);
/// Pitched roofs: how much faces toward the light brighten.
const ROOF_SHADE: f32 = 0.5;

/// Darkening of what lies in a structure's shadow.
const SHADOW_STRENGTH: f32 = 0.5;
/// Darkening of the ground right next to a structure (ambient occlusion).
const AMBIENT_OCCLUSION: f32 = 0.25;

/// Outside the combat area: blended toward this colour by this much, with hatching.
const OUTSIDE: [f32; 3] = rgb(0x13181e);
const OUTSIDE_DARKEN: f32 = 0.45;
/// Share of the colour's saturation kept outside.
const OUTSIDE_SATURATION: f32 = 0.7;
const HATCH: [f32; 3] = rgb(0x4a525c);
const HATCH_OPACITY: f32 = 0.45;
/// Distance between hatching lines and their width, meters.
const HATCH_SPACING: f32 = 6.0;
const HATCH_WIDTH: f32 = 1.2;
/// The combat area's border line and its width in meters.
const BORDER: [f32; 3] = rgb(0xd9e0e6);
const BORDER_WIDTH: f32 = 2.0;

// ---------------------------------------------------------------------------------------------
// Geometry.
// ---------------------------------------------------------------------------------------------

/// Meters per texel we aim for, and the texture sizes allowed.
const METERS_PER_TEXEL: f32 = 0.5;
const MIN_SIZE: u32 = 512;
const MAX_SIZE: u32 = 4096;
/// Tops this high above the terrain are structures (lower: kerbs, low walls, rubble).
const STRUCTURE_MIN_HEIGHT: f32 = 1.5;
/// Objects narrower than this (meters, both ways) are poles and posts, not structures.
const MIN_FOOTPRINT: f32 = 1.2;
/// The light comes from the north-west (top left of the map) at this elevation (degrees).
const LIGHT_ELEVATION: f32 = 45.0;
/// Shadows fade in over this height difference (m) and are blurred by this much (m).
const SHADOW_SOFTNESS: f32 = 0.6;
const SHADOW_BLUR: f32 = 1.0;

const fn rgb(hex: u32) -> [f32; 3] {
    [
        ((hex >> 16) & 0xff) as f32 / 255.0,
        ((hex >> 8) & 0xff) as f32 / 255.0,
        (hex & 0xff) as f32 / 255.0,
    ]
}

pub struct TacticalMapPlugin;

impl Plugin for TacticalMapPlugin {
    fn build(&self, app: &mut App) {
        app.init_resource::<TacticalMap>()
            .init_resource::<Generator>()
            .add_systems(
                Update,
                (start_generation.run_if(resource_exists_and_changed::<LoadedLevel>), poll_generation).chain(),
            );
    }
}

/// The generation in progress and its results.
#[derive(Resource, Default)]
struct Generator {
    base_task: Option<Task<Option<Arc<BaseMap>>>>,
    base: Option<Arc<BaseMap>>,
    compose_task: Option<Task<Composed>>,
    /// The team the last composite was started for.
    team: Option<u8>,
}

/// The map without the combat area.
struct BaseMap {
    level: String,
    /// Texels per side.
    size: u32,
    /// Row-major RGBA (sRGB), row 0 in the north.
    rgba: Vec<u8>,
}

struct Composed {
    image: Image,
    bounds: Option<Image>,
    level: String,
    team: u8,
    millis: f32,
}

/// What the base picture is made from, gathered on the main thread.
struct Inputs {
    level: String,
    heightmap: Arc<Heightmap>,
    water: Option<f32>,
    /// Collision shapes of the static objects (not vegetation) in world space.
    structures: Vec<(SharedShape, Affine3A)>,
    /// Vegetation meshes (`.glb`) of static objects and where they stand.
    trees: Vec<(String, Affine3A)>,
    roads: Vec<(String, Vec3)>,
    /// The level's vegetation file, relative to the imported root.
    vegetation: Option<String>,
    paths: GamePaths,
    cache: Option<Cache>,
}

fn team_number(team: Team) -> u8 {
    match team {
        Team::Two => 2,
        _ => 1,
    }
}

#[allow(clippy::type_complexity)]
fn start_generation(
    level: Res<LoadedLevel>,
    paths: Option<Res<GamePaths>>,
    colliders: Query<
        (&Collider, &Transform, &CollisionLayers, Option<&StaticMesh>),
        (With<LevelEntity>, Without<ColliderDisabled>, Without<Inactive>),
    >,
    visuals: Query<(&StaticMesh, &Transform), (With<LevelEntity>, Without<Collider>, Without<Inactive>)>,
    cache: Option<Res<Cache>>,
    mut generator: ResMut<Generator>,
    mut map: ResMut<TacticalMap>,
) {
    *generator = Generator::default();
    if map.image.is_some() || map.bounds.is_some() {
        map.image = None;
        map.bounds = None;
        map.version += 1;
    }
    let (Some(heightmap), Some(paths)) = (level.heightmap.clone(), paths) else {
        return;
    };
    let is_vegetation = |path: &str| path.contains("vegitation") || path.contains("vegetation");
    let mut structures = Vec::new();
    let mut trees = Vec::new();
    for (collider, transform, layers, mesh) in &colliders {
        // Only visible objects' world collision: no invisible walls, no vehicle-only copies.
        let Some(mesh) = mesh else { continue };
        if !layers.memberships.has_all(GameLayer::World) {
            continue;
        }
        if is_vegetation(&mesh.path) {
            trees.push((mesh.path.clone(), transform.compute_affine()));
        } else {
            structures.push((collider.shape().clone(), transform.compute_affine()));
        }
    }
    for (mesh, transform) in &visuals {
        if is_vegetation(&mesh.path) {
            trees.push((mesh.path.clone(), transform.compute_affine()));
        }
    }
    let name = level.desc.name.clone();
    // Imported levels only: the built-in test range is quick to draw.
    let cache = cache.filter(|_| level.dir.is_some()).map(|c| c.clone());
    let inputs = Inputs {
        level: name.clone(),
        heightmap,
        water: level.desc.water.as_ref().map(|w| w.height),
        structures,
        trees,
        roads: level.desc.roads.iter().map(|r| (r.mesh.clone(), Vec3::from_array(r.position))).collect(),
        vegetation: level.desc.vegetation.as_ref().map(|v| format!("levels/{name}/{v}")),
        paths: paths.clone(),
        cache,
    };
    generator.base_task = Some(AsyncComputeTaskPool::get().spawn(async move { load_or_generate(inputs).map(Arc::new) }));
}

/// Where to write finished maps for checking, if anywhere (see the module docs).
fn dump_dir(cli: Option<&crate::Cli>) -> Option<PathBuf> {
    if let Some(dir) = std::env::var_os("BF2_TACTICAL_MAP_DUMP") {
        return Some(dir.into());
    }
    let scenario = cli?.scenario.as_ref()?;
    let folder = scenario.parent()?.file_name()?;
    (folder == "tactical_map").then(|| PathBuf::from("target/scenarios/tactical_map"))
}

fn poll_generation(
    level: Option<Res<LoadedLevel>>,
    cli: Option<Res<crate::Cli>>,
    info: Query<&MatchInfo>,
    local: Query<&Team, With<LocalPlayer>>,
    mut generator: ResMut<Generator>,
    mut map: ResMut<TacticalMap>,
    mut images: ResMut<Assets<Image>>,
) {
    if let Some(task) = &mut generator.base_task
        && let Some(base) = check_ready(task)
    {
        generator.base_task = None;
        generator.base = base;
        generator.team = None;
    }
    let (Some(level), Some(base)) = (level, generator.base.clone()) else {
        return;
    };
    let team = local.iter().next().copied().map_or(1, team_number);
    let dump = dump_dir(cli.as_deref());
    if generator.compose_task.is_none() && generator.team != Some(team) {
        generator.team = Some(team);
        let area = info.iter().next().and_then(|info| {
            let layout = level.game_mode(&info.mode, info.size)?;
            let layout = if layout.combat_areas.is_empty() {
                level.base_layout(&info.mode, info.size)?
            } else {
                layout
            };
            Some(layout.ground_combat_area(team)?.points.clone())
        });
        let heightmap = level.heightmap.clone();
        generator.compose_task = heightmap.map(|heightmap| {
            AsyncComputeTaskPool::get()
                .spawn(async move { compose(&base, &heightmap, area.as_deref(), team, dump.as_deref()) })
        });
    }
    if let Some(task) = &mut generator.compose_task
        && let Some(composed) = check_ready(task)
    {
        generator.compose_task = None;
        if composed.level != level.desc.name {
            return;
        }
        info!(
            "tactical map ready: {} ({}x{}, team {}, combat area {}), composed in {:.0} ms",
            composed.level,
            composed.image.width(),
            composed.image.height(),
            composed.team,
            if composed.bounds.is_some() { "yes" } else { "none" },
            composed.millis
        );
        map.image = Some(images.add(composed.image));
        map.bounds = composed.bounds.map(|b| images.add(b));
        map.version += 1;
    }
}

// ---------------------------------------------------------------------------------------------
// The base picture.
// ---------------------------------------------------------------------------------------------

/// Texels per side for a map this many meters across.
fn resolution(meters: f32) -> u32 {
    ((meters / METERS_PER_TEXEL).ceil().max(1.0) as u32).next_power_of_two().clamp(MIN_SIZE, MAX_SIZE)
}

fn load_or_generate(inputs: Inputs) -> Option<BaseMap> {
    let started = Instant::now();
    let size = resolution(inputs.heightmap.world_size());
    let key = inputs_key(&inputs, size);
    if let Some(rgba) = inputs
        .cache
        .as_ref()
        .and_then(|cache| cache.load(CACHE_KIND, &inputs.level, CACHE_NAME, key))
        .filter(|rgba| rgba.len() == size as usize * size as usize * 4)
    {
        info!(
            "tactical map: {} loaded from cache ({key:016x}) in {:.0} ms",
            inputs.level,
            started.elapsed().as_secs_f32() * 1000.0
        );
        return Some(BaseMap {
            level: inputs.level,
            size,
            rgba,
        });
    }
    let rgba = generate(&inputs, size);
    info!(
        "tactical map: {} generated in {:.0} ms ({size}x{size}, {} structure meshes, {} trees from statics)",
        inputs.level,
        started.elapsed().as_secs_f32() * 1000.0,
        inputs.structures.len(),
        inputs.trees.len()
    );
    if let Some(cache) = &inputs.cache {
        let started = Instant::now();
        match cache.store(CACHE_KIND, &inputs.level, CACHE_NAME, key, &rgba) {
            Ok(_) => info!("tactical map: {} cached in {:.0} ms", inputs.level, started.elapsed().as_secs_f32() * 1000.0),
            Err(err) => warn!("tactical map: can't cache {} in {}: {err:#}", inputs.level, cache.root().display()),
        }
    }
    Some(BaseMap {
        level: inputs.level,
        size,
        rgba,
    })
}

/// FNV-1a, for the cache key.
struct Fnv(u64);

impl Fnv {
    fn new() -> Self {
        Self(0xcbf2_9ce4_8422_2325)
    }
    fn bytes(&mut self, bytes: &[u8]) {
        for &b in bytes {
            self.0 = (self.0 ^ b as u64).wrapping_mul(0x0100_0000_01b3);
        }
    }
    fn f32(&mut self, v: f32) {
        self.bytes(&v.to_bits().to_le_bytes());
    }
}

fn affine_hash(h: &mut Fnv, affine: &Affine3A) {
    for v in affine.to_cols_array() {
        h.f32(v);
    }
}

/// Identifies the picture the inputs make.
fn inputs_key(inputs: &Inputs, size: u32) -> u64 {
    let mut h = Fnv::new();
    h.bytes(&GENERATOR_VERSION.to_le_bytes());
    h.bytes(&size.to_le_bytes());
    let hm = &inputs.heightmap;
    h.bytes(&hm.resolution.to_le_bytes());
    h.f32(hm.spacing);
    hm.origin.to_array().into_iter().for_each(|v| h.f32(v));
    for v in &hm.heights {
        h.f32(*v);
    }
    h.f32(inputs.water.unwrap_or(f32::NAN));
    // Summed per object, so the order doesn't matter.
    let mut objects = 0u64;
    for (shape, affine) in &inputs.structures {
        let mut o = Fnv::new();
        let mut triangles = 0u32;
        for_each_triangle(shape, Affine3A::IDENTITY, &mut |_| triangles += 1);
        o.bytes(&triangles.to_le_bytes());
        affine_hash(&mut o, affine);
        objects = objects.wrapping_add(o.0);
    }
    for (path, affine) in &inputs.trees {
        let mut o = Fnv::new();
        o.bytes(path.as_bytes());
        affine_hash(&mut o, affine);
        objects = objects.wrapping_add(o.0);
    }
    h.bytes(&objects.to_le_bytes());
    for (path, position) in &inputs.roads {
        h.bytes(path.as_bytes());
        position.to_array().into_iter().for_each(|v| h.f32(v));
    }
    if let Some(vegetation) = &inputs.vegetation
        && let Ok(meta) = std::fs::metadata(inputs.paths.find(vegetation))
    {
        h.bytes(&meta.len().to_le_bytes());
        if let Ok(modified) = meta.modified()
            && let Ok(since) = modified.duration_since(std::time::UNIX_EPOCH)
        {
            h.bytes(&since.as_secs().to_le_bytes());
        }
    }
    h.0
}

/// The cache's folder and entry name for base pictures.
const CACHE_KIND: &str = "tactical";
const CACHE_NAME: &str = "base";

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

/// Runs `f(chunk index, chunk)` over `data` in chunks of `chunk` elements on a few threads.
fn par_chunks<T: Send>(data: &mut [T], chunk: usize, f: impl Fn(usize, &mut [T]) + Sync) {
    let threads = std::thread::available_parallelism().map_or(2, |n| n.get()).div_ceil(2).max(1);
    let work = Mutex::new(data.chunks_mut(chunk.max(1)).enumerate());
    std::thread::scope(|scope| {
        for _ in 0..threads {
            scope.spawn(|| {
                loop {
                    let next = work.lock().unwrap().next();
                    let Some((index, chunk)) = next else { break };
                    f(index, chunk);
                }
            });
        }
    });
}

/// Maps world XZ to texel coordinates (texel centres at whole numbers).
#[derive(Clone, Copy)]
struct Grid {
    size: usize,
    corner: Vec2,
    /// Meters per texel.
    texel: f32,
}

impl Grid {
    fn to_texel(self, x: f32, z: f32) -> Vec2 {
        (Vec2::new(x, z) - self.corner) / self.texel - 0.5
    }
    fn to_world(self, x: usize, y: usize) -> Vec2 {
        self.corner + (Vec2::new(x as f32, y as f32) + 0.5) * self.texel
    }
}

/// Rows of texels processed together when rasterizing.
const BAND: usize = 32;

/// A triangle in texel XY and world height.
#[derive(Clone, Copy)]
struct TexelTriangle {
    p: [Vec2; 3],
    h: [f32; 3],
    /// Nearly vertical (a wall): drawn as a line along its longest edge at its top height.
    wall: bool,
}

/// The highest structure surface over each texel (`-inf` where there is none).
fn rasterize_structures(inputs: &Inputs, grid: Grid) -> Vec<f32> {
    let n = grid.size;
    let mut triangles: Vec<TexelTriangle> = Vec::new();
    for (shape, affine) in &inputs.structures {
        let aabb = shape.compute_local_aabb();
        let (lo, hi) = (aabb.mins, aabb.maxs);
        let (min, max) = (0..8).fold((Vec3::MAX, Vec3::MIN), |(min, max), i| {
            let corner = Vec3::new(
                if i & 1 == 0 { lo.x } else { hi.x },
                if i & 2 == 0 { lo.y } else { hi.y },
                if i & 4 == 0 { lo.z } else { hi.z },
            );
            let p = affine.transform_point3(corner);
            (min.min(p), max.max(p))
        });
        if max.x - min.x < MIN_FOOTPRINT && max.z - min.z < MIN_FOOTPRINT {
            continue;
        }
        for_each_triangle(shape, *affine, &mut |[a, b, c]| {
            let normal = (b - a).cross(c - a);
            let length = normal.length();
            if length < 1e-6 {
                return;
            }
            triangles.push(TexelTriangle {
                p: [a, b, c].map(|v| grid.to_texel(v.x, v.z)),
                h: [a.y, b.y, c.y],
                wall: (normal.y / length).abs() < 0.2,
            });
        });
    }
    // Which triangles touch which band of rows.
    let bands = n.div_ceil(BAND);
    let mut binned: Vec<Vec<u32>> = vec![Vec::new(); bands];
    for (index, t) in triangles.iter().enumerate() {
        let min_y = t.p.iter().map(|p| p.y).fold(f32::MAX, f32::min).floor() - 1.0;
        let max_y = t.p.iter().map(|p| p.y).fold(f32::MIN, f32::max).ceil() + 1.0;
        if max_y < 0.0 || min_y >= n as f32 {
            continue;
        }
        let first = (min_y.max(0.0) as usize) / BAND;
        let last = ((max_y as usize).min(n - 1)) / BAND;
        for band in &mut binned[first..=last] {
            band.push(index as u32);
        }
    }
    let mut roof = vec![f32::NEG_INFINITY; n * n];
    par_chunks(&mut roof, BAND * n, |band, rows| {
        let y0 = band * BAND;
        let y1 = y0 + rows.len() / n;
        for &index in &binned[band] {
            let t = &triangles[index as usize];
            if t.wall {
                draw_wall(t, rows, n, y0, y1);
            } else {
                fill_triangle(t, rows, n, y0, y1);
            }
        }
    });
    roof
}

/// Fills a triangle into rows `y0..y1` (`rows` starts at row `y0`), keeping the highest.
fn fill_triangle(t: &TexelTriangle, rows: &mut [f32], n: usize, y0: usize, y1: usize) {
    let [a, b, c] = t.p;
    let area = (b - a).perp_dot(c - a);
    if area.abs() < 1e-9 {
        return;
    }
    let min = a.min(b).min(c);
    let max = a.max(b).max(c);
    let (x_start, x_end) = ((min.x.ceil().max(0.0)) as usize, (max.x.floor().min(n as f32 - 1.0)));
    if x_end < 0.0 {
        return;
    }
    let (row_start, row_end) = ((min.y.ceil().max(y0 as f32)) as usize, max.y.floor().min(y1 as f32 - 1.0));
    if row_end < row_start as f32 {
        return;
    }
    let inv = 1.0 / area;
    for y in row_start..=row_end as usize {
        let py = y as f32;
        for x in x_start..=x_end as usize {
            let p = Vec2::new(x as f32, py);
            let w0 = (c - b).perp_dot(p - b) * inv;
            let w1 = (a - c).perp_dot(p - c) * inv;
            let w2 = 1.0 - w0 - w1;
            if w0 < -1e-4 || w1 < -1e-4 || w2 < -1e-4 {
                continue;
            }
            let h = w0 * t.h[0] + w1 * t.h[1] + w2 * t.h[2];
            let texel = &mut rows[(y - y0) * n + x];
            if h > *texel {
                *texel = h;
            }
        }
    }
}

/// A wall's footprint: a one-texel line along the triangle's longest edge, at its top.
fn draw_wall(t: &TexelTriangle, rows: &mut [f32], n: usize, y0: usize, y1: usize) {
    let [a, b, c] = t.p;
    let (from, to) = [(a, b), (b, c), (c, a)]
        .into_iter()
        .max_by(|x, y| x.0.distance_squared(x.1).total_cmp(&y.0.distance_squared(y.1)))
        .unwrap();
    let top = t.h[0].max(t.h[1]).max(t.h[2]);
    let steps = from.distance(to).ceil().max(1.0) as usize;
    for i in 0..=steps {
        let p = from.lerp(to, i as f32 / steps as f32).round();
        if p.x < 0.0 || p.y < y0 as f32 || p.x >= n as f32 || p.y >= y1 as f32 {
            continue;
        }
        let texel = &mut rows[(p.y as usize - y0) * n + p.x as usize];
        if top > *texel {
            *texel = top;
        }
    }
}

/// Road coverage (0..1) over each texel.
fn rasterize_roads(inputs: &Inputs, grid: Grid) -> Vec<f32> {
    let n = grid.size;
    let mut coverage = vec![0.0f32; n * n];
    for (path, position) in &inputs.roads {
        let Ok(bytes) = std::fs::read(inputs.paths.find(path)) else {
            continue;
        };
        let Ok(gltf) = bevy::gltf::gltf::Gltf::from_slice(&bytes) else {
            continue;
        };
        let blob = gltf.blob.as_deref().unwrap_or_default();
        for mesh in gltf.meshes() {
            for primitive in mesh.primitives() {
                let name = primitive.material().name().unwrap_or_default().to_ascii_lowercase();
                let opacity = if name.contains("dirt") || name.contains("gravel") || name.contains("trail") {
                    DIRT_ROAD_OPACITY
                } else {
                    1.0
                };
                let reader = primitive.reader(|_| Some(blob));
                let Some(positions) = reader.read_positions() else { continue };
                let points: Vec<Vec2> = positions
                    .map(|p| grid.to_texel(p[0] + position.x, p[2] + position.z))
                    .collect();
                let alpha: Vec<f32> = match reader.read_colors(0) {
                    Some(colors) => colors.into_rgba_f32().map(|c| c[3]).collect(),
                    None => vec![1.0; points.len()],
                };
                let Some(indices) = reader.read_indices() else { continue };
                let indices: Vec<u32> = indices.into_u32().collect();
                for tri in indices.chunks_exact(3) {
                    let [i, j, k] = [tri[0], tri[1], tri[2]].map(|i| i as usize);
                    if i.max(j).max(k) >= points.len() {
                        continue;
                    }
                    let t = TexelTriangle {
                        p: [points[i], points[j], points[k]],
                        h: [alpha[i] * opacity, alpha[j] * opacity, alpha[k] * opacity],
                        wall: false,
                    };
                    fill_triangle(&t, &mut coverage, n, 0, n);
                }
            }
        }
    }
    coverage
}

/// Radius of a tree's crown (meters, before scaling) from its mesh's extent.
fn crown_radius(paths: &GamePaths, path: &str) -> Option<f32> {
    let bytes = std::fs::read(paths.find(path)).ok()?;
    let gltf = bevy::gltf::gltf::Gltf::from_slice(&bytes).ok()?;
    let (mut min, mut max) = (Vec3::MAX, Vec3::MIN);
    for mesh in gltf.meshes() {
        for primitive in mesh.primitives() {
            let bounds = primitive.bounding_box();
            min = min.min(Vec3::from_array(bounds.min));
            max = max.max(Vec3::from_array(bounds.max));
        }
    }
    (max.x > min.x).then(|| ((max.x - min.x) + (max.z - min.z)) * 0.25)
}

/// Tree crowns: coverage, and how lit each covered texel is (0 shadow side, 1 lit side).
struct Trees {
    coverage: Vec<f32>,
    light: Vec<f32>,
    shadow: Vec<f32>,
}

fn splat_trees(inputs: &Inputs, grid: Grid) -> Trees {
    let n = grid.size;
    let mut trees = Trees {
        coverage: vec![0.0; n * n],
        light: vec![0.0; n * n],
        shadow: vec![0.0; n * n],
    };
    let mut radii: HashMap<String, Option<f32>> = HashMap::default();
    let mut radius = |path: &str| *radii.entry(path.to_string()).or_insert_with(|| crown_radius(&inputs.paths, path));
    let mut instances: Vec<(Vec2, f32)> = Vec::new();
    for (path, affine) in &inputs.trees {
        if let Some(r) = radius(path) {
            let scale = affine.matrix3.x_axis.length();
            instances.push((affine.translation.xz(), r * scale));
        }
    }
    if let Some(vegetation) = &inputs.vegetation
        && let Ok(desc) = inputs.paths.read_ron::<VegetationDesc>(vegetation)
    {
        for overgrowth in &desc.overgrowth {
            let Some(r) = radius(&overgrowth.mesh) else { continue };
            for placement in &overgrowth.instances {
                instances.push((Vec2::new(placement.position[0], placement.position[2]), r * placement.scale[0].abs()));
            }
        }
    }
    for (center, r) in instances {
        let r = r.clamp(0.4, 14.0) / grid.texel;
        let c = grid.to_texel(center.x, center.y);
        // The shadow falls to the south-east.
        splat(&mut trees.shadow, n, c + Vec2::splat(r * 0.45), r * 1.05, |_| 1.0);
        splat_crown(&mut trees, n, c, r);
    }
    trees
}

/// Adds a soft disc (value `f(offset / radius)`, 1 in the middle) to `buffer`, keeping the
/// highest.
fn splat(buffer: &mut [f32], n: usize, center: Vec2, r: f32, f: impl Fn(Vec2) -> f32) {
    let (lo, hi) = ((center - r - 1.0).max(Vec2::ZERO), (center + r + 1.0).min(Vec2::splat(n as f32 - 1.0)));
    if hi.x < lo.x || hi.y < lo.y {
        return;
    }
    for y in lo.y.ceil() as usize..=hi.y as usize {
        for x in lo.x.ceil() as usize..=hi.x as usize {
            let d = (Vec2::new(x as f32, y as f32) - center) / r;
            let edge = (1.0 - d.length()) * r; // texels inside the rim
            let coverage = edge.clamp(0.0, 1.0) * f(d);
            let texel = &mut buffer[y * n + x];
            *texel = texel.max(coverage);
        }
    }
}

fn splat_crown(trees: &mut Trees, n: usize, center: Vec2, r: f32) {
    let (lo, hi) = ((center - r - 1.0).max(Vec2::ZERO), (center + r + 1.0).min(Vec2::splat(n as f32 - 1.0)));
    if hi.x < lo.x || hi.y < lo.y {
        return;
    }
    for y in lo.y.ceil() as usize..=hi.y as usize {
        for x in lo.x.ceil() as usize..=hi.x as usize {
            let d = (Vec2::new(x as f32, y as f32) - center) / r;
            let coverage = ((1.0 - d.length()) * r).clamp(0.0, 1.0);
            if coverage <= 0.0 {
                continue;
            }
            let i = y * n + x;
            // Lit toward the north-west, like a ball.
            let light = (0.55 - (d.x + d.y) * 0.4).clamp(0.0, 1.0);
            if coverage >= trees.coverage[i] {
                trees.light[i] = light;
            }
            trees.coverage[i] = trees.coverage[i].max(coverage);
        }
    }
}

/// Separable box blur with this radius (texels), in place.
fn box_blur(buffer: &mut [f32], n: usize, radius: usize) {
    if radius == 0 {
        return;
    }
    let window = (2 * radius + 1) as f32;
    let blur_line = |line: &mut [f32], scratch: &mut Vec<f32>| {
        scratch.clear();
        scratch.extend_from_slice(line);
        let len = line.len();
        let mut sum: f32 = (0..=radius).map(|i| scratch[i.min(len - 1)]).sum::<f32>() + scratch[0] * radius as f32;
        for i in 0..len {
            line[i] = sum / window;
            let add = scratch[(i + radius + 1).min(len - 1)];
            let remove = scratch[i.saturating_sub(radius)];
            sum += add - remove;
        }
    };
    par_chunks(buffer, n, |_, row| blur_line(row, &mut Vec::new()));
    // Columns: transpose, blur rows, transpose back.
    let mut transposed = vec![0.0; n * n];
    for y in 0..n {
        for x in 0..n {
            transposed[x * n + y] = buffer[y * n + x];
        }
    }
    par_chunks(&mut transposed, n, |_, row| blur_line(row, &mut Vec::new()));
    for y in 0..n {
        for x in 0..n {
            buffer[y * n + x] = transposed[x * n + y];
        }
    }
}

fn lerp3(a: [f32; 3], b: [f32; 3], t: f32) -> [f32; 3] {
    [a[0] + (b[0] - a[0]) * t, a[1] + (b[1] - a[1]) * t, a[2] + (b[2] - a[2]) * t]
}

fn scale3(a: [f32; 3], s: f32) -> [f32; 3] {
    [a[0] * s, a[1] * s, a[2] * s]
}

fn smoothstep(e0: f32, e1: f32, x: f32) -> f32 {
    let t = ((x - e0) / (e1 - e0)).clamp(0.0, 1.0);
    t * t * (3.0 - 2.0 * t)
}

fn generate(inputs: &Inputs, size: u32) -> Vec<u8> {
    let hm = &inputs.heightmap;
    let n = size as usize;
    let grid = Grid {
        size: n,
        corner: Vec2::new(hm.origin.x, hm.origin.z),
        texel: hm.world_size() / n as f32,
    };
    let timer = Instant::now();
    let lap = |what: &str, timer: &mut Instant| {
        debug!("tactical map: {what} {:.0} ms", timer.elapsed().as_secs_f32() * 1000.0);
        *timer = Instant::now();
    };
    let mut timer = timer;

    // Terrain height and its (smooth) gradient per texel.
    let r = hm.resolution as usize;
    let gradients: Vec<Vec2> = (0..r * r)
        .map(|i| {
            let (x, z) = ((i % r) as u32, (i / r) as u32);
            let dx = hm.sample(x + 1, z) - hm.sample(x.saturating_sub(1), z);
            let dz = hm.sample(x, z + 1) - hm.sample(x, z.saturating_sub(1));
            let span = |a: u32, b: u32| (b - a).max(1) as f32 * hm.spacing;
            Vec2::new(dx / span(x.saturating_sub(1), (x + 1).min(hm.resolution - 1)), dz / span(z.saturating_sub(1), (z + 1).min(hm.resolution - 1)))
        })
        .collect();
    let mut ground = vec![0.0f32; n * n];
    let mut slope = vec![Vec2::ZERO; n * n];
    par_chunks(&mut ground, n, |y, row| {
        for (x, h) in row.iter_mut().enumerate() {
            let w = grid.to_world(x, y);
            *h = hm.height_at(w.x, w.y);
        }
    });
    par_chunks(&mut slope, n, |y, row| {
        for (x, g) in row.iter_mut().enumerate() {
            let w = grid.to_world(x, y);
            let fx = ((w.x - hm.origin.x) / hm.spacing).clamp(0.0, (r - 1) as f32);
            let fz = ((w.y - hm.origin.z) / hm.spacing).clamp(0.0, (r - 1) as f32);
            let (x0, z0) = ((fx as usize).min(r - 2), (fz as usize).min(r - 2));
            let (u, v) = (fx - x0 as f32, fz - z0 as f32);
            let at = |x: usize, z: usize| gradients[z * r + x];
            *g = at(x0, z0).lerp(at(x0 + 1, z0), u).lerp(at(x0, z0 + 1).lerp(at(x0 + 1, z0 + 1), u), v);
        }
    });
    let (low, high) = hm.heights.iter().fold((f32::MAX, f32::MIN), |(lo, hi), &h| (lo.min(h), hi.max(h)));
    let (low, high) = (low + hm.origin.y, high + hm.origin.y);
    lap("terrain", &mut timer);

    let roof = rasterize_structures(inputs, grid);
    lap("structures", &mut timer);
    let roads = rasterize_roads(inputs, grid);
    lap("roads", &mut timer);
    let trees = splat_trees(inputs, grid);
    lap("trees", &mut timer);

    // Height of structures above the terrain (0 where there are none).
    let relative: Vec<f32> = roof
        .iter()
        .zip(&ground)
        .map(|(&top, &h)| if top - h > STRUCTURE_MIN_HEIGHT { top - h } else { 0.0 })
        .collect();

    // Shadows: sweep toward the south-east, carrying the shadow's top down by the light's
    // slope per diagonal step.
    let drop = grid.texel * std::f32::consts::SQRT_2 * LIGHT_ELEVATION.to_radians().tan();
    let mut shadow = vec![0.0f32; n * n];
    let mut carried = vec![f32::NEG_INFINITY; n];
    let mut next = vec![f32::NEG_INFINITY; n];
    for y in 0..n {
        for x in 0..n {
            let i = y * n + x;
            let surface = if relative[i] > 0.0 { roof[i] } else { ground[i] };
            let cast = if x > 0 { carried[x - 1] - drop } else { f32::NEG_INFINITY };
            shadow[i] = ((cast - surface) / SHADOW_SOFTNESS).clamp(0.0, 1.0);
            // Only structures cast shadows (the hillshade already darkens slopes).
            next[x] = cast.max(if relative[i] > 0.0 { surface } else { f32::NEG_INFINITY });
        }
        std::mem::swap(&mut carried, &mut next);
    }
    box_blur(&mut shadow, n, (SHADOW_BLUR / grid.texel).round() as usize);
    let mut occlusion: Vec<f32> = relative.iter().map(|&r| if r > 0.0 { 1.0 } else { 0.0 }).collect();
    box_blur(&mut occlusion, n, (1.5 / grid.texel).round().max(1.0) as usize);
    let mut tree_shadow = trees.shadow;
    box_blur(&mut tree_shadow, n, (0.8 / grid.texel).round() as usize);
    lap("shadows", &mut timer);

    let light = Vec3::new(-1.0, std::f32::consts::SQRT_2 * LIGHT_ELEVATION.to_radians().tan(), -1.0).normalize();
    let water = inputs.water;
    let mut rgba = vec![0u8; n * n * 4];
    par_chunks(&mut rgba, n * 4, |y, row| {
        for x in 0..n {
            let i = y * n + x;
            let h = ground[i];
            let g = slope[i];
            let t = ((h - low) / (high - low).max(1.0)).clamp(0.0, 1.0);
            let mut color = lerp3(GROUND_LOW, GROUND_HIGH, t);
            // Hillshade.
            let normal = Vec3::new(-g.x * HILLSHADE_EXAGGERATION, 1.0, -g.y * HILLSHADE_EXAGGERATION).normalize();
            let shade = normal.dot(light) - light.y;
            color = scale3(color, (1.0 + HILLSHADE_STRENGTH * shade).clamp(0.4, 1.6));
            let depth = water.map_or(f32::NEG_INFINITY, |w| w - h);
            if depth > 0.0 {
                let deep = lerp3(WATER_SHALLOW, WATER_DEEP, (depth / WATER_DEEP_DEPTH).clamp(0.0, 1.0));
                let rim = (g.length() * grid.texel * 1.5).max(0.05);
                color = lerp3(deep, WATER_EDGE, (1.0 - smoothstep(0.0, rim, depth)) * 0.8);
            } else {
                // Contour lines, about a texel wide whatever the slope.
                let per_texel = (g.length() * grid.texel).max(1e-4);
                let level = h / CONTOUR_INTERVAL;
                let distance = (level - level.round()).abs() * CONTOUR_INTERVAL / per_texel;
                let line = 1.0 - smoothstep(0.3, 1.0, distance);
                let major = (level.round() as i64).rem_euclid(5) == 0;
                let strength = if major { CONTOUR_MAJOR_STRENGTH } else { CONTOUR_STRENGTH };
                // Faded out on steep slopes, where they would crowd together.
                let crowd = 1.0 - smoothstep(1.0, 4.0, g.length() * CONTOUR_INTERVAL.recip() * 8.0);
                color = scale3(color, 1.0 - strength * line * crowd);
            }
            let above = roof[i] - h;
            let pavement = if (PAVEMENT_HEIGHTS[0]..PAVEMENT_HEIGHTS[1]).contains(&above) { PAVEMENT_OPACITY } else { 0.0 };
            color = lerp3(color, ROAD, (roads[i] * ROAD_OPACITY).max(pavement).clamp(0.0, 1.0));
            color = scale3(color, 1.0 - TREE_SHADOW * tree_shadow[i]);
            if trees.coverage[i] > 0.0 {
                let crown = lerp3(TREE, TREE_LIGHT, trees.light[i]);
                color = lerp3(color, crown, trees.coverage[i] * TREE_OPACITY);
            }
            let structure = relative[i] > 0.0;
            if structure {
                let t = ((relative[i] - STRUCTURE_MIN_HEIGHT) / BUILDING_TALL).clamp(0.0, 1.0).sqrt();
                let mut c = lerp3(BUILDING_LOW, BUILDING_HIGH, t);
                // Roof slopes, from neighbours that are structure too.
                let roof_at = |dx: isize, dy: isize| {
                    let (x, y) = (x as isize + dx, y as isize + dy);
                    if x < 0 || y < 0 || x >= n as isize || y >= n as isize {
                        return None;
                    }
                    let j = y as usize * n + x as usize;
                    (relative[j] > 0.0).then_some(roof[j])
                };
                let slope_x = match (roof_at(-1, 0), roof_at(1, 0)) {
                    (Some(a), Some(b)) => (b - a) / (2.0 * grid.texel),
                    _ => 0.0,
                };
                let slope_y = match (roof_at(0, -1), roof_at(0, 1)) {
                    (Some(a), Some(b)) => (b - a) / (2.0 * grid.texel),
                    _ => 0.0,
                };
                let (slope_x, slope_y) = (slope_x.clamp(-3.0, 3.0), slope_y.clamp(-3.0, 3.0));
                let roof_normal = Vec3::new(-slope_x, 1.0, -slope_y).normalize();
                c = scale3(c, 1.0 + ROOF_SHADE * (roof_normal.dot(light) - light.y));
                // Edges: lit toward the light, dark away from it.
                let lower = |dx: isize, dy: isize| roof_at(dx, dy).is_none_or(|r| roof[i] - r > 1.0);
                if lower(-1, 0) || lower(0, -1) {
                    c = lerp3(c, BUILDING_EDGE, 0.55);
                } else if lower(1, 0) || lower(0, 1) {
                    c = scale3(c, 0.82);
                }
                color = scale3(c, 1.0 - SHADOW_STRENGTH * 0.6 * shadow[i]);
            } else {
                color = scale3(color, 1.0 - SHADOW_STRENGTH * shadow[i]);
                color = scale3(color, 1.0 - AMBIENT_OCCLUSION * occlusion[i]);
            }
            let out = &mut row[x * 4..x * 4 + 4];
            for k in 0..3 {
                out[k] = (color[k].clamp(0.0, 1.0) * 255.0 + 0.5) as u8;
            }
            out[3] = 255;
        }
    });
    lap("colour", &mut timer);
    rgba
}

// ---------------------------------------------------------------------------------------------
// The combat area.
// ---------------------------------------------------------------------------------------------

/// The base with the outside of the combat area (`area`, world XZ) darkened and hatched, and
/// the area's mask; mipmapped.
fn compose(base: &BaseMap, heightmap: &Heightmap, area: Option<&[[f32; 2]]>, team: u8, dump_to: Option<&Path>) -> Composed {
    let started = Instant::now();
    let n = base.size as usize;
    let grid = Grid {
        size: n,
        corner: Vec2::new(heightmap.origin.x, heightmap.origin.z),
        texel: heightmap.world_size() / n as f32,
    };
    let mut rgba = base.rgba.clone();
    let mut mask_bytes = None;
    if let Some(area) = area.filter(|a| a.len() >= 3) {
        let polygon: Vec<Vec2> = area.iter().map(|p| grid.to_texel(p[0], p[1])).collect();
        let mask = fill_polygon(&polygon, n);
        let spacing = (HATCH_SPACING / grid.texel).max(3.0);
        let width = (HATCH_WIDTH / grid.texel).max(1.0);
        par_chunks(&mut rgba, n * 4, |y, row| {
            for x in 0..n {
                if mask[y * n + x] != 0 {
                    continue;
                }
                let texel = &mut row[x * 4..x * 4 + 3];
                let mut c = [texel[0], texel[1], texel[2]].map(|v| v as f32 / 255.0);
                let grey = c[0] * 0.3 + c[1] * 0.59 + c[2] * 0.11;
                c = lerp3([grey; 3], c, OUTSIDE_SATURATION);
                c = lerp3(c, OUTSIDE, OUTSIDE_DARKEN);
                // "/" lines: x + y constant, antialiased.
                let phase = ((x + y) as f32).rem_euclid(spacing);
                let distance = phase.min(spacing - phase);
                let line = (width * 0.5 + 0.5 - distance).clamp(0.0, 1.0);
                c = lerp3(c, HATCH, line * HATCH_OPACITY);
                for k in 0..3 {
                    texel[k] = (c[k].clamp(0.0, 1.0) * 255.0 + 0.5) as u8;
                }
            }
        });
        draw_polyline(&mut rgba, n, &polygon, BORDER_WIDTH / grid.texel, BORDER);
        mask_bytes = Some(mask);
    }
    let (data, levels) = mip_chain(rgba, n);
    let mut image = Image::new_uninit(
        Extent3d {
            width: base.size,
            height: base.size,
            depth_or_array_layers: 1,
        },
        TextureDimension::D2,
        TextureFormat::Rgba8UnormSrgb,
        RenderAssetUsages::RENDER_WORLD,
    );
    image.texture_descriptor.mip_level_count = levels;
    image.data = Some(data);
    image.sampler = ImageSampler::Descriptor(sampler(true));
    let bounds = mask_bytes.map(|mask| {
        let mut bounds = Image::new(
            Extent3d {
                width: base.size,
                height: base.size,
                depth_or_array_layers: 1,
            },
            TextureDimension::D2,
            mask,
            TextureFormat::R8Unorm,
            RenderAssetUsages::RENDER_WORLD,
        );
        bounds.sampler = ImageSampler::Descriptor(sampler(false));
        bounds
    });
    let millis = started.elapsed().as_secs_f32() * 1000.0;
    if let Some(dir) = dump_to {
        dump(dir, base, image.data.as_deref().unwrap_or_default(), bounds.as_ref());
    }
    Composed {
        image,
        bounds,
        level: base.level.clone(),
        team,
        millis,
    }
}

fn sampler(mipmaps: bool) -> ImageSamplerDescriptor {
    ImageSamplerDescriptor {
        address_mode_u: ImageAddressMode::ClampToEdge,
        address_mode_v: ImageAddressMode::ClampToEdge,
        address_mode_w: ImageAddressMode::ClampToEdge,
        mag_filter: ImageFilterMode::Linear,
        min_filter: ImageFilterMode::Linear,
        mipmap_filter: ImageFilterMode::Linear,
        // Anisotropic filtering needs every filter linear; the mask has no mipmaps anyway.
        anisotropy_clamp: if mipmaps { 16 } else { 1 },
        ..ImageSamplerDescriptor::linear()
    }
}

/// 255 inside the polygon (texel coordinates), 0 outside: scanline fill, even-odd.
fn fill_polygon(polygon: &[Vec2], n: usize) -> Vec<u8> {
    let mut mask = vec![0u8; n * n];
    par_chunks(&mut mask, n, |y, row| {
        let py = y as f32;
        let mut crossings: Vec<f32> = Vec::new();
        for i in 0..polygon.len() {
            let (a, b) = (polygon[i], polygon[(i + 1) % polygon.len()]);
            if (a.y > py) != (b.y > py) {
                crossings.push(a.x + (py - a.y) / (b.y - a.y) * (b.x - a.x));
            }
        }
        crossings.sort_by(f32::total_cmp);
        for span in crossings.chunks_exact(2) {
            let start = span[0].ceil().max(0.0) as usize;
            let end = span[1].floor().min(n as f32 - 1.0);
            if end < 0.0 {
                continue;
            }
            for texel in row.iter_mut().take(end as usize + 1).skip(start) {
                *texel = 255;
            }
        }
    });
    mask
}

/// Draws a closed polyline (texel coordinates) `width` texels wide, antialiased.
fn draw_polyline(rgba: &mut [u8], n: usize, polygon: &[Vec2], width: f32, color: [f32; 3]) {
    let half = (width * 0.5).max(0.75);
    for i in 0..polygon.len() {
        let (a, b) = (polygon[i], polygon[(i + 1) % polygon.len()]);
        let lo = (a.min(b) - half - 1.0).max(Vec2::ZERO);
        let hi = (a.max(b) + half + 1.0).min(Vec2::splat(n as f32 - 1.0));
        if hi.x < lo.x || hi.y < lo.y {
            continue;
        }
        let ab = b - a;
        let length2 = ab.length_squared().max(1e-6);
        for y in lo.y as usize..=hi.y as usize {
            for x in lo.x as usize..=hi.x as usize {
                let p = Vec2::new(x as f32, y as f32);
                let t = ((p - a).dot(ab) / length2).clamp(0.0, 1.0);
                let distance = p.distance(a + ab * t);
                let coverage = (half + 0.5 - distance).clamp(0.0, 1.0);
                if coverage <= 0.0 {
                    continue;
                }
                let texel = &mut rgba[(y * n + x) * 4..(y * n + x) * 4 + 3];
                let current = [texel[0], texel[1], texel[2]].map(|v| v as f32 / 255.0);
                // Overlapping segments at corners: don't blend twice past the line colour.
                let c = lerp3(current, color, coverage);
                for k in 0..3 {
                    let v = (c[k].clamp(0.0, 1.0) * 255.0 + 0.5) as u8;
                    texel[k] = if color[k] >= current[k] { texel[k].max(v) } else { texel[k].min(v) };
                }
            }
        }
    }
}

/// The image and its mipmaps (2x2 box filter), level after level, and the number of levels.
fn mip_chain(level0: Vec<u8>, n: usize) -> (Vec<u8>, u32) {
    let mut data = level0;
    let mut levels = 1;
    let (mut offset, mut size) = (0, n);
    while size > 1 {
        let half = size / 2;
        let mut next = vec![0u8; half * half * 4];
        {
            let src = &data[offset..offset + size * size * 4];
            for y in 0..half {
                for x in 0..half {
                    for k in 0..4 {
                        let at = |xx: usize, yy: usize| src[(yy * size + xx) * 4 + k] as u32;
                        let sum = at(2 * x, 2 * y) + at(2 * x + 1, 2 * y) + at(2 * x, 2 * y + 1) + at(2 * x + 1, 2 * y + 1);
                        next[(y * half + x) * 4 + k] = ((sum + 2) / 4) as u8;
                    }
                }
            }
        }
        offset += size * size * 4;
        data.extend_from_slice(&next);
        size = half;
        levels += 1;
    }
    (data, levels)
}

/// Writes the map (level 0) and its mask as PNGs, for checking.
fn dump(dir: &Path, base: &BaseMap, rgba: &[u8], bounds: Option<&Image>) {
    let size = Extent3d {
        width: base.size,
        height: base.size,
        depth_or_array_layers: 1,
    };
    let texels = base.size as usize * base.size as usize;
    let save = |data: Vec<u8>, format: TextureFormat, name: String| {
        let image = Image::new(size, TextureDimension::D2, data, format, RenderAssetUsages::all());
        let path = dir.join(name);
        let result = std::fs::create_dir_all(dir)
            .map_err(|e| e.to_string())
            .and_then(|_| image.try_into_dynamic().map_err(|e| e.to_string()))
            .and_then(|dynamic| dynamic.save(&path).map_err(|e| e.to_string()));
        match result {
            Ok(()) => info!("tactical map: wrote {}", path.display()),
            Err(err) => warn!("tactical map: writing {}: {err}", path.display()),
        }
    };
    save(rgba[..texels * 4].to_vec(), TextureFormat::Rgba8UnormSrgb, format!("{}.png", base.level));
    if let Some(bounds) = bounds.and_then(|b| b.data.clone()) {
        save(bounds, TextureFormat::R8Unorm, format!("{}_bounds.png", base.level));
    }
}
