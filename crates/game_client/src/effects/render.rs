//! Drawing sprite particles: one storage buffer of particles per texture and render layer,
//! drawn by chunk meshes whose quads the vertex shader places (see `particles.wgsl`). The
//! meshes never change, so nothing but the buffers is uploaded each frame.

use bevy::{
    asset::{RenderAssetUsages, embedded_asset},
    camera::visibility::{NoFrustumCulling, RenderLayers},
    light::{NotShadowCaster, NotShadowReceiver},
    mesh::{Indices, MeshVertexBufferLayoutRef, PrimitiveTopology},
    pbr::{Material, MaterialPipeline, MaterialPipelineKey, MaterialPlugin},
    platform::collections::HashMap,
    prelude::*,
    render::{
        render_resource::{AsBindGroup, RenderPipelineDescriptor, SpecializedMeshPipelineError},
        storage::ShaderBuffer,
    },
    shader::ShaderRef,
};

pub struct ParticleRenderPlugin;

impl Plugin for ParticleRenderPlugin {
    fn build(&self, app: &mut App) {
        embedded_asset!(app, "particles.wgsl");
        app.add_plugins(MaterialPlugin::<ParticleMaterial>::default())
            .init_resource::<Batches>();
    }
}

/// Quads per chunk mesh; a batch shows as many chunks as it has particles.
const CHUNK: usize = 2048;
/// Floats per particle in the storage buffer (see `Particle` in the shader).
const FLOATS: usize = 20;

#[derive(Asset, TypePath, AsBindGroup, Clone)]
pub struct ParticleMaterial {
    #[storage(0, read_only)]
    particles: Handle<ShaderBuffer>,
    #[texture(1)]
    #[sampler(2)]
    texture: Handle<Image>,
}

impl Material for ParticleMaterial {
    fn vertex_shader() -> ShaderRef {
        "embedded://client/effects/particles.wgsl".into()
    }

    fn fragment_shader() -> ShaderRef {
        "embedded://client/effects/particles.wgsl".into()
    }

    fn alpha_mode(&self) -> AlphaMode {
        AlphaMode::Premultiplied
    }

    fn enable_prepass() -> bool {
        false
    }

    fn enable_shadows() -> bool {
        false
    }

    fn specialize(
        _pipeline: &MaterialPipeline,
        descriptor: &mut RenderPipelineDescriptor,
        _layout: &MeshVertexBufferLayoutRef,
        _key: MaterialPipelineKey<Self>,
    ) -> Result<(), SpecializedMeshPipelineError> {
        descriptor.primitive.cull_mode = None;
        Ok(())
    }
}

/// One particle as the shader reads it.
#[derive(Clone, Copy, Default)]
pub struct GpuParticle {
    pub position: Vec3,
    /// Half the width.
    pub size: f32,
    pub axis: Vec3,
    pub rotation: f32,
    pub color: Vec4,
    pub uv: Vec4,
    /// Mode (0 camera, 1 velocity, 2 horizontal), length / width, additive, soft distance.
    pub params: Vec4,
}

impl GpuParticle {
    fn write(&self, out: &mut Vec<u8>) {
        let floats: [f32; FLOATS] = [
            self.position.x,
            self.position.y,
            self.position.z,
            self.size,
            self.axis.x,
            self.axis.y,
            self.axis.z,
            self.rotation,
            self.color.x,
            self.color.y,
            self.color.z,
            self.color.w,
            self.uv.x,
            self.uv.y,
            self.uv.z,
            self.uv.w,
            self.params.x,
            self.params.y,
            self.params.z,
            self.params.w,
        ];
        for value in floats {
            out.extend_from_slice(&value.to_le_bytes());
        }
    }
}

/// What a frame draws: particles by texture and render layer, collected by the simulation.
#[derive(Resource, Default)]
pub struct Batches {
    batches: HashMap<(AssetId<Image>, usize), Batch>,
    chunk_meshes: Vec<Handle<Mesh>>,
}

struct Batch {
    texture: Handle<Image>,
    layer: usize,
    buffer: Option<Handle<ShaderBuffer>>,
    material: Option<Handle<ParticleMaterial>>,
    /// Particles the buffer holds.
    capacity: usize,
    chunks: Vec<Entity>,
    particles: Vec<GpuParticle>,
    /// Distance along the view of each particle, to sort far to near.
    depths: Vec<(f32, u32)>,
    shown: usize,
}

impl Batches {
    /// Queues a particle for this frame.
    pub fn push(&mut self, texture: &Handle<Image>, layer: usize, depth: f32, particle: GpuParticle) {
        let batch = self.batches.entry((texture.id(), layer)).or_insert_with(|| Batch {
            texture: texture.clone(),
            layer,
            buffer: None,
            material: None,
            capacity: 0,
            chunks: Vec::new(),
            particles: Vec::new(),
            depths: Vec::new(),
            shown: 0,
        });
        batch.depths.push((depth, batch.particles.len() as u32));
        batch.particles.push(particle);
    }

    pub fn particle_count(&self) -> usize {
        self.batches.values().map(|b| b.particles.len()).sum()
    }
}

/// Uploads the queued particles, sorted far to near, and shows the chunks they need.
#[allow(clippy::too_many_arguments)]
pub fn upload(
    mut commands: Commands,
    mut batches: ResMut<Batches>,
    mut meshes: ResMut<Assets<Mesh>>,
    mut buffers: ResMut<Assets<ShaderBuffer>>,
    mut materials: ResMut<Assets<ParticleMaterial>>,
    camera: Query<&GlobalTransform, With<crate::camera::PlayerCamera>>,
    mut chunks: Query<(&mut Visibility, &mut Transform)>,
) {
    let (camera_position, forward) = camera
        .single()
        .map_or((Vec3::ZERO, Vec3::NEG_Z), |c| (c.translation(), c.forward().as_vec3()));
    let Batches { batches, chunk_meshes } = &mut *batches;
    for batch in batches.values_mut() {
        let count = batch.particles.len();
        if count == 0 && batch.shown == 0 {
            continue;
        }
        if count > batch.capacity {
            batch.capacity = count.next_power_of_two().max(256);
            let buffer = buffers.add(ShaderBuffer::new(
                &vec![0; batch.capacity * FLOATS * 4],
                RenderAssetUsages::default(),
            ));
            match &batch.material {
                Some(material) => {
                    if let Some(mut material) = materials.get_mut(material) {
                        material.particles = buffer.clone();
                    }
                }
                None => {
                    batch.material = Some(materials.add(ParticleMaterial {
                        particles: buffer.clone(),
                        texture: batch.texture.clone(),
                    }))
                }
            }
            batch.buffer = Some(buffer);
        }
        let (Some(buffer), Some(material)) = (&batch.buffer, &batch.material) else {
            continue;
        };

        // Far to near, so alpha blending comes out right.
        batch.depths.sort_unstable_by(|a, b| b.0.total_cmp(&a.0));
        let mut data = Vec::with_capacity(batch.capacity * FLOATS * 4);
        for &(_, index) in &batch.depths {
            batch.particles[index as usize].write(&mut data);
        }
        data.resize(batch.capacity * FLOATS * 4, 0);
        if let Some(mut buffer) = buffers.get_mut(buffer) {
            buffer.data = Some(data);
        }

        // Chunks draw in order: each a little nearer the camera than the one before, at the
        // batch's center so batches sort among themselves.
        let needed = count.div_ceil(CHUNK);
        while chunk_meshes.len() < needed {
            chunk_meshes.push(meshes.add(chunk_mesh(chunk_meshes.len())));
        }
        while batch.chunks.len() < needed {
            let index = batch.chunks.len();
            batch.chunks.push(
                commands
                    .spawn((
                        Mesh3d(chunk_meshes[index].clone()),
                        MeshMaterial3d(material.clone()),
                        Transform::default(),
                        Visibility::Hidden,
                        NoFrustumCulling,
                        NotShadowCaster,
                        NotShadowReceiver,
                        RenderLayers::layer(batch.layer),
                    ))
                    .id(),
            );
        }
        let center = if count > 0 {
            batch.depths.iter().map(|d| d.0).sum::<f32>() / count as f32
        } else {
            0.0
        };
        for (index, &chunk) in batch.chunks.iter().enumerate() {
            let Ok((mut visibility, mut transform)) = chunks.get_mut(chunk) else {
                continue;
            };
            let visible = index < needed;
            visibility.set_if_neq(if visible { Visibility::Visible } else { Visibility::Hidden });
            if visible {
                transform.translation = camera_position + forward * (center - index as f32 * 0.01);
            }
        }
        batch.shown = count;
        batch.particles.clear();
        batch.depths.clear();
    }
}

/// Quads `first * CHUNK ..` as `(particle, corner u, corner v)` positions.
fn chunk_mesh(chunk: usize) -> Mesh {
    let first = chunk * CHUNK;
    let mut positions = Vec::with_capacity(CHUNK * 4);
    let mut indices = Vec::with_capacity(CHUNK * 6);
    for quad in 0..CHUNK {
        let index = (first + quad) as f32;
        let base = (quad * 4) as u32;
        positions.extend([[index, 0.0, 0.0], [index, 1.0, 0.0], [index, 1.0, 1.0], [index, 0.0, 1.0]]);
        indices.extend([base, base + 1, base + 2, base, base + 2, base + 3]);
    }
    Mesh::new(PrimitiveTopology::TriangleList, RenderAssetUsages::RENDER_WORLD)
        .with_inserted_attribute(Mesh::ATTRIBUTE_POSITION, positions)
        .with_inserted_indices(Indices::U32(indices))
}
