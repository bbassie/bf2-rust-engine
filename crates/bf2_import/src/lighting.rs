//! The light of a level's world: the colours `sky.con` gives BF2's shaders for static
//! objects, terrain, trees, soldiers and vehicles (see [`WorldLighting`] for how each is
//! lit), and how bright the sun is in the terrain lightmaps.

use std::path::Path;

use anyhow::{Context, Result};
use bf2_formats::{
    Bf2Install, LevelInfo, Side,
    con::{Interpreter, World, parse_vec3},
};
use game_data::{AdaptedLighting, LevelDesc, TerrainDesc, WorldLighting};

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
    let u32_at = |o: usize| data.get(o..o + 4).map(|b| u32::from_le_bytes([b[0], b[1], b[2], b[3]]));
    if data.len() < 128 || &data[..4] != b"DDS " {
        return None;
    }
    let (height, width) = (u32_at(12)? as usize, u32_at(16)? as usize);
    let (flags, bits, green_mask) = (u32_at(80)?, u32_at(88)?, u32_at(96)?);
    let pixels = &data[128..];
    let mut out = vec![0u8; width * height];
    if flags & 0x4 != 0 && &data[84..88] == b"DXT1" {
        let (bw, bh) = (width.div_ceil(4), height.div_ceil(4));
        if pixels.len() < bw * bh * 8 {
            return None;
        }
        let green = |c: u16| (((c >> 5) & 63) as u32 * 255 / 63) as u8;
        for by in 0..bh {
            for bx in 0..bw {
                let block = &pixels[(by * bw + bx) * 8..][..8];
                let c0 = u16::from_le_bytes([block[0], block[1]]);
                let c1 = u16::from_le_bytes([block[2], block[3]]);
                let (g0, g1) = (green(c0) as u32, green(c1) as u32);
                let palette = if c0 > c1 {
                    [g0, g1, (2 * g0 + g1) / 3, (g0 + 2 * g1) / 3]
                } else {
                    [g0, g1, (g0 + g1) / 2, 0]
                };
                let indices = u32::from_le_bytes([block[4], block[5], block[6], block[7]]);
                for i in 0..16 {
                    let (x, y) = (bx * 4 + i % 4, by * 4 + i / 4);
                    if x < width && y < height {
                        out[y * width + x] = palette[((indices >> (2 * i)) & 3) as usize] as u8;
                    }
                }
            }
        }
    } else if flags & 0x4 == 0 && bits == 32 && green_mask != 0 {
        if pixels.len() < width * height * 4 {
            return None;
        }
        let shift = green_mask.trailing_zeros();
        let max = green_mask >> shift;
        for (o, p) in out.iter_mut().zip(pixels.chunks_exact(4)) {
            let v = u32::from_le_bytes([p[0], p[1], p[2], p[3]]);
            *o = (((v & green_mask) >> shift) * 255 / max.max(1)) as u8;
        }
    } else {
        return None;
    }
    Some((width, height, out))
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
