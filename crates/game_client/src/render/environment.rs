//! Sun, sky, ambient light and fog from the level's environment settings.

use bevy::{
    asset::LoadState,
    gltf::{GltfAssetLabel, GltfMesh},
    light::{CascadeShadowConfigBuilder, NotShadowCaster, NotShadowReceiver},
    prelude::*,
};
use game_shared::level::LoadedLevel;

use crate::camera::PlayerCamera;

pub struct EnvironmentPlugin;

impl Plugin for EnvironmentPlugin {
    fn build(&self, app: &mut App) {
        app.insert_resource(ClearColor(Color::srgb(0.55, 0.68, 0.85)))
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

/// The level's sun (lights the world layer only).
#[derive(Component)]
pub struct Sun;

/// The sky dome; it moves with the camera so it always surrounds it.
#[derive(Component)]
struct SkyDome {
    mesh: Handle<GltfMesh>,
    material: Handle<StandardMaterial>,
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
    mut materials: ResMut<Assets<StandardMaterial>>,
    cli: Res<crate::Cli>,
) {
    let env = &level.desc.environment;
    let rgb = |c: [f32; 3]| Color::srgb(c[0], c[1], c[2]);

    for entity in suns.iter().chain(&skies) {
        commands.entity(entity).despawn();
    }
    let direction = Vec3::from_array(env.sun_direction).normalize_or(Vec3::NEG_Y);
    commands.spawn((
        Sun,
        DirectionalLight {
            color: rgb(env.sun_color),
            illuminance: 12_000.0,
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
            directional_light_color: rgb(env.sun_color).with_alpha(0.3),
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

    // Sky dome just inside the far plane, unlit and unfogged.
    if let Some(sky) = &env.sky {
        let scale = (fog_end + 40.0) / sky.radius.max(1.0);
        commands.spawn((
            SkyDome {
                mesh: asset_server
                    .load(GltfAssetLabel::Mesh(0).from_asset(format!("imported://{}", sky.mesh))),
                material: materials.add(StandardMaterial {
                    base_color_texture: Some(asset_server.load(format!("imported://{}", sky.texture))),
                    unlit: true,
                    fog_enabled: false,
                    cull_mode: None,
                    ..default()
                }),
                spawned: false,
            },
            Transform::from_rotation(Quat::from_rotation_y(-sky.rotation.to_radians()))
                .with_scale(Vec3::splat(scale)),
            Visibility::default(),
        ));
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
