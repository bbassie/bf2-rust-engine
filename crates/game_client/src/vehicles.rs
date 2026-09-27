//! Vehicles on the client: where to draw them, the seat keys, the view from a seat and the
//! vehicle line on the HUD.
//!
//! Connected to a remote server, vehicles are shown [`INTERPOLATION_DELAY`] in the past,
//! blending between received states like remote soldiers. There is no vehicle prediction yet:
//! a driver sees the vehicle react one round trip plus that delay after pressing a key (about
//! 130 ms on a LAN). Hosting, the simulation is local and avian interpolates between ticks.

use std::collections::VecDeque;

use bevy::prelude::*;
use bevy_replicon::prelude::*;
use game_shared::vehicle::{Seated, Vehicle, VehicleData, VehicleHealth, VehicleMotion, VehicleShot, VehicleState};

use crate::{
    audio::PlaySound,
    combat::{EffectAssets, spawn_tracer},
    effects::{EffectLibrary, SpawnEffect},
    local_input::LookState,
    net::LocalSoldier,
};

/// How far in the past vehicles are shown when connected to a remote server.
const INTERPOLATION_DELAY: f64 = 0.1;

pub struct ClientVehiclesPlugin;

impl Plugin for ClientVehiclesPlugin {
    fn build(&self, app: &mut App) {
        app.init_resource::<SeatRequest>()
            .add_observer(add_view)
            .add_systems(Startup, spawn_hud)
            .add_systems(
                PreUpdate,
                record_snapshots
                    .after(ClientSystems::Receive)
                    .run_if(in_state(ClientState::Connected)),
            )
            .add_systems(Update, (read_seat_keys, update_hud, receive_shots))
            .add_systems(
                PostUpdate,
                (place_vehicles, follow_vehicle_heading)
                    .chain()
                    .in_set(VehicleViewSystems)
                    .before(crate::camera::CameraSystems)
                    .before(TransformSystems::Propagate),
            );
    }
}

/// Computes [`VehicleView`] in `PostUpdate`; vehicle visuals and the camera run after it.
#[derive(SystemSet, Debug, Clone, PartialEq, Eq, Hash)]
pub struct VehicleViewSystems;

/// How a vehicle should be drawn this frame.
#[derive(Component, Clone, Debug, Default)]
pub struct VehicleView {
    pub transform: Transform,
    pub joints: Vec<[f32; 3]>,
    pub wheels: Vec<f32>,
    /// Along the hull's forward axis, m/s.
    pub speed: f32,
}

/// Received states, by local receive time.
#[derive(Component, Default)]
struct Snapshots(VecDeque<(f64, VehicleMotion, VehicleState)>);

/// The seat the player asked for with F1..F8 (1-based, 0 = none). Sent with every input frame
/// until we sit there.
#[derive(Resource, Default)]
pub struct SeatRequest(pub u8);

fn add_view(add: On<Add, Vehicle>, mut commands: Commands, motions: Query<&VehicleMotion>) {
    let motion = motions.get(add.entity).copied().unwrap_or_default();
    commands.entity(add.entity).insert((
        VehicleView {
            transform: motion.transform(),
            ..default()
        },
        Snapshots::default(),
    ));
}

fn record_snapshots(
    time: Res<Time<Real>>,
    mut vehicles: Query<(Ref<VehicleMotion>, Ref<VehicleState>, &mut Snapshots)>,
) {
    let now = time.elapsed_secs_f64();
    for (motion, state, mut snapshots) in &mut vehicles {
        if motion.is_changed() || state.is_changed() {
            snapshots.0.push_back((now, *motion, (*state).clone()));
        }
        while snapshots.0.len() > 2 && snapshots.0[1].0 < now - 1.0 {
            snapshots.0.pop_front();
        }
    }
}

fn place_vehicles(
    real: Res<Time<Real>>,
    state: Res<State<ClientState>>,
    mut vehicles: Query<(&VehicleMotion, &VehicleState, &Snapshots, &mut VehicleView, &mut Transform)>,
) {
    let connected = *state.get() == ClientState::Connected;
    let at = real.elapsed_secs_f64() - INTERPOLATION_DELAY;
    for (motion, current, snapshots, mut view, mut transform) in &mut vehicles {
        if connected {
            let (a, b, t) = interpolate(&snapshots.0, at).unwrap_or((
                (*motion, current.clone()),
                (*motion, current.clone()),
                1.0,
            ));
            *transform = Transform::from_translation(a.0.position.lerp(b.0.position, t))
                .with_rotation(a.0.rotation.slerp(b.0.rotation, t));
            view.joints = lerp_joints(&a.1.joints, &b.1.joints, t);
            view.wheels = a.1.wheels.iter().zip(&b.1.wheels).map(|(x, y)| x + (y - x) * t).collect();
            view.speed = a.0.forward_speed() + (b.0.forward_speed() - a.0.forward_speed()) * t;
        } else {
            // Hosting: avian already interpolates the body's transform between ticks.
            view.joints.clone_from(&current.joints);
            view.wheels.clone_from(&current.wheels);
            view.speed = motion.forward_speed();
        }
        view.transform = *transform;
    }
}

type Sample = (VehicleMotion, VehicleState);

fn interpolate(snapshots: &VecDeque<(f64, VehicleMotion, VehicleState)>, at: f64) -> Option<(Sample, Sample, f32)> {
    let last = snapshots.back()?;
    if at >= last.0 {
        return Some(((last.1, last.2.clone()), (last.1, last.2.clone()), 1.0));
    }
    let i = snapshots.iter().rposition(|(t, ..)| *t <= at)?;
    let (t0, m0, s0) = &snapshots[i];
    let (t1, m1, s1) = &snapshots[i + 1];
    let t = ((at - t0) / (t1 - t0).max(1e-6)) as f32;
    Some(((*m0, s0.clone()), (*m1, s1.clone()), t.clamp(0.0, 1.0)))
}

fn lerp_joints(a: &[[f32; 3]], b: &[[f32; 3]], t: f32) -> Vec<[f32; 3]> {
    a.iter()
        .zip(b)
        .map(|(x, y)| std::array::from_fn(|i| x[i] + wrap(y[i] - x[i]) * t))
        .collect()
}

fn wrap(a: f32) -> f32 {
    use std::f32::consts::{PI, TAU};
    (a + PI).rem_euclid(TAU) - PI
}

/// Heading of a transform around +Y (0 = facing -Z, positive turns left).
pub fn heading(rotation: Quat) -> f32 {
    let forward = rotation * Vec3::NEG_Z;
    (-forward.x).atan2(-forward.z)
}

fn read_seat_keys(
    keys: Res<ButtonInput<KeyCode>>,
    mut request: ResMut<SeatRequest>,
    seated: Query<&Seated, With<LocalSoldier>>,
) {
    const KEYS: [KeyCode; 8] = [
        KeyCode::F1,
        KeyCode::F2,
        KeyCode::F3,
        KeyCode::F4,
        KeyCode::F5,
        KeyCode::F6,
        KeyCode::F7,
        KeyCode::F8,
    ];
    if let Some(index) = KEYS.iter().position(|k| keys.just_pressed(*k)) {
        request.0 = index as u8 + 1;
    }
    // Done once we sit there, or when we're not in a vehicle at all.
    match seated.single() {
        Ok(seated) if request.0 == seated.seat + 1 => request.0 = 0,
        Err(_) => request.0 = 0,
        _ => {}
    }
}

/// In a seat that doesn't aim (driving, riding along) the view turns with the vehicle, so
/// looking ahead stays looking ahead through corners. Gunners keep their aim steady.
fn follow_vehicle_heading(
    seated: Query<&Seated, With<LocalSoldier>>,
    vehicles: Query<(&VehicleView, &VehicleData)>,
    mut look: ResMut<LookState>,
    mut last: Local<Option<(Entity, u8, f32)>>,
) {
    let Some((seated, view, data)) = seated
        .single()
        .ok()
        .and_then(|s| vehicles.get(s.vehicle).ok().map(|(v, d)| (s, v, d)))
    else {
        *last = None;
        return;
    };
    let now = heading(view.transform.rotation);
    if let Some((vehicle, seat, before)) = *last
        && vehicle == seated.vehicle
        && seat == seated.seat
        && !data.0.seat_aims(seat as usize)
    {
        look.yaw += wrap(now - before);
    }
    *last = Some((seated.vehicle, seated.seat, now));
}

/// Where the camera goes for a seat: the seat camera's eye point and chase settings.
pub struct SeatView {
    pub eye: Vec3,
    pub vehicle: Transform,
    pub chase_distance: f32,
    pub chase_height: f32,
}

pub fn seat_view(seated: &Seated, vehicles: &Query<(&VehicleView, &VehicleData)>) -> Option<SeatView> {
    let (view, data) = vehicles.get(seated.vehicle).ok()?;
    let model = &data.0;
    let transforms = model.part_transforms(&view.joints);
    let seat = model.desc.seats.get(seated.seat as usize)?;
    let local = match &seat.camera {
        Some(camera) => model.attachment(&transforms, &camera.attachment),
        None => model.seat_transform(&transforms, seated.seat as usize) * Transform::from_xyz(0.0, 0.6, 0.0),
    };
    let (chase_distance, chase_height) = seat
        .camera
        .as_ref()
        .map_or((12.0, 1.0), |c| (c.chase_distance, c.chase_offset[1]));
    Some(SeatView {
        eye: (view.transform * local).translation,
        vehicle: view.transform,
        chase_distance,
        chase_height,
    })
}

#[derive(Component)]
struct VehicleHudText;

fn spawn_hud(mut commands: Commands) {
    commands.spawn((
        VehicleHudText,
        Text::new(""),
        TextFont {
            font_size: FontSize::Px(16.0),
            ..default()
        },
        TextColor(Color::srgb(0.92, 0.94, 0.96)),
        TextShadow {
            offset: Vec2::splat(1.0),
            color: Color::srgba(0.0, 0.0, 0.0, 0.8),
        },
        Node {
            position_type: PositionType::Absolute,
            bottom: px(28),
            left: percent(50),
            margin: UiRect::left(px(-320)),
            width: px(640),
            justify_content: JustifyContent::Center,
            ..default()
        },
        TextLayout::justify(Justify::Center),
    ));
}

fn update_hud(
    seated: Query<&Seated, With<LocalSoldier>>,
    vehicles: Query<(&VehicleView, &VehicleData, Option<&VehicleHealth>)>,
    mut text: Single<&mut Text, With<VehicleHudText>>,
) {
    let line = seated
        .single()
        .ok()
        .and_then(|s| vehicles.get(s.vehicle).ok().map(|(v, d, h)| (s, v, d, h)))
        .map(|(seated, view, data, health)| {
            let model = &data.0;
            let seat = seated.seat as usize;
            let role = if seat == 0 {
                "Driver"
            } else if model.seat_aims(seat) {
                "Gunner"
            } else {
                "Passenger"
            };
            let hit_points = health.map_or(model.desc.hit_points, |h| h.current);
            format!(
                "{}   |   {role} ({}/{})   |   {:.0} km/h   |   {:.0} HP",
                model.desc.display_name,
                seat + 1,
                model.desc.seats.len(),
                view.speed.abs() * 3.6,
                hit_points.ceil()
            )
        })
        .unwrap_or_default();
    if text.0 != line {
        text.0 = line;
    }
}

/// Tracers, muzzle flashes and sounds of vehicle guns.
fn receive_shots(
    mut commands: Commands,
    mut shots: MessageReader<VehicleShot>,
    assets: Res<EffectAssets>,
    library: Option<Res<EffectLibrary>>,
    mut sounds: MessageWriter<PlaySound>,
    mut effects: MessageWriter<SpawnEffect>,
    vehicles: Query<&VehicleData>,
) {
    for shot in shots.read() {
        let Some(weapon) = vehicles.get(shot.vehicle).ok().and_then(|d| d.0.guns.get(shot.gun as usize)) else {
            continue;
        };
        spawn_tracer(&mut commands, &assets, shot.origin, shot.direction, weapon, Some(shot.vehicle), library.as_deref());
        if let Some((muzzle, _)) = library.as_deref().and_then(|l| l.muzzle(&weapon.name)) {
            effects.write(SpawnEffect::new(muzzle, shot.origin).with_forward(shot.direction));
        }
        if let Some(sound) = &weapon.sounds.fire_3p {
            sounds.write(PlaySound::at(sound.clone(), shot.origin).emitter(shot.vehicle).reason("vehicle gun"));
        }
    }
}
