//! Terrain: heightmap, color maps, water.

use std::path::Path;

use anyhow::{Context, Result};
use bf2_formats::{Vfs, con::World, con::parse_vec3};
use game_data::{TerrainDesc, WaterDesc};

use crate::dds;

/// Settings of one heightmap in the heightmap cluster.
#[derive(Default, Debug)]
struct HeightmapSettings {
    size: Option<u32>,
    scale: Option<[f32; 3]>,
    data: Option<String>,
}

/// Converts the primary heightmap and its color maps into `level_dir`.
///
/// BF2 heightmaps have row 0 at the south edge (min Z); ours has row 0 at the north edge
/// (-Z in engine space), so rows are flipped. Color maps are flipped the same way.
pub fn import(vfs: &Vfs, world: &World, level_name: &str, level_dir: &Path) -> Result<(TerrainDesc, Option<WaterDesc>)> {
    let mut primary = HeightmapSettings::default();
    let mut current_is_primary = false;
    let mut water_level = None;
    for command in &world.commands {
        let arg = |i: usize| command.args.get(i).map(String::as_str);
        match command.name.as_str() {
            "heightmapcluster.addheightmap" => {
                current_is_primary = arg(1) == Some("0") && arg(2) == Some("0");
            }
            "heightmap.setsize" if current_is_primary => {
                primary.size = arg(0).and_then(|s| s.parse().ok());
            }
            "heightmap.setscale" if current_is_primary => {
                primary.scale = arg(0).and_then(parse_vec3);
            }
            "heightmap.loadheightdata" if current_is_primary => {
                primary.data = arg(0).map(str::to_string);
            }
            "heightmapcluster.setseawaterlevel" => {
                water_level = arg(0).and_then(|s| s.parse::<f32>().ok());
            }
            _ => {}
        }
    }

    let size = primary.size.context("heightmap size not set")? as usize;
    let scale = primary.scale.context("heightmap scale not set")?;
    let data_path = primary.data.context("heightmap data not set")?;
    let raw = vfs.read(&data_path).with_context(|| format!("reading {data_path}"))?;
    anyhow::ensure!(
        raw.len() == size * size * 2,
        "{data_path}: {} bytes for a {size}x{size} 16-bit heightmap",
        raw.len()
    );

    let mut flipped = vec![0u8; raw.len()];
    let row_bytes = size * 2;
    for row in 0..size {
        let src = row * row_bytes;
        let dst = (size - 1 - row) * row_bytes;
        flipped[dst..dst + row_bytes].copy_from_slice(&raw[src..src + row_bytes]);
    }
    std::fs::create_dir_all(level_dir)?;
    std::fs::write(level_dir.join("heightmap.r16"), &flipped)?;

    // Color maps: one per 128-cell patch, `txXXxZZ.dds` with XX along +X and ZZ along
    // BF2 +Z (north). Our grid row 0 is north.
    let cells = size - 1;
    let patches = (cells / 128).max(1);
    let mut color_maps = Vec::with_capacity(patches * patches);
    let colormap_dir = level_dir.join("colormaps");
    for pz in 0..patches {
        for px in 0..patches {
            let bf2_row = patches - 1 - pz;
            let source = format!("levels/{level_name}/colormaps/tx{px:02}x{bf2_row:02}.dds");
            let Ok(data) = vfs.read(&source) else {
                // Patches entirely under water have no color map.
                color_maps.push(String::new());
                continue;
            };
            let name = format!("colormaps/tx{px:02}x{pz:02}.dds");
            match dds::flip_vertical(&data) {
                Ok(flipped) => {
                    std::fs::create_dir_all(&colormap_dir)?;
                    std::fs::write(level_dir.join(&name), flipped)?;
                    color_maps.push(name);
                }
                Err(err) => {
                    log::warn!("{source}: {err}");
                    color_maps.push(String::new());
                }
            }
        }
    }

    let half = cells as f32 * scale[0] * 0.5;
    let terrain = TerrainDesc {
        heightmap: "heightmap.r16".into(),
        resolution: size as u32,
        spacing: scale[0],
        height_scale: scale[1],
        origin: [-half, 0.0, -half],
        color_maps,
        color_map_tiles: patches as u32,
        detail_map: None,
    };
    let water = water_level.filter(|&h| h > -50.0).map(|height| WaterDesc {
        height,
        color: water_color(world),
    });
    Ok((terrain, water))
}

fn water_color(world: &World) -> [f32; 4] {
    let color = world
        .setting("renderer.watercolor")
        .and_then(parse_vec3)
        .map(normalize_color)
        .unwrap_or([0.1, 0.25, 0.3]);
    [color[0], color[1], color[2], 0.85]
}

/// Colors are 0..1, except a few written as 0..255.
pub fn normalize_color(c: [f32; 3]) -> [f32; 3] {
    if c.iter().any(|&v| v > 1.0) {
        c.map(|v| v / 255.0)
    } else {
        c
    }
}
