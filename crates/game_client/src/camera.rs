//! First-person camera on our soldier, or a free-flying spectator camera.

use bevy::{prelude::*, window::CursorOptions};

use game_shared::level::LoadedLevel;

use crate::{
    local_input::{LookState, cursor_locked},
    net::LocalSoldier,
    prediction::{RenderStateSystems, SoldierRender},
};

pub struct CameraPlugin;

impl Plugin for CameraPlugin {
    fn build(&self, app: &mut App) {
        app.add_systems(Startup, spawn_camera)
            .add_systems(
                Update,
                overview_on_level_load.run_if(resource_exists_and_changed::<LoadedLevel>),
            )
            .add_systems(
                PostUpdate,
                update_camera
                    .after(RenderStateSystems)
                    .before(TransformSystems::Propagate),
            );
    }
}

#[derive(Component)]
pub struct PlayerCamera;

/// Position of the spectator camera when we have no soldier.
#[derive(Component)]
struct Spectator {
    position: Vec3,
}

fn spawn_camera(mut commands: Commands) {
    commands.spawn((
        PlayerCamera,
        Camera3d::default(),
        Projection::from(PerspectiveProjection {
            fov: 75f32.to_radians(),
            near: 0.05,
            far: 3000.0,
            ..default()
        }),
        DistanceFog::default(),
        Spectator {
            position: Vec3::new(0.0, 40.0, 60.0),
        },
        Transform::from_xyz(0.0, 40.0, 60.0),
    ));
}

/// Puts the spectator camera above the level, looking over it.
fn overview_on_level_load(
    level: Res<LoadedLevel>,
    soldier: Query<(), With<LocalSoldier>>,
    mut look: ResMut<LookState>,
    mut spectator: Single<&mut Spectator>,
) {
    // Our soldier may already exist (replicated before the level finished loading).
    let Some(heightmap) = level.heightmap.as_ref().filter(|_| soldier.is_empty()) else {
        return;
    };
    let center = heightmap.center();
    let ground = heightmap.height_at(center.x, center.z);
    let size = heightmap.world_size();
    spectator.position = Vec3::new(center.x, ground + size * 0.35, center.z + size * 0.55);
    look.yaw = 0.0;
    look.pitch = -0.55;
}

fn update_camera(
    time: Res<Time>,
    keys: Res<ButtonInput<KeyCode>>,
    cursor: Single<&CursorOptions>,
    look: Res<LookState>,
    soldier: Query<&SoldierRender, With<LocalSoldier>>,
    camera: Single<(&mut Transform, &mut Spectator), With<PlayerCamera>>,
) {
    let (mut transform, mut spectator) = camera.into_inner();
    let rotation = look.rotation();

    if let Ok(render) = soldier.single() {
        transform.translation = render.eye_position();
        transform.rotation = rotation;
        spectator.position = transform.translation;
        return;
    }

    // Spectator fly-cam.
    if cursor_locked(&cursor) {
        let axis = |pos: KeyCode, neg: KeyCode| keys.pressed(pos) as i8 as f32 - keys.pressed(neg) as i8 as f32;
        let local = Vec3::new(
            axis(KeyCode::KeyD, KeyCode::KeyA),
            axis(KeyCode::Space, KeyCode::ControlLeft),
            -axis(KeyCode::KeyW, KeyCode::KeyS),
        );
        let speed = if keys.pressed(KeyCode::ShiftLeft) { 120.0 } else { 30.0 };
        spectator.position += rotation * local * speed * time.delta_secs();
    }
    transform.translation = spectator.position;
    transform.rotation = rotation;
}
