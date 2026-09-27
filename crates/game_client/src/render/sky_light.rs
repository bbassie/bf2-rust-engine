//! Directional sky light: the level's ambient light as an environment map (a bright sky above,
//! a haze at the horizon and the lit ground below) instead of one uniform colour, so surfaces
//! facing up get more light than walls, and walls more than undersides.
//!
//! The cube maps are made on the CPU: the radiance only depends on the direction's height, so
//! the diffuse (cosine) and specular (per roughness) convolutions are 1D tables.

use bevy::{
    asset::RenderAssetUsages,
    image::{ImageSampler, ImageSamplerDescriptor},
    prelude::*,
    render::render_resource::{
        Extent3d, TextureDataOrder, TextureDimension, TextureFormat, TextureViewDescriptor, TextureViewDimension,
    },
};

/// Radiance of the sky and the ground (linear light, 1 = the level's light unit).
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct SkyGradient {
    /// Straight up.
    pub zenith: Vec3,
    /// At the horizon (the haze, usually brighter and less saturated than the zenith).
    pub horizon: Vec3,
    /// Below the horizon: the lit ground seen from above it.
    pub ground: Vec3,
}

impl SkyGradient {
    /// Radiance in a direction of height `y` (sine of the elevation).
    pub fn radiance(&self, y: f32) -> Vec3 {
        if y >= 0.0 {
            // Most of the sky is close to the zenith colour; the haze is a band.
            self.horizon.lerp(self.zenith, (y / 0.35).clamp(0.0, 1.0).powf(0.7))
        } else {
            // The ground takes over a few degrees below the horizon.
            self.horizon.lerp(self.ground, (-y / 0.08).clamp(0.0, 1.0))
        }
    }

    /// Cosine-weighted mean radiance seen by a surface whose normal has height `ny`: what a
    /// white surface facing that way reflects.
    pub fn irradiance(&self, ny: f32) -> Vec3 {
        let n = normal_of(ny);
        let samples = sphere_samples();
        let mut sum = Vec3::ZERO;
        let mut weight = 0.0;
        for d in samples {
            let c = n.dot(*d);
            if c > 0.0 {
                sum += self.radiance(d.y) * c;
            }
            weight += 1.0;
        }
        // Uniform samples: E/pi = (4 pi / N) sum(L cos) / pi.
        sum * 4.0 / weight
    }

    /// Mean radiance in a power-cosine lobe of `exponent` around a direction of height `ry`
    /// (the specular reflection of a rough surface).
    fn prefiltered(&self, ry: f32, exponent: f32) -> Vec3 {
        if exponent > 400.0 {
            return self.radiance(ry);
        }
        let r = normal_of(ry);
        let mut sum = Vec3::ZERO;
        let mut weight = 0.0;
        for d in sphere_samples() {
            let c = r.dot(*d);
            if c > 0.0 {
                let w = c.powf(exponent);
                sum += self.radiance(d.y) * w;
                weight += w;
            }
        }
        if weight > 0.0 { sum / weight } else { self.radiance(ry) }
    }
}

/// A unit vector of height `y`.
fn normal_of(y: f32) -> Vec3 {
    let y = y.clamp(-1.0, 1.0);
    Vec3::new((1.0 - y * y).max(0.0).sqrt(), y, 0.0)
}

/// Evenly spread directions (Fibonacci sphere).
fn sphere_samples() -> &'static [Vec3] {
    static SAMPLES: std::sync::OnceLock<Vec<Vec3>> = std::sync::OnceLock::new();
    SAMPLES.get_or_init(|| {
        const N: usize = 4096;
        let golden = std::f32::consts::PI * (3.0 - 5f32.sqrt());
        (0..N)
            .map(|i| {
                let y = 1.0 - (i as f32 + 0.5) / N as f32 * 2.0;
                let r = (1.0 - y * y).sqrt();
                let a = golden * i as f32;
                Vec3::new(r * a.cos(), y, r * a.sin())
            })
            .collect()
    })
}

/// Height of the direction through texel `(x, y)` of cube face `face` (`size` texels a side).
fn texel_height(face: usize, x: usize, y: usize, size: usize) -> f32 {
    let u = 2.0 * (x as f32 + 0.5) / size as f32 - 1.0;
    let v = 2.0 * (y as f32 + 0.5) / size as f32 - 1.0;
    let dir = match face {
        0 => Vec3::new(1.0, -v, -u),
        1 => Vec3::new(-1.0, -v, u),
        2 => Vec3::new(u, 1.0, v),
        3 => Vec3::new(u, -1.0, -v),
        4 => Vec3::new(u, -v, 1.0),
        _ => Vec3::new(-u, -v, -1.0),
    };
    dir.normalize().y
}

/// Table of `f(height)` over `ROWS` heights, looked up linearly.
struct HeightTable(Vec<Vec3>);

const ROWS: usize = 65;

impl HeightTable {
    fn new(f: impl Fn(f32) -> Vec3) -> Self {
        Self((0..ROWS).map(|i| f(i as f32 / (ROWS - 1) as f32 * 2.0 - 1.0)).collect())
    }

    fn get(&self, y: f32) -> Vec3 {
        let t = (y.clamp(-1.0, 1.0) + 1.0) * 0.5 * (ROWS - 1) as f32;
        let i = (t.floor() as usize).min(ROWS - 2);
        self.0[i].lerp(self.0[i + 1], t - i as f32)
    }
}

/// `f32` to IEEE half (non-negative, flushing tiny values to zero).
fn half_bits(v: f32) -> u16 {
    let v = v.clamp(0.0, 65_000.0);
    let bits = v.to_bits();
    let exponent = ((bits >> 23) & 0xff) as i32 - 127 + 15;
    if exponent <= 0 {
        return 0;
    }
    let mantissa = ((bits >> 13) & 0x3ff) as u16;
    let round = ((bits >> 12) & 1) as u16;
    ((exponent as u16) << 10 | mantissa).saturating_add(round)
}

/// A cube map (`Rgba16Float`) with `mips` levels, texel colours from their direction's height.
fn cube_map(size: usize, mips: usize, texel: impl Fn(usize, f32) -> Vec3) -> Image {
    let mut data = Vec::new();
    // Layer major: all levels of face 0, then face 1, ...
    for face in 0..6 {
        for mip in 0..mips {
            let s = (size >> mip).max(1);
            for y in 0..s {
                for x in 0..s {
                    let c = texel(mip, texel_height(face, x, y, s));
                    for v in [c.x, c.y, c.z, 1.0] {
                        data.extend_from_slice(&half_bits(v).to_le_bytes());
                    }
                }
            }
        }
    }
    let mut image = Image::new_uninit(
        Extent3d {
            width: size as u32,
            height: size as u32,
            depth_or_array_layers: 6,
        },
        TextureDimension::D2,
        TextureFormat::Rgba16Float,
        RenderAssetUsages::RENDER_WORLD,
    );
    image.data = Some(data);
    image.data_order = TextureDataOrder::LayerMajor;
    image.texture_descriptor.mip_level_count = mips as u32;
    image.texture_view_descriptor = Some(TextureViewDescriptor {
        dimension: Some(TextureViewDimension::Cube),
        ..default()
    });
    image.sampler = ImageSampler::Descriptor(ImageSamplerDescriptor::linear());
    image
}

/// Mip levels of the specular map (64 texels down to 1).
const SPECULAR_SIZE: usize = 64;
const SPECULAR_MIPS: usize = 7;
const DIFFUSE_SIZE: usize = 16;

/// The diffuse (irradiance) and specular (prefiltered radiance) cube maps of `sky`, in the
/// layout Bevy's `EnvironmentMapLight` expects: the specular level for perceptual roughness
/// `r` is `r x (levels - 1)`.
pub fn cube_maps(sky: &SkyGradient) -> (Image, Image) {
    let irradiance = HeightTable::new(|y| sky.irradiance(y));
    let diffuse = cube_map(DIFFUSE_SIZE, 1, |_, y| irradiance.get(y));
    let lobes: Vec<HeightTable> = (0..SPECULAR_MIPS)
        .map(|mip| {
            let roughness = mip as f32 / (SPECULAR_MIPS - 1) as f32;
            let alpha = (roughness * roughness).max(1e-3);
            // The power-cosine lobe closest to GGX of this roughness.
            let exponent = (2.0 / (alpha * alpha) - 2.0).max(0.5);
            HeightTable::new(|y| sky.prefiltered(y, exponent))
        })
        .collect();
    let specular = cube_map(SPECULAR_SIZE, SPECULAR_MIPS, |mip, y| lobes[mip].get(y));
    (diffuse, specular)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn uniform_sky_irradiance_is_its_radiance() {
        let c = Vec3::new(0.3, 0.4, 0.5);
        let sky = SkyGradient { zenith: c, horizon: c, ground: c };
        for ny in [-1.0, -0.3, 0.0, 0.5, 1.0] {
            assert!((sky.irradiance(ny) - c).abs().max_element() < 0.01, "{ny}");
        }
    }

    #[test]
    fn split_sky_irradiance_follows_the_normal() {
        let sky = SkyGradient { zenith: Vec3::ONE, horizon: Vec3::ONE, ground: Vec3::ZERO };
        // Up: all sky; sideways: about half; down: almost none (the haze band below 0).
        assert!(sky.irradiance(1.0).x > 0.99);
        assert!((sky.irradiance(0.0).x - 0.5).abs() < 0.05);
        assert!(sky.irradiance(-1.0).x < 0.05);
    }

    #[test]
    fn halves() {
        assert_eq!(half_bits(1.0), 0x3c00);
        assert_eq!(half_bits(0.5), 0x3800);
        assert_eq!(half_bits(0.0), 0);
    }

    #[test]
    fn cube_face_heights() {
        assert!(texel_height(2, 8, 8, 16) > 0.99);
        assert!(texel_height(3, 8, 8, 16) < -0.99);
        // Side faces: row 0 is up.
        assert!(texel_height(0, 8, 0, 16) > 0.9);
        assert!(texel_height(4, 8, 15, 16) < -0.9);
    }
}
