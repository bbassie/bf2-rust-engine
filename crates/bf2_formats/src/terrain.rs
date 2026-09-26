//! `terraindata.raw`: the compiled terrain. Only its header matters to us (texture base
//! names and the detail texture setup); the mesh data after it is rebuilt from heightmaps.

use crate::reader::{ReadError, Reader};

#[derive(Clone, Debug)]
pub struct DetailTexture {
    /// VFS path without extension, e.g. `common\terrain\textures\detail\detail_grass05`.
    /// Empty for unused slots.
    pub texture: String,
    /// Projected on the X and Z planes too (cliffs), not only from above.
    pub tri_planar: bool,
    /// Repeats per 256 m patch on the side planes (U, V).
    pub side_tiling: [f32; 2],
    /// Repeats per 256 m patch on the top plane.
    pub top_tiling: f32,
    pub y_offset: f32,
    pub env_map: bool,
}

#[derive(Clone, Debug)]
pub struct TerrainDataHeader {
    pub version: u32,
    pub primary_world_scale: [f32; 3],
    pub secondary_world_scale: [f32; 3],
    pub max_height: f32,
    pub min_height: f32,
    /// Heightmap cells per patch side (128).
    pub patch_size: u32,
    pub patches_per_side: u32,
    pub colormap_base: String,
    pub detailmap_base: String,
    pub low_detailmap_base: String,
    pub lightmap_base: String,
    pub sun_color: [f32; 3],
    pub gi_color: [f32; 3],
    pub water_color: [f32; 3],
    /// Up to 6. Weight of texture `i` is channel B, G, R (`i % 3`) of detail map `_1`
    /// (i < 3) or `_2` (i >= 3).
    pub detail_textures: Vec<DetailTexture>,
}

fn line(r: &mut Reader) -> Result<String, ReadError> {
    let mut bytes = Vec::new();
    loop {
        match r.u8()? {
            b'\n' => break,
            b => bytes.push(b),
        }
    }
    Ok(bytes.iter().map(|&b| b as char).collect())
}

impl TerrainDataHeader {
    pub fn parse(data: &[u8]) -> Result<Self, ReadError> {
        let mut r = Reader::new(data);
        let version = r.u32()?;
        let primary_world_scale = r.vec3()?;
        let secondary_world_scale = r.vec3()?;
        let _uninitialized = r.u32()?;
        let max_height = r.f32()?;
        let min_height = r.f32()?;
        let patch_size = r.u32()?;
        let _subdivide = r.u8()?;
        let patches_per_side = r.u32()?;
        let _colormap_size = r.u32()?;
        let _low_detailmap_size = r.u32()?;
        let colormap_base = line(&mut r)?;
        let detailmap_base = line(&mut r)?;
        let low_detailmap_base = line(&mut r)?;
        let lightmap_base = line(&mut r)?;
        r.skip(8 + 12)?; // far side tiling, far top tiling hi/low, far y offset
        let sun_color = r.vec3()?;
        let gi_color = r.vec3()?;
        let water_color = r.vec3()?;
        let count = r.count(22)?;
        let mut detail_textures = Vec::with_capacity(count);
        for _ in 0..count {
            let texture = line(&mut r)?;
            let tri_planar = r.u8()? != 0;
            let side_u = r.f32()?;
            let side_v = r.f32()?;
            let top_tiling = r.f32()?;
            let y_offset = r.f32()?;
            let env_map = r.u8()? != 0;
            detail_textures.push(DetailTexture {
                texture,
                tri_planar,
                side_tiling: [side_u, side_v],
                top_tiling,
                y_offset,
                env_map,
            });
        }
        Ok(Self {
            version,
            primary_world_scale,
            secondary_world_scale,
            max_height,
            min_height,
            patch_size,
            patches_per_side,
            colormap_base,
            detailmap_base,
            low_detailmap_base,
            lightmap_base,
            sun_color,
            gi_color,
            water_color,
            detail_textures,
        })
    }
}
