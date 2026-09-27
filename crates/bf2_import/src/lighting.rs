//! The light of a level's world: the colours `sky.con` gives BF2's shaders for static
//! objects, terrain, trees, soldiers and vehicles (see [`WorldLighting`] for how each is
//! lit), and how bright the sun is in the terrain lightmaps.

use std::path::Path;

use anyhow::{Context, Result};
use bf2_formats::{
    Bf2Install, LevelInfo, Side,
    con::{Interpreter, World, parse_vec3},
    vfs::Vfs,
};
use game_data::{AdaptedLighting, LevelDesc, StaticLightmapEntry, StaticLightmaps, TerrainDesc, WorldLighting};

use crate::terrain::normalize_color;

/// The level's world lighting, `None` if its scripts set none of it (then renderers light
/// everything from the dynamic sun and ambient colours).
pub fn world_lighting(world: &World) -> Option<WorldLighting> {
    let color = |name: &str| world.setting(name).and_then(parse_color);
    let scalar = |name: &str| world.setting(name).and_then(|s| s.trim().parse::<f32>().ok());
    let static_sky = color("lightmanager.staticskycolor");
    let terrain_gi = color("terrain.gicolor");
    if static_sky.is_none() && terrain_gi.is_none() {
        return None;
    }
    let d = WorldLighting::default();
    let mut lighting = WorldLighting {
        static_sky: static_sky.unwrap_or(d.static_sky),
        static_sun: color("lightmanager.staticsuncolor").unwrap_or(d.static_sun),
        static_specular: color("lightmanager.staticspecularcolor").unwrap_or(d.static_specular),
        point: color("lightmanager.singlepointcolor").unwrap_or(d.point),
        terrain_sun: color("terrain.suncolor").unwrap_or(d.terrain_sun),
        terrain_gi: terrain_gi.unwrap_or(d.terrain_gi),
        terrain_sun_scale: 1.0,
        water_sun_intensity: scalar("terrain.watersunintensity").unwrap_or(d.water_sun_intensity),
        tree_ambient: color("lightmanager.treeambientcolor").unwrap_or(d.tree_ambient),
        tree_sun: color("lightmanager.treesuncolor").unwrap_or(d.tree_sun),
        tree_sky: color("lightmanager.treeskycolor").unwrap_or(d.tree_sky),
        dynamic_sky: color("lightmanager.skycolor").unwrap_or(d.dynamic_sky),
        hemi_lerp_bias: scalar("lightmanager.hemilerpbias").unwrap_or(d.hemi_lerp_bias),
        dynamic_point: color("lightmanager.dynamicpointcolor").unwrap_or(d.dynamic_point),
        effect_sun: color("lightmanager.effectsuncolor").unwrap_or(d.effect_sun),
        effect_shadow: color("lightmanager.effectshadowcolor").unwrap_or(d.effect_shadow),
        dark_adapted: None,
        bright_adapted: None,
        adaptation_seconds: [
            scalar("renderer.fakehdrblendingtimelight").unwrap_or(d.adaptation_seconds[0]),
            scalar("renderer.fakehdrblendingtimedark").unwrap_or(d.adaptation_seconds[1]),
        ],
    };
    // Faked HDR: `<setting>Low` (adapted to bright light) and `<setting>High` (to the dark).
    let sun = color("lightmanager.suncolor").unwrap_or([1.0; 3]);
    let adapted = |suffix: &str| -> Option<AdaptedLighting> {
        let get = |name: &str| color(&format!("{name}{suffix}"));
        let values = [
            get("lightmanager.staticskycolor"),
            get("terrain.gicolor"),
            get("lightmanager.singlepointcolor"),
            get("lightmanager.suncolor"),
            get("lightmanager.skycolor"),
            get("lightmanager.dynamicpointcolor"),
        ];
        values.iter().any(Option::is_some).then(|| AdaptedLighting {
            static_sky: values[0].unwrap_or(lighting.static_sky),
            terrain_gi: values[1].unwrap_or(lighting.terrain_gi),
            point: values[2].unwrap_or(lighting.point),
            sun: values[3].unwrap_or(sun),
            dynamic_sky: values[4].unwrap_or(lighting.dynamic_sky),
            dynamic_point: values[5].unwrap_or(lighting.dynamic_point),
        })
    };
    let (dark, bright) = (adapted("high"), adapted("low"));
    lighting.dark_adapted = dark;
    lighting.bright_adapted = bright;
    Some(lighting)
}

/// [`world_lighting`] with the terrain lightmaps' sun scale measured from the imported
/// terrain (`level_dir`). `sun_direction` is the engine-space direction sunlight travels.
pub fn level_lighting(
    world: &World,
    terrain: Option<&TerrainDesc>,
    level_dir: &Path,
    sun_direction: [f32; 3],
) -> Option<WorldLighting> {
    let mut lighting = world_lighting(world)?;
    let height = -glam::Vec3::from_array(sun_direction).normalize_or_zero().y;
    if let Some(scale) = terrain.and_then(|t| terrain_sun_scale(level_dir, t, height)) {
        lighting.terrain_sun_scale = scale;
    }
    Some(lighting)
}

/// `bf2-import light`: rewrites only the lighting of an imported level's `level.ron`.
pub fn import_only(install: &Bf2Install, level: &LevelInfo, out: &Path) -> Result<Option<WorldLighting>> {
    let name = level.name.to_lowercase();
    let level_dir = out.join("levels").join(&name);
    let path = level_dir.join("level.ron");
    let mut desc: LevelDesc = game_data::read_ron(&path).with_context(|| format!("{name} isn't imported yet"))?;
    let vfs = install.level_vfs(level, Side::Both)?;
    let mut interp = Interpreter::new(&vfs);
    interp.run(&format!("levels/{}/init.con", level.name), &[]);
    let lighting = level_lighting(
        &interp.world,
        desc.terrain.as_ref(),
        &level_dir,
        desc.environment.sun_direction,
    );
    desc.environment.lighting = lighting.clone();
    desc.environment.ground_albedo = desc.terrain.as_ref().and_then(|t| ground_albedo(&level_dir, t));
    desc.environment.static_lightmaps = static_lightmaps(&vfs, &level.name, &level_dir)?;
    game_data::write_ron(&path, &desc)?;
    Ok(lighting)
}

/// `r/g/b` (0..1, or 0..255 like the fog colours), or one number for all three.
fn parse_color(s: &str) -> Option<[f32; 3]> {
    parse_vec3(s)
        .map(normalize_color)
        .or_else(|| s.trim().parse::<f32>().ok().map(|v| [v; 3]))
}

/// Median sun visibility of the terrain lightmaps (their green channel) on sunlit flat
/// ground, divided by the sun's height (N.L there): 1 when the lightmaps were baked with the
/// level's sun, as for most levels. `None` without enough flat sunlit ground.
fn terrain_sun_scale(level_dir: &Path, terrain: &TerrainDesc, sun_height: f32) -> Option<f32> {
    if sun_height < 0.05 {
        return None;
    }
    let n = terrain.resolution as usize;
    let bytes = std::fs::read(level_dir.join(&terrain.heightmap)).ok()?;
    if n < 3 || bytes.len() != n * n * 2 {
        return None;
    }
    let heights: Vec<f32> = bytes
        .chunks_exact(2)
        .map(|b| u16::from_le_bytes([b[0], b[1]]) as f32 * terrain.height_scale)
        .collect();
    let tiles = terrain.color_map_tiles.max(1) as usize;
    let spacing = terrain.spacing.max(0.01);
    let mut samples = Vec::new();
    for (index, path) in terrain.lightmaps.iter().enumerate() {
        if path.is_empty() {
            continue;
        }
        let Some((w, h, green)) = std::fs::read(level_dir.join(path)).ok().and_then(|d| green_channel(&d)) else {
            continue;
        };
        // Patches and images are north-up like the heightmap (row 0 at -Z).
        let (px, pz) = ((index % tiles) as f32, (index / tiles) as f32);
        for y in (0..h).step_by(2) {
            for x in (0..w).step_by(2) {
                let g = green[y * w + x] as f32 / 255.0;
                if g < 0.08 {
                    continue; // in shadow
                }
                let fx = (px + (x as f32 + 0.5) / w as f32) / tiles as f32;
                let fz = (pz + (y as f32 + 0.5) / h as f32) / tiles as f32;
                let col = (fx * (n - 1) as f32).round() as usize;
                let row = (fz * (n - 1) as f32).round() as usize;
                if col == 0 || row == 0 || col >= n - 1 || row >= n - 1 {
                    continue;
                }
                let dx = (heights[row * n + col + 1] - heights[row * n + col - 1]) / (2.0 * spacing);
                let dz = (heights[(row + 1) * n + col] - heights[(row - 1) * n + col]) / (2.0 * spacing);
                if 1.0 / (1.0 + dx * dx + dz * dz).sqrt() > 0.995 {
                    samples.push(g);
                }
            }
        }
    }
    if samples.len() < 200 {
        return None;
    }
    let middle = samples.len() / 2;
    let (_, median, _) = samples.select_nth_unstable_by(middle, f32::total_cmp);
    Some((*median / sun_height).clamp(0.25, 2.0))
}

/// Width, height and green channel of a DDS file's top level (DXT1 or uncompressed 32-bit).
fn green_channel(data: &[u8]) -> Option<(usize, usize, Vec<u8>)> {
    let (width, height, rgb) = decode_rgb(data)?;
    Some((width, height, rgb.iter().map(|p| p[1]).collect()))
}

/// Width, height and RGB texels of a DDS file's top level (DXT1/3/5 or uncompressed 32-bit).
pub fn decode_rgb(data: &[u8]) -> Option<(usize, usize, Vec<[u8; 3]>)> {
    let u32_at = |o: usize| data.get(o..o + 4).map(|b| u32::from_le_bytes([b[0], b[1], b[2], b[3]]));
    if data.len() < 128 || &data[..4] != b"DDS " {
        return None;
    }
    let (height, width) = (u32_at(12)? as usize, u32_at(16)? as usize);
    let flags = u32_at(80)?;
    let bits = u32_at(88)?;
    let masks = [u32_at(92)?, u32_at(96)?, u32_at(100)?];
    let pixels = &data[128..];
    let mut out = vec![[0u8; 3]; width * height];
    let four_cc = &data[84..88];
    if flags & 0x4 != 0 && matches!(four_cc, b"DXT1" | b"DXT3" | b"DXT5") {
        // DXT3/5 blocks are an 8-byte alpha block, then a DXT1 colour block (always 4 colours).
        let (block_size, colour_at) = if four_cc == b"DXT1" { (8, 0) } else { (16, 8) };
        let (bw, bh) = (width.div_ceil(4), height.div_ceil(4));
        if pixels.len() < bw * bh * block_size {
            return None;
        }
        let rgb = |c: u16| {
            [
                ((c >> 11) & 31) as u32 * 255 / 31,
                ((c >> 5) & 63) as u32 * 255 / 63,
                (c & 31) as u32 * 255 / 31,
            ]
        };
        let mix = |a: [u32; 3], b: [u32; 3], wa: u32, wb: u32| {
            [0, 1, 2].map(|i| (a[i] * wa + b[i] * wb) / (wa + wb))
        };
        for by in 0..bh {
            for bx in 0..bw {
                let block = &pixels[(by * bw + bx) * block_size + colour_at..][..8];
                let c0 = u16::from_le_bytes([block[0], block[1]]);
                let c1 = u16::from_le_bytes([block[2], block[3]]);
                let (p0, p1) = (rgb(c0), rgb(c1));
                let palette = if c0 > c1 || block_size == 16 {
                    [p0, p1, mix(p0, p1, 2, 1), mix(p0, p1, 1, 2)]
                } else {
                    [p0, p1, mix(p0, p1, 1, 1), [0; 3]]
                };
                let indices = u32::from_le_bytes([block[4], block[5], block[6], block[7]]);
                for i in 0..16 {
                    let (x, y) = (bx * 4 + i % 4, by * 4 + i / 4);
                    if x < width && y < height {
                        out[y * width + x] = palette[((indices >> (2 * i)) & 3) as usize].map(|v| v as u8);
                    }
                }
            }
        }
    } else if flags & 0x4 == 0 && bits == 32 && masks.iter().all(|m| *m != 0) {
        if pixels.len() < width * height * 4 {
            return None;
        }
        for (o, p) in out.iter_mut().zip(pixels.chunks_exact(4)) {
            let v = u32::from_le_bytes([p[0], p[1], p[2], p[3]]);
            *o = masks.map(|mask| {
                let shift = mask.trailing_zeros();
                (((v & mask) >> shift) * 255 / (mask >> shift).max(1)) as u8
            });
        }
    } else {
        return None;
    }
    Some((width, height, out))
}

/// Side of the static lightmap atlas layers we write (BF2's are mostly 2048; sky light is
/// smooth), halved for levels with many atlases to bound the memory they take.
fn atlas_size(atlases: usize) -> usize {
    if atlases > 24 { 512 } else { 1024 }
}

/// The sky channel of the level's static object lightmaps (`lightmaps/objects/`: atlases
/// and `LightmapAtlas.tai`, where each object's lightmap lies in them) as one texture array
/// `lightmaps/objects_sky.dds` and `lightmaps/objects.ron` ([`StaticLightmaps`]). Returns the
/// RON's path relative to the level folder, `None` if the level has no object lightmaps.
pub fn static_lightmaps(vfs: &Vfs, level: &str, level_dir: &Path) -> Result<Option<String>> {
    let base = format!("levels/{level}/lightmaps/objects");
    let Ok(tai) = vfs.read_text(&format!("{base}/lightmapatlas.tai")) else {
        return Ok(None);
    };
    let (entries, atlases) = parse_atlas_index(&tai);
    if entries.is_empty() {
        return Ok(None);
    }
    let size = atlas_size(atlases.len());
    let mut layers = Vec::with_capacity(atlases.len());
    for atlas in &atlases {
        let sky = vfs
            .read(atlas)
            .ok()
            .and_then(|data| decode_rgb(&data))
            .map(|(w, h, rgb)| resample(&rgb.iter().map(|p| p[2]).collect::<Vec<_>>(), w, h, size));
        if sky.is_none() {
            log::warn!("{atlas}: missing or unreadable");
        }
        layers.push(sky.unwrap_or_else(|| vec![217; size * size]));
    }
    // A single layer would load as a plain 2D texture; shaders expect an array.
    if layers.len() == 1 {
        layers.push(layers[0].clone());
    }
    let dir = level_dir.join("lightmaps");
    std::fs::create_dir_all(&dir)?;
    std::fs::write(dir.join("objects_sky.dds"), r8_array_dds(&layers, size))?;
    let desc = StaticLightmaps {
        atlas: "lightmaps/objects_sky.dds".into(),
        entries,
    };
    game_data::write_ron(&dir.join("objects.ron"), &desc)?;
    Ok(Some("lightmaps/objects.ron".into()))
}

/// Entries of a `LightmapAtlas.tai` (lines `<dir>/<name>=<geometry><lod>=<x>=<y>=<z>.dds
/// <atlas path>, <index>, <u offset>, <v offset>, <width>, <height>`) and the atlas paths by
/// index.
fn parse_atlas_index(tai: &str) -> (Vec<StaticLightmapEntry>, Vec<String>) {
    let mut entries = Vec::new();
    let mut atlases: Vec<String> = Vec::new();
    for line in tai.lines() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let Some((texture, rest)) = line.split_once(char::is_whitespace) else {
            continue;
        };
        let fields: Vec<&str> = rest.split(',').map(str::trim).collect();
        let [atlas, index, u, v, width, height] = fields[..] else {
            continue;
        };
        let stem = texture.rsplit('/').next().unwrap_or(texture).trim_end_matches(".dds");
        let parts: Vec<&str> = stem.split('=').collect();
        let [name, geometry_lod, x, y, z] = parts[..] else {
            continue;
        };
        let number = |s: &str| s.parse::<f32>().ok();
        let (Ok(index), Some(u), Some(v), Some(width), Some(height), Some(x), Some(y), Some(z)) = (
            index.parse::<usize>(),
            number(u),
            number(v),
            number(width),
            number(height),
            number(x),
            number(y),
            number(z),
        ) else {
            continue;
        };
        let digit = |i: usize| geometry_lod.chars().nth(i).and_then(|c| c.to_digit(10)).unwrap_or(0);
        if atlases.len() <= index {
            atlases.resize(index + 1, String::new());
        }
        atlases[index] = atlas.to_ascii_lowercase();
        entries.push(StaticLightmapEntry {
            name: name.to_ascii_lowercase(),
            geometry: digit(0),
            lod: digit(1),
            position: crate::coords::position([x, y, z]),
            layer: index as u32,
            offset: [u, v],
            scale: [width, height],
        });
    }
    (entries, atlases)
}

/// A square single-channel image resampled to `size` (box filter down, nearest up).
fn resample(pixels: &[u8], width: usize, height: usize, size: usize) -> Vec<u8> {
    let mut out = vec![0u8; size * size];
    for y in 0..size {
        let (y0, y1) = (y * height / size, ((y + 1) * height / size).max(y * height / size + 1));
        for x in 0..size {
            let (x0, x1) = (x * width / size, ((x + 1) * width / size).max(x * width / size + 1));
            let (mut sum, mut n) = (0u32, 0u32);
            for sy in y0..y1.min(height) {
                for sx in x0..x1.min(width) {
                    sum += pixels[sy * width + sx] as u32;
                    n += 1;
                }
            }
            out[y * size + x] = (sum / n.max(1)) as u8;
        }
    }
    out
}

/// A DDS (DX10 header) texture array of `R8_UNORM` layers, `size` square, with all mip levels.
fn r8_array_dds(layers: &[Vec<u8>], size: usize) -> Vec<u8> {
    let mips = size.max(1).ilog2() as usize + 1;
    let mut out = Vec::new();
    let mut put = |v: u32| out.extend_from_slice(&v.to_le_bytes());
    put(u32::from_le_bytes(*b"DDS "));
    put(124); // header size
    put(0x1 | 0x2 | 0x4 | 0x8 | 0x1000 | 0x20000); // caps, height, width, pitch, pixel format, mips
    put(size as u32); // height
    put(size as u32); // width
    put(size as u32); // pitch
    put(0); // depth
    put(mips as u32);
    for _ in 0..11 {
        put(0);
    }
    put(32); // pixel format size
    put(0x4); // four cc
    put(u32::from_le_bytes(*b"DX10"));
    for _ in 0..5 {
        put(0);
    }
    put(0x1000 | 0x8 | 0x40_0000); // texture, complex, mipmap
    for _ in 0..4 {
        put(0);
    }
    put(61); // DXGI_FORMAT_R8_UNORM
    put(3); // texture 2D
    put(0);
    put(layers.len() as u32);
    put(0);
    for layer in layers {
        let mut level = layer.clone();
        let mut s = size;
        for _ in 0..mips {
            out.extend_from_slice(&level);
            if s > 1 {
                level = resample(&level, s, s, s / 2);
                s /= 2;
            }
        }
    }
    out
}

/// Mean linear colour of the terrain's colour maps (`None` without any readable one).
pub fn ground_albedo(level_dir: &Path, terrain: &TerrainDesc) -> Option<[f32; 3]> {
    let to_linear = |v: u8| (v as f32 / 255.0).powf(2.2);
    let (mut sum, mut count) = ([0f64; 3], 0usize);
    for path in terrain.color_maps.iter().filter(|p| !p.is_empty()) {
        let Some((_, _, rgb)) = std::fs::read(level_dir.join(path)).ok().and_then(|d| decode_rgb(&d)) else {
            continue;
        };
        for texel in rgb.iter().step_by(7) {
            for (s, v) in sum.iter_mut().zip(texel) {
                *s += to_linear(*v) as f64;
            }
            count += 1;
        }
    }
    (count > 0).then(|| sum.map(|s| (s / count as f64) as f32))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn colors_and_scalars() {
        assert_eq!(parse_color("0.5/0.25/1"), Some([0.5, 0.25, 1.0]));
        assert_eq!(parse_color("1"), Some([1.0; 3]));
        let fog = parse_color("8.00/12.00/24.00").unwrap();
        assert!((fog[2] - 24.0 / 255.0).abs() < 1e-6);
    }

    #[test]
    fn atlas_index() {
        let tai = "# comment\n\nlevels/x/lightmaps/objects/house_01=01=-226=166=59.dds\t\tlevels/x/lightmaps/objects/LightmapAtlas3.dds, 3, 0.25, 0.5, 0.125, 0.125\n";
        let (entries, atlases) = parse_atlas_index(tai);
        assert_eq!(atlases.len(), 4);
        assert_eq!(atlases[3], "levels/x/lightmaps/objects/lightmapatlas3.dds");
        assert_eq!(
            entries,
            vec![StaticLightmapEntry {
                name: "house_01".into(),
                geometry: 0,
                lod: 1,
                position: [-226.0, 166.0, -59.0],
                layer: 3,
                offset: [0.25, 0.5],
                scale: [0.125, 0.125],
            }]
        );
    }

    #[test]
    fn r8_array_layout() {
        let dds = r8_array_dds(&[vec![10; 16], vec![20; 16]], 4);
        // Header, DX10 header, then 2 layers of 16 + 4 + 1 texels.
        assert_eq!(dds.len(), 128 + 20 + 2 * 21);
        assert_eq!(u32::from_le_bytes(dds[128..132].try_into().unwrap()), 61);
        assert_eq!(dds[148 + 21], 20);
    }

    #[test]
    fn dxt1_green() {
        // One block: colour 0 pure green, colour 1 black, texel i uses index i % 4.
        let mut dds = vec![0u8; 128 + 8];
        dds[..4].copy_from_slice(b"DDS ");
        dds[12..16].copy_from_slice(&4u32.to_le_bytes());
        dds[16..20].copy_from_slice(&4u32.to_le_bytes());
        dds[80..84].copy_from_slice(&4u32.to_le_bytes());
        dds[84..88].copy_from_slice(b"DXT1");
        dds[128..130].copy_from_slice(&0x07E0u16.to_le_bytes());
        dds[132..136].copy_from_slice(&0b11_10_01_00_11_10_01_00_11_10_01_00_11_10_01_00u32.to_le_bytes());
        let (w, h, g) = green_channel(&dds).unwrap();
        assert_eq!((w, h), (4, 4));
        assert_eq!(&g[..4], &[255, 0, 170, 85]);
    }
}
