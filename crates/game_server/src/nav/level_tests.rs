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
        let collected = collect_geometry(level.heightmap.clone(), colliders.iter(&world), ladders.iter(&world), Some(layout));
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
