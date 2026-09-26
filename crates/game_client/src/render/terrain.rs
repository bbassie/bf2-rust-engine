//! Terrain rendering: the heightfield is cut into chunks so off-screen parts are culled.

use bevy::{
    asset::RenderAssetUsages,
    image::{ImageLoaderSettings, ImageSampler},
    mesh::Indices,
    prelude::*,
    render::render_resource::PrimitiveTopology,
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

    for cz in (0..cells).step_by(chunk_cells as usize) {
        for cx in (0..cells).step_by(chunk_cells as usize) {
            let x1 = (cx + chunk_cells).min(cells);
            let z1 = (cz + chunk_cells).min(cells);
            let index = ((cz / chunk_cells) * tiles.max(1) + cx / chunk_cells) as usize;
            let color_map = desc
                .and_then(|t| t.color_maps.get(index))
                .and_then(|p| load_patch(p, true));
            let weights = desc.and_then(|t| t.detail_weights.get(index));
            let mut extension = TerrainLayers {
                params,
                weights_a: weights.and_then(|w| load_patch(&w[0], false)),
                weights_b: weights.and_then(|w| load_patch(&w[1], false)),
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
            let mesh = chunk_mesh(heightmap, cx..=x1, cz..=z1, tiles == 0);
            commands.spawn((
                TerrainChunk,
                LevelEntity,
                Mesh3d(meshes.add(mesh)),
                MeshMaterial3d(material),
                Transform::from_translation(Vec3::new(
                    heightmap.origin.x + cx as f32 * heightmap.spacing,
                    heightmap.origin.y,
                    heightmap.origin.z + cz as f32 * heightmap.spacing,
                )),
            ));
        }
    }

    if let Some(water) = &level.desc.water {
        let size = heightmap.world_size() * 3.0;
        let [r, g, b, a] = water.color;
        commands.spawn((
            TerrainChunk,
            LevelEntity,
            Mesh3d(meshes.add(Plane3d::new(Vec3::Y, Vec2::splat(size * 0.5)))),
            MeshMaterial3d(materials.add(StandardMaterial {
                base_color: Color::srgba(r, g, b, a),
                alpha_mode: AlphaMode::Blend,
                perceptual_roughness: 0.1,
                reflectance: 0.6,
                ..default()
            })),
            Transform::from_translation(heightmap.center().with_y(water.height)),
        ));
    }
}

fn chunk_mesh(
    h: &Heightmap,
    xs: std::ops::RangeInclusive<u32>,
    zs: std::ops::RangeInclusive<u32>,
    vertex_colors: bool,
) -> Mesh {
    let (x0, z0) = (*xs.start(), *zs.start());
    let width = xs.end() - x0 + 1;
    let depth = zs.end() - z0 + 1;
    // UVs span the chunk, which is one color map patch on imported levels.
    let inv_x = 1.0 / (width - 1).max(1) as f32;
    let inv_z = 1.0 / (depth - 1).max(1) as f32;

    let mut positions = Vec::with_capacity((width * depth) as usize);
    let mut normals = Vec::with_capacity(positions.capacity());
    let mut uvs = Vec::with_capacity(positions.capacity());
    let mut colors = Vec::new();
    for z in zs.clone() {
        for x in xs.clone() {
            let height = h.sample(x, z);
            positions.push([
                (x - x0) as f32 * h.spacing,
                height,
                (z - z0) as f32 * h.spacing,
            ]);
            // Central differences over the whole map, so chunk seams line up.
            let dx = h.sample(x + 1, z) - h.sample(x.saturating_sub(1), z);
            let dz = h.sample(x, z + 1) - h.sample(x, z.saturating_sub(1));
            let normal = Vec3::new(-dx, 2.0 * h.spacing, -dz).normalize();
            normals.push(normal.to_array());
            uvs.push([(x - x0) as f32 * inv_x, (z - z0) as f32 * inv_z]);
            if vertex_colors {
                colors.push(ground_color(normal, height));
            }
        }
    }

    let mut indices = Vec::with_capacity(((width - 1) * (depth - 1) * 6) as usize);
    for z in 0..depth - 1 {
        for x in 0..width - 1 {
            // Split along (x, z)-(x+1, z+1), matching physics and `Heightmap::height_at`.
            let i = z * width + x;
            let (a, b, c, d) = (i, i + width, i + 1, i + width + 1);
            indices.extend_from_slice(&[a, b, d, a, d, c]);
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
