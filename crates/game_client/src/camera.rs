//! First-person camera on our soldier, or a free-flying spectator camera.

use bevy::{pbr::ScreenSpaceAmbientOcclusion, prelude::*, window::CursorOptions};

use game_shared::{
    level::LoadedLevel,
    vehicle::{Seated, VehicleData},
};

use crate::{
    local_input::{LookState, cursor_locked},
    net::LocalSoldier,
    prediction::{RenderStateSystems, SoldierRender},
};

pub struct CameraPlugin;

impl Plugin for CameraPlugin {
    fn build(&self, app: &mut App) {
        app.insert_resource(ThirdPerson(false))
            .add_systems(Startup, spawn_camera)
            .add_systems(Update, toggle_third_person)
            .add_systems(
                Update,
                overview_on_level_load.run_if(resource_exists_and_changed::<LoadedLevel>),
            )
            .add_systems(
                PostUpdate,
                update_camera
                    .in_set(CameraSystems)
                    .after(RenderStateSystems)
                    .before(TransformSystems::Propagate),
            );
    }
}

#[derive(Component)]
pub struct PlayerCamera;

/// Positions the camera in `PostUpdate`; things that follow the camera run after it.
#[derive(SystemSet, Debug, Clone, PartialEq, Eq, Hash)]
pub struct CameraSystems;

/// Position of the spectator camera when we have no soldier.
#[derive(Component)]
pub struct Spectator {
    pub position: Vec3,
}

fn spawn_camera(mut commands: Commands, cli: Res<crate::Cli>) {
    // SSAO needs MSAA off; the view model camera smooths the final image with SMAA then.
    let msaa = if cli.no_ssao { Msaa::default() } else { Msaa::Off };
    let mut camera = commands.spawn((
        PlayerCamera,
        Camera3d::default(),
        msaa,
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
        SpatialListener::new(0.25),
    ));
    if !cli.no_ssao {
        // Contact shadows in corners and under objects, which BF2 baked into lightmaps.
        camera.insert(ScreenSpaceAmbientOcclusion::default());
    }
}

/// Puts the spectator camera above the level, looking over it.
fn overview_on_level_load(
    level: Res<LoadedLevel>,
    cli: Res<crate::Cli>,
    soldier: Query<(), With<LocalSoldier>>,
    mut look: ResMut<LookState>,
    mut spectator: Single<&mut Spectator>,
) {
    if let Some(&[x, y, z, yaw, pitch]) = cli.camera.as_deref() {
        spectator.position = Vec3::new(x, y, z);
        look.yaw = yaw.to_radians();
        look.pitch = pitch.to_radians();
        return;
    }
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

/// Whether the camera follows our soldier from behind instead of through its eyes.
#[derive(Resource, Default)]
pub struct ThirdPerson(pub bool);

fn toggle_third_person(keys: Res<ButtonInput<KeyCode>>, mut third_person: ResMut<ThirdPerson>) {
    if keys.just_pressed(KeyCode::KeyV) {
        third_person.0 = !third_person.0;
    }
}

fn update_camera(
    time: Res<Time>,
    keys: Res<ButtonInput<KeyCode>>,
    cursor: Single<&CursorOptions>,
    look: Res<LookState>,
    third_person: Res<ThirdPerson>,
    cli: Res<crate::Cli>,
    spatial: avian3d::prelude::SpatialQuery,
    soldier: Query<(&SoldierRender, Option<&Seated>), With<LocalSoldier>>,
    vehicles: Query<(&crate::vehicles::VehicleView, &VehicleData)>,
    camera: Single<(&mut Transform, &mut Spectator), With<PlayerCamera>>,
) {
    let (mut transform, mut spectator) = camera.into_inner();
    let rotation = look.rotation();

    // In a vehicle: through the seat's camera, or chasing the vehicle (V).
    if let Some(view) = soldier
        .single()
        .ok()
        .and_then(|(_, seated)| seated)
        .and_then(|seated| crate::vehicles::seat_view(seated, &vehicles))
    {
        transform.translation = if third_person.0 {
            let pivot = view.vehicle.translation + Vec3::Y * 1.5;
            let offset = rotation * Vec3::new(0.0, 0.0, view.chase_distance * 0.7) + Vec3::Y * view.chase_height;
            let distance = Dir3::new(offset)
                .ok()
                .and_then(|dir| {
                    spatial.cast_ray(
                        pivot,
                        dir,
                        offset.length(),
                        true,
                        &avian3d::prelude::SpatialQueryFilter::from_mask(
                            game_shared::physics::GameLayer::World,
                        ),
                    )
                })
                .map_or(offset.length(), |hit| (hit.distance - 0.3).max(1.0));
            pivot + offset.normalize_or_zero() * distance
        } else {
            view.eye
        };
        transform.rotation = rotation;
        spectator.position = transform.translation;
        return;
    }

    if let Ok((render, _)) = soldier.single() {
        let eye = render.eye_position();
        transform.translation = if third_person.0 {
            // Over the shoulder, pulled in if a wall is in the way.
            let local = match cli.tp_offset.as_deref() {
                Some(&[x, y, z]) => Vec3::new(x, y, z),
                _ => Vec3::new(0.6, 0.3, 3.2),
            };
            let offset = rotation * local;
            let distance = Dir3::new(offset)
                .ok()
                .and_then(|dir| {
                    spatial.cast_ray(
                        eye,
                        dir,
                        offset.length(),
                        true,
                        &avian3d::prelude::SpatialQueryFilter::from_mask(
                            game_shared::physics::GameLayer::World,
                        ),
                    )
                })
                .map_or(offset.length(), |hit| (hit.distance - 0.2).max(0.3));
            eye + offset.normalize_or_zero() * distance
        } else {
            eye
        };
        transform.rotation = rotation;
        if third_person.0 && cli.tp_offset.is_some() {
            // Debug camera placement: look back at the soldier.
            transform.look_at(eye - Vec3::Y * 0.4, Vec3::Y);
        }
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
