//! Sun, sky, ambient light and fog from the level's environment settings.

use bevy::{
    asset::{LoadState, embedded_asset},
    gltf::{GltfAssetLabel, GltfMesh},
    light::{CascadeShadowConfigBuilder, NotShadowCaster, NotShadowReceiver},
    mesh::MeshVertexBufferLayoutRef,
    pbr::{MaterialPipeline, MaterialPipelineKey},
    prelude::*,
    render::render_resource::{AsBindGroup, RenderPipelineDescriptor, ShaderType, SpecializedMeshPipelineError},
    shader::ShaderRef,
};
use game_shared::level::LoadedLevel;

use crate::camera::PlayerCamera;

pub struct EnvironmentPlugin;

impl Plugin for EnvironmentPlugin {
    fn build(&self, app: &mut App) {
        embedded_asset!(app, "shaders/sky.wgsl");
        app.insert_resource(ClearColor(Color::srgb(0.55, 0.68, 0.85)))
            .add_plugins(MaterialPlugin::<SkyMaterial>::default())
            .add_systems(
                Update,
                (
                    apply_environment.run_if(resource_exists_and_changed::<LoadedLevel>),
                    spawn_sky_mesh,
                ),
            )
            .add_systems(
                PostUpdate,
                follow_camera
                    .after(crate::camera::CameraSystems)
                    .before(TransformSystems::Propagate),
            );
    }
}

/// Illuminance (lux) of the sun.
const SUN_ILLUMINANCE: f32 = 12_000.0;

/// The level's sun (lights the world layer only).
#[derive(Component)]
pub struct Sun;

/// The sky dome; it moves with the camera so it always surrounds it.
#[derive(Component)]
struct SkyDome {
    mesh: Handle<GltfMesh>,
    material: Handle<SkyMaterial>,
    spawned: bool,
}

fn apply_environment(
    mut commands: Commands,
    level: Res<LoadedLevel>,
    suns: Query<Entity, With<Sun>>,
    mut clear: ResMut<ClearColor>,
    mut ambient: ResMut<GlobalAmbientLight>,
    skies: Query<Entity, With<SkyDome>>,
    mut cameras: Query<(&mut DistanceFog, &mut Projection), With<PlayerCamera>>,
    asset_server: Res<AssetServer>,
    mut sky_materials: ResMut<Assets<SkyMaterial>>,
    cli: Res<crate::Cli>,
) {
    let env = &level.desc.environment;
    let rgb = |c: [f32; 3]| Color::srgb(c[0], c[1], c[2]);

    for entity in suns.iter().chain(&skies) {
        commands.entity(entity).despawn();
    }
    let direction = Vec3::from_array(env.sun_direction).normalize_or(Vec3::NEG_Y);
    // Many levels have an overbright sun (components up to ~2.3), made for BF2's clamped
    // lighting: keep its hue at the usual brightness (brighter looks washed out here).
    let sun = Vec3::from_array(env.sun_color);
    let sun_color = rgb((sun / sun.max_element().max(1.0)).to_array());
    commands.spawn((
        Sun,
        DirectionalLight {
            color: sun_color,
            illuminance: SUN_ILLUMINANCE,
            shadow_maps_enabled: !cli.no_shadows,
            ..default()
        },
        Transform::default().looking_to(direction, Vec3::Y),
        // Real-time shadows only near the player, like BF2 (distant shadows come from
        // baked lightmaps there). Every cascade redraws all casters inside it.
        CascadeShadowConfigBuilder {
            num_cascades: 3,
            first_cascade_far_bound: 15.0,
            maximum_distance: 120.0,
            ..default()
        }
        .build(),
    ));

    clear.0 = rgb(env.sky_color);
    *ambient = GlobalAmbientLight {
        color: rgb(env.ambient_color),
        brightness: 350.0,
        ..default()
    };
    // BF2's view distances were tuned for 2005 hardware (Karkand: 140 m). Stretch them.
    // TODO: make this a user setting.
    let fog_end = (env.fog_range[1] * 4.0).max(600.0);
    let fog_start = fog_end * 0.3;
    for (mut fog, mut projection) in &mut cameras {
        *fog = DistanceFog {
            color: rgb(env.fog_color),
            directional_light_color: sun_color.with_alpha(0.3),
            directional_light_exponent: 20.0,
            falloff: FogFalloff::Linear {
                start: fog_start,
                end: fog_end,
            },
        };
        if let Projection::Perspective(perspective) = projection.as_mut() {
            perspective.far = fog_end + 100.0;
        }
    }

    // Sky dome around the camera; its shader puts it behind everything, so its size only
    // has to keep it inside the view frustum's far plane for culling.
    if let Some(sky) = &env.sky {
        let scale = fog_end * 0.5 / sky.radius.max(1.0);
        commands.spawn((
            SkyDome {
                mesh: asset_server
                    .load(GltfAssetLabel::Mesh(0).from_asset(format!("imported://{}", sky.mesh))),
                material: sky_materials.add(SkyMaterial {
                    texture: asset_server.load(format!("imported://{}", sky.texture)),
                    params: SkyParams { haze: Vec4::new(SKY_HAZE, 0.0, 0.0, 0.0) },
                }),
                spawned: false,
            },
            Transform::from_rotation(Quat::from_rotation_y(-sky.rotation.to_radians()))
                .with_scale(Vec3::splat(scale)),
            Visibility::default(),
        ));
    }
}

/// Height of the view direction (sine of the elevation) up to which the sky fades from the
/// fog colour at the horizon to its own texture.
const SKY_HAZE: f32 = 0.08;

/// The sky texture on the dome mesh, drawn at the far plane and fogged below the horizon.
#[derive(Asset, AsBindGroup, TypePath, Debug, Clone)]
struct SkyMaterial {
    #[texture(0)]
    #[sampler(1)]
    texture: Handle<Image>,
    #[uniform(2)]
    params: SkyParams,
}

#[derive(ShaderType, Debug, Clone, Copy)]
struct SkyParams {
    haze: Vec4,
}

impl Material for SkyMaterial {
    fn vertex_shader() -> ShaderRef {
        "embedded://client/render/shaders/sky.wgsl".into()
    }

    fn fragment_shader() -> ShaderRef {
        "embedded://client/render/shaders/sky.wgsl".into()
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
        layout: &MeshVertexBufferLayoutRef,
        _key: MaterialPipelineKey<Self>,
    ) -> Result<(), SpecializedMeshPipelineError> {
        descriptor.vertex.buffers = vec![layout.0.get_layout(&[
            Mesh::ATTRIBUTE_POSITION.at_shader_location(0),
            Mesh::ATTRIBUTE_UV_0.at_shader_location(1),
        ])?];
        descriptor.primitive.cull_mode = None;
        Ok(())
    }
}

fn spawn_sky_mesh(
    mut commands: Commands,
    mut skies: Query<(Entity, &mut SkyDome)>,
    meshes: Res<Assets<GltfMesh>>,
    asset_server: Res<AssetServer>,
) {
    for (entity, mut sky) in &mut skies {
        if sky.spawned {
            continue;
        }
        if let LoadState::Failed(err) = asset_server.load_state(&sky.mesh) {
            warn!("sky dome failed to load: {err}");
            sky.spawned = true;
            continue;
        }
        let Some(mesh) = meshes.get(&sky.mesh) else {
            continue;
        };
        for primitive in &mesh.primitives {
            commands.entity(entity).with_child((
                Mesh3d(primitive.mesh.clone()),
                MeshMaterial3d(sky.material.clone()),
                NotShadowCaster,
                NotShadowReceiver,
            ));
        }
        sky.spawned = true;
    }
}

fn follow_camera(
    camera: Single<&Transform, (With<PlayerCamera>, Without<SkyDome>)>,
    mut skies: Query<&mut Transform, With<SkyDome>>,
) {
    for mut transform in &mut skies {
        transform.translation = camera.translation;
    }
}
