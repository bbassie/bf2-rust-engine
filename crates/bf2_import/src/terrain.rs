//! Terrain: heightmap, color maps, water.

use std::path::Path;

use anyhow::{Context, Result};
use bf2_formats::{
    Vfs,
    con::{World, parse_vec3},
    terrain::TerrainDataHeader,
};
use game_data::{TerrainDesc, TerrainDetailDesc, WaterDesc};

use crate::{dds, meshes::MeshConverter};

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
pub fn import(
    vfs: &Vfs,
    world: &World,
    converter: &MeshConverter,
    level_name: &str,
    level_dir: &Path,
) -> Result<(TerrainDesc, Option<WaterDesc>)> {
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

    // Per-patch images: `txXXxZZ*.dds` with XX along +X and ZZ along BF2 +Z (north).
    // Our grid row 0 is north, and images are flipped to be north-up.
    let header = vfs
        .read(&format!("levels/{level_name}/terraindata.raw"))
        .ok()
        .and_then(|data| TerrainDataHeader::parse(&data).map_err(|e| log::warn!("terraindata.raw: {e}")).ok());
    let base = |name: Option<&String>, fallback: &str| {
        name.filter(|n| !n.is_empty())
            .cloned()
            .unwrap_or_else(|| format!("levels/{level_name}/{fallback}/tx"))
    };
    let colormap_base = base(header.as_ref().map(|h| &h.colormap_base), "colormaps");
    let detailmap_base = base(header.as_ref().map(|h| &h.detailmap_base), "detailmaps");
    let lightmap_base = base(header.as_ref().map(|h| &h.lightmap_base), "lightmaps");

    let cells = size - 1;
    let patches = (cells / 128).max(1);
    let mut color_maps = Vec::with_capacity(patches * patches);
    let mut detail_weights = Vec::with_capacity(patches * patches);
    let mut lightmaps = Vec::with_capacity(patches * patches);
    for pz in 0..patches {
        for px in 0..patches {
            let bf2 = format!("{px:02}x{:02}", patches - 1 - pz);
            let ours = format!("{px:02}x{pz:02}");
            color_maps.push(patch_image(vfs, &format!("{colormap_base}{bf2}.dds"), level_dir, &format!("colormaps/tx{ours}.dds"))?);
            detail_weights.push([
                patch_image(vfs, &format!("{detailmap_base}{bf2}_1.dds"), level_dir, &format!("detailmaps/tx{ours}_1.dds"))?,
                patch_image(vfs, &format!("{detailmap_base}{bf2}_2.dds"), level_dir, &format!("detailmaps/tx{ours}_2.dds"))?,
            ]);
            lightmaps.push(patch_image(vfs, &format!("{lightmap_base}{bf2}.dds"), level_dir, &format!("lightmaps/tx{ours}.dds"))?);
        }
    }

    // Tiling is given in repeats per 256 m patch.
    let patch_world = 128.0 * scale[0];
    let detail_textures = header
        .iter()
        .flat_map(|h| &h.detail_textures)
        .map(|d| {
            // Keep empty slots: a texture's index selects its weight channel.
            let texture = converter.texture(&d.texture).unwrap_or_default();
            let per = |repeats: f32| if repeats > 0.0 { patch_world / repeats } else { 8.0 };
            TerrainDetailDesc {
                texture,
                top_tile_size: per(d.top_tiling),
                side_tile_size: [per(d.side_tiling[0]), per(d.side_tiling[1])],
                tri_planar: d.tri_planar,
            }
        })
        .collect();

    let half = cells as f32 * scale[0] * 0.5;
    let terrain = TerrainDesc {
        heightmap: "heightmap.r16".into(),
        resolution: size as u32,
        spacing: scale[0],
        height_scale: scale[1],
        origin: [-half, 0.0, -half],
        color_maps,
        color_map_tiles: patches as u32,
        detail_textures,
        detail_weights,
        lightmaps,
    };
    let water = water_level.filter(|&h| h > -50.0).map(|height| WaterDesc {
        height,
        color: water_color(world),
    });
    Ok((terrain, water))
}

/// Copies one per-patch image into the level folder, flipped north-up and in a format the GPU
/// can sample. Returns its path relative to the level folder, or an empty string if the
/// level has no such image (patches fully under water have none).
fn patch_image(vfs: &Vfs, source: &str, level_dir: &Path, name: &str) -> Result<String> {
    let Ok(data) = vfs.read(source) else {
        return Ok(String::new());
    };
    let data = dds::rgb565_to_bgra8(&data).unwrap_or(data);
    match dds::flip_vertical(&data) {
        Ok(flipped) => {
            let target = level_dir.join(name);
            std::fs::create_dir_all(target.parent().expect("has a parent"))?;
            std::fs::write(target, flipped)?;
            Ok(name.to_string())
        }
        Err(err) => {
            log::warn!("{source}: {err}");
            Ok(String::new())
        }
    }
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
