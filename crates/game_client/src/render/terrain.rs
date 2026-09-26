//! Terrain rendering: the heightfield is cut into chunks so off-screen parts are culled.

use bevy::{asset::RenderAssetUsages, mesh::Indices, prelude::*, render::render_resource::PrimitiveTopology};
use game_shared::level::{Heightmap, LevelEntity, LoadedLevel, Terrain};

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
    let vertex_colored = materials.add(StandardMaterial {
        perceptual_roughness: 0.95,
        reflectance: 0.15,
        ..default()
    });
    let untextured = materials.add(StandardMaterial {
        base_color: Color::srgb(0.35, 0.32, 0.25),
        perceptual_roughness: 0.95,
        ..default()
    });

    for cz in (0..cells).step_by(chunk_cells as usize) {
        for cx in (0..cells).step_by(chunk_cells as usize) {
            let x1 = (cx + chunk_cells).min(cells);
            let z1 = (cz + chunk_cells).min(cells);
            let material = if tiles == 0 {
                vertex_colored.clone()
            } else {
                let index = (cz / chunk_cells) * tiles + cx / chunk_cells;
                match desc.and_then(|t| t.color_maps.get(index as usize)).filter(|p| !p.is_empty()) {
                    Some(path) => materials.add(StandardMaterial {
                        base_color_texture: Some(asset_server.load(format!(
                            "imported://levels/{}/{path}",
                            level.desc.name
                        ))),
                        perceptual_roughness: 0.95,
                        reflectance: 0.15,
                        ..default()
                    }),
                    None => untextured.clone(),
                }
            };
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
