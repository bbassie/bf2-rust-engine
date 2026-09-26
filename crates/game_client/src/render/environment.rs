//! Sun, sky, ambient light and fog from the level's environment settings.

use bevy::{light::CascadeShadowConfigBuilder, prelude::*};
use game_shared::level::LoadedLevel;

use crate::camera::PlayerCamera;

pub struct EnvironmentPlugin;

impl Plugin for EnvironmentPlugin {
    fn build(&self, app: &mut App) {
        app.insert_resource(ClearColor(Color::srgb(0.55, 0.68, 0.85)))
            .add_systems(
                Update,
                apply_environment.run_if(resource_exists_and_changed::<LoadedLevel>),
            );
    }
}

#[derive(Component)]
struct Sun;

fn apply_environment(
    mut commands: Commands,
    level: Res<LoadedLevel>,
    suns: Query<Entity, With<Sun>>,
    mut clear: ResMut<ClearColor>,
    mut ambient: ResMut<GlobalAmbientLight>,
    mut cameras: Query<(&mut DistanceFog, &mut Projection), With<PlayerCamera>>,
) {
    let env = &level.desc.environment;
    let rgb = |c: [f32; 3]| Color::srgb(c[0], c[1], c[2]);

    for sun in &suns {
        commands.entity(sun).despawn();
    }
    let direction = Vec3::from_array(env.sun_direction).normalize_or(Vec3::NEG_Y);
    commands.spawn((
        Sun,
        DirectionalLight {
            color: rgb(env.sun_color),
            illuminance: 12_000.0,
            shadow_maps_enabled: true,
            ..default()
        },
        Transform::default().looking_to(direction, Vec3::Y),
        CascadeShadowConfigBuilder {
            num_cascades: 4,
            first_cascade_far_bound: 12.0,
            maximum_distance: 350.0,
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
}
