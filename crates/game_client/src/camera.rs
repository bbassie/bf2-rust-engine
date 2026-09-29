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
    settings::{Action, Actions, Settings},
};

pub struct CameraPlugin;

impl Plugin for CameraPlugin {
    fn build(&self, app: &mut App) {
        app.insert_resource(ThirdPerson(false))
            .init_resource::<ChaseZoom>()
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

fn spawn_camera(mut commands: Commands, cli: Res<crate::Cli>, settings: Res<Settings>) {
    let ssao = settings.ssao_on(&cli);
    // SSAO needs MSAA off; the view model camera smooths the final image with SMAA then.
    let msaa = if ssao { Msaa::Off } else { Msaa::default() };
    let mut camera = commands.spawn((
        PlayerCamera,
        Camera3d::default(),
        msaa,
        Projection::from(PerspectiveProjection {
            fov: settings.field_of_view.to_radians(),
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
    if ssao {
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

/// `pivot + offset`, pulled in if the world is in the way (at least `min` meters out).
fn chase_position(spatial: &avian3d::prelude::SpatialQuery, pivot: Vec3, offset: Vec3, min: f32) -> Vec3 {
    let filter = avian3d::prelude::SpatialQueryFilter::from_mask(game_shared::physics::GameLayer::World);
    let distance = Dir3::new(offset)
        .ok()
        .and_then(|dir| spatial.cast_ray(pivot, dir, offset.length(), true, &filter))
        .map_or(offset.length(), |hit| (hit.distance - 0.3).max(min));
    pivot + offset.normalize_or_zero() * distance
}

/// Whether the camera follows our soldier from behind instead of through its eyes.
#[derive(Resource, Default)]
pub struct ThirdPerson(pub bool);

/// Scales how far the vehicle chase camera stays out (scenarios take close-ups with it).
#[derive(Resource)]
pub struct ChaseZoom(pub f32);

impl Default for ChaseZoom {
    fn default() -> Self {
        Self(1.0)
    }
}

fn toggle_third_person(actions: Actions, mut third_person: ResMut<ThirdPerson>) {
    if actions.just_pressed(Action::ThirdPerson) {
        third_person.0 = !third_person.0;
    }
}

fn update_camera(
    time: Res<Time>,
    actions: Actions,
    cursor: Single<&CursorOptions>,
    look: Res<LookState>,
    third_person: Res<ThirdPerson>,
    zoom: Res<ChaseZoom>,
    cli: Res<crate::Cli>,
    spatial: avian3d::prelude::SpatialQuery,
    soldier: Query<(&SoldierRender, Option<&Seated>), With<LocalSoldier>>,
    vehicles: Query<(&crate::vehicles::VehicleView, &VehicleData)>,
    flight: Res<crate::vehicles::FlightStick>,
    camera: Single<(&mut Transform, &mut Spectator), With<PlayerCamera>>,
    mut pilot: Local<crate::vehicles::PilotCamera>,
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
        // Piloting: the cockpit view is fixed to the aircraft (turned by free look); the chase
        // camera trails it (see `vehicles::pilot_camera`).
        if flight.active
            && let Some(seated) = soldier.single().ok().and_then(|(_, seated)| seated)
            && let Ok((vehicle, data)) = vehicles.get(seated.vehicle)
        {
            let filter = avian3d::prelude::SpatialQueryFilter::from_mask(game_shared::physics::GameLayer::World);
            let altitude = spatial
                .cast_ray(view.vehicle.translation, Dir3::NEG_Y, 200.0, true, &filter)
                .map_or(200.0, |hit| hit.distance);
            let (placed, chase) = crate::vehicles::pilot_camera(
                &mut pilot,
                &view,
                vehicle,
                data,
                flight.look,
                third_person.0,
                zoom.0,
                altitude,
                time.delta_secs(),
            );
            *transform = placed;
            if let Some((pivot, offset)) = chase {
                transform.translation = chase_position(&spatial, pivot, offset, 3.0);
            }
            spectator.position = transform.translation;
            return;
        }
        *pilot = default();
        transform.translation = if third_person.0 {
            let pivot = view.vehicle.translation + Vec3::Y * 1.5;
            let offset = rotation * Vec3::new(0.0, 0.0, view.chase_distance * 0.7 * zoom.0) + Vec3::Y * view.chase_height;
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
        // Gunners look where they aim; the turret catches up (see `vehicles::limit_gunner_aim`).
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

    // Spectator fly-cam: keyboard, or a gamepad's left stick (strafe/forward) and triggers
    // (climb/descend).
    if cursor_locked(&cursor) {
        let mut local = Vec3::new(
            actions.axis(Action::MoveRight, Action::MoveLeft),
            actions.axis(Action::Jump, Action::Crouch),
            -actions.axis(Action::MoveForward, Action::MoveBack),
        );
        if let Some(gamepad) = actions.gamepad() {
            let stick = crate::local_input::deadzone(gamepad.left_stick(), 0.2);
            local.x += stick.x;
            local.z -= stick.y;
            local.y += gamepad.get(bevy::input::gamepad::GamepadButton::RightTrigger2).unwrap_or(0.0)
                - gamepad.get(bevy::input::gamepad::GamepadButton::LeftTrigger2).unwrap_or(0.0);
        }
        let speed = if actions.pressed(Action::Sprint) { 120.0 } else { 30.0 };
        spectator.position += rotation * local.clamp_length_max(1.0) * speed * time.delta_secs();
    }
    transform.translation = spectator.position;
    transform.rotation = rotation;
}
