//! Vehicles: descriptions, replicated state, physics bodies and the driving simulation.
//!
//! The server simulates every vehicle as an avian rigid body (a compound of convex hulls from
//! the hull's collision mesh) pushed around by raycast suspension springs, tyre or track
//! friction and engine forces, driven by the [`InputFrame`]s of its occupants. Clients get
//! [`VehicleMotion`] and [`VehicleState`] replicated and show the vehicle slightly in the past
//! (see the client's `vehicles` module); there is no vehicle prediction yet.

use std::{
    collections::HashMap,
    path::Path,
    sync::Arc,
};

use avian3d::prelude::*;
use bevy::{ecs::entity::MapEntities, prelude::*};
use bevy_replicon::prelude::*;
use game_data::{DriveKind, JointInput, VehicleDesc, WeaponDesc};
use serde::{Deserialize, Serialize};

use crate::{
    config::GamePaths,
    input::{Buttons, InputFrame},
    physics::GameLayer,
};

pub struct VehiclePlugin;

impl Plugin for VehiclePlugin {
    fn build(&self, app: &mut App) {
        app.init_resource::<VehicleLibrary>()
            .add_observer(add_vehicle_physics)
            .add_systems(
                FixedUpdate,
                simulate_vehicles
                    .in_set(VehicleSystems::Simulate)
                    .run_if(in_state(ClientState::Disconnected)),
            )
            .add_systems(
                FixedPostUpdate,
                record_motion
                    .in_set(VehicleSystems::Record)
                    .after(PhysicsSystems::Writeback)
                    .run_if(in_state(ClientState::Disconnected)),
            );
    }
}

/// Where vehicle systems run. `Simulate` (FixedUpdate) applies forces from this tick's seat
/// inputs, `Record` (FixedPostUpdate, after physics) publishes the new state.
#[derive(SystemSet, Debug, Clone, PartialEq, Eq, Hash)]
pub enum VehicleSystems {
    Simulate,
    Record,
}

/// Standard gravity; vehicles multiply it by their gravity modifier.
const GRAVITY: f32 = 9.81;

/// A vehicle. Replicated; the description is `vehicles/<template>.ron`.
#[derive(Component, Serialize, Deserialize, Clone, Debug, PartialEq)]
#[require(Transform, VehicleMotion, VehicleState)]
pub struct Vehicle {
    pub template: String,
}

/// Where the vehicle is. Authoritative on the server, replicated.
#[derive(Component, Serialize, Deserialize, Clone, Copy, Debug, PartialEq)]
pub struct VehicleMotion {
    pub position: Vec3,
    pub rotation: Quat,
    pub velocity: Vec3,
}

impl Default for VehicleMotion {
    fn default() -> Self {
        Self {
            position: Vec3::ZERO,
            rotation: Quat::IDENTITY,
            velocity: Vec3::ZERO,
        }
    }
}

impl VehicleMotion {
    pub fn transform(&self) -> Transform {
        Transform::from_translation(self.position).with_rotation(self.rotation)
    }

    /// Speed along the hull's forward axis, m/s.
    pub fn forward_speed(&self) -> f32 {
        self.velocity.dot(self.rotation * Vec3::NEG_Z)
    }
}

/// The moving parts. Replicated.
#[derive(Component, Serialize, Deserialize, Clone, Debug, Default, PartialEq)]
pub struct VehicleState {
    /// Yaw, pitch, roll in radians of every part with a joint, in part order.
    pub joints: Vec<[f32; 3]>,
    /// How far each wheel hangs below its rest position, meters (negative: pushed up).
    pub wheels: Vec<f32>,
}

/// Hit points. Replicated. At 0 the vehicle is a wreck.
#[derive(Component, Serialize, Deserialize, Clone, Copy, Debug, PartialEq)]
pub struct VehicleHealth {
    pub current: f32,
    pub max: f32,
}

impl VehicleHealth {
    pub fn wrecked(&self) -> bool {
        self.current <= 0.0
    }
}

/// How much of a projectile's damage a vehicle material takes, from BF2's damage table
/// (`materialManagerSettings.con`, see gameplay-data.md §7.2): projectile materials 38..113
/// against hull armour 26..30 and blast sensitivity 71/72/110. Cells marked "default" in the
/// table are 1, missing ones 0. A stopgap until the whole table is imported.
pub fn armor_damage_modifier(projectile: u32, target: u32) -> f32 {
    const TARGETS: [u32; 8] = [26, 27, 28, 29, 30, 71, 72, 110];
    let row: [f32; 8] = match projectile {
        38 => [0.3, 0.0, 0.0, 0.0, 0.0, 0.0, 0.0, 0.0],
        39 => [0.25, 0.0, 0.0, 0.0, 0.0, 0.0, 0.0, 0.0],
        40 => [0.5, 0.25, 0.0, 0.0, 0.0, 0.05, 0.0, 0.05],
        41 => [0.0, 0.0, 0.0, 0.0, 0.0, 0.0, 0.0, 0.4],
        42 => [1.34, 0.75, 0.6, 0.2, 0.15, 0.04, 0.025, 2.0],
        87 | 113 => [0.8, 0.35, 0.05, 0.005, 0.0, 1.0, 0.05, 0.0],
        57 => [1.79, 0.1, 0.1, 0.1, 0.0, 1.0, 0.2, 0.0],
        88 => [1.0, 0.5, 0.5, 0.25, 0.15, 1.0, 1.0, 0.0],
        43 => [1.6, 0.9, 0.9, 0.7, 0.52, 0.0, 0.0, 0.0],
        45 => [1.34, 1.0, 0.5, 0.25, 0.1, 1.0, 1.0, 0.0],
        46 => [0.0, 0.0, 0.0, 0.0, 0.0, 0.1, 0.1, 0.1],
        49 => [1.0, 1.0, 1.0, 1.0, 1.0, 0.0, 0.0, 0.0],
        50 => [0.5, 0.25, 0.25, 1.0, 1.0, 0.5, 0.0, 2.0],
        52 => [1.59, 0.93, 0.93, 0.62, 0.475, 0.0, 0.0, 0.0],
        53 => [0.25, 0.25, 0.25, 0.15, 0.1, 0.25, 0.15, 0.5],
        55 => [1.2, 0.975, 0.6, 0.6, 0.6, 0.0, 0.0, 0.5],
        56 => [1.0, 1.0, 1.0, 1.0, 1.0, 2.0, 1.4, 2.0],
        69 => [1.0, 1.0, 1.0, 1.0, 1.0, 2.6, 0.4, 5.0],
        70 => [0.25, 0.25, 0.25, 0.0, 0.0, 1.0, 0.05, 2.0],
        80 => [1.0, 1.0, 1.0, 1.0, 1.0, 1.0, 0.7, 0.0],
        109 => [0.5, 0.5, 0.5, 0.0, 0.0, 0.4, 0.2, 2.0],
        _ => return 0.0,
    };
    TARGETS.iter().position(|t| *t == target).map_or(0.0, |i| row[i])
}

/// Blast damage uses this material (BF2's `Explosion_blastwave`, what tank shells and most
/// rockets detonate with).
pub const BLAST_MATERIAL: u32 = 70;

/// A soldier riding in a vehicle. Replicated.
#[derive(Component, Serialize, Deserialize, Clone, Copy, Debug, PartialEq)]
pub struct Seated {
    #[entities]
    pub vehicle: Entity,
    pub seat: u8,
}

/// Server -> clients: a vehicle's gun fired (for the tracer and the sound).
#[derive(Message, Serialize, Deserialize, Clone, Debug, MapEntities)]
pub struct VehicleShot {
    #[entities]
    pub vehicle: Entity,
    /// Index into the vehicle's guns.
    pub gun: u8,
    pub origin: Vec3,
    pub direction: Vec3,
}

/// Server-side: this tick's input of each seat's occupant (`None` for empty seats).
#[derive(Component, Default, Debug)]
pub struct SeatInputs(pub Vec<Option<InputFrame>>);

/// The loaded description of a vehicle, with what we derive from it.
pub struct VehicleModel {
    pub desc: VehicleDesc,
    /// Per part: index into [`VehicleState::joints`].
    pub joint_index: Vec<Option<usize>>,
    pub joint_count: usize,
    /// Per part: rest transform relative to its parent.
    pub rest: Vec<Transform>,
    /// Hull collision (convex hulls of the hull and turret parts), if it has any.
    pub collider: Option<Collider>,
    /// The guns' weapon descriptions, shared with the projectiles they fire.
    pub guns: Vec<Arc<WeaponDesc>>,
}

impl VehicleModel {
    fn new(desc: VehicleDesc, paths: &GamePaths) -> Self {
        let mut joint_index = Vec::with_capacity(desc.parts.len());
        let mut joint_count = 0;
        for part in &desc.parts {
            joint_index.push(part.joint.as_ref().map(|_| {
                joint_count += 1;
                joint_count - 1
            }));
        }
        let rest = desc
            .parts
            .iter()
            .map(|p| crate::level::placement_transform(&p.placement))
            .collect();
        let guns = desc.weapons.iter().map(|w| Arc::new(w.weapon.clone())).collect();
        let mut model = Self {
            desc,
            joint_index,
            joint_count,
            rest,
            collider: None,
            guns,
        };
        model.collider = model.build_collider(paths);
        model
    }

    /// Transform of every part in hull space, with joints at the given angles.
    pub fn part_transforms(&self, joints: &[[f32; 3]]) -> Vec<Transform> {
        let mut out: Vec<Transform> = Vec::with_capacity(self.rest.len());
        for (i, part) in self.desc.parts.iter().enumerate() {
            let mut local = self.rest[i];
            if let Some(angles) = self.joint_index[i].and_then(|j| joints.get(j)) {
                local.rotation *= joint_rotation(*angles);
            }
            let parent = part.parent.map_or(Transform::IDENTITY, |p| out[p as usize]);
            out.push(parent * local);
        }
        out
    }

    /// A point on a part in hull space.
    pub fn attachment(&self, transforms: &[Transform], attachment: &game_data::Attachment) -> Transform {
        let part = transforms.get(attachment.part as usize).copied().unwrap_or_default();
        part * crate::level::placement_transform(&attachment.placement)
    }

    /// Where a seat's occupant sits, in hull space: the seat position, else its camera,
    /// else the seat part.
    pub fn seat_transform(&self, transforms: &[Transform], seat: usize) -> Transform {
        let Some(desc) = self.desc.seats.get(seat) else {
            return Transform::IDENTITY;
        };
        match (&desc.soldier, &desc.camera) {
            (Some(soldier), _) => self.attachment(transforms, soldier),
            (None, Some(camera)) => self.attachment(transforms, &camera.attachment),
            _ => transforms.get(desc.part as usize).copied().unwrap_or_default(),
        }
    }

    /// A gun's muzzle in hull space, facing where it fires (-Z).
    pub fn muzzle(&self, transforms: &[Transform], gun: usize) -> Transform {
        let Some(weapon) = self.desc.weapons.get(gun) else {
            return Transform::IDENTITY;
        };
        let part = transforms.get(weapon.part as usize).copied().unwrap_or_default();
        part * Transform::from_translation(Vec3::from_array(weapon.muzzle))
    }

    /// Where a seat looks from, in hull space: its camera, else just above the seat.
    pub fn eye(&self, transforms: &[Transform], seat: usize) -> Vec3 {
        match self.desc.seats.get(seat).and_then(|s| s.camera.as_ref()) {
            Some(camera) => self.attachment(transforms, &camera.attachment).translation,
            None => self.seat_transform(transforms, seat).translation + Vec3::Y * 0.6,
        }
    }

    /// Whether this seat turns any joint towards its aim (a gunner's seat).
    pub fn seat_aims(&self, seat: usize) -> bool {
        self.desc.parts.iter().filter_map(|p| p.joint.as_ref()).any(|j| {
            j.seat as usize == seat
                && j.axes.iter().any(|a| matches!(a.input, Some(JointInput::AimYaw | JointInput::AimPitch)))
        })
    }

    fn build_collider(&self, paths: &GamePaths) -> Option<Collider> {
        let transforms = self.part_transforms(&[]);
        let mut hulls = Vec::new();
        for (i, part) in self.desc.parts.iter().enumerate() {
            let Some(path) = part.collision.as_deref() else {
                continue;
            };
            if !self.desc.is_hull_part(i) {
                continue;
            }
            let points = match load_collision_points(&paths.find(path), part.collision_part) {
                Ok(points) => points,
                Err(err) => {
                    warn!("vehicle collision {path}: {err:#}");
                    continue;
                }
            };
            let points: Vec<Vec3> = points.into_iter().map(|p| transforms[i].transform_point(p)).collect();
            if let Some(hull) = Collider::convex_hull(points) {
                hulls.push((Vec3::ZERO, Quat::IDENTITY, hull));
            }
        }
        if hulls.is_empty() {
            // Nothing usable in the collision mesh: fall back to the bounds.
            let [min, max] = self.desc.physics.bounds.map(Vec3::from_array);
            let size = (max - min).max(Vec3::splat(0.2));
            return Some(Collider::compound(vec![(
                (min + max) * 0.5,
                Quat::IDENTITY,
                Collider::cuboid(size.x, size.y, size.z),
            )]));
        }
        Some(Collider::compound(hulls))
    }
}

/// Rotation of a joint: yaw about +Y, then pitch about +X, then roll about +Z.
pub fn joint_rotation(angles: [f32; 3]) -> Quat {
    Quat::from_euler(EulerRot::YXZ, angles[0], angles[1], angles[2])
}

/// Vertices of one part's vehicle collision mesh (falls back to the projectile mesh).
fn load_collision_points(path: &Path, part: u32) -> anyhow::Result<Vec<Vec3>> {
    let bytes = std::fs::read(path)?;
    let gltf = gltf::Gltf::from_slice(&bytes)?;
    let blob = gltf.blob.as_deref().unwrap_or_default();
    let mesh = ["vehicle", "projectile"].iter().find_map(|kind| {
        let name = format!("part{part}_{kind}");
        gltf.meshes().find(|m| m.name() == Some(name.as_str()))
    });
    let Some(mesh) = mesh else {
        return Ok(Vec::new());
    };
    let mut points = Vec::new();
    for primitive in mesh.primitives() {
        let reader = primitive.reader(|_| Some(blob));
        if let Some(positions) = reader.read_positions() {
            points.extend(positions.map(Vec3::from_array));
        }
    }
    Ok(points)
}

/// Vehicle descriptions, loaded from `imported/vehicles/` when first needed.
#[derive(Resource, Default)]
pub struct VehicleLibrary {
    models: HashMap<String, Option<Arc<VehicleModel>>>,
}

impl VehicleLibrary {
    pub fn get(&mut self, name: &str, paths: &GamePaths) -> Option<Arc<VehicleModel>> {
        self.models
            .entry(name.to_string())
            .or_insert_with(|| {
                paths
                    .read_ron::<VehicleDesc>(format!("vehicles/{name}.ron"))
                    .map(|desc| Arc::new(VehicleModel::new(desc, paths)))
                    .map_err(|err| debug!("{err}"))
                    .ok()
            })
            .clone()
    }

    /// Whether a template is a vehicle we can simulate.
    pub fn exists(&mut self, name: &str, paths: &GamePaths) -> bool {
        self.get(name, paths).is_some()
    }
}

/// The vehicle's model, on every vehicle entity (client and server).
#[derive(Component, Clone)]
pub struct VehicleData(pub Arc<VehicleModel>);

/// Server-side simulation state.
#[derive(Component, Default)]
pub struct VehicleSim {
    /// Suspension compression per wheel last tick, meters.
    compression: Vec<f32>,
}

/// Gives every vehicle its collider, and on the server a dynamic rigid body.
fn add_vehicle_physics(
    add: On<Add, Vehicle>,
    mut commands: Commands,
    vehicles: Query<&Vehicle>,
    mut library: ResMut<VehicleLibrary>,
    paths: Res<GamePaths>,
    state: Res<State<ClientState>>,
) {
    let Ok(vehicle) = vehicles.get(add.entity) else {
        return;
    };
    let Some(model) = library.get(&vehicle.template, &paths) else {
        warn!("unknown vehicle `{}`", vehicle.template);
        return;
    };
    let desc = &model.desc;
    let mut entity = commands.entity(add.entity);
    entity.insert((
        VehicleData(model.clone()),
        CollisionLayers::new(GameLayer::Vehicle, [GameLayer::World, GameLayer::Vehicle]),
    ));
    if let Some(collider) = &model.collider {
        entity.insert(collider.clone());
    }
    if *state.get() != ClientState::Disconnected {
        // Clients place the vehicle from replicated state.
        entity.insert(RigidBody::Kinematic);
        return;
    }
    let [min, max] = desc.physics.bounds.map(Vec3::from_array);
    let size = max - min;
    let mass = desc.physics.mass;
    // A solid box of the hull's size.
    let inertia = Vec3::new(
        size.y * size.y + size.z * size.z,
        size.x * size.x + size.z * size.z,
        size.x * size.x + size.y * size.y,
    ) * mass
        / 12.0;
    entity.insert((
        RigidBody::Dynamic,
        Mass(mass),
        CenterOfMass(Vec3::from_array(desc.physics.center_of_mass)),
        AngularInertia::new(inertia),
        GravityScale(desc.physics.gravity),
        LinearDamping(0.02),
        AngularDamping(0.3),
        Friction::new(0.4),
        TransformInterpolation,
        VehicleState {
            joints: vec![[0.0; 3]; model.joint_count],
            wheels: vec![0.0; desc.wheels.len()],
        },
        VehicleSim {
            compression: vec![0.0; desc.wheels.len()],
        },
        SeatInputs(vec![None; desc.seats.len()]),
        VehicleHealth {
            current: desc.hit_points,
            max: desc.hit_points,
        },
    ));
}

/// Compression room above a wheel's rest position, meters.
fn bump_travel(drive: DriveKind) -> f32 {
    match drive {
        DriveKind::Wheeled => 0.25,
        DriveKind::Tracked => 0.2,
    }
}

/// How far guns look for something to converge on, and the nearest point they converge to.
const AIM_RANGE: f32 = 800.0;
const MIN_AIM_DISTANCE: f32 = 8.0;

/// Share of the per-tick velocity error the tyres correct (lower is softer).
const TYRE_STIFFNESS: f32 = 0.4;
/// Tyre forces act this fraction of the way up from the ground to the center of mass, so
/// cornering doesn't roll the vehicle over as easily.
const ANTI_ROLL: f32 = 0.6;

#[allow(clippy::type_complexity)]
fn simulate_vehicles(
    time: Res<Time>,
    spatial: SpatialQuery,
    mut vehicles: Query<(
        Entity,
        &VehicleData,
        &SeatInputs,
        &mut VehicleState,
        &mut VehicleSim,
        Forces,
        Has<Sleeping>,
    )>,
) {
    let dt = time.delta_secs();
    if dt <= 0.0 {
        return;
    }
    for (entity, data, inputs, mut state, mut sim, mut forces, sleeping) in &mut vehicles {
        let occupied = inputs.0.iter().any(Option::is_some);
        if sleeping && !occupied {
            continue;
        }
        let model = &data.0;
        let desc = &model.desc;
        let body_pos = forces.position().0;
        let body_rot = forces.rotation().0;
        let driver = inputs.0.first().copied().flatten();
        let (throttle, steer, handbrake) = driver.map_or((0.0, 0.0, false), |input| {
            let m = input.movement_vec();
            (m.y, m.x, input.pressed(Buttons::JUMP))
        });
        let velocity = forces.linear_velocity();
        let forward = body_rot * Vec3::NEG_Z;
        let forward_speed = velocity.dot(forward);
        let top = desc.engine.top_speed.max(1.0);

        // What each gunner looks at: guns converge on the point under their crosshair.
        let aim_filter = SpatialQueryFilter::from_mask([GameLayer::World, GameLayer::Vehicle])
            .with_excluded_entities([entity]);
        let previous = model.part_transforms(&state.joints);
        let aim_points: Vec<Option<Vec3>> = inputs
            .0
            .iter()
            .enumerate()
            .map(|(seat, input)| {
                let input = input.as_ref().filter(|_| model.seat_aims(seat))?;
                let eye = body_pos + body_rot * model.eye(&previous, seat);
                let dir = Dir3::new(Quat::from_euler(EulerRot::YXZ, input.yaw, input.pitch, 0.0) * Vec3::NEG_Z).ok()?;
                let distance = spatial
                    .cast_ray(eye, dir, AIM_RANGE, true, &aim_filter)
                    .map_or(AIM_RANGE, |hit| hit.distance.max(MIN_AIM_DISTANCE));
                Some(eye + *dir * distance)
            })
            .collect();

        // Joints: turrets follow their gunner's aim, steering follows the driver.
        let mut joints = state.joints.clone();
        joints.resize(model.joint_count, [0.0; 3]);
        let mut hull: Vec<Transform> = Vec::with_capacity(desc.parts.len());
        for (i, part) in desc.parts.iter().enumerate() {
            let parent = part.parent.map_or(Transform::IDENTITY, |p| hull[p as usize]);
            if let (Some(joint), Some(j)) = (&part.joint, model.joint_index[i]) {
                let seat = joint.seat as usize;
                let rest = parent * model.rest[i];
                let frame = body_rot * rest.rotation;
                let pivot = body_pos + body_rot * rest.translation;
                let aim = aim_points
                    .get(seat)
                    .copied()
                    .flatten()
                    .and_then(|point| (point - pivot).try_normalize());
                let steer_scale = 1.0 - 0.6 * (forward_speed.abs() / top).min(1.0);
                joints[j] = step_joint(joint, joints[j], aim, frame, steer * steer_scale, dt);
            }
            let mut local = model.rest[i];
            if let Some(angles) = model.joint_index[i].map(|j| joints[j]) {
                local.rotation *= joint_rotation(angles);
            }
            hull.push(parent * local);
        }

        // Suspension, tyres and tracks.
        let tracked = desc.drive == DriveKind::Tracked;
        let up = body_rot * Vec3::Y;
        let Ok(down) = Dir3::new(-up) else {
            continue;
        };
        let carrying = |w: &game_data::WheelDesc| w.contact || tracked;
        let carriers = desc.wheels.iter().filter(|w| carrying(w)).count().max(1) as f32;
        let wheel_mass = desc.physics.mass / carriers;
        let max_strength = desc.wheels.iter().map(|w| w.strength).fold(0.0, f32::max).max(1.0);
        let g = GRAVITY * desc.physics.gravity;
        let travel_up = bump_travel(desc.drive);
        let com_local = Vec3::from_array(desc.physics.center_of_mass);
        let com_world = body_pos + body_rot * com_local;
        let com_height = com_local.y;
        let filter = SpatialQueryFilter::from_mask([GameLayer::World, GameLayer::Vehicle])
            .with_excluded_entities([entity]);
        let [mu_long, mu_lat] = desc.engine.grip;
        let yaw_rate_cmd = -steer * desc.engine.turn_rate * (1.0 - 0.4 * (forward_speed.abs() / top).min(1.0));
        let track_speed_cmd = if throttle >= 0.0 {
            throttle * top
        } else {
            throttle * desc.engine.reverse_speed
        };

        sim.compression.resize(desc.wheels.len(), 0.0);
        let mut wheel_offsets = state.wheels.clone();
        wheel_offsets.resize(desc.wheels.len(), 0.0);
        // Forces and where they act, applied together at the end.
        let mut pushes: Vec<(Vec3, Vec3)> = Vec::with_capacity(desc.wheels.len() * 2 + 1);
        for (wi, wheel) in desc.wheels.iter().enumerate() {
            let strength = if wheel.strength > 0.0 { wheel.strength } else { max_strength };
            let omega2 = strength * if tracked { 9.0 } else { 3.0 };
            let k = wheel_mass * omega2;
            let zeta = (0.2 + 0.08 * wheel.damping).clamp(0.3, 0.9) + if tracked { 0.2 } else { 0.0 };
            let c = 2.0 * zeta * (k * wheel_mass).sqrt();
            let droop = (wheel_mass * g / k).min(0.4);
            let travel = travel_up + droop;

            let frame = hull.get(wheel.part as usize).copied().unwrap_or_default();
            let top_point = body_pos + body_rot * (frame.translation + Vec3::Y * travel_up);
            let hit = spatial.cast_ray(top_point, down, travel + wheel.radius, true, &filter);
            let Some(hit) = hit else {
                sim.compression[wi] = 0.0;
                wheel_offsets[wi] = droop;
                continue;
            };
            let center_distance = hit.distance - wheel.radius;
            wheel_offsets[wi] = (center_distance - travel_up).clamp(-travel_up, droop);
            if !carrying(wheel) {
                continue;
            }
            let x = travel - center_distance;
            let rate = (x - sim.compression[wi]) / dt;
            sim.compression[wi] = x;
            let mut spring = k * x + c * rate;
            if center_distance < 0.0 {
                // Bottomed out: a much stiffer bump stop.
                spring += k * 20.0 * -center_distance;
            }
            let load = spring.max(0.0);
            pushes.push((up * load, top_point));

            // Friction at the contact patch.
            let contact = top_point + *down * hit.distance;
            let normal = hit.normal;
            let heading = body_rot * (frame.rotation * Vec3::NEG_Z);
            let Ok(fwd) = Dir3::new(heading - normal * heading.dot(normal)) else {
                continue;
            };
            let side = fwd.cross(normal);
            let v = forces.velocity_at_point(contact);
            let (v_long, v_lat) = (v.dot(*fwd), v.dot(side));
            let per_tick = wheel_mass / dt * TYRE_STIFFNESS;
            let brake = desc.engine.brake_force / carriers;
            let drive = desc.engine.drive_force / carriers;

            let (long, lat) = if tracked {
                // Tracks hold the hull to the commanded speed and turn rate, as hard as the
                // engine and friction allow.
                let spin = (up * yaw_rate_cmd).cross(contact - com_world);
                let want_long = track_speed_cmd + spin.dot(*fwd);
                let want_lat = spin.dot(side);
                let accelerating = (want_long - v_long) * want_long.signum() > 0.0 && want_long.abs() > 0.1;
                let limit = if driver.is_none() || handbrake {
                    brake
                } else if accelerating {
                    drive
                } else if throttle == 0.0 && steer == 0.0 {
                    brake * 0.3
                } else {
                    brake
                };
                (
                    ((want_long - v_long) * per_tick).clamp(-limit, limit),
                    (want_lat - v_lat) * per_tick,
                )
            } else {
                let stop = (-v_long * per_tick).clamp(-brake, brake);
                let long = if driver.is_none() || handbrake {
                    stop
                } else if throttle != 0.0 && forward_speed * throttle < -0.5 {
                    // Pressing against the direction of travel brakes.
                    stop
                } else if throttle != 0.0 {
                    let limit = if throttle > 0.0 { top } else { desc.engine.reverse_speed.max(1.0) };
                    let ratio = (forward_speed.abs() / limit).min(1.3);
                    drive * throttle * (1.0 - ratio * ratio).max(-0.3)
                } else {
                    // Rolling resistance and engine braking.
                    stop * 0.08
                };
                (long, -v_lat * per_tick)
            };
            // Friction ellipse: the tyre can't give more than its load allows.
            let (max_long, max_lat) = (mu_long * load, mu_lat * load);
            let usage = ((long / max_long.max(1e-3)).powi(2) + (lat / max_lat.max(1e-3)).powi(2)).sqrt();
            let scale = if usage > 1.0 { 1.0 / usage } else { 1.0 };
            let contact_height = frame.translation.y - wheel.radius - wheel_offsets[wi];
            let lift = (com_height - contact_height).max(0.0) * ANTI_ROLL;
            pushes.push(((*fwd * long + side * lat) * scale, contact + up * lift));
        }

        // Air drag.
        let speed = velocity.length();
        if speed > 0.1 {
            pushes.push((-velocity * speed * desc.physics.drag * 1.2, com_world));
        }

        // An empty vehicle at rest may fall asleep: don't keep it awake with its own springs.
        if occupied {
            for (force, point) in pushes {
                forces.apply_force_at_point(force, point);
            }
        } else {
            let mut forces = forces.non_waking();
            for (force, point) in pushes {
                forces.apply_force_at_point(force, point);
            }
        }

        let next = VehicleState {
            joints,
            wheels: wheel_offsets,
        };
        state.set_if_neq(next);
    }
}

/// Moves one joint towards what its seat asks for. `frame` is the joint's rest orientation
/// in world space (hull rotation included).
fn step_joint(
    joint: &game_data::JointDesc,
    mut angles: [f32; 3],
    aim: Option<Vec3>,
    frame: Quat,
    steer: f32,
    dt: f32,
) -> [f32; 3] {
    // The aim direction in the joint's frame.
    let aim = aim.map(|d| frame.inverse() * d);
    for axis in 0..3 {
        let a = &joint.axes[axis];
        let Some(kind) = a.input else {
            continue;
        };
        let (min, max) = (a.min.to_radians(), a.max.to_radians());
        let target = match kind {
            JointInput::AimYaw => aim.map(|d| (-d.x).atan2(-d.z)),
            JointInput::AimPitch => aim.map(|d| {
                // Pitch after this joint's own yaw.
                let d = Quat::from_rotation_y(-angles[0]) * d;
                d.y.atan2(Vec2::new(d.x, d.z).length())
            }),
            JointInput::Steer => {
                let t = (steer * a.speed.signum()).clamp(-1.0, 1.0);
                Some(if t >= 0.0 { t * max } else { -t * min })
            }
            JointInput::Throttle => None,
        };
        let Some(target) = target else {
            continue;
        };
        let rate = match kind {
            JointInput::Steer => a.acceleration.max(a.speed.abs()).max(30.0),
            _ => a.speed.abs().max(1.0),
        }
        .to_radians();
        let current = angles[axis];
        let mut delta = target - current;
        if !a.limited() {
            delta = wrap_angle(delta);
        }
        let mut next = current + delta.clamp(-rate * dt, rate * dt);
        if a.limited() {
            next = next.clamp(min.min(max), max.max(min));
        } else {
            next = wrap_angle(next);
        }
        angles[axis] = next;
    }
    angles
}

fn wrap_angle(a: f32) -> f32 {
    use std::f32::consts::{PI, TAU};
    (a + PI).rem_euclid(TAU) - PI
}

/// Publishes where simulated vehicles ended up after the physics step.
fn record_motion(mut vehicles: Query<(&Position, &Rotation, &LinearVelocity, &mut VehicleMotion), With<VehicleSim>>) {
    for (position, rotation, velocity, mut motion) in &mut vehicles {
        motion.set_if_neq(VehicleMotion {
            position: position.0,
            rotation: rotation.0,
            velocity: velocity.0,
        });
    }
}
