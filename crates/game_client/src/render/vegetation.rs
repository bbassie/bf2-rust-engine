//! Vegetation: undergrowth (grass and small plants) generated in chunks around the camera,
//! and draw-only overgrowth trees.
//!
//! Each undergrowth chunk is one mesh with every plant baked in (placed on the terrain by the
//! level's material map), built on a background thread. The shader sways, thins and fades the
//! plants, so a chunk never needs rebuilding while it stays in range.

use std::sync::Arc;

use bevy::{
    asset::RenderAssetUsages,
    image::{ImageLoaderSettings, ImageSampler},
    light::NotShadowCaster,
    mesh::{Indices, MeshVertexBufferLayoutRef},
    pbr::{MaterialPipeline, MaterialPipelineKey},
    platform::collections::HashMap,
    prelude::*,
    render::render_resource::{
        AsBindGroup, PrimitiveTopology, RenderPipelineDescriptor, ShaderType, SpecializedMeshPipelineError,
    },
    shader::ShaderRef,
    tasks::{AsyncComputeTaskPool, Task, futures::check_ready},
};
use game_data::{UndergrowthDesc, VegetationDesc};
use game_shared::{
    level::{Heightmap, LevelEntity, LoadedLevel},
    statics::StaticMesh,
};

use super::materials::clamped_sampler;
use crate::camera::PlayerCamera;

pub struct VegetationRenderPlugin;

impl Plugin for VegetationRenderPlugin {
    fn build(&self, app: &mut App) {
        embedded_shader!(app, "shaders/undergrowth.wgsl");
        app.add_plugins(MaterialPlugin::<UndergrowthMaterial>::default())
            .add_systems(
                Update,
                (
                    load_vegetation.run_if(resource_exists_and_changed::<LoadedLevel>),
                    // Undergrowth outlives the level when a match is left.
                    stream_undergrowth
                        .run_if(resource_exists::<Undergrowth>)
                        .run_if(resource_exists::<LoadedLevel>),
                    clear_undergrowth.run_if(resource_removed::<LoadedLevel>),
                )
                    .chain(),
            );
    }
}

/// Chunk side in meters.
const CHUNK_SIZE: f32 = 16.0;
/// Plants are drawn this much further than the level asks for (BF2's 30..75 m were tuned
/// for 2005 hardware).
const DISTANCE_SCALE: f32 = 1.25;
const MAX_DISTANCE: f32 = 60.0;
/// Fraction of the plants still drawn where they start to fade out.
const KEEP_AT_FADE: f32 = 0.35;
/// Chunk builds started per frame.
const BUILDS_PER_FRAME: usize = 4;

#[derive(Asset, AsBindGroup, TypePath, Debug, Clone)]
pub struct UndergrowthMaterial {
    #[uniform(0)]
    params: UndergrowthParams,
    #[texture(1)]
    #[sampler(2)]
    atlas: Handle<Image>,
    #[texture(3)]
    #[sampler(4)]
    ground: Option<Handle<Image>>,
    /// The terrain patch's lightmap: its sky visibility occludes the plants' ambient light.
    #[texture(5)]
    #[sampler(6)]
    lightmap: Option<Handle<Image>>,
    alpha_cutoff: f32,
}

#[derive(ShaderType, Debug, Clone, Copy)]
struct UndergrowthParams {
    ground_rect: Vec4,
    distances: Vec4,
    wind: Vec4,
    /// Lit like the terrain (`environment::LightScale::uniforms`).
    light_sun: Vec4,
    light_ambient: Vec4,
    /// Baked sky occlusion (`materials::BakedSky::uniform`; strength 0 without a lightmap).
    sky: Vec4,
}

impl Material for UndergrowthMaterial {
    fn vertex_shader() -> ShaderRef {
        "embedded://client/render/shaders/undergrowth.wgsl".into()
    }

    fn fragment_shader() -> ShaderRef {
        "embedded://client/render/shaders/undergrowth.wgsl".into()
    }

    fn alpha_mode(&self) -> AlphaMode {
        AlphaMode::Mask(self.alpha_cutoff)
    }

    // Swaying geometry would have to match exactly in every pass; grass is cheaper without.
    fn enable_prepass() -> bool {
        false
    }

    fn enable_shadows() -> bool {
        false
    }

    fn specialize(
        _pipeline: &MaterialPipeline,
        descriptor: &mut RenderPipelineDescriptor,
        layout: &MeshVertexBufferLayoutRef,
        _key: MaterialPipelineKey<Self>,
    ) -> Result<(), SpecializedMeshPipelineError> {
        let vertex_layout = layout.0.get_layout(&[
            Mesh::ATTRIBUTE_POSITION.at_shader_location(0),
            Mesh::ATTRIBUTE_NORMAL.at_shader_location(1),
            Mesh::ATTRIBUTE_UV_0.at_shader_location(2),
            Mesh::ATTRIBUTE_UV_1.at_shader_location(3),
            Mesh::ATTRIBUTE_COLOR.at_shader_location(4),
        ])?;
        descriptor.vertex.buffers = vec![vertex_layout];
        descriptor.primitive.cull_mode = None;
        Ok(())
    }
}

/// Undergrowth of the loaded level.
#[derive(Resource)]
struct Undergrowth {
    data: Arc<PlantData>,
    view_distance: f32,
    atlas: Handle<Image>,
    /// One material per terrain colour map patch, created on first use.
    materials: HashMap<IVec2, Handle<UndergrowthMaterial>>,
    /// Chunks in range; `None` for chunks without plants.
    chunks: HashMap<IVec2, Option<Entity>>,
    building: HashMap<IVec2, Task<Option<Mesh>>>,
}

/// Everything a chunk build needs, shared with the build threads.
struct PlantData {
    desc: UndergrowthDesc,
    /// Material id per sample, row 0 at -Z.
    map: Vec<u8>,
    map_size: usize,
    /// Index into `desc.materials` per material id.
    lookup: [Option<u8>; 256],
    heightmap: Arc<Heightmap>,
}

#[derive(Component)]
struct UndergrowthChunk;

/// Leaving a match removes the level: its undergrowth goes too (chunks, and the plants that
/// would otherwise be streamed in again for the old level).
fn clear_undergrowth(mut commands: Commands, chunks: Query<Entity, With<UndergrowthChunk>>) {
    commands.remove_resource::<Undergrowth>();
    for chunk in &chunks {
        commands.entity(chunk).try_despawn();
    }
}

fn load_vegetation(
    mut commands: Commands,
    level: Res<LoadedLevel>,
    old_chunks: Query<Entity, With<UndergrowthChunk>>,
    asset_server: Res<AssetServer>,
    settings: Res<crate::settings::Settings>,
) {
    commands.remove_resource::<Undergrowth>();
    for chunk in &old_chunks {
        commands.entity(chunk).despawn();
    }
    let (Some(dir), Some(file)) = (&level.dir, &level.desc.vegetation) else {
        return;
    };
    let vegetation: VegetationDesc = match game_data::read_ron(dir.join(file)) {
        Ok(v) => v,
        Err(err) => {
            warn!("vegetation: {err}");
            return;
        }
    };

    // Trees are ordinary static meshes without collision.
    for tree in &vegetation.overgrowth {
        for placement in &tree.instances {
            commands.spawn((
                LevelEntity,
                game_shared::level::placement_transform(placement),
                StaticMesh {
                    path: tree.mesh.clone(),
                    index: 0,
                },
            ));
        }
    }

    let (Some(desc), Some(heightmap)) = (vegetation.undergrowth, level.heightmap.clone()) else {
        return;
    };
    let map = match std::fs::read(dir.join(&desc.material_map)) {
        Ok(map) => map,
        Err(err) => {
            warn!("undergrowth map: {err}");
            return;
        }
    };
    let map_size = (map.len() as f64).sqrt() as usize;
    if map_size < 2 || map_size * map_size != map.len() {
        warn!("undergrowth map is not square");
        return;
    }
    let mut lookup = [None; 256];
    for (i, material) in desc.materials.iter().enumerate() {
        lookup[material.id as usize] = Some(i as u8);
    }
    let atlas = asset_server.load(format!("imported://levels/{}/{}", level.desc.name, desc.atlas));
    // The `vegetation_density` graphics setting: lower presets draw undergrowth over a
    // shorter radius, thinning how much of it is visible at once (`stream_undergrowth` keeps
    // this current as the setting changes live).
    let view_distance = (desc.view_distance * DISTANCE_SCALE * settings.vegetation_density).min(MAX_DISTANCE);
    commands.insert_resource(Undergrowth {
        data: Arc::new(PlantData {
            desc,
            map,
            map_size,
            lookup,
            heightmap,
        }),
        view_distance,
        atlas,
        materials: HashMap::default(),
        chunks: HashMap::default(),
        building: HashMap::default(),
    });
}

/// Spawns chunks coming into range, despawns those leaving it.
fn stream_undergrowth(
    mut commands: Commands,
    mut undergrowth: ResMut<Undergrowth>,
    level: Res<LoadedLevel>,
    camera: Query<&GlobalTransform, With<PlayerCamera>>,
    asset_server: Res<AssetServer>,
    mut meshes: ResMut<Assets<Mesh>>,
    mut materials: ResMut<Assets<UndergrowthMaterial>>,
    settings: Res<crate::settings::Settings>,
) {
    if settings.is_changed() {
        // The baked sky occlusion setting.
        let sky = super::materials::BakedSky::uniform(settings.baked_ao);
        for (_, material) in materials.iter_mut() {
            if material.lightmap.is_some() && material.params.sky != sky {
                material.params.sky = sky;
            }
        }
        // The vegetation density setting, live (see `load_vegetation`'s initial value).
        let wanted = (undergrowth.data.desc.view_distance * DISTANCE_SCALE * settings.vegetation_density).min(MAX_DISTANCE);
        undergrowth.view_distance = wanted;
    }
    let Ok(camera) = camera.single() else {
        return;
    };
    let eye = camera.translation();
    let undergrowth = &mut *undergrowth;
    let radius = undergrowth.view_distance + CHUNK_SIZE * 0.75;
    let in_range = |key: IVec2| {
        let center = (key.as_vec2() + 0.5) * CHUNK_SIZE;
        center.distance(eye.xz()) < radius
    };

    // Finished builds.
    let finished: Vec<(IVec2, Option<Mesh>)> = undergrowth
        .building
        .iter_mut()
        .filter_map(|(key, task)| check_ready(task).map(|mesh| (*key, mesh)))
        .collect();
    for (key, mesh) in finished {
        undergrowth.building.remove(&key);
        if !in_range(key) {
            continue;
        }
        let entity = mesh.map(|mesh| {
            let material = patch_material(undergrowth, &level, key, &asset_server, &mut materials, settings.baked_ao);
            commands
                .spawn((
                    UndergrowthChunk,
                    LevelEntity,
                    Mesh3d(meshes.add(mesh)),
                    MeshMaterial3d(material),
                    Transform::from_translation(Vec3::new(key.x as f32 * CHUNK_SIZE, 0.0, key.y as f32 * CHUNK_SIZE)),
                    NotShadowCaster,
                ))
                .id()
        });
        undergrowth.chunks.insert(key, entity);
    }

    // Chunks out of range.
    undergrowth.chunks.retain(|key, entity| {
        let keep = in_range(*key);
        if let (false, Some(entity)) = (keep, entity) {
            commands.entity(*entity).despawn();
        }
        keep
    });

    // New chunks, nearest first.
    let reach = (radius / CHUNK_SIZE).ceil() as i32;
    let center = (eye.xz() / CHUNK_SIZE).floor().as_ivec2();
    let mut wanted: Vec<IVec2> = (-reach..=reach)
        .flat_map(|z| (-reach..=reach).map(move |x| center + IVec2::new(x, z)))
        .filter(|key| in_range(*key))
        .filter(|key| !undergrowth.chunks.contains_key(key) && !undergrowth.building.contains_key(key))
        .collect();
    wanted.sort_by_key(|key| (key.as_vec2() + 0.5 - eye.xz() / CHUNK_SIZE).length_squared() as i32);
    let pool = AsyncComputeTaskPool::get();
    for key in wanted.into_iter().take(BUILDS_PER_FRAME) {
        let data = undergrowth.data.clone();
        undergrowth.building.insert(key, pool.spawn(async move { build_chunk(&data, key) }));
    }
}

/// The material for the terrain patch a chunk lies in (its colour map tints the plants).
fn patch_material(
    undergrowth: &mut Undergrowth,
    level: &LoadedLevel,
    chunk: IVec2,
    asset_server: &AssetServer,
    materials: &mut Assets<UndergrowthMaterial>,
    baked_ao: bool,
) -> Handle<UndergrowthMaterial> {
    let heightmap = &undergrowth.data.heightmap;
    let terrain = level.desc.terrain.as_ref();
    let tiles = terrain.map_or(1, |t| t.color_map_tiles.max(1)) as i32;
    let patch_size = heightmap.world_size() / tiles as f32;
    let center = (chunk.as_vec2() + 0.5) * CHUNK_SIZE;
    let patch = ((center - heightmap.origin.xz()) / patch_size)
        .floor()
        .as_ivec2()
        .clamp(IVec2::ZERO, IVec2::splat(tiles - 1));
    if let Some(material) = undergrowth.materials.get(&patch) {
        return material.clone();
    }
    let patch_image = |paths: Option<&Vec<String>>, srgb: bool| {
        paths
            .and_then(|p| p.get((patch.y * tiles + patch.x) as usize))
            .filter(|p| !p.is_empty())
            .map(|path| {
                asset_server
                    .load_builder()
                    .with_settings(move |s: &mut ImageLoaderSettings| {
                        s.is_srgb = srgb;
                        s.sampler = ImageSampler::Descriptor(clamped_sampler());
                    })
                    .load(format!("imported://levels/{}/{path}", level.desc.name))
            })
    };
    let ground = patch_image(terrain.map(|t| &t.color_maps), true);
    let lightmap = patch_image(terrain.map(|t| &t.lightmaps), false);
    let desc = &undergrowth.data.desc;
    let corner = heightmap.origin.xz() + patch.as_vec2() * patch_size;
    let fade_start = undergrowth.view_distance * (1.0 - desc.fade.clamp(0.05, 1.0));
    let wind = Vec2::new(0.8, 0.6);
    // BF2 lit undergrowth with the terrain's light.
    let light = super::environment::LevelLight::new(&level.desc.environment);
    let (light_sun, light_ambient) = light.terrain.uniforms();
    let material = materials.add(UndergrowthMaterial {
        params: UndergrowthParams {
            ground_rect: Vec4::new(corner.x, corner.y, patch_size, patch_size),
            distances: Vec4::new(
                undergrowth.view_distance,
                fade_start,
                desc.sway,
                // Without a colour map the plants aren't darkened by the ground colour.
                if ground.is_some() { desc.brightness } else { 1.0 },
            ),
            wind: Vec4::new(wind.x, wind.y, desc.alpha_cutoff, KEEP_AT_FADE),
            light_sun,
            light_ambient,
            sky: if lightmap.is_some() { super::materials::BakedSky::uniform(baked_ao) } else { Vec4::ZERO },
        },
        atlas: undergrowth.atlas.clone(),
        ground,
        lightmap,
        alpha_cutoff: desc.alpha_cutoff,
    });
    undergrowth.materials.insert(patch, material.clone());
    material
}

/// Bakes the plants of one chunk into a mesh (positions relative to the chunk corner).
fn build_chunk(data: &PlantData, key: IVec2) -> Option<Mesh> {
    let heightmap = &data.heightmap;
    let corner = key.as_vec2() * CHUNK_SIZE;
    let map_spacing = heightmap.world_size() / (data.map_size - 1) as f32;
    let origin = heightmap.origin.xz();
    let material_at = |p: Vec2| -> Option<&game_data::UndergrowthMaterial> {
        let cell = ((p - origin) / map_spacing).round();
        let max = (data.map_size - 1) as f32;
        if cell.x < 0.0 || cell.y < 0.0 || cell.x > max || cell.y > max {
            return None;
        }
        let id = data.map[cell.y as usize * data.map_size + cell.x as usize];
        data.lookup[id as usize].map(|i| &data.desc.materials[i as usize])
    };

    let mut positions: Vec<[f32; 3]> = Vec::new();
    let mut normals: Vec<[f32; 3]> = Vec::new();
    let mut uvs: Vec<[f32; 2]> = Vec::new();
    let mut sway: Vec<[f32; 2]> = Vec::new();
    let mut plant: Vec<[f32; 4]> = Vec::new();
    let mut indices: Vec<u32> = Vec::new();

    // Plants are scattered per 2 m cell, seeded by the cell, so chunks rebuild identically.
    const CELL: f32 = 2.0;
    let cells = (CHUNK_SIZE / CELL) as i32;
    for cz in 0..cells {
        for cx in 0..cells {
            let cell_corner = corner + Vec2::new(cx as f32, cz as f32) * CELL;
            let world_cell = (cell_corner / CELL).floor().as_ivec2();
            let mut rng = Rng::new(world_cell);
            let Some(material) = material_at(cell_corner + CELL * 0.5) else {
                continue;
            };
            for (type_index, plant_type) in material.types.iter().enumerate() {
                let expected = plant_type.density * CELL * CELL;
                let count = expected.floor() as u32 + u32::from(rng.next() < expected.fract());
                for _ in 0..count {
                    let p = cell_corner + Vec2::new(rng.next(), rng.next()) * CELL;
                    let (r_yaw, r_scale, r_thin, r_light) = (rng.next(), rng.next(), rng.next(), rng.next());
                    let (r_tilt_x, r_tilt_z) = (rng.next() - 0.5, rng.next() - 0.5);
                    // The material under the plant itself decides, not the cell's.
                    if !std::ptr::eq(material_at(p).unwrap_or(material), material)
                        || !patchy(p, type_index, plant_type.variation)
                    {
                        continue;
                    }
                    let scale = plant_type.scale[0] + (plant_type.scale[1] - plant_type.scale[0]) * r_scale;
                    let mut rotation = Quat::from_rotation_y(r_yaw * std::f32::consts::TAU);
                    if plant_type.skew {
                        rotation = Quat::from_rotation_x(r_tilt_x * 0.6) * Quat::from_rotation_z(r_tilt_z * 0.6) * rotation;
                    }
                    let root_height = heightmap.height_at(p.x, p.y);
                    let normal = terrain_normal(heightmap, p);
                    let base = positions.len() as u32;
                    let mesh = &plant_type.mesh;
                    for (i, &local) in mesh.positions.iter().enumerate() {
                        let offset = rotation * (Vec3::from_array(local) * scale);
                        let (x, z) = (p.x + offset.x, p.y + offset.z);
                        // Follow the ground under every vertex, so wide tufts hug slopes.
                        let ground = heightmap.height_at(x, z).min(root_height + 0.5);
                        positions.push([x - corner.x, ground + offset.y, z - corner.y]);
                        normals.push(normal.to_array());
                        uvs.push(mesh.uvs[i]);
                        sway.push([mesh.sway[i], offset.y.max(0.0)]);
                        plant.push([plant_type.ground_tint, r_thin, 0.85 + 0.3 * r_light, 0.0]);
                    }
                    indices.extend(mesh.indices.iter().map(|&i| base + i as u32));
                }
            }
        }
    }
    if indices.is_empty() {
        return None;
    }
    Some(
        Mesh::new(PrimitiveTopology::TriangleList, RenderAssetUsages::RENDER_WORLD)
            .with_inserted_attribute(Mesh::ATTRIBUTE_POSITION, positions)
            .with_inserted_attribute(Mesh::ATTRIBUTE_NORMAL, normals)
            .with_inserted_attribute(Mesh::ATTRIBUTE_UV_0, uvs)
            .with_inserted_attribute(Mesh::ATTRIBUTE_UV_1, sway)
            .with_inserted_attribute(Mesh::ATTRIBUTE_COLOR, plant)
            .with_inserted_indices(Indices::U32(indices)),
    )
}

/// Whether a plant with this `variation` grows at `p`: 0 everywhere, 1 only in the patches of
/// a noise pattern, -1 only in its gaps.
fn patchy(p: Vec2, type_index: usize, variation: f32) -> bool {
    if variation == 0.0 {
        return true;
    }
    let offset = type_index as f32 * 17.3;
    let n = value_noise(p * 0.09 + offset) * 0.65 + value_noise(p * 0.27 - offset) * 0.35;
    let n = if variation < 0.0 { 1.0 - n } else { n };
    // Keep probability falls from 1 to 0 across the pattern as variation grows.
    let threshold = variation.abs().min(1.0) * 0.7;
    n > threshold * (1.0 - hash(p * 7.31) * 0.3)
}

fn terrain_normal(heightmap: &Heightmap, p: Vec2) -> Vec3 {
    let d = 1.0;
    let dx = heightmap.height_at(p.x + d, p.y) - heightmap.height_at(p.x - d, p.y);
    let dz = heightmap.height_at(p.x, p.y + d) - heightmap.height_at(p.x, p.y - d);
    Vec3::new(-dx, 2.0 * d, -dz).normalize()
}

fn hash(p: Vec2) -> f32 {
    let h = p.dot(Vec2::new(127.1, 311.7)).sin() * 43_758.547;
    h - h.floor()
}

/// Smooth value noise in 0..1.
fn value_noise(p: Vec2) -> f32 {
    let i = p.floor();
    let f = p - i;
    let u = f * f * (Vec2::splat(3.0) - 2.0 * f);
    let a = hash(i);
    let b = hash(i + Vec2::X);
    let c = hash(i + Vec2::Y);
    let d = hash(i + Vec2::ONE);
    a + (b - a) * u.x + (c - a) * u.y + (a - b - c + d) * u.x * u.y
}

/// Per-cell deterministic random numbers (xorshift).
struct Rng(u32);

impl Rng {
    fn new(cell: IVec2) -> Self {
        // Murmur3 finalizer, so neighbouring cells get unrelated sequences.
        let mut h = (cell.x as u32).wrapping_mul(0x8DA6_B343) ^ (cell.y as u32).wrapping_mul(0xD816_3841);
        h ^= h >> 16;
        h = h.wrapping_mul(0x85EB_CA6B);
        h ^= h >> 13;
        h = h.wrapping_mul(0xC2B2_AE35);
        h ^= h >> 16;
        Self(h | 1)
    }

    /// Uniform in [0, 1).
    fn next(&mut self) -> f32 {
        self.0 ^= self.0 << 13;
        self.0 ^= self.0 >> 17;
        self.0 ^= self.0 << 5;
        (self.0 >> 8) as f32 / (1u32 << 24) as f32
    }
}
