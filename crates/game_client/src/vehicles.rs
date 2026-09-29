//! Vehicles on the client: where to draw them, the seat keys, the view from a seat and the
//! vehicle line on the HUD.
//!
//! Connected to a remote server, vehicles are shown [`INTERPOLATION_DELAY`] in the past,
//! blending between received states like remote soldiers. There is no vehicle prediction yet:
//! a driver sees the vehicle react one round trip plus that delay after pressing a key (about
//! 130 ms on a LAN). Hosting, the simulation is local and avian interpolates between ticks.

use std::collections::VecDeque;

use bevy::{input::mouse::AccumulatedMouseMotion, prelude::*, window::CursorOptions};
use bevy_replicon::prelude::*;
use game_shared::vehicle::{Seated, Vehicle, VehicleData, VehicleMotion, VehicleShot, VehicleState, VehicleWeapons};

use crate::{
    audio::PlaySound,
    combat::{EffectAssets, spawn_tracer},
    effects::{EffectLibrary, SpawnEffect},
    local_input::{LookState, cursor_locked},
    net::LocalSoldier,
    settings::{Action, Actions},
    vehicle_prediction::PredictedVehicle,
};

/// How far in the past vehicles are shown when connected to a remote server.
const INTERPOLATION_DELAY: f64 = 0.1;

pub struct ClientVehiclesPlugin;

impl Plugin for ClientVehiclesPlugin {
    fn build(&self, app: &mut App) {
        app.init_resource::<SeatRequest>()
            .init_resource::<FlightStick>()
            .add_observer(add_view)
            .add_plugins(crate::vehicle_hud::VehicleHudPlugin)
            .add_systems(
                PreUpdate,
                record_snapshots
                    .after(ClientSystems::Receive)
                    .run_if(in_state(ClientState::Connected)),
            )
            .init_resource::<VehicleSight>()
            .add_systems(Update, (read_seat_keys, receive_shots))
            .add_systems(PostUpdate, update_sight.after(VehicleViewSystems))
            .add_systems(Update, (fly, limit_gunner_aim).after(crate::local_input::LookSystems))
            .add_systems(FixedFirst, remember_joints.run_if(in_state(ClientState::Disconnected)))
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
    /// World space, m/s.
    pub velocity: Vec3,
    /// Rotor speed or throttle, 0..1.
    pub engine: f32,
    /// Afterburner meter, 0..1.
    pub boost: f32,
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
        PreviousJoints::default(),
    ));
}

/// Hosting: the joint angles before this tick, so turrets and control surfaces are drawn
/// between ticks like avian draws the body (at 60 Hz they visibly stepped at higher frame
/// rates, and a gunner's view rode on the stepping turret).
#[derive(Component, Default)]
struct PreviousJoints(Vec<[f32; 3]>);

fn remember_joints(mut vehicles: Query<(&VehicleState, &mut PreviousJoints)>) {
    for (state, mut previous) in &mut vehicles {
        previous.0.clone_from(&state.joints);
    }
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

#[allow(clippy::type_complexity)]
fn place_vehicles(
    time: Res<Time>,
    real: Res<Time<Real>>,
    fixed: Res<Time<Fixed>>,
    state: Res<State<ClientState>>,
    mut vehicles: Query<(
        &VehicleMotion,
        &VehicleState,
        &Snapshots,
        Option<&mut PredictedVehicle>,
        &PreviousJoints,
        &mut VehicleView,
        &mut Transform,
    )>,
) {
    let connected = *state.get() == ClientState::Connected;
    let at = real.elapsed_secs_f64() - INTERPOLATION_DELAY;
    let alpha = fixed.overstep_fraction();
    for (motion, current, snapshots, predicted, previous, mut view, mut transform) in &mut vehicles {
        if let Some(mut predicted) = predicted {
            // The vehicle we drive, predicted (see `vehicle_prediction`).
            *transform = predicted.transform(alpha, time.delta_secs());
            view.joints = between_ticks(&predicted.previous_joints, &predicted.state.joints, alpha);
            view.wheels.clone_from(&predicted.state.wheels);
            view.velocity = predicted.velocity();
            view.speed = view.velocity.dot(transform.rotation * Vec3::NEG_Z);
            view.engine = predicted.state.engine;
            view.boost = predicted.state.boost;
        } else if connected {
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
            view.velocity = a.0.velocity.lerp(b.0.velocity, t);
            view.engine = a.1.engine + (b.1.engine - a.1.engine) * t;
            view.boost = b.1.boost;
        } else {
            // Hosting: avian already interpolates the body's transform between ticks.
            view.joints = between_ticks(&previous.0, &current.joints, alpha);
            view.wheels.clone_from(&current.wheels);
            view.speed = motion.forward_speed();
            view.velocity = motion.velocity;
            view.engine = current.engine;
            view.boost = current.boost;
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

/// Joint angles `alpha` of the way through the tick from `previous` (which may be missing, a
/// new vehicle) to `current`.
fn between_ticks(previous: &[[f32; 3]], current: &[[f32; 3]], alpha: f32) -> Vec<[f32; 3]> {
    if previous.len() == current.len() {
        lerp_joints(previous, current, alpha)
    } else {
        current.to_vec()
    }
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
    actions: Actions,
    mut request: ResMut<SeatRequest>,
    seated: Query<&Seated, With<LocalSoldier>>,
) {
    if let Some(index) = (1..=8).position(|seat| actions.just_pressed(Action::Seat(seat))) {
        request.0 = index as u8 + 1;
    }
    // Done once we sit there, or when we're not in a vehicle at all.
    match seated.single() {
        Ok(seated) if request.0 == seated.seat + 1 => request.0 = 0,
        Err(_) => request.0 = 0,
        _ => {}
    }
}

/// The pilot's stick: the mouse and the arrow keys move it, and it centres itself when let
/// go. Holding free look turns the view instead.
#[derive(Resource, Default)]
pub struct FlightStick {
    /// Piloting an aircraft: the mouse flies instead of looking around.
    pub active: bool,
    /// x = roll right, y = nose up, -1..1.
    pub stick: Vec2,
    /// The mouse's share of the stick.
    mouse: Vec2,
    /// Free look: the view's yaw (left positive) and pitch away from straight ahead.
    pub look: Vec2,
}

/// Stick travel per mouse count at sensitivity 1, and how quickly it centres (1/s).
const STICK_PER_COUNT: f32 = 0.012;
const STICK_CENTERING: f32 = 3.0;
/// How quickly the free-look view swings back ahead once let go (1/s).
const LOOK_RETURN: f32 = 6.0;

/// Whether the local player pilots this seat: the first seat of a jet or helicopter.
pub fn pilots(data: &VehicleData, seat: u8) -> bool {
    seat == 0 && data.0.desc.category.flies()
}

#[allow(clippy::too_many_arguments)]
fn fly(
    time: Res<Time>,
    mouse: Res<AccumulatedMouseMotion>,
    actions: Actions,
    cursor: Single<&CursorOptions>,
    settings: Res<crate::settings::Settings>,
    mut look: ResMut<LookState>,
    mut flight: ResMut<FlightStick>,
    seated: Query<&Seated, With<LocalSoldier>>,
    vehicles: Query<(&VehicleView, &VehicleData)>,
) {
    let pilot = seated
        .single()
        .ok()
        .and_then(|s| vehicles.get(s.vehicle).ok().filter(|(_, d)| pilots(d, s.seat)));
    flight.active = pilot.is_some();
    let Some((view, data)) = pilot else {
        flight.stick = Vec2::ZERO;
        flight.mouse = Vec2::ZERO;
        flight.look = Vec2::ZERO;
        return;
    };
    let dt = time.delta_secs();
    let delta = if cursor_locked(&cursor) { mouse.delta } else { Vec2::ZERO };
    if actions.pressed(Action::FreeLook) {
        // Free look turns the view like looking around on foot.
        let vertical = if look.invert_y { -delta.y } else { delta.y };
        let sensitivity = look.sensitivity;
        flight.look.x -= delta.x * sensitivity;
        flight.look.y = (flight.look.y - vertical * sensitivity).clamp(-1.4, 1.4);
        if let Some(gamepad) = actions.gamepad() {
            let stick = crate::local_input::deadzone(gamepad.right_stick(), settings.gamepad.look_deadzone);
            let gv = if settings.gamepad.invert_look_y { -stick.y } else { stick.y };
            flight.look.x -= stick.x * sensitivity * 60.0 * dt;
            flight.look.y = (flight.look.y - gv * sensitivity * 60.0 * dt).clamp(-1.4, 1.4);
        }
    } else {
        // The mouse moves the stick: up raises the nose (like looking up), unless the invert
        // pitch setting makes it a flight stick (see below).
        let scale = STICK_PER_COUNT * look.sensitivity / crate::local_input::BASE_SENSITIVITY;
        flight.mouse = (flight.mouse + Vec2::new(delta.x, -delta.y) * scale).clamp(Vec2::NEG_ONE, Vec2::ONE);
        flight.look *= (-LOOK_RETURN * dt).exp();
    }
    flight.mouse *= (-STICK_CENTERING * dt).exp();
    let keys = Vec2::new(
        actions.axis(Action::RollRight, Action::RollLeft),
        actions.axis(Action::PitchUp, Action::PitchDown),
    );
    // Gamepad: the right stick is the flight stick directly (held, not accumulated like the
    // mouse), unless free look is redirecting it to look around instead.
    let gamepad_stick = if actions.pressed(Action::FreeLook) {
        Vec2::ZERO
    } else {
        actions
            .gamepad()
            .map(|gamepad| crate::local_input::deadzone(gamepad.right_stick(), settings.gamepad.look_deadzone))
            .unwrap_or_default()
    };
    let helicopter = data.0.desc.category == game_data::VehicleCategory::Helicopter;
    flight.stick = compose_stick(flight.mouse, keys, gamepad_stick, settings.invert_pitch(helicopter));
    // The soldier looks where the camera does (the server aims with it, and it's the
    // facing when getting out).
    let (yaw, pitch, _) = flight_view(view.transform.rotation, flight.look).to_euler(EulerRot::YXZ);
    look.yaw = yaw;
    look.pitch = pitch.clamp(-1.5, 1.5);
}

/// The flight stick from the mouse's share, the keys (pitch-up key +y) and the gamepad's
/// right stick (pushed forward +y), all "up raises the nose"; inverted (flight-stick style,
/// BF2's default) they all push it down.
fn compose_stick(mouse: Vec2, keys: Vec2, gamepad: Vec2, invert: bool) -> Vec2 {
    let mut stick = (mouse + keys + gamepad).clamp(Vec2::NEG_ONE, Vec2::ONE);
    if invert {
        stick.y = -stick.y;
    }
    stick
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pitch_keys_follow_the_mouse() {
        // Up on the keys, the mouse (its share goes up when moved up) and the gamepad raises
        // the nose; inverted, all of them lower it.
        let up = Vec2::Y;
        for (mouse, keys, gamepad) in [(up, Vec2::ZERO, Vec2::ZERO), (Vec2::ZERO, up, Vec2::ZERO), (Vec2::ZERO, Vec2::ZERO, up)] {
            assert_eq!(compose_stick(mouse, keys, gamepad, false).y, 1.0);
            assert_eq!(compose_stick(mouse, keys, gamepad, true).y, -1.0);
        }
        // Roll never inverts.
        assert_eq!(compose_stick(Vec2::X, Vec2::ZERO, Vec2::ZERO, true).x, 1.0);
    }

    #[test]
    fn default_pitch_keys() {
        use crate::settings::{Action, Binding, Settings};
        let settings = Settings::default();
        assert_eq!(settings.binding(Action::PitchUp), Some(Binding::Key(KeyCode::ArrowUp)));
        assert_eq!(settings.binding(Action::PitchDown), Some(Binding::Key(KeyCode::ArrowDown)));
        assert!(!settings.invert_jet_pitch && !settings.invert_heli_pitch);
    }
}

/// The pilot's view: along the aircraft, turned by free look.
pub fn flight_view(vehicle: Quat, look: Vec2) -> Quat {
    vehicle * Quat::from_euler(EulerRot::YXZ, look.x, look.y, 0.0)
}

/// In a seat that doesn't aim (driving, riding along) the view turns with the vehicle, so
/// looking ahead stays looking ahead through corners. Gunners keep their aim steady.
fn follow_vehicle_heading(
    seated: Query<&Seated, With<LocalSoldier>>,
    vehicles: Query<(&VehicleView, &VehicleData)>,
    flight: Res<FlightStick>,
    mut look: ResMut<LookState>,
    mut last: Local<Option<(Entity, u8, f32)>>,
) {
    let Some((seated, view, data)) = seated
        .single()
        .ok()
        .filter(|_| !flight.active)
        .and_then(|s| vehicles.get(s.vehicle).ok().map(|(v, d)| (s, v, d)))
    else {
        *last = None;
        return;
    };
    let now = heading(view.transform.rotation);
    match *last {
        Some((vehicle, seat, before)) if vehicle == seated.vehicle && seat == seated.seat => {
            if !data.0.seat_aims(seat as usize) {
                look.yaw += wrap(now - before);
            }
        }
        // A new seat: look the way its camera faces (firing ports look out sideways).
        _ => {
            let model = &data.0;
            let facing = model.desc.seats.get(seated.seat as usize).and_then(|s| s.camera.as_ref()).map_or(
                view.transform.rotation,
                |camera| {
                    let transforms = model.part_transforms(&view.joints);
                    view.transform.rotation * model.attachment(&transforms, &camera.attachment).rotation
                },
            );
            let forward = facing * Vec3::NEG_Z;
            look.yaw = (-forward.x).atan2(-forward.z);
            look.pitch = forward.y.clamp(-1.0, 1.0).asin().clamp(-1.2, 1.2);
        }
    }
    *last = Some((seated.vehicle, seated.seat, now));
}

/// How far a gunner's aim may turn from the hull (radians, hull space): the limits of the
/// turret (yaw) and gun (pitch) the seat aims, if they are limited.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct AimLimits {
    pub yaw: Option<(f32, f32)>,
    pub pitch: Option<(f32, f32)>,
}

/// The aim limits of a seat: of the joints its camera rides on, else of the first ones the
/// seat turns.
pub fn aim_limits(model: &game_shared::vehicle::VehicleModel, seat: usize) -> AimLimits {
    use game_data::JointInput;
    let desc = &model.desc;
    let mut chain = Vec::new();
    let mut part = desc.seats.get(seat).and_then(|s| s.camera.as_ref()).map(|c| c.attachment.part as usize);
    while let Some(index) = part {
        chain.push(index);
        part = desc.parts.get(index).and_then(|p| p.parent).map(|p| p as usize);
    }
    let find = |input: JointInput| {
        let axis_of = |index: usize| {
            let joint = desc.parts.get(index)?.joint.as_ref().filter(|j| j.seat as usize == seat)?;
            joint.axes.iter().find(|a| a.input == Some(input))
        };
        chain
            .iter()
            .find_map(|i| axis_of(*i))
            .or_else(|| (0..desc.parts.len()).find_map(axis_of))
            .filter(|a| a.limited())
            .map(|a| (a.min.min(a.max).to_radians(), a.max.max(a.min).to_radians()))
    };
    AimLimits {
        yaw: find(JointInput::AimYaw),
        pitch: find(JointInput::AimPitch),
    }
}

/// A gunner's view turns with the mouse at once (the turret and gun follow at their BF2
/// speeds; the HUD marks where the gun points meanwhile), but no further than they can
/// reach: looking past the gun's depression, say, would leave the mouse turning nothing on
/// the way back.
fn limit_gunner_aim(
    seated: Query<&Seated, With<LocalSoldier>>,
    vehicles: Query<(&VehicleView, &VehicleData)>,
    flight: Res<FlightStick>,
    mut look: ResMut<LookState>,
) {
    let Some((seated, view, data)) = seated
        .single()
        .ok()
        .filter(|_| !flight.active)
        .and_then(|s| vehicles.get(s.vehicle).ok().map(|(v, d)| (s, v, d)))
    else {
        return;
    };
    let seat = seated.seat as usize;
    if !data.0.seat_aims(seat) {
        return;
    }
    let limits = aim_limits(&data.0, seat);
    if limits == AimLimits::default() {
        return;
    }
    let hull = view.transform.rotation;
    let direction = hull.inverse() * (Quat::from_euler(EulerRot::YXZ, look.yaw, look.pitch, 0.0) * Vec3::NEG_Z);
    let (yaw, pitch) = ((-direction.x).atan2(-direction.z), direction.y.clamp(-1.0, 1.0).asin());
    let clamp = |angle: f32, limits: Option<(f32, f32)>| limits.map_or(angle, |(min, max)| angle.clamp(min, max));
    let (limited_yaw, limited_pitch) = (clamp(yaw, limits.yaw), clamp(pitch, limits.pitch));
    if (limited_yaw - yaw).abs() < 1e-4 && (limited_pitch - pitch).abs() < 1e-4 {
        return;
    }
    let world = hull * (Quat::from_euler(EulerRot::YXZ, limited_yaw, limited_pitch, 0.0) * Vec3::NEG_Z);
    look.yaw = (-world.x).atan2(-world.z);
    look.pitch = world.y.clamp(-1.0, 1.0).asin();
}

/// How quickly the aircraft chase camera turns after the aircraft (1/s).
const CHASE_STIFFNESS: f32 = 6.0;
/// The pilot's camera looks this far ahead into the aircraft's turn (seconds of its turn
/// rate, at most `LEAD_MAX` radians); the cockpit view half as far.
const LEAD_TIME: f32 = 0.25;
const LEAD_MAX: f32 = 0.2;
/// How quickly the measured turn rate follows the aircraft (1/s).
const TURN_RATE_SMOOTHING: f32 = 8.0;
/// The chase camera pulls back this share further at 100 m/s.
const CHASE_SPEED_STRETCH: f32 = 0.2;
/// Stall buffet: the view shakes up to this much (radians).
const BUFFET: f32 = 0.012;

/// The pilot camera's memory from frame to frame.
#[derive(Default)]
pub struct PilotCamera {
    smoothed: Option<Quat>,
    last: Option<Quat>,
    /// The aircraft's turn rate (hull space), smoothed.
    turn_rate: Vec3,
    time: f32,
}

/// Where the pilot's camera goes: the chase camera trails the aircraft's rotation a little,
/// pulls back with speed and looks into the turn; the cockpit view is fixed to the aircraft,
/// turned a little into the turn. Free look turns both. A stalling jet shakes.
#[allow(clippy::too_many_arguments)]
pub fn pilot_camera(
    camera: &mut PilotCamera,
    seat: &SeatView,
    view: &VehicleView,
    data: &VehicleData,
    look: Vec2,
    third_person: bool,
    zoom: f32,
    altitude: f32,
    dt: f32,
) -> (Transform, Option<(Vec3, Vec3)>) {
    let rotation = view.transform.rotation;
    if let Some(last) = camera.last
        && dt > 0.0
    {
        let turn = rotation.inverse() * ((rotation * last.inverse()).to_scaled_axis() / dt);
        camera.turn_rate += (turn - camera.turn_rate) * (1.0 - (-TURN_RATE_SMOOTHING * dt).exp());
    }
    camera.last = Some(rotation);
    camera.time += dt;
    let wanted = flight_view(rotation, look);
    let smoothed = camera
        .smoothed
        .map_or(wanted, |c| c.slerp(wanted, 1.0 - (-CHASE_STIFFNESS * dt).exp()));
    camera.smoothed = Some(smoothed);
    let lead = Vec2::new(camera.turn_rate.y, camera.turn_rate.x) * LEAD_TIME;
    let lead = lead.clamp_length_max(LEAD_MAX);
    let body = game_shared::flight::BodyState {
        position: view.transform.translation,
        rotation,
        velocity: view.velocity,
        angular_velocity: Vec3::ZERO,
    };
    let stall = if game_shared::flight::jet_airborne(altitude) {
        game_shared::flight::jet_stall(&data.0.desc, &body)
    } else {
        0.0
    };
    let t = camera.time;
    let buffet = Vec2::new((t * 71.0).sin() + (t * 43.0).sin(), (t * 59.0).sin() + (t * 37.0).cos()) * 0.5 * BUFFET * stall;
    let turn = |share: f32| Quat::from_euler(EulerRot::YXZ, lead.x * share + buffet.x, lead.y * share + buffet.y, 0.0);
    if third_person {
        let stretch = 1.0 + CHASE_SPEED_STRETCH * (view.velocity.length() / 100.0).min(1.5);
        let offset = smoothed * Vec3::new(0.0, seat.chase_height, seat.chase_distance * 0.8 * zoom * stretch);
        // The caller pulls the camera in if the world is in the way.
        (
            Transform::from_translation(seat.vehicle.translation + offset).with_rotation(smoothed * turn(1.0)),
            Some((seat.vehicle.translation, offset)),
        )
    } else {
        (Transform::from_translation(seat.eye).with_rotation(wanted * turn(0.5)), None)
    }
}

/// BF2's sight of the weapon fired from our seat (its vehicle HUD: reticle, sight frame), over
/// the first-person view. The soldier's crosshair hides meanwhile.
#[derive(Resource, Default)]
pub struct VehicleSight {
    pub active: bool,
    /// What is drawn: vehicle, gun and window size.
    shown: Option<(Entity, usize, UVec2)>,
    root: Option<Entity>,
}

#[allow(clippy::too_many_arguments)]
fn update_sight(
    mut commands: Commands,
    mut sight: ResMut<VehicleSight>,
    third_person: Res<crate::camera::ThirdPerson>,
    window: Single<&Window, With<bevy::window::PrimaryWindow>>,
    asset_server: Res<AssetServer>,
    seated: Query<&Seated, With<LocalSoldier>>,
    vehicles: Query<(&VehicleData, Option<&VehicleWeapons>)>,
) {
    // The gun of our seat whose sight to show: the chosen one on the main trigger.
    let wanted = seated
        .single()
        .ok()
        .filter(|_| !third_person.0)
        .and_then(|s| {
            let (data, weapons) = vehicles.get(s.vehicle).ok()?;
            let desc = &data.0.desc;
            let guns = desc.weapons.iter().enumerate().filter(|(_, w)| w.seat == s.seat as u32 && !w.sight.is_empty());
            let selected = |i: usize| weapons.and_then(|w| w.guns.get(i)).is_some_and(|g| g.selected);
            let mut guns: Vec<(usize, bool)> = guns.map(|(i, w)| (i, w.alt_fire)).collect();
            guns.sort_by_key(|&(i, alt)| (alt, !selected(i)));
            guns.first().map(|&(i, _)| (s.vehicle, i))
        });
    let size = UVec2::new(window.width() as u32, window.height() as u32);
    let key = wanted.map(|(vehicle, gun)| (vehicle, gun, size));
    if key == sight.shown {
        return;
    }
    sight.shown = key;
    sight.active = key.is_some();
    if let Some(root) = sight.root.take() {
        commands.entity(root).despawn();
    }
    let Some((vehicle, gun)) = wanted else {
        return;
    };
    let Some(pictures) = vehicles.get(vehicle).ok().and_then(|(d, _)| d.0.desc.weapons.get(gun)).map(|w| w.sight.clone()) else {
        return;
    };
    // BF2's HUD is laid out on an 800x600 screen: scaled with the height, centred.
    let scale = window.height() / 600.0;
    let left = (window.width() - 800.0 * scale) * 0.5;
    let root = commands
        .spawn((
            Node {
                position_type: PositionType::Absolute,
                width: percent(100),
                height: percent(100),
                ..default()
            },
            GlobalZIndex(-1),
            Pickable::IGNORE,
        ))
        .id();
    for picture in pictures {
        let [x, y, w, h] = picture.rect;
        let [r, g, b, a] = picture.color;
        commands.spawn((
            ImageNode::new(asset_server.load(format!("imported://{}", picture.texture))).with_color(Color::srgba(r, g, b, a)),
            Node {
                position_type: PositionType::Absolute,
                left: px(left + x * scale),
                top: px(y * scale),
                width: px(w * scale),
                height: px(h * scale),
                ..default()
            },
            ChildOf(root),
        ));
    }
    sight.root = Some(root);
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
        None => {
            let height = model.head_height(seated.seat as usize);
            model.seat_transform(&transforms, seated.seat as usize) * Transform::from_xyz(0.0, height, 0.0)
        }
    };
    let (chase_distance, chase_height) = seat
        .camera
        .as_ref()
        .map_or((12.0, 1.0), |c| (c.chase_distance, c.chase_offset[1]));
    let world = view.transform * local;
    Some(SeatView {
        eye: world.translation,
        vehicle: view.transform,
        chase_distance,
        chase_height,
    })
}

/// A gun's template name for people: `air_j10_archerlauncher` on the J-10 is `Archerlauncher`.
pub fn readable_name(name: &str, vehicle: &str) -> String {
    let short = name.strip_prefix(vehicle).map_or(name, |s| s.trim_start_matches('_'));
    let mut words = short.replace('_', " ").trim().to_string();
    if let Some(first) = words.get_mut(..1) {
        first.make_ascii_uppercase();
    }
    words
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
        // Shells and missiles are replicated projectiles; bullets are tracers.
        if !weapon.projectile.is_object() {
            spawn_tracer(&mut commands, &assets, shot.origin, shot.direction, weapon, Some(shot.vehicle));
        }
        if let Some((muzzle, _)) = library.as_deref().and_then(|l| l.muzzle(&weapon.name)) {
            effects.write(SpawnEffect::new(muzzle, shot.origin).with_forward(shot.direction));
        }
        if let Some(sound) = &weapon.sounds.fire_3p {
            sounds.write(PlaySound::at(sound.clone(), shot.origin).emitter(shot.vehicle).reason("vehicle gun"));
        }
    }
}
