//! Terrain rendering: the heightfield is cut into chunks so off-screen parts are culled, and
//! the coarse surrounding terrain continues it to the horizon. Water is in `water`.

use bevy::{
    asset::RenderAssetUsages,
    camera::visibility::VisibilityRange,
    image::{ImageLoaderSettings, ImageSampler},
    light::NotShadowCaster,
    mesh::Indices,
    prelude::*,
    render::render_resource::{Extent3d, PrimitiveTopology, TextureDimension, TextureFormat},
};
use game_shared::level::{Heightmap, LevelEntity, LoadedLevel, Terrain};

use super::materials::{TerrainLayerParams, TerrainLayers, TerrainMaterial, clamped_sampler};

pub struct TerrainRenderPlugin;

impl Plugin for TerrainRenderPlugin {
    fn build(&self, app: &mut App) {
        app.add_systems(
            Update,
            build_terrain_visuals.run_if(resource_exists_and_changed::<LoadedLevel>),
        );
    }
}

/// Cells per chunk side.
const CHUNK_CELLS: u32 = 64;

/// Terrain levels of detail: sample step and the camera distances (to the chunk centre) it
/// is drawn at, cross-fading over the margins. The chunk under the camera is always full
/// detail (a 256 m chunk's centre is at most 181 m away).
const LODS: [(u32, VisibilityRange); 3] = [
    (1, lod_range(0.0, 330.0)),
    (2, lod_range(330.0, 750.0)),
    (4, lod_range(750.0, f32::MAX)),
];

const fn lod_range(start: f32, end: f32) -> VisibilityRange {
    const FADE: f32 = 40.0;
    VisibilityRange {
        start_margin: if start > 0.0 { start - FADE..start } else { 0.0..0.0 },
        end_margin: if end < f32::MAX { end - FADE..end } else { f32::MAX..f32::MAX },
        use_aabb: false,
    }
}

/// Skirts hang this far below chunk edges, hiding cracks between neighbouring LODs.
const SKIRT_DEPTH: f32 = 6.0;

#[derive(Component)]
struct TerrainChunk;

fn build_terrain_visuals(
    mut commands: Commands,
    level: Res<LoadedLevel>,
    terrains: Query<&Terrain>,
    old_chunks: Query<Entity, With<TerrainChunk>>,
    asset_server: Res<AssetServer>,
    mut meshes: ResMut<Assets<Mesh>>,
    mut materials: ResMut<Assets<StandardMaterial>>,
    mut terrain_materials: ResMut<Assets<TerrainMaterial>>,
    mut images: ResMut<Assets<Image>>,
) {
    for chunk in &old_chunks {
        commands.entity(chunk).despawn();
    }
    let Ok(Terrain(heightmap)) = terrains.single() else {
        return;
    };

    // Imported levels have one color map per patch; the test level uses vertex colors.
    let cells = heightmap.resolution - 1;
    let desc = level.desc.terrain.as_ref();
    let tiles = desc
        .filter(|t| !t.color_maps.is_empty())
        .map_or(0, |t| t.color_map_tiles.max(1));
    let chunk_cells = if tiles > 0 { (cells / tiles).max(1) } else { CHUNK_CELLS };

    let level_file = |path: &str| format!("imported://levels/{}/{path}", level.desc.name);
    let load_patch = |path: &str, srgb: bool| -> Option<Handle<Image>> {
        (!path.is_empty()).then(|| {
            asset_server
                .load_builder()
                .with_settings(move |s: &mut ImageLoaderSettings| {
                    s.is_srgb = srgb;
                    s.sampler = ImageSampler::Descriptor(clamped_sampler());
                })
                .load(level_file(path))
        })
    };
    // Detail textures are shared by all patches.
    let details: Vec<Option<Handle<Image>>> = (0..6)
        .map(|i| {
            desc.and_then(|t| t.detail_textures.get(i))
                .filter(|d| !d.texture.is_empty())
                .map(|d| asset_server.load(format!("imported://{}", d.texture)))
        })
        .collect();
    let mut params = TerrainLayerParams {
        side0_fade: Vec4::new(8.0, 16.0, 80.0, 220.0),
        ..default()
    };
    if let Some(t) = desc {
        let tile = |i: usize| t.detail_textures.get(i).map_or(8.0, |d| d.top_tile_size);
        params.tile_a = Vec4::new(tile(0), tile(1), tile(2), tile(3));
        params.tile_b = Vec4::new(tile(4), tile(5), 8.0, 8.0);
        if let Some(rock) = t.detail_textures.first() {
            params.side0_fade.x = rock.side_tile_size[0];
            params.side0_fade.y = rock.side_tile_size[1];
            if rock.tri_planar {
                params.flags |= TerrainLayerParams::TRI_PLANAR_0;
            }
        }
    }
    let untextured = StandardMaterial {
        base_color: Color::srgb(0.35, 0.32, 0.25),
        perceptual_roughness: 0.95,
        ..default()
    };
    let no_weights = images.add(Image::new_fill(
        Extent3d::default(),
        TextureDimension::D2,
        &[0, 0, 0, 0],
        TextureFormat::Rgba8Unorm,
        RenderAssetUsages::RENDER_WORLD,
    ));

    for cz in (0..cells).step_by(chunk_cells as usize) {
        for cx in (0..cells).step_by(chunk_cells as usize) {
            let x1 = (cx + chunk_cells).min(cells);
            let z1 = (cz + chunk_cells).min(cells);
            let index = ((cz / chunk_cells) * tiles.max(1) + cx / chunk_cells) as usize;
            let color_map = desc
                .and_then(|t| t.color_maps.get(index))
                .and_then(|p| load_patch(p, true));
            let weights = desc.and_then(|t| t.detail_weights.get(index));
            let weights_a = weights.and_then(|w| load_patch(&w[0], false));
            // Patches using only textures 0..2 have no second map; unbound it would read white.
            let weights_b = weights
                .and_then(|w| load_patch(&w[1], false))
                .or_else(|| weights_a.is_some().then(|| no_weights.clone()));
            let mut extension = TerrainLayers {
                params,
                weights_a,
                weights_b,
                detail_0: details[0].clone(),
                detail_1: details[1].clone(),
                detail_2: details[2].clone(),
                detail_3: details[3].clone(),
                detail_4: details[4].clone(),
                detail_5: details[5].clone(),
            };
            if extension.weights_a.is_some() {
                extension.params.flags |= TerrainLayerParams::HAS_WEIGHTS;
            }
            let base = match (&color_map, tiles) {
                (_, 0) => StandardMaterial {
                    perceptual_roughness: 0.95,
                    reflectance: 0.15,
                    ..default()
                },
                (Some(texture), _) => StandardMaterial {
                    base_color_texture: Some(texture.clone()),
                    perceptual_roughness: 0.95,
                    reflectance: 0.15,
                    ..default()
                },
                (None, _) => untextured.clone(),
            };
            let material = terrain_materials.add(TerrainMaterial { base, extension });
            // Chunks sit at their centre: LOD distances are measured from the origin.
            let center = Vec3::new(
                heightmap.origin.x + (cx + x1) as f32 * 0.5 * heightmap.spacing,
                heightmap.origin.y,
                heightmap.origin.z + (cz + z1) as f32 * 0.5 * heightmap.spacing,
            );
            for (lod, (step, range)) in LODS.iter().enumerate() {
                let step = *step;
                if lod > 0 && (x1 - cx) % step != 0 {
                    break;
                }
                let mesh = chunk_mesh(heightmap, cx..=x1, cz..=z1, step, tiles == 0);
                let mut chunk = commands.spawn((
                    TerrainChunk,
                    LevelEntity,
                    Mesh3d(meshes.add(mesh)),
                    MeshMaterial3d(material.clone()),
                    Transform::from_translation(center),
                ));
                // Chunks without coarser LODs stay visible at any distance.
                let last = LODS.get(lod + 1).is_none_or(|(next, _)| (x1 - cx) % next != 0);
                chunk.insert(if last {
                    VisibilityRange {
                        start_margin: range.start_margin.clone(),
                        end_margin: f32::MAX..f32::MAX,
                        use_aabb: false,
                    }
                } else {
                    range.clone()
                });
            }
        }
    }

    if let (Some(surrounding), Some(dir)) = (desc.and_then(|t| t.surrounding.as_ref()), &level.dir) {
        match Surrounding::load(dir, surrounding) {
            Ok(ring) => {
                for (cell, path) in surrounding.color_maps.iter().enumerate().filter(|(i, _)| *i != 4) {
                    let material = materials.add(StandardMaterial {
                        base_color_texture: load_patch(path, true),
                        base_color: if path.is_empty() { untextured.base_color } else { Color::WHITE },
                        perceptual_roughness: 0.95,
                        reflectance: 0.15,
                        cull_mode: None,
                        ..default()
                    });
                    commands.spawn((
                        TerrainChunk,
                        LevelEntity,
                        Mesh3d(meshes.add(ring.cell_mesh(cell))),
                        MeshMaterial3d(material),
                        Transform::from_translation(ring.origin),
                        NotShadowCaster,
                    ));
                }
            }
            Err(err) => warn!("surrounding terrain: {err:#}"),
        }
    }
}

/// The coarse terrain around the playable one (a 3x3 grid of terrain-sized cells; the
/// middle one is covered by the real terrain).
struct Surrounding {
    resolution: usize,
    spacing: f32,
    origin: Vec3,
    heights: Vec<f32>,
}

/// Samples between surrounding terrain vertices (it is only seen from afar).
const SURROUNDING_STEP: usize = 2;

impl Surrounding {
    fn load(dir: &std::path::Path, desc: &game_data::SurroundingTerrainDesc) -> anyhow::Result<Self> {
        let bytes = std::fs::read(dir.join(&desc.heightmap))?;
        let resolution = desc.resolution as usize;
        anyhow::ensure!(bytes.len() == resolution * resolution * 2, "{} has the wrong size", desc.heightmap);
        let heights = bytes
            .chunks_exact(2)
            .map(|b| u16::from_le_bytes([b[0], b[1]]) as f32 * desc.height_scale)
            .collect();
        Ok(Self {
            resolution,
            spacing: desc.spacing,
            origin: Vec3::from_array(desc.origin),
            heights,
        })
    }

    fn height(&self, col: usize, row: usize) -> f32 {
        let n = self.resolution;
        self.heights[row.min(n - 1) * n + col.min(n - 1)]
    }

    /// Mesh of one cell (index row-major from -Z), positions relative to `origin`. Edges
    /// facing the playable terrain get a skirt hanging down, hiding cracks where the finer
    /// terrain meets this coarser one.
    fn cell_mesh(&self, cell: usize) -> Mesh {
        let per_cell = (self.resolution - 1) / 3;
        let (cell_col, cell_row) = (cell % 3, cell / 3);
        let (c0, r0) = (cell_col * per_cell, cell_row * per_cell);
        let steps = per_cell / SURROUNDING_STEP;
        let width = steps + 1;

        let mut positions = Vec::with_capacity(width * width + 2 * width);
        let mut normals = Vec::with_capacity(positions.capacity());
        let mut uvs = Vec::with_capacity(positions.capacity());
        let s = SURROUNDING_STEP;
        for j in 0..width {
            for i in 0..width {
                let (col, row) = (c0 + i * s, r0 + j * s);
                positions.push([col as f32 * self.spacing, self.height(col, row), row as f32 * self.spacing]);
                let dx = self.height(col + s, row) - self.height(col.saturating_sub(s), row);
                let dz = self.height(col, row + s) - self.height(col, row.saturating_sub(s));
                normals.push(Vec3::new(-dx, 2.0 * s as f32 * self.spacing, -dz).normalize().to_array());
                uvs.push([i as f32 / steps as f32, j as f32 / steps as f32]);
            }
        }
        let mut indices = Vec::with_capacity(steps * steps * 6);
        for j in 0..steps {
            for i in 0..steps {
                let a = (j * width + i) as u32;
                let (b, c, d) = (a + width as u32, a + 1, a + width as u32 + 1);
                indices.extend_from_slice(&[a, b, d, a, d, c]);
            }
        }
        // Skirt along the edge next to the middle cell (side cells only).
        let edge: Option<Vec<usize>> = match (cell_col, cell_row) {
            (1, 0) => Some(((steps * width)..(width * width)).collect()),
            (1, 2) => Some((0..width).collect()),
            (0, 1) => Some((0..width).map(|j| j * width + steps).collect()),
            (2, 1) => Some((0..width).map(|j| j * width).collect()),
            _ => None,
        };
        if let Some(edge) = edge {
            for pair in edge.windows(2) {
                let base = positions.len() as u32;
                for &v in pair {
                    let [x, y, z] = positions[v];
                    positions.push([x, y - 12.0, z]);
                    normals.push(normals[v]);
                    uvs.push(uvs[v]);
                }
                let (a, b) = (pair[0] as u32, pair[1] as u32);
                indices.extend_from_slice(&[a, base, b, b, base, base + 1]);
            }
        }
        Mesh::new(PrimitiveTopology::TriangleList, RenderAssetUsages::RENDER_WORLD)
            .with_inserted_attribute(Mesh::ATTRIBUTE_POSITION, positions)
            .with_inserted_attribute(Mesh::ATTRIBUTE_NORMAL, normals)
            .with_inserted_attribute(Mesh::ATTRIBUTE_UV_0, uvs)
            .with_inserted_indices(Indices::U32(indices))
    }
}

fn chunk_mesh(
    h: &Heightmap,
    xs: std::ops::RangeInclusive<u32>,
    zs: std::ops::RangeInclusive<u32>,
    step: u32,
    vertex_colors: bool,
) -> Mesh {
    let (x0, z0) = (*xs.start(), *zs.start());
    let (x1, z1) = (*xs.end(), *zs.end());
    let width = (x1 - x0) / step + 1;
    let depth = (z1 - z0) / step + 1;
    // Positions relative to the chunk centre; UVs span the chunk (one colour map patch).
    let center = Vec2::new((x0 + x1) as f32, (z0 + z1) as f32) * 0.5 * h.spacing;
    let inv_x = 1.0 / (x1 - x0).max(1) as f32;
    let inv_z = 1.0 / (z1 - z0).max(1) as f32;

    let vertex_count = (width * depth + 2 * (width + depth)) as usize;
    let mut positions = Vec::with_capacity(vertex_count);
    let mut normals = Vec::with_capacity(vertex_count);
    let mut uvs = Vec::with_capacity(vertex_count);
    let mut colors = Vec::new();
    for j in 0..depth {
        for i in 0..width {
            let (x, z) = (x0 + i * step, z0 + j * step);
            let height = h.sample(x, z);
            positions.push([
                x as f32 * h.spacing - center.x,
                height,
                z as f32 * h.spacing - center.y,
            ]);
            // Central differences over the whole map (at this LOD's spacing), so seams line up.
            let dx = h.sample(x + step, z) - h.sample(x.saturating_sub(step), z);
            let dz = h.sample(x, z + step) - h.sample(x, z.saturating_sub(step));
            let normal = Vec3::new(-dx, 2.0 * step as f32 * h.spacing, -dz).normalize();
            normals.push(normal.to_array());
            uvs.push([(x - x0) as f32 * inv_x, (z - z0) as f32 * inv_z]);
            if vertex_colors {
                colors.push(ground_color(normal, height));
            }
        }
    }

    let mut indices = Vec::with_capacity(((width - 1) * (depth - 1) * 6 + 12 * (width + depth)) as usize);
    for j in 0..depth - 1 {
        for i in 0..width - 1 {
            // Split along (x, z)-(x+1, z+1), matching physics and `Heightmap::height_at`.
            let v = j * width + i;
            let (a, b, c, d) = (v, v + width, v + 1, v + width + 1);
            indices.extend_from_slice(&[a, b, d, a, d, c]);
        }
    }

    // Skirts: each edge repeated lower, facing outwards.
    let edges: [Vec<u32>; 4] = [
        (0..width).rev().collect(),                             // north, walked west
        (0..width).map(|i| (depth - 1) * width + i).collect(), // south, walked east
        (0..depth).map(|j| j * width).collect(),               // west, walked south
        (0..depth).rev().map(|j| j * width + width - 1).collect(), // east, walked north
    ];
    for edge in edges {
        let base = positions.len() as u32;
        for &v in &edge {
            let [x, y, z] = positions[v as usize];
            positions.push([x, y - SKIRT_DEPTH, z]);
            normals.push(normals[v as usize]);
            uvs.push(uvs[v as usize]);
            if vertex_colors {
                colors.push(colors[v as usize]);
            }
        }
        for k in 0..edge.len() as u32 - 1 {
            let (a, b) = (edge[k as usize], edge[k as usize + 1]);
            let (a_low, b_low) = (base + k, base + k + 1);
            indices.extend_from_slice(&[a, a_low, b, b, a_low, b_low]);
        }
    }

    let mut mesh = Mesh::new(
        PrimitiveTopology::TriangleList,
        RenderAssetUsages::RENDER_WORLD,
    )
    .with_inserted_attribute(Mesh::ATTRIBUTE_POSITION, positions)
    .with_inserted_attribute(Mesh::ATTRIBUTE_NORMAL, normals)
    .with_inserted_attribute(Mesh::ATTRIBUTE_UV_0, uvs)
    .with_inserted_indices(Indices::U32(indices));
    if vertex_colors {
        mesh.insert_attribute(Mesh::ATTRIBUTE_COLOR, colors);
    }
    mesh
}

/// Grass on flat ground, dirt and rock on slopes.
fn ground_color(normal: Vec3, height: f32) -> [f32; 4] {
    let grass = LinearRgba::from(Color::srgb(0.30, 0.42, 0.18));
    let dry = LinearRgba::from(Color::srgb(0.47, 0.45, 0.26));
    let rock = LinearRgba::from(Color::srgb(0.42, 0.38, 0.33));
    let variation = ((height * 0.37).sin() * 0.5 + 0.5) * 0.6;
    let base = grass.mix(&dry, variation);
    let slope = ((1.0 - normal.y) * 5.0).clamp(0.0, 1.0);
    base.mix(&rock, slope).to_f32_array()
}
