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
use std::fmt::Write as _;

use avian3d::prelude::{SpatialQuery, SpatialQueryFilter};
use game_data::VehicleCategory;
use game_shared::{
    physics::GameLayer,
    vehicle::{Seated, Vehicle, VehicleData, VehicleHealth, VehicleMotion, VehicleShot, VehicleState, VehicleWeapons},
};

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
            .add_systems(Startup, spawn_hud)
            .add_systems(
                PreUpdate,
                record_snapshots
                    .after(ClientSystems::Receive)
                    .run_if(in_state(ClientState::Connected)),
            )
            .init_resource::<VehicleSight>()
            .add_systems(Update, (read_seat_keys, update_hud, receive_shots))
            .add_systems(PostUpdate, update_sight.after(VehicleViewSystems))
            .add_systems(Update, fly.after(crate::local_input::LookSystems))
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
        &mut VehicleView,
        &mut Transform,
    )>,
) {
    let connected = *state.get() == ClientState::Connected;
    let at = real.elapsed_secs_f64() - INTERPOLATION_DELAY;
    for (motion, current, snapshots, predicted, mut view, mut transform) in &mut vehicles {
        if let Some(mut predicted) = predicted {
            // The vehicle we drive, predicted (see `vehicle_prediction`).
            *transform = predicted.transform(fixed.overstep_fraction(), time.delta_secs());
            view.joints.clone_from(&predicted.state.joints);
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
            view.joints.clone_from(&current.joints);
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

/// The pilot's stick: the mouse and the arrow keys move it, and it centres itself when let
/// go. Holding free look turns the view instead.
#[derive(Resource, Default)]
pub struct FlightStick {
    /// Piloting an aircraft: the mouse flies instead of looking around.
    pub active: bool,
    /// x = roll right, y = pitch up (pulled back), -1..1.
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
    let Some((view, _)) = pilot else {
        flight.stick = Vec2::ZERO;
        flight.mouse = Vec2::ZERO;
        flight.look = Vec2::ZERO;
        return;
    };
    let dt = time.delta_secs();
    let delta = if cursor_locked(&cursor) { mouse.delta } else { Vec2::ZERO };
    let vertical = if look.invert_y { -delta.y } else { delta.y };
    if actions.pressed(Action::FreeLook) {
        let sensitivity = look.sensitivity;
        flight.look.x -= delta.x * sensitivity;
        flight.look.y = (flight.look.y - vertical * sensitivity).clamp(-1.4, 1.4);
    } else {
        let scale = STICK_PER_COUNT * look.sensitivity / crate::local_input::BASE_SENSITIVITY;
        flight.mouse = (flight.mouse + Vec2::new(delta.x, vertical) * scale).clamp(Vec2::NEG_ONE, Vec2::ONE);
        flight.look *= (-LOOK_RETURN * dt).exp();
    }
    flight.mouse *= (-STICK_CENTERING * dt).exp();
    let keys = Vec2::new(
        actions.axis(Action::RollRight, Action::RollLeft),
        actions.axis(Action::PitchUp, Action::PitchDown),
    );
    flight.stick = (flight.mouse + keys).clamp(Vec2::NEG_ONE, Vec2::ONE);
    // The soldier looks where the camera does (the server aims with it, and it's the
    // facing when getting out).
    let (yaw, pitch, _) = flight_view(view.transform.rotation, flight.look).to_euler(EulerRot::YXZ);
    look.yaw = yaw;
    look.pitch = pitch.clamp(-1.5, 1.5);
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
    /// Gunners' cameras ride on the turret or gun they aim: the view turns with it (at its
    /// speed, as in BF2) rather than with the mouse.
    pub aimed: Option<Quat>,
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
    let aims = |axis: &game_data::JointAxis| {
        matches!(axis.input, Some(game_data::JointInput::AimYaw | game_data::JointInput::AimPitch))
    };
    let mut part = seat.camera.as_ref().map(|c| c.attachment.part as usize);
    let mut aimed = false;
    while let Some(index) = part {
        let desc = &model.desc.parts[index];
        aimed |= desc.joint.as_ref().is_some_and(|j| j.axes.iter().any(aims));
        part = desc.parent.map(|p| p as usize);
    }
    let world = view.transform * local;
    Some(SeatView {
        eye: world.translation,
        aimed: aimed.then_some(world.rotation),
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
            margin: UiRect::left(px(-410)),
            width: px(820),
            justify_content: JustifyContent::Center,
            ..default()
        },
        TextLayout::justify(Justify::Center),
    ));
}

/// The vehicle lines of the HUD: vehicle, seat, speed and hit points (aircraft add altitude,
/// throttle and afterburner), then the seat's guns with rounds, heat and lock.
fn update_hud(
    seated: Query<&Seated, With<LocalSoldier>>,
    vehicles: Query<(&VehicleView, &VehicleData, Option<&VehicleHealth>, Option<&VehicleWeapons>)>,
    spatial: SpatialQuery,
    mut text: Single<&mut Text, With<VehicleHudText>>,
) {
    let lines = seated
        .single()
        .ok()
        .and_then(|s| vehicles.get(s.vehicle).ok().map(|(v, d, h, w)| (s, v, d, h, w)))
        .map(|(seated, view, data, health, weapons)| {
            let model = &data.0;
            let desc = &model.desc;
            let seat = seated.seat as usize;
            let role = match seat {
                0 if desc.category.flies() => "Pilot",
                0 if desc.category == VehicleCategory::Stationary => "Gunner",
                0 => "Driver",
                _ if model.seat_aims(seat) => "Gunner",
                _ => "Passenger",
            };
            let hit_points = health.map_or(desc.hit_points, |h| h.current);
            let mut line = format!(
                "{}   |   {role} ({}/{})   |   {:.0} km/h",
                desc.display_name,
                seat + 1,
                desc.seats.len(),
                view.velocity.length() * 3.6,
            );
            if desc.category.flies() {
                let filter = SpatialQueryFilter::from_mask(GameLayer::World);
                let altitude = spatial
                    .cast_ray(view.transform.translation, Dir3::NEG_Y, 2000.0, true, &filter)
                    .map_or(2000.0, |hit| hit.distance);
                let _ = write!(line, "   |   ALT {altitude:.0} m   |   ");
                let _ = match desc.category {
                    VehicleCategory::Air => write!(line, "THR {:.0}%", view.engine * 100.0),
                    _ => write!(line, "ROTOR {:.0}%", view.engine * 100.0),
                };
                if desc.afterburner.is_some() {
                    let _ = write!(line, "   AB {:.0}%", view.boost * 100.0);
                }
            }
            let _ = write!(line, "   |   {:.0} HP", hit_points.ceil());
            // The guns this seat fires.
            let guns: Vec<String> = desc
                .weapons
                .iter()
                .zip(&model.guns)
                .enumerate()
                .filter(|(_, (w, _))| w.seat as usize == seat)
                .map(|(i, (w, gun))| {
                    let status = weapons.and_then(|s| s.guns.get(i)).copied().unwrap_or_default();
                    // Unlocalized names are template names.
                    let unnamed = gun.display_name.is_empty()
                        || gun.display_name == desc.display_name
                        || gun.display_name.contains('_');
                    let name = if unnamed { readable_name(&gun.name, &desc.name) } else { gun.display_name.clone() };
                    let mut entry = format!("{}{}{name}", if status.selected { "> " } else { "  " }, if w.alt_fire { "[2] " } else { "" });
                    let _ = match (status.reloading, status.rounds) {
                        (true, _) => write!(entry, "  reloading"),
                        (false, u16::MAX) => Ok(()),
                        (false, rounds) => write!(entry, "  {rounds}"),
                    };
                    if gun.fire.overheat.is_some() {
                        let _ = match status.heat {
                            255 => write!(entry, "  OVERHEATED"),
                            heat => write!(entry, "  heat {:.0}%", heat as f32 / 2.54),
                        };
                    }
                    if gun.fire.lock.is_some() {
                        let _ = match status.lock {
                            255 => write!(entry, "  LOCKED"),
                            0 => Ok(()),
                            lock => write!(entry, "  locking {:.0}%", lock as f32 / 2.55),
                        };
                    }
                    entry
                })
                .collect();
            if guns.is_empty() { line } else { format!("{line}
{}", guns.join("      ")) }
        })
        .unwrap_or_default();
    if text.0 != lines {
        text.0 = lines;
    }
}

/// A gun's template name for people: `air_j10_archerlauncher` on the J-10 is `Archerlauncher`.
fn readable_name(name: &str, vehicle: &str) -> String {
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
