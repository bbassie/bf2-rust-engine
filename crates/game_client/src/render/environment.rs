//! Sun, sky, ambient light and fog from the level's environment settings.

use bevy::{
    asset::{LoadState, embedded_asset},
    gltf::{GltfAssetLabel, GltfMesh},
    light::{CascadeShadowConfig, CascadeShadowConfigBuilder, NotShadowCaster, NotShadowReceiver},
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
                (follow_camera, update_view_distance)
                    .after(crate::camera::CameraSystems)
                    .before(TransformSystems::Propagate),
            );
    }
}

/// Illuminance (lux) of the sun. BF2 adds about 2 x sun on top of 2 x ambient (in gamma
/// terms) and saturates; ~4000 lux adds about 1.3x the face-value luminance of the ambient
/// model below, so sunlit surfaces stay readable and shade isn't crushed by tonemapping.
const SUN_ILLUMINANCE: f32 = 4_000.0;

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
    mut cameras: Query<&mut DistanceFog, With<PlayerCamera>>,
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
        shadow_cascades(0.0),
    ));

    clear.0 = rgb(env.sky_color);
    // BF2 lights in gamma space: colour = texture * 2 * (ambient + sun * n.l), so a surface
    // in shade shows at 2 * ambient of its texture brightness; in linear terms that is
    // (2 * ambient)^2.2 of the luminance at which a texture shows at face value (~1000
    // cd/m2 at the default exposure).
    let ambient_linear = env.ambient_color.map(|c| (2.0 * c).clamp(0.0, 1.0).powf(2.2));
    *ambient = GlobalAmbientLight {
        color: Color::linear_rgb(ambient_linear[0], ambient_linear[1], ambient_linear[2]),
        brightness: 1000.0,
        ..default()
    };
    // BF2's view distances were tuned for 2005 hardware (Karkand: 140 m). Stretch them;
    // `update_view_distance` scales this by the setting and the camera's height.
    let fog_end = (env.fog_range[1] * 4.0).max(600.0);
    commands.insert_resource(LevelFog { end: fog_end });
    for mut fog in &mut cameras {
        *fog = DistanceFog {
            color: rgb(env.fog_color),
            directional_light_color: sun_color.with_alpha(0.3),
            directional_light_exponent: 20.0,
            falloff: FogFalloff::Linear {
                start: fog_end * FOG_START,
                end: fog_end,
            },
        };
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

/// Where fog starts, as a fraction of where it ends.
const FOG_START: f32 = 0.3;
/// The fog never ends further away than this (the world ends about there).
const MAX_FOG_END: f32 = 15_000.0;
/// Height above the ground (m) that adds the level's fog distance once more, up to
/// `MAX_ALTITUDE_BOOST` times: pilots see the ground they fly over.
const ALTITUDE_PER_BOOST: f32 = 150.0;
const MAX_ALTITUDE_BOOST: f32 = 4.0;

/// The level's fog end (m) at the normal view distance, on the ground.
#[derive(Resource)]
pub struct LevelFog {
    pub end: f32,
}

/// Real-time sun shadows. Near the ground they only cover 120 m, like BF2 (it baked distant
/// shadows into lightmaps); they reach further as the camera climbs, so the ground below
/// an aircraft still has shadows. Every cascade redraws all casters inside it.
fn shadow_cascades(height: f32) -> CascadeShadowConfig {
    CascadeShadowConfigBuilder {
        num_cascades: 3,
        first_cascade_far_bound: 15.0 + height * 0.25,
        maximum_distance: (120.0 + height * 1.5).min(700.0),
        ..default()
    }
    .build()
}

/// Scales fog, far plane and shadow range with the view distance setting and the camera's
/// height above the terrain (smoothed, so climbing opens the view gradually).
fn update_view_distance(
    time: Res<Time>,
    settings: Res<crate::settings::Settings>,
    level_fog: Option<Res<LevelFog>>,
    level: Option<Res<LoadedLevel>>,
    mut cameras: Query<(&Transform, &mut DistanceFog, &mut Projection), With<PlayerCamera>>,
    mut suns: Query<(&mut CascadeShadowConfig, Ref<Sun>)>,
    mut height: Local<Option<f32>>,
    mut shadow_height: Local<f32>,
) {
    let Some(level_fog) = level_fog else {
        return;
    };
    for (transform, mut fog, mut projection) in &mut cameras {
        let eye = transform.translation;
        let ground = level
            .as_ref()
            .and_then(|l| l.heightmap.as_ref())
            .map_or(0.0, |h| h.height_at(eye.x, eye.z));
        let target = (eye.y - ground).max(0.0);
        let smoothed = match *height {
            Some(h) => h + (target - h) * (1.0 - (-time.delta_secs() * 2.0).exp()),
            None => target,
        };
        *height = Some(smoothed);

        let boost = (1.0 + smoothed / ALTITUDE_PER_BOOST).min(MAX_ALTITUDE_BOOST);
        let end = (level_fog.end * settings.view_distance.scale() * boost).min(MAX_FOG_END);
        // Skip tiny changes: every write re-uploads the view.
        let differs = |a: f32, b: f32| (a - b).abs() > b * 0.005;
        if let FogFalloff::Linear { end: old_end, .. } = fog.falloff
            && differs(end, old_end)
        {
            fog.falloff = FogFalloff::Linear {
                start: end * FOG_START,
                end,
            };
        }
        if let Projection::Perspective(perspective) = projection.as_mut()
            && differs(end + 100.0, perspective.far)
        {
            perspective.far = end + 100.0;
        }
        let new_sun = suns.iter().any(|(_, sun)| sun.is_added());
        if new_sun || (smoothed - *shadow_height).abs() > 5.0 + *shadow_height * 0.1 {
            *shadow_height = smoothed;
            for (mut cascades, _) in &mut suns {
                *cascades = shadow_cascades(smoothed);
            }
        }
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
