//! Terrain: heightmap, color maps, the surrounding low-detail terrain, water.

use std::{collections::HashMap, path::Path};

use anyhow::{Context, Result, bail, ensure};
use bf2_formats::{
    Vfs,
    con::{World, parse_vec, parse_vec3},
    terrain::TerrainDataHeader,
};
use game_data::{SurroundingTerrainDesc, TerrainDesc, TerrainDetailDesc, WaterDesc};

use crate::{dds, meshes::MeshConverter};

/// Settings of one heightmap in the heightmap cluster.
#[derive(Default, Debug, Clone)]
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
    // The 3x3 heightmap cluster: cell (0, 0) is the playable terrain.
    let mut cells: HashMap<(i32, i32), HeightmapSettings> = HashMap::new();
    let mut current = None;
    let mut cell_size = None;
    let mut water_level = None;
    for command in &world.commands {
        let arg = |i: usize| command.args.get(i).map(String::as_str);
        let cell = current.and_then(|c| cells.get_mut(&c));
        match command.name.as_str() {
            "heightmapcluster.setheightmapsize" => {
                cell_size = arg(0).and_then(|s| s.parse::<f32>().ok());
            }
            "heightmapcluster.addheightmap" => {
                current = arg(1).zip(arg(2)).and_then(|(x, y)| Some((x.parse().ok()?, y.parse().ok()?)));
                if let Some(c) = current {
                    cells.insert(c, HeightmapSettings::default());
                }
            }
            "heightmap.setsize" => {
                if let Some(cell) = cell {
                    cell.size = arg(0).and_then(|s| s.parse().ok());
                }
            }
            "heightmap.setscale" => {
                if let Some(cell) = cell {
                    cell.scale = arg(0).and_then(parse_vec3);
                }
            }
            "heightmap.loadheightdata" => {
                if let Some(cell) = cell {
                    cell.data = arg(0).map(str::to_string);
                }
            }
            "heightmapcluster.setseawaterlevel" => {
                water_level = arg(0).and_then(|s| s.parse::<f32>().ok());
            }
            _ => {}
        }
    }

    let primary = cells.get(&(0, 0)).cloned().unwrap_or_default();
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

    let grid = size - 1;
    let patches = (grid / 128).max(1);
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

    let world_size = grid as f32 * scale[0];
    let surrounding = cell_size.and_then(|s| {
        surrounding(vfs, &cells, &raw, size, scale, s, &colormap_base, (&color_maps, patches), level_dir)
            .map_err(|e| log::warn!("{level_name}: surrounding terrain: {e:#}"))
            .ok()
            .flatten()
    });

    let half = world_size * 0.5;
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
        surrounding,
    };
    let water = water_level
        .filter(|&h| h > -50.0)
        .map(|height| water(vfs, world, converter, level_name, height));
    Ok((terrain, water))
}

/// Merges the 8 secondary heightmaps around the playable terrain (and a downsampled copy of
/// the primary in the middle) into one grid, and copies the colour maps of the outer cells.
///
/// Secondaries are `u8` with the same height range as the primary (`scale.y` 256 times
/// larger), 257 samples per `cell_size` meters. Cell `(cx, cy)` has `cy` growing north.
#[allow(clippy::too_many_arguments)]
fn surrounding(
    vfs: &Vfs,
    cells: &HashMap<(i32, i32), HeightmapSettings>,
    primary: &[u8],
    primary_size: usize,
    primary_scale: [f32; 3],
    cell_size: f32,
    colormap_base: &str,
    (primary_maps, patches): (&[String], usize),
    level_dir: &Path,
) -> Result<Option<SurroundingTerrainDesc>> {
    let mut secondaries = HashMap::new();
    for (&cell, settings) in cells.iter().filter(|(c, _)| **c != (0, 0)) {
        let (Some(n), Some(scale), Some(path)) = (settings.size, settings.scale, settings.data.as_ref()) else {
            continue;
        };
        let Ok(data) = vfs.read(path) else { continue };
        // Devils_Perch's R1 has 130 stray bytes at the end.
        if data.len() < (n * n) as usize {
            log::debug!("{path}: {} bytes, expected {n}x{n} 8-bit", data.len());
            continue;
        }
        secondaries.insert(cell, (n as usize, scale, data));
    }
    if secondaries.len() < 8 {
        return Ok(None);
    }
    let n = secondaries.values().next().map(|s| s.0).unwrap_or(257);
    ensure!(secondaries.values().all(|s| s.0 == n) && n > 1, "secondary heightmaps differ in size");
    let per_cell = n - 1;
    let resolution = 3 * per_cell + 1;
    let spacing = cell_size / per_cell as f32;

    // Engine grid: row 0 at -Z (north), col 0 at -X; heights in primary height units.
    let to_units = |meters: f32| (meters / primary_scale[1]).round().clamp(0.0, 65535.0) as u16;
    let step = (primary_size - 1) as f32 / per_cell as f32;
    let mut heights = vec![0u16; resolution * resolution];
    for row in 0..resolution {
        for col in 0..resolution {
            let (cell_col, cell_row) = ((col / per_cell).min(2), (row / per_cell).min(2));
            let (local_col, local_row) = (col - cell_col * per_cell, row - cell_row * per_cell);
            let (cx, cy) = (cell_col as i32 - 1, 1 - cell_row as i32);
            // BF2 rows start at the south edge.
            let south_row = per_cell - local_row;
            heights[row * resolution + col] = if (cx, cy) == (0, 0) {
                let (pr, pc) = ((south_row as f32 * step) as usize, (local_col as f32 * step) as usize);
                let i = (pr.min(primary_size - 1) * primary_size + pc.min(primary_size - 1)) * 2;
                u16::from_le_bytes([primary[i], primary[i + 1]])
            } else {
                let (_, scale, data) = &secondaries[&(cx, cy)];
                to_units(data[south_row * n + local_col] as f32 * scale[1])
            };
        }
    }
    let bytes: Vec<u8> = heights.iter().flat_map(|h| h.to_le_bytes()).collect();
    std::fs::write(level_dir.join("surrounding.r16"), bytes)?;

    // `tx_s0..7` go clockwise from the north-west, each turned a quarter further (verified by
    // correlating each image with the slopes of the secondary heightmaps).
    const RING: [((i32, i32), Turn); 8] = [
        ((-1, 1), Turn::Ccw270),
        ((0, 1), Turn::None),
        ((1, 1), Turn::None),
        ((1, 0), Turn::Ccw90),
        ((1, -1), Turn::Ccw90),
        ((0, -1), Turn::Ccw180),
        ((-1, -1), Turn::Ccw180),
        ((-1, 0), Turn::Ccw270),
    ];
    let mut color_maps = vec![String::new(); 9];
    for (i, ((cx, cy), turn)) in RING.into_iter().enumerate() {
        let source = format!("{colormap_base}_s{i}.dds");
        let Ok(data) = vfs.read(&source) else { continue };
        let name = format!("colormaps/surrounding_{i}.dds");
        match orient_north_up(&data, turn) {
            Ok(image) => {
                let target = level_dir.join(&name);
                std::fs::create_dir_all(target.parent().expect("has a parent"))?;
                std::fs::write(target, image)?;
                color_maps[((1 - cy) * 3 + cx + 1) as usize] = name;
            }
            Err(e) => log::warn!("{source}: {e:#}"),
        }
    }

    let tint = seam_tint(level_dir, &color_maps, primary_maps, patches);
    let half = 1.5 * cell_size;
    Ok(Some(SurroundingTerrainDesc {
        heightmap: "surrounding.r16".into(),
        resolution: resolution as u32,
        spacing,
        height_scale: primary_scale[1],
        origin: [-half, 0.0, -half],
        color_maps,
        tint,
    }))
}

/// BF2's surrounding colour maps are often more saturated than the terrain's (it hid them
/// in fog). Compares both along the four seams (32 m strips, sRGB) and returns the linear
/// multiplier that makes the surrounding match.
fn seam_tint(level_dir: &Path, ring: &[String], primary: &[String], patches: usize) -> [f32; 3] {
    let image = |name: &str| -> Option<Vec<u8>> { (!name.is_empty()).then(|| std::fs::read(level_dir.join(name)).ok())? };
    let (mut ring_sum, mut primary_sum) = ([0.0f64; 3], [0.0f64; 3]);
    // Side cells (index in the 3x3 grid) and which strip of each image touches the seam.
    enum Side { North, South, West, East }
    for (cell, side) in [(1, Side::North), (7, Side::South), (3, Side::West), (5, Side::East)] {
        let Some(ring_image) = ring.get(cell).and_then(|n| image(n)) else { continue };
        // The ring cell's strip facing the terrain, and the terrain patches' strips facing it.
        let (ring_strip, patch_indices, patch_strip): (Strip, Vec<usize>, Strip) = match side {
            Side::North => (Strip::Bottom, (0..patches).collect(), Strip::Top),
            Side::South => (Strip::Top, (0..patches).map(|x| (patches - 1) * patches + x).collect(), Strip::Bottom),
            Side::West => (Strip::Right, (0..patches).map(|z| z * patches).collect(), Strip::Left),
            Side::East => (Strip::Left, (0..patches).map(|z| z * patches + patches - 1).collect(), Strip::Right),
        };
        let Some(ring_mean) = dds_mean(&ring_image, ring_strip, 1.0 / 32.0) else { continue };
        let means: Vec<[f64; 3]> = patch_indices
            .iter()
            .filter_map(|&i| image(primary.get(i)?)) 
            .filter_map(|data| dds_mean(&data, patch_strip, 1.0 / 8.0))
            .collect();
        if means.is_empty() {
            continue;
        }
        for c in 0..3 {
            ring_sum[c] += ring_mean[c];
            primary_sum[c] += means.iter().map(|m| m[c]).sum::<f64>() / means.len() as f64;
        }
    }
    if ring_sum.iter().any(|&v| v <= 0.0) {
        return [1.0; 3];
    }
    std::array::from_fn(|c| ((primary_sum[c] / ring_sum[c]) as f32).powf(2.2).clamp(0.3, 3.0))
}

#[derive(Clone, Copy)]
enum Strip {
    Top,
    Bottom,
    Left,
    Right,
}

/// Mean sRGB colour (0..1) of a strip along one edge of a DXT1 or 32-bit DDS image,
/// `fraction` of its size wide (top mip only).
fn dds_mean(data: &[u8], strip: Strip, fraction: f32) -> Option<[f64; 3]> {
    if data.len() < 128 || &data[..4] != b"DDS " {
        return None;
    }
    let u32_at = |o: usize| u32::from_le_bytes([data[o], data[o + 1], data[o + 2], data[o + 3]]) as usize;
    let (height, width) = (u32_at(12), u32_at(16));
    let dxt1 = u32_at(80) & 4 != 0 && &data[84..88] == b"DXT1";
    let bgra = u32_at(80) & 4 == 0 && u32_at(88) == 32;
    let (w, h) = (((width as f32 * fraction) as usize).max(1), ((height as f32 * fraction) as usize).max(1));
    let (x0, y0, x1, y1) = match strip {
        Strip::Top => (0, 0, width, h),
        Strip::Bottom => (0, height - h, width, height),
        Strip::Left => (0, 0, w, height),
        Strip::Right => (width - w, 0, width, height),
    };
    let mut sum = [0.0f64; 3];
    let mut count = 0.0;
    if dxt1 {
        let blocks_x = width.div_ceil(4);
        for by in y0 / 4..y1.div_ceil(4) {
            for bx in x0 / 4..x1.div_ceil(4) {
                let o = 128 + (by * blocks_x + bx) * 8;
                let block = data.get(o..o + 8)?;
                let rgb = |c: u16| [((c >> 11) & 31) as f64 / 31.0, ((c >> 5) & 63) as f64 / 63.0, (c & 31) as f64 / 31.0];
                let (c0, c1) = (u16::from_le_bytes([block[0], block[1]]), u16::from_le_bytes([block[2], block[3]]));
                let (p0, p1) = (rgb(c0), rgb(c1));
                let palette: [[f64; 3]; 4] = if c0 > c1 {
                    [p0, p1, std::array::from_fn(|i| (2.0 * p0[i] + p1[i]) / 3.0), std::array::from_fn(|i| (p0[i] + 2.0 * p1[i]) / 3.0)]
                } else {
                    [p0, p1, std::array::from_fn(|i| (p0[i] + p1[i]) / 2.0), [0.0; 3]]
                };
                for texel in 0..16 {
                    let index = (block[4 + texel / 4] >> (2 * (texel % 4))) & 3;
                    for c in 0..3 {
                        sum[c] += palette[index as usize][c];
                    }
                    count += 1.0;
                }
            }
        }
    } else if bgra {
        for y in y0..y1 {
            for x in x0..x1 {
                let o = 128 + (y * width + x) * 4;
                let px = data.get(o..o + 4)?;
                for (c, &v) in [px[2], px[1], px[0]].iter().enumerate() {
                    sum[c] += v as f64 / 255.0;
                }
                count += 1.0;
            }
        }
    } else {
        return None;
    }
    (count > 0.0).then(|| sum.map(|s| s / count))
}

/// How a surrounding colour map is turned relative to the primary colour maps.
#[derive(Clone, Copy)]
enum Turn {
    None,
    Ccw90,
    Ccw180,
    Ccw270,
}

/// Rewrites a DXT1 DDS so its row 0 is north and column 0 west, from BF2's surrounding
/// colour map layout (`numpy.rot90(image, turn)` has row 0 south). Blocks and their texels
/// are permuted, nothing is re-encoded.
fn orient_north_up(data: &[u8], turn: Turn) -> Result<Vec<u8>> {
    ensure!(data.len() >= 128 && &data[..4] == b"DDS ", "not a DDS file");
    let u32_at = |o: usize| u32::from_le_bytes([data[o], data[o + 1], data[o + 2], data[o + 3]]);
    let (height, width, mips) = (u32_at(12) as usize, u32_at(16) as usize, u32_at(28).max(1) as usize);
    if u32_at(80) & 4 == 0 || &data[84..88] != b"DXT1" {
        bail!("surrounding colour maps are expected to be DXT1");
    }
    ensure!(width == height, "not square");
    // target[r][c] = source[r'][c'] with (r', c') from (transpose, flip rows, flip columns).
    let (transpose, flip_rows, flip_cols) = match turn {
        Turn::None => (false, true, false),
        Turn::Ccw90 => (true, false, false),
        Turn::Ccw180 => (false, false, true),
        Turn::Ccw270 => (true, true, true),
    };
    let map = |r: usize, c: usize, n: usize| {
        let (r, c) = if transpose { (c, r) } else { (r, c) };
        (if flip_rows { n - 1 - r } else { r }, if flip_cols { n - 1 - c } else { c })
    };
    let mut out = data.to_vec();
    let mut offset = 128;
    let mut size = width;
    for _ in 0..mips {
        let blocks = size.div_ceil(4);
        let level = blocks * blocks * 8;
        if offset + level > data.len() {
            break;
        }
        for r in 0..blocks {
            for c in 0..blocks {
                let (sr, sc) = map(r, c, blocks);
                let src = &data[offset + (sr * blocks + sc) * 8..][..8];
                let dst = &mut out[offset + (r * blocks + c) * 8..][..8];
                dst[..4].copy_from_slice(&src[..4]);
                for y in 0..4 {
                    let mut row = 0u8;
                    for x in 0..4 {
                        let (ty, tx) = map(y, x, 4);
                        row |= ((src[4 + ty] >> (2 * tx)) & 3) << (2 * x);
                    }
                    dst[4 + y] = row;
                }
            }
        }
        offset += level;
        size = (size / 2).max(1);
    }
    Ok(out)
}

/// Water plane at `height` with the level's `Water.con` look: colour, sun glint, the wave
/// animation, BF2's animated normal map volume and the level's reflection cube map.
fn water(vfs: &Vfs, world: &World, converter: &MeshConverter, level_name: &str, height: f32) -> WaterDesc {
    let vec = |name: &str| world.setting(name).and_then(parse_vec);
    let number = |name: &str| world.setting(name).and_then(|s| s.parse::<f32>().ok());
    let specular = vec("renderer.waterspecularcolor").unwrap_or_default();
    let specular = match specular.as_slice() {
        [r, g, b, a, ..] => {
            let [r, g, b] = normalize_color([*r, *g, *b]);
            [r, g, b, *a]
        }
        [r, g, b] => {
            let [r, g, b] = normalize_color([*r, *g, *b]);
            [r, g, b, 1.0]
        }
        _ => [1.0, 0.95, 0.85, 1.0],
    };
    // BF2 advances its water clock by `waterAnimSpeed / 1000` per second [I]; the normal map
    // repeats every ~30 m and scrolls by `waterScroll` repeats per clock unit. BF2's shader
    // loops the volume's 8 slices every 0.1 units, but the slices are unrelated wave patterns
    // (neighbouring slices correlate at ~0), so blending through them at that rate (6-12
    // slices a second) replaces the whole pattern several times a second and flickers; one
    // loop per clock unit reads as waves that change shape [I].
    let clock = number("renderer.wateranimspeed").unwrap_or(50.0) / 1000.0;
    let scroll = vec("renderer.waterscroll").unwrap_or_default();
    let (sx, sz) = (scroll.first().copied().unwrap_or(0.0), scroll.get(1).copied().unwrap_or(0.0));
    WaterDesc {
        height,
        color: water_color(world),
        opaque_depth: 4.0,
        specular,
        specular_power: number("renderer.waterspecularpower").filter(|p| *p > 0.0).unwrap_or(40.0),
        wave_drift: [sx * 29.13 * clock, -sz * 31.81 * clock],
        wave_speed: clock * 8.0,
        normal_map: water_normal_map(vfs, converter),
        reflection_map: converter.file(&format!("levels/{level_name}/water/envmap.dds")),
    }
}

/// BF2's water normal map volume (8 frames of 256x256, R5G6B5), converted to 32-bit.
fn water_normal_map(vfs: &Vfs, converter: &MeshConverter) -> Option<String> {
    let path = "common/textures/watervolume.dds";
    let target = converter.out.join(path);
    if !target.exists() {
        let data = dds::rgb565_to_bgra8(&vfs.read(path).ok()?)?;
        std::fs::create_dir_all(target.parent()?).ok()?;
        std::fs::write(&target, data).ok()?;
    }
    Some(path.into())
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

/// Colors are 0..1, except a few written as 0..255 (fog colours, and some terrain water
/// colours by mistake). Light colours may be overbright (up to 2.34 in retail levels), while
/// every 0..255 colour has a component of at least 11.
pub fn normalize_color(c: [f32; 3]) -> [f32; 3] {
    if c.iter().any(|&v| v > 3.0) {
        c.map(|v| v / 255.0)
    } else {
        c
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A 4x4 DXT1 image whose texel (r, c) has index (r + 2c) % 4, one mip.
    fn tiny_dxt1() -> Vec<u8> {
        let mut d = vec![0u8; 128];
        d[..4].copy_from_slice(b"DDS ");
        d[12..16].copy_from_slice(&4u32.to_le_bytes());
        d[16..20].copy_from_slice(&4u32.to_le_bytes());
        d[28..32].copy_from_slice(&1u32.to_le_bytes());
        d[80..84].copy_from_slice(&4u32.to_le_bytes());
        d[84..88].copy_from_slice(b"DXT1");
        d.extend_from_slice(&[1, 2, 3, 4]);
        for r in 0..4 {
            let mut row = 0u8;
            for c in 0..4 {
                row |= (((r + 2 * c) % 4) as u8) << (2 * c);
            }
            d.push(row);
        }
        d
    }

    fn texel(d: &[u8], r: usize, c: usize) -> u8 {
        (d[128 + 4 + r] >> (2 * c)) & 3
    }

    #[test]
    fn turns_dxt1_texels() {
        let source = tiny_dxt1();
        // No turn: a vertical flip (BF2 row 0 is south).
        let flipped = orient_north_up(&source, Turn::None).unwrap();
        assert_eq!(texel(&flipped, 0, 1), texel(&source, 3, 1));
        // A quarter turn is a transpose.
        let turned = orient_north_up(&source, Turn::Ccw90).unwrap();
        assert_eq!(texel(&turned, 1, 3), texel(&source, 3, 1));
        let half = orient_north_up(&source, Turn::Ccw180).unwrap();
        assert_eq!(texel(&half, 0, 0), texel(&source, 0, 3));
        assert_eq!(&half[128..132], &[1, 2, 3, 4]);
    }
}
