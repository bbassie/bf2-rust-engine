//! Checks against imported levels (ignored by default: they need `imported/`):
//!
//! `cargo test -p game_server --lib carriers_on_levels -- --ignored --nocapture`
//!
//! Builds the infantry grid of the carrier maps with and without detail patches, like the
//! server does at level load, and prints build times, memory, the spawn check, whether every
//! spawn on a carrier reaches the carrier's vehicles and ladders, and path query times. Set
//! `NAV_LEVELS=dalian_plant:64,...` to pick levels and `NAV_IMAGES=dir` for top-down pictures
//! of the patches (`.ppm`: green reachable from the first carrier spawn, red not).

use std::{collections::VecDeque, io::Write, sync::Arc, time::Instant};

use avian3d::prelude::*;
use bevy::prelude::*;
use game_shared::{
    config::GamePaths,
    ladder::LadderPart,
    level::{LevelEntity, load_level},
    soldier::SoldierTuning,
    statics::spawn_statics,
};

use super::{CellRef, NavGrid, NavParams, SpawnCheck, build, collect_geometry, detail_patches, patch};

/// Cells reachable from `start` (walking, dropping, ladders, portals).
fn reachable(grid: &NavGrid, start: CellRef) -> Vec<bool> {
    let mut seen = vec![false; grid.cell_count()];
    let mut queue = VecDeque::from([start]);
    seen[start.index as usize] = true;
    while let Some(c) = queue.pop_front() {
        let walks = (0..4).filter_map(|d| grid.neighbour(c, d));
        let ladders = grid.ladders_at(c.index).filter_map(|l| {
            if l.bottom.index == c.index {
                Some(l.top)
            } else {
                l.down.then_some(l.bottom)
            }
        });
        let next: Vec<CellRef> = walks.chain(ladders).chain(grid.portals_at(c.index).iter().copied()).collect();
        for n in next {
            if !seen[n.index as usize] {
                seen[n.index as usize] = true;
                queue.push_back(n);
            }
        }
    }
    seen
}

/// Top-down picture of a patch: per column its highest cell within `band` of heights, green
/// if reachable, red if not, brighter higher up; `marks` (world) in white.
fn picture(grid: &NavGrid, patch: usize, seen: &[bool], band: (f32, f32), marks: &[Vec3], path: &std::path::Path) {
    let p = grid.patches()[patch];
    let space = NavGrid::patch_space(&p);
    let (w, d) = (p.width as usize, p.depth as usize);
    let mut pixels = vec![[0u8; 3]; w * d];
    for z in 0..p.depth {
        for x in 0..p.width {
            let top = grid
                .space_column(&space, x, z)
                .filter(|&i| (band.0..=band.1).contains(&grid.cells[i as usize].y))
                .next_back();
            if let Some(i) = top {
                let y = grid.cells[i as usize].y;
                let shade = (80.0 + 175.0 * ((y - band.0) / (band.1 - band.0).max(1.0))).clamp(0.0, 255.0) as u8;
                pixels[z as usize * w + x as usize] = if seen[i as usize] { [0, shade, 0] } else { [shade, 0, 0] };
            }
        }
    }
    for m in marks {
        let l = p.frame.to_local(m.xz());
        let (x, z) = (((l.x - p.origin.x) / p.cell) as i64, ((l.y - p.origin.y) / p.cell) as i64);
        for (dx, dz) in [(0, 0), (1, 0), (-1, 0), (0, 1), (0, -1)] {
            let (x, z) = (x + dx, z + dz);
            if (0..w as i64).contains(&x) && (0..d as i64).contains(&z) {
                pixels[z as usize * w + x as usize] = [255, 255, 255];
            }
        }
    }
    let mut f = std::io::BufWriter::new(std::fs::File::create(path).unwrap());
    write!(f, "P6\n{w} {d}\n255\n").unwrap();
    for px in pixels {
        f.write_all(&px).unwrap();
    }
}

#[test]
#[ignore]
fn carriers_on_levels() {
    let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../imported");
    let paths = GamePaths::resolve(Some(root));
    let levels = std::env::var("NAV_LEVELS")
        .unwrap_or_else(|_| "dalian_plant:32,dalian_plant:64,wake_island_2007:64,gulf_of_oman:32,gulf_of_oman:64".into());
    let images = std::env::var("NAV_IMAGES").ok().map(std::path::PathBuf::from);
    let params = NavParams::from_tuning(&SoldierTuning::default());
    for spec in levels.split(',') {
        let (name, size) = spec.split_once(':').unwrap_or((spec, "64"));
        let size: u32 = size.parse().unwrap();
        let Ok(level) = load_level(&paths, name) else {
            println!("{name}: not imported");
            continue;
        };
        let Some(layout) = level.base_layout("gpm_cq", size) else {
            println!("{name} {size}: no such layout");
            continue;
        };
        let mut world = World::new();
        {
            let mut commands = world.commands();
            spawn_statics(&mut commands, &level.desc.statics, &paths);
        }
        world.flush();
        let mut colliders =
            world.query_filtered::<(&Collider, &Transform, &CollisionLayers), (With<LevelEntity>, Without<ColliderDisabled>)>();
        let mut ladders = world.query_filtered::<(&Collider, &Transform), (With<LadderPart>, Without<ColliderDisabled>)>();
        let collected = collect_geometry(level.heightmap.clone(), colliders.iter(&world), ladders.iter(&world), Some(layout), &[]);
        let geometry = collected.geometry;
        let patches = detail_patches(&level, &geometry, Some(&paths));

        let started = Instant::now();
        let plain = Arc::new(build::build(&geometry, params));
        let plain_s = started.elapsed().as_secs_f32();
        let started = Instant::now();
        let grid = Arc::new(patch::build_level(&geometry, &patches, params));
        let grid_s = started.elapsed().as_secs_f32();
        println!(
            "\n== {name} {size}: level grid {:.2} m cells\n  without patches: {} cells, {:.1} MB, built in {plain_s:.2} s\n  with {} patches: {} cells ({} in patches, {} portals), {:.1} MB, built in {grid_s:.2} s",
            grid.params.cell,
            plain.cell_count(),
            plain.memory_bytes() as f32 / 1e6,
            grid.patches().len(),
            grid.cell_count(),
            grid.patch_cell_count(),
            grid.portals.values().map(Vec::len).sum::<usize>(),
            grid.memory_bytes() as f32 / 1e6,
        );
        for (i, (p, r)) in grid.patches().iter().zip(&patches).enumerate() {
            println!(
                "  patch {i}: {}x{} columns of {} m, {:.0}x{:.0} m around {:.0}",
                p.width,
                p.depth,
                p.cell,
                r.max.x - r.min.x,
                r.max.y - r.min.y,
                p.frame.center
            );
        }

        // (vehicles) Ladders with their foot in the water near the patches (out of a well deck,
        // down a hull's side), and how deep the water is over their bottom cell.
        if let Some(water) = level.desc.water.as_ref().map(|w| w.height) {
            for l in grid.ladders() {
                let bottom = grid.cell(l.bottom);
                let near = grid.patches().iter().any(|p| p.frame.center.distance(l.foot.xz()) < 160.0);
                if near && water - l.foot.y > -1.5 {
                    println!(
                        "  ladder foot {:.1} top {:.1}: {:.1} m of water over its bottom cell, {:.1} m from a wall",
                        l.foot,
                        grid.position(l.top),
                        water - bottom.y,
                        bottom.dist as f32 * grid.cell_size(l.bottom) * 0.5
                    );
                }
            }
        }

        let check = SpawnCheck {
            spawns: layout
                .spawn_points
                .iter()
                .map(|sp| (sp.control_point.clone(), Vec3::from_array(sp.placement.position)))
                .collect(),
            control_points: layout.control_points.iter().map(|cp| (cp.id.clone(), Vec3::from_array(cp.position))).collect(),
        };
        for (label, g) in [("without", &plain), ("with", &grid)] {
            let cut = check.cut_off(g);
            println!("  spawn check {label} patches: {} cut off: {}", cut.len(), cut.join(", "));
        }

        // Every spawn on a carrier: to its vehicles and ladders.
        for (i, rect) in patches.iter().enumerate() {
            let spawns: Vec<Vec3> = check.spawns.iter().map(|(_, at)| *at).filter(|at| rect.contains(at.xz())).collect();
            let vehicles: Vec<(String, Vec3)> = layout
                .vehicle_spawners
                .iter()
                .filter(|v| rect.contains(Vec2::new(v.placement.position[0], v.placement.position[2])))
                .map(|v| {
                    let name = v.templates.iter().flatten().next().cloned().unwrap_or_default();
                    (name, Vec3::from_array(v.placement.position))
                })
                .collect();
            let first = grid.patches()[i].first_cell;
            let ladder_ends: Vec<(String, Vec3)> = grid
                .ladders()
                .iter()
                .filter(|l| l.bottom.index >= first)
                .flat_map(|l| [("ladder foot".to_string(), l.foot), ("ladder top".to_string(), l.head)])
                .collect();
            println!(
                "  carrier {i}: {} spawns, {} vehicle spawners, {} ladders",
                spawns.len(),
                vehicles.len(),
                ladder_ends.len() / 2
            );
            for (label, g) in [("without", &plain), ("with", &grid)] {
                let (mut ok, mut total, mut ms, mut max_ms) = (0, 0, 0.0f32, 0.0f32);
                let mut misses = Vec::new();
                for from in &spawns {
                    for (what, to) in vehicles.iter().chain(&ladder_ends) {
                        let started = Instant::now();
                        let path = g.find_path(*from, *to);
                        let took = started.elapsed().as_secs_f32() * 1000.0;
                        ms += took;
                        max_ms = max_ms.max(took);
                        total += 1;
                        let end = path.as_ref().and_then(|p| p.waypoints.last()).map(|w| w.position);
                        let reached = path.as_ref().is_some_and(|p| p.complete)
                            && end.is_some_and(|e| e.xz().distance(to.xz()) < 6.0 && (e.y - to.y).abs() < 4.0);
                        if reached {
                            ok += 1;
                        } else {
                            misses.push(format!(
                                "{from:.0} -> {what} {to:.0} (ends at {})",
                                end.map_or("-".into(), |e| format!("{e:.0}"))
                            ));
                        }
                    }
                }
                println!(
                    "    {label} patches: {ok} of {total} reached, {:.2} ms avg, {max_ms:.1} ms max",
                    ms / total.max(1) as f32
                );
                for m in misses.iter().take(8) {
                    println!("      {m}");
                }
            }
            // Cut off from the land by walking (bots take boats and aircraft off it)?
            if let Some(from) = spawns.first() {
                let region = |g: &NavGrid, at: Vec3| g.locate(at, 3.0, None).map(|c| g.cell(c).region);
                let land: Vec<&String> = check
                    .control_points
                    .iter()
                    .filter(|(_, at)| !rect.contains(at.xz()) && region(&grid, *at).is_some() && region(&grid, *at) == region(&grid, *from))
                    .map(|(id, _)| id)
                    .collect();
                println!("    flags off the carrier in the carrier's region: {land:?}");
            }
            if let (Some(dir), Some(from)) = (&images, spawns.first()) {
                let start = grid.locate(*from, 2.5, None).unwrap();
                let seen = reachable(&grid, start);
                std::fs::create_dir_all(dir).unwrap();
                let marks: Vec<Vec3> = spawns.iter().copied().chain(vehicles.iter().map(|v| v.1)).collect();
                let y = from.y;
                picture(&grid, i, &seen, (y - 30.0, y + 40.0), &marks, &dir.join(format!("{name}_{size}_{i}_top.ppm")));
                picture(&grid, i, &seen, (y - 1.5, y + 1.5), &marks, &dir.join(format!("{name}_{size}_{i}_spawn.ppm")));
                picture(&grid, i, &seen, (y + 5.0, y + 20.0), &marks, &dir.join(format!("{name}_{size}_{i}_deck.ppm")));
            }
        }

        // Path query times over the whole map: every spawn to every flag.
        for (label, g) in [("without", &plain), ("with", &grid)] {
            let (mut n, mut ms, mut max_ms, mut complete) = (0, 0.0f32, 0.0f32, 0);
            for (_, from) in check.spawns.iter().step_by(3) {
                for (_, to) in &check.control_points {
                    let started = Instant::now();
                    let path = g.find_path(*from, *to);
                    let took = started.elapsed().as_secs_f32() * 1000.0;
                    n += 1;
                    ms += took;
                    max_ms = max_ms.max(took);
                    complete += usize::from(path.is_some_and(|p| p.complete));
                }
            }
            println!(
                "  paths spawns -> flags {label} patches: {n} paths ({complete} complete), {:.2} ms avg, {max_ms:.1} ms max",
                ms / n.max(1) as f32
            );
        }
    }
}

/// Prints the infantry grid around points, to see why bots get stuck there:
///
/// `NAV_AROUND="gulf_of_oman:64:-685,535@21;wake_island_2007:64:35,-215" cargo test -p game_server --lib nav_around -- --ignored --nocapture`
///
/// Per point (optionally `@height`: the floor of interest, else every floor in the column
/// there): a 0.5 m map, 24 m across, of the cells within a meter of that height (`.` level,
/// `+`/`-` up to a meter higher or lower, `,` next to an edge, `#` none; letters for other
/// regions, `L` ladder ends), and the statics nearby.
#[test]
#[ignore]
fn nav_around() {
    let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../imported");
    let paths = GamePaths::resolve(Some(root));
    let specs = std::env::var("NAV_AROUND").unwrap_or_else(|_| "gulf_of_oman:64:-685,535".into());
    let params = NavParams::from_tuning(&SoldierTuning::default());
    let mut loaded: Option<(String, u32, Arc<NavGrid>, game_shared::level::LoadedLevel)> = None;
    for spec in specs.split(';').filter(|s| !s.trim().is_empty()) {
        let mut parts = spec.trim().splitn(3, ':');
        let name = parts.next().unwrap().to_string();
        let size: u32 = parts.next().unwrap_or("64").parse().unwrap();
        let point = parts.next().unwrap_or("0,0");
        let (xz, height) = match point.split_once('@') {
            Some((xz, h)) => (xz, Some(h.parse::<f32>().unwrap())),
            None => (point, None),
        };
        let (x, z) = xz.split_once(',').unwrap();
        let (x, z): (f32, f32) = (x.parse().unwrap(), z.parse().unwrap());
        if loaded.as_ref().is_none_or(|(n, s, ..)| *n != name || *s != size) {
            let Ok(level) = load_level(&paths, &name) else {
                println!("{name}: not imported");
                continue;
            };
            let Some(layout) = level.base_layout("gpm_cq", size).cloned() else {
                println!("{name} {size}: no such layout");
                continue;
            };
            let mut world = World::new();
            {
                let mut commands = world.commands();
                spawn_statics(&mut commands, &level.desc.statics, &paths);
            }
            world.flush();
            let mut colliders = world
                .query_filtered::<(&Collider, &Transform, &CollisionLayers), (With<LevelEntity>, Without<ColliderDisabled>)>();
            let mut ladders = world.query_filtered::<(&Collider, &Transform), (With<LadderPart>, Without<ColliderDisabled>)>();
            let collected = collect_geometry(level.heightmap.clone(), colliders.iter(&world), ladders.iter(&world), Some(&layout), &[]);
            let patches = detail_patches(&level, &collected.geometry, Some(&paths));
            let grid = Arc::new(patch::build_level(&collected.geometry, &patches, params));
            loaded = Some((name.clone(), size, grid, level));
        }
        let (_, _, grid, level) = loaded.as_ref().unwrap();
        let heights: Vec<f32> = match height {
            Some(h) => vec![h],
            None => {
                let mut hs: Vec<f32> = grid
                    .cells_near(Vec2::new(x, z), 1.0)
                    .map(|c| grid.cell(c).y)
                    .collect();
                hs.sort_by(f32::total_cmp);
                hs.dedup_by(|a, b| (*a - *b).abs() < 1.5);
                hs
            }
        };
        println!("\n== {name} {size} around ({x}, {z}): floors {heights:?}");
        for h in heights {
            let center = grid.locate(Vec3::new(x, h + 0.5, z), 3.0, None);
            let home = center.map(|c| grid.cell(c).region);
            println!("  floor {h:.1}: nearest cell {:?} region {home:?}", center.map(|c| grid.position(c)));
            let mut regions: Vec<u16> = Vec::new();
            for row in 0..48 {
                let wz = z - 12.0 + row as f32 * 0.5;
                let mut line = String::new();
                for col in 0..48 {
                    let wx = x - 12.0 + col as f32 * 0.5;
                    let here = row == 24 && col == 24;
                    let probe = Vec3::new(wx, h + 1.0, wz);
                    let cell = grid
                        .locate(probe, 0.3, None)
                        .filter(|c| grid.position(*c).xz().distance(probe.xz()) < 0.45 && (grid.cell(*c).y - h).abs() < 1.0);
                    let ch = match cell {
                        _ if here => '@',
                        None => '#',
                        Some(c) => {
                            let cell = grid.cell(c);
                            if grid.ladders_at(c.index).next().is_some() {
                                'L'
                            } else if Some(cell.region) != home {
                                let i = regions.iter().position(|r| *r == cell.region).unwrap_or_else(|| {
                                    regions.push(cell.region);
                                    regions.len() - 1
                                });
                                (b'a' + (i % 26) as u8) as char
                            } else if cell.y > h + 0.3 {
                                '+'
                            } else if cell.y < h - 0.3 {
                                '-'
                            } else if cell.dist == 0 {
                                ','
                            } else {
                                '.'
                            }
                        }
                    };
                    line.push(ch);
                }
                println!("  {wz:7.1} {line}");
            }
            println!("  (x from {:.1} to {:.1}; other regions: {regions:?})", x - 12.0, x + 11.5);
        }
        let mut near: Vec<(f32, String)> = level
            .desc
            .statics
            .iter()
            .map(|s| {
                let p = Vec3::from_array(s.placement.position);
                (p.xz().distance(Vec2::new(x, z)), format!("{} at {p:.1}", s.template))
            })
            .filter(|(d, _)| *d < 25.0)
            .collect();
        near.sort_by(|a, b| a.0.total_cmp(&b.0));
        for (d, s) in near.iter().take(10) {
            println!("  static {d:.0} m: {s}");
        }
    }
}

/// Builds the grids of levels with and without their combat areas (see [`super::area`]),
/// like the server does at level load, and prints build times, cells, memory and cache file
/// sizes, the spawn check and path query times of both:
///
/// `NAV_LEVELS=strike_at_karkand:64,gulf_of_oman:64 cargo test -p game_server --lib combat_areas_on_levels -- --ignored --nocapture`
#[test]
#[ignore]
fn combat_areas_on_levels() {
    let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../..");
    let paths = GamePaths::resolve_with_mods(Some(root.join("imported")), Some(root.join("mods")));
    let levels = std::env::var("NAV_LEVELS")
        .unwrap_or_else(|_| "strike_at_karkand:64,gulf_of_oman:64,dalian_plant:64,aix2_aix_archipelago:64".into());
    let params = NavParams::from_tuning(&SoldierTuning::default());
    let file_size = |grid: &NavGrid| game_shared::cache::encode(1, &super::cache::encode(grid)).len() as f32 / 1e6;
    // The fastest of two builds.
    fn timed<T>(mut f: impl FnMut() -> T) -> (T, f32) {
        let started = Instant::now();
        let _ = f();
        let first = started.elapsed().as_secs_f32();
        let started = Instant::now();
        let out = f();
        (out, first.min(started.elapsed().as_secs_f32()))
    }
    for spec in levels.split(',') {
        let (name, size) = spec.split_once(':').unwrap_or((spec, "64"));
        let size: u32 = size.parse().unwrap();
        let Ok(level) = load_level(&paths, name) else {
            println!("{name}: not imported");
            continue;
        };
        let Some(layout) = level.base_layout("gpm_cq", size).cloned() else {
            println!("{name} {size}: no such layout");
            continue;
        };
        if layout.combat_areas.is_empty() {
            println!("{name} {size}: no combat areas (imported before they were)");
            continue;
        }
        let plain_layout = game_data::GameModeDesc {
            combat_areas: Vec::new(),
            ..layout.clone()
        };
        let mut world = World::new();
        {
            let mut commands = world.commands();
            spawn_statics(&mut commands, &level.desc.statics, &paths);
        }
        world.flush();
        let mut colliders =
            world.query_filtered::<(&Collider, &Transform, &CollisionLayers), (With<LevelEntity>, Without<ColliderDisabled>)>();
        let mut ladders = world.query_filtered::<(&Collider, &Transform), (With<LadderPart>, Without<ColliderDisabled>)>();
        let strategic = super::strategic_points(&level, Some(&layout));
        println!("\n== {name} {size}");
        let mut grids = Vec::new();
        for (label, l) in [("before", &plain_layout), ("after", &layout)] {
            let collected =
                collect_geometry(level.heightmap.clone(), colliders.iter(&world), ladders.iter(&world), Some(l), &strategic);
            let geometry = collected.geometry;
            let patches = detail_patches(&level, &geometry, Some(&paths));
            let (grid, build_s) = timed(|| patch::build_level(&geometry, &patches, params));
            let (lo, hi) = geometry.bounds.unwrap_or_default();
            println!(
                "  infantry {label}: {:.0}x{:.0} m, {}x{} columns of {} m, {} cells ({} in {} patches), {:.1} MB, file {:.1} MB, built in {build_s:.2} s",
                hi.x - lo.x,
                hi.y - lo.y,
                grid.width,
                grid.depth,
                grid.params.cell,
                grid.cell_count(),
                grid.patch_cell_count(),
                grid.patches().len(),
                grid.memory_bytes() as f32 / 1e6,
                file_size(&grid),
            );
            let (vehicle_geometry, areas) = super::vehicle_geometry(&geometry, collected.vehicle_meshes, Some(l));
            let input = super::vehicle::VehicleGeometry {
                geometry: vehicle_geometry,
                roads: Vec::new(),
                water: level.desc.water.as_ref().map(|w| w.height),
                areas,
            };
            let (vehicles, vehicle_s) = timed(|| super::vehicle::build_all(&input, None));
            println!(
                "  vehicles {label}: land {}x{} ({} cells, {:.1} MB, file {:.1} MB), water {}, built in {vehicle_s:.2} s",
                vehicles.land.width,
                vehicles.land.depth,
                vehicles.land.cell_count(),
                vehicles.land.memory_bytes() as f32 / 1e6,
                file_size(&vehicles.land),
                vehicles.water.as_ref().map_or("none".into(), |w| format!("{}x{}", w.width, w.depth)),
            );
            grids.push(grid);
        }
        let check = SpawnCheck {
            spawns: layout
                .spawn_points
                .iter()
                .map(|sp| (sp.control_point.clone(), Vec3::from_array(sp.placement.position)))
                .collect(),
            control_points: layout.control_points.iter().map(|cp| (cp.id.clone(), Vec3::from_array(cp.position))).collect(),
        };
        for (label, g) in ["before", "after"].iter().zip(&grids) {
            let cut = check.cut_off(g);
            let (mut n, mut ms, mut max_ms, mut complete) = (0, 0.0f32, 0.0f32, 0);
            for (_, from) in check.spawns.iter().step_by(2) {
                for (_, to) in &check.control_points {
                    let started = Instant::now();
                    let path = g.find_path(*from, *to);
                    let took = started.elapsed().as_secs_f32() * 1000.0;
                    n += 1;
                    ms += took;
                    max_ms = max_ms.max(took);
                    complete += usize::from(path.is_some_and(|p| p.complete));
                }
            }
            println!(
                "  {label}: {} spawns cut off from their flag; spawns -> flags: {n} paths ({complete} complete), {:.2} ms avg, {max_ms:.1} ms max",
                cut.len(),
                ms / n.max(1) as f32
            );
        }
    }
}

/// Places the charges of every Rush layout of every level (imported and in the mods) the way
/// the server does and lists each charge whose spot from the layout needed correcting (under
/// water, indoors, on a roof, next to the other charge, ...) or that found no walkable ground:
///
/// `cargo test -p game_server --lib charges_on_levels -- --ignored --nocapture`
///
/// `CHARGE_LEVELS=leviathan,strike_at_karkand` picks levels. Takes the cached grids (see
/// [`game_shared::cache`]) where there are, else builds and caches them (`CHARGE_BUILD=0`:
/// skip those layouts instead).
#[test]
#[ignore]
fn charges_on_levels() {
    use crate::modes::rush::{CHARGES_APART, ChargeToPlace, flag_ground, place_all};
    let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../..");
    let paths = GamePaths::resolve_with_mods(Some(root.join("imported")), Some(root.join("mods")));
    let wanted = std::env::var("CHARGE_LEVELS").ok();
    let build_missing = std::env::var("CHARGE_BUILD").map_or(true, |v| v != "0");
    // `CHARGE_VERBOSE=1`: every charge's spot.
    let verbose = std::env::var("CHARGE_VERBOSE").is_ok_and(|v| v == "1");
    let tuning = NavParams::from_tuning(&SoldierTuning::default());
    let cache = game_shared::cache::Cache::resolve(None, None);
    let (mut layouts, mut total, mut corrected, mut stuck, mut close) = (0, 0, 0, 0, 0);
    let mut report = Vec::new();
    for name in paths.level_names() {
        if wanted.as_ref().is_some_and(|w| !w.split(',').any(|x| x == name)) {
            continue;
        }
        let Ok(level) = load_level(&paths, &name) else {
            println!("{name}: doesn't load");
            continue;
        };
        let params = NavParams {
            water_height: level.desc.water.as_ref().map(|w| w.height),
            ..tuning
        };
        let rush: Vec<game_data::GameModeDesc> =
            level.desc.game_modes.iter().filter(|l| l.mode == game_data::modes::RUSH).cloned().collect();
        for layout in rush {
            let Some(staged) = &layout.staged else { continue };
            let Some(base) = level.base_layout(&layout.mode, layout.size) else { continue };
            let mut world = World::new();
            {
                let mut commands = world.commands();
                spawn_statics(&mut commands, &level.desc.statics, &paths);
            }
            world.flush();
            let mut colliders = world
                .query_filtered::<(&Collider, &Transform, &CollisionLayers), (With<LevelEntity>, Without<ColliderDisabled>)>();
            let mut ladders = world.query_filtered::<(&Collider, &Transform), (With<LadderPart>, Without<ColliderDisabled>)>();
            let strategic = super::strategic_points(&level, Some(base));
            let collected =
                collect_geometry(level.heightmap.clone(), colliders.iter(&world), ladders.iter(&world), Some(base), &strategic);
            let patches = detail_patches(&level, &collected.geometry, Some(&paths));
            let started = Instant::now();
            let key = super::build::geometry_key(&collected.geometry, &params)
                ^ super::build::words_key(&super::patch::hash_rects(&patches));
            let grid = match cache.as_ref().and_then(|c| super::cache::load(c, &level.desc.name, "infantry", key, params)) {
                Some(grid) => grid,
                None if build_missing => {
                    let grid = super::load_or_build(&collected.geometry, &patches, params, cache.as_ref(), &level.desc.name);
                    println!("{name} {}: grid built in {:.1} s", layout.size, started.elapsed().as_secs_f32());
                    grid
                }
                None => {
                    println!("{name} {}: no cached grid, skipped", layout.size);
                    continue;
                }
            };
            layouts += 1;
            let charges: Vec<(String, ChargeToPlace)> = staged
                .stages
                .iter()
                .enumerate()
                .flat_map(|(s, stage)| {
                    let layout = &layout;
                    stage.charges.iter().map(move |c| {
                        let near = c.control_point.as_deref().and_then(|id| flag_ground(layout, id));
                        let label = format!(
                            "stage {} charge {} at {}",
                            s + 1,
                            c.name,
                            c.control_point
                                .as_ref()
                                .and_then(|id| layout.control_points.iter().find(|cp| &cp.id == id))
                                .map_or("-", |cp| cp.name.as_str())
                        );
                        (
                            label,
                            ChargeToPlace {
                                stage: s as u8,
                                wanted: Vec3::from_array(c.position),
                                near,
                                approximate: c.approximate,
                            },
                        )
                    })
                })
                .collect();
            let list: Vec<ChargeToPlace> = charges.iter().map(|(_, c)| c.clone()).collect();
            let spawns: Vec<Vec3> = layout.spawn_points.iter().map(|sp| Vec3::from_array(sp.placement.position)).collect();
            let placed = place_all(&grid, &list, &spawns);
            let at: Vec<Vec3> = list.iter().zip(&placed).map(|(c, p)| p.spot.unwrap_or(c.wanted)).collect();
            for (i, ((label, charge), placed)) in charges.iter().zip(&placed).enumerate() {
                if !charge.approximate {
                    continue;
                }
                total += 1;
                let apart = list
                    .iter()
                    .zip(&at)
                    .enumerate()
                    .filter(|(j, (other, _))| *j != i && other.stage == charge.stage)
                    .map(|(_, (_, p))| p.xz().distance(at[i].xz()))
                    .fold(f32::INFINITY, f32::min);
                if verbose {
                    let before = previous_spot(&grid, level.heightmap.as_deref(), charge.wanted, charge.near);
                    println!(
                        "  {name} {} {label}: layout {:.1}, before {}, now {}{}",
                        layout.size,
                        charge.wanted,
                        before.map_or("-".to_string(), |b| format!(
                            "{b:.1}{}",
                            params.water_height.filter(|w| b.y < *w).map_or(String::new(), |w| format!(" ({:.1} m under water)", w - b.y))
                        )),
                        placed.spot.map_or("-".to_string(), |s| format!("{s:.1}")),
                        if placed.remaining.is_empty() { String::new() } else { format!(" ({})", placed.remaining.join(", ")) }
                    );
                }
                match placed.spot {
                    None => {
                        stuck += 1;
                        report.push(format!("{name} {} {label}: NOT PLACED ({})", layout.size, placed.problems.join(", ")));
                    }
                    Some(spot) if !placed.problems.is_empty() => {
                        corrected += 1;
                        report.push(format!(
                            "{name} {} {label}: moved {:.1} m to {spot:.0} (was {}){}{}",
                            layout.size,
                            spot.distance(charge.wanted),
                            placed.problems.join(", "),
                            if placed.remaining.is_empty() {
                                String::new()
                            } else {
                                format!(", still {}", placed.remaining.join(", "))
                            },
                            if apart < CHARGES_APART { format!(", {apart:.0} m from the other") } else { String::new() }
                        ));
                    }
                    // Nothing better near (a flag indoors keeps its charges indoors: not listed).
                    Some(spot) if placed.remaining.iter().any(|p| *p != "indoors") => {
                        report.push(format!(
                            "{name} {} {label}: at {spot:.0}, {} (nothing better near)",
                            layout.size,
                            placed.remaining.join(", ")
                        ));
                    }
                    Some(_) => {}
                }
                if apart < CHARGES_APART {
                    close += 1;
                }
            }
        }
    }
    println!("\n{} charges needing a correction or not placed:", report.len());
    for line in &report {
        println!("  {line}");
    }
    println!(
        "\n{layouts} Rush layouts, {total} generated charges: {corrected} corrected, {stuck} not placed, {close} less than {CHARGES_APART} m from their partner"
    );
}

/// Where the server put a generated charge before its placement checked for water, indoors
/// and the other charge: the walkable cell in its flag's region within 16 m scoring best on
/// distance, room and height above the terrain.
fn previous_spot(grid: &NavGrid, heightmap: Option<&game_shared::level::Heightmap>, wanted: Vec3, near: Option<Vec3>) -> Option<Vec3> {
    let region = crate::ai::strategy::walk_region(grid, near.unwrap_or(wanted));
    let mut best: Option<(f32, Vec3)> = None;
    for cell in grid.cells_near(wanted.xz(), 16.0) {
        let c = grid.cell(cell);
        if region.is_some_and(|r| r != c.region) {
            continue;
        }
        let spot = grid.position(cell);
        let mut score = spot.xz().distance(wanted.xz());
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
