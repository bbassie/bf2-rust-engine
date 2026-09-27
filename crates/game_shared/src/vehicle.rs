//! Vehicles: descriptions, replicated state, physics bodies and the driving simulation.
//!
//! The server simulates every vehicle as an avian rigid body (a compound of convex hulls from
//! the hull's collision mesh) pushed around by raycast suspension springs, tyre or track
//! friction and engine forces, and for aircraft and boats by wings, thrusters, rotors and
//! floaters (see [`crate::flight`]), driven by the [`InputFrame`]s of its occupants.
//! Stationary weapons are static bodies whose joints aim. Clients get [`VehicleMotion`] and
//! [`VehicleState`] replicated and show vehicles slightly in the past, except the one they
//! drive, which they predict with the same [`step_vehicle`] (see the client's `vehicles`
//! module).

use std::{
    collections::HashMap,
    path::Path,
    sync::Arc,
};

use avian3d::prelude::*;
use bevy::{ecs::entity::MapEntities, prelude::*};
use bevy_replicon::prelude::*;
use game_data::{DriveKind, GearboxDesc, JointInput, VehicleCategory, VehicleDesc, WeaponDesc};
use serde::{Deserialize, Serialize};

use crate::{
    config::GamePaths,
    flight::{self, BodyState, Controls, FlightState, GRAVITY, Push, Surroundings},
    input::InputFrame,
    level::LoadedLevel,
    physics::GameLayer,
    weapons::Armory,
};

pub struct VehiclePlugin;

impl Plugin for VehiclePlugin {
    fn build(&self, app: &mut App) {
        app.init_resource::<VehicleLibrary>()
            .add_observer(add_vehicle_physics)
            .add_systems(
                FixedUpdate,
                (simulate_vehicles, aim_stationary)
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
    /// World space, radians per second (the driver's prediction replays from it).
    pub angular_velocity: Vec3,
    /// Sequence number of the driver's input this state followed from (0 without a driver):
    /// the driver's prediction replays its inputs after it.
    pub ack: u32,
}

impl Default for VehicleMotion {
    fn default() -> Self {
        Self {
            position: Vec3::ZERO,
            rotation: Quat::IDENTITY,
            velocity: Vec3::ZERO,
            angular_velocity: Vec3::ZERO,
            ack: 0,
        }
    }
}

impl VehicleMotion {
    pub fn transform(&self) -> Transform {
        Transform::from_translation(self.position).with_rotation(self.rotation)
    }

    pub fn body(&self) -> BodyState {
        BodyState {
            position: self.position,
            rotation: self.rotation,
            velocity: self.velocity,
            angular_velocity: self.angular_velocity,
        }
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
    /// Rotor speed, throttle or (with a gearbox) revs, 0..1 (for rotor blades, sounds and the
    /// HUD).
    #[serde(default)]
    pub engine: f32,
    /// Gearbox: the gear, 0 for first, -1 for reverse.
    #[serde(default)]
    pub gear: i8,
    /// Afterburner meter, 0..1 (for the HUD).
    #[serde(default)]
    pub boost: f32,
}

/// The guns' state, for the HUD. Replicated.
#[derive(Component, Serialize, Deserialize, Clone, Debug, Default, PartialEq)]
pub struct VehicleWeapons {
    pub guns: Vec<GunStatus>,
}

#[derive(Serialize, Deserialize, Clone, Copy, Debug, Default, PartialEq)]
pub struct GunStatus {
    /// Rounds left in the magazine; `u16::MAX` for a bottomless one.
    pub rounds: u16,
    pub reloading: bool,
    /// 0..=255; 255 while overheated.
    pub heat: u8,
    /// The gun its seat fires among those on the same trigger (the weapon keys choose).
    pub selected: bool,
    /// Heat seekers: how far the lock is, 0..=255 (255: locked on).
    pub lock: u8,
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
    /// Per part: rest transform in hull space.
    pub rest_hull: Vec<Transform>,
    /// Per part: it is a wing (its steering joint isn't eased off with speed).
    pub is_wing: Vec<bool>,
    /// Principal moments of inertia about the hull axes (kg m²).
    pub inertia: Vec3,
    /// Hull collision (convex hulls of the hull and turret parts), if it has any.
    pub collider: Option<Collider>,
    /// The guns' weapon descriptions, shared with the projectiles they fire.
    pub guns: Vec<Arc<WeaponDesc>>,
    /// The faces direct hits land on (see [`VehicleDesc::armor_mesh`]).
    pub armor: Vec<ArmorFaces>,
}

/// A part's armour: its projectile collision triangles (part space) and their materials.
pub struct ArmorFaces {
    pub part: usize,
    pub triangles: Vec<[Vec3; 3]>,
    pub materials: Vec<u32>,
}

impl VehicleModel {
    pub fn new(desc: VehicleDesc, paths: &GamePaths) -> Self {
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
        let mut is_wing = vec![false; desc.parts.len()];
        for wing in &desc.wings {
            if let Some(flag) = is_wing.get_mut(wing.part as usize) {
                *flag = true;
            }
        }
        // A solid box of the hull's size, scaled like BF2's `inertiaModifier`.
        let physics = &desc.physics;
        let [min, max] = physics.bounds.map(Vec3::from_array);
        let size = (max - min).max(Vec3::splat(0.5));
        let inertia = Vec3::new(
            size.y * size.y + size.z * size.z,
            size.x * size.x + size.z * size.z,
            size.x * size.x + size.y * size.y,
        ) * physics.mass
            / 12.0
            * Vec3::from(physics.inertia_modifier);
        let mut model = Self {
            desc,
            joint_index,
            joint_count,
            rest,
            rest_hull: Vec::new(),
            is_wing,
            inertia,
            collider: None,
            guns,
            armor: Vec::new(),
        };
        model.rest_hull = model.part_transforms(&[]);
        model.collider = model.build_collider(paths);
        if let Some(path) = &model.desc.armor_mesh {
            model.armor = load_armor(&paths.find(path)).unwrap_or_else(|err| {
                warn!("vehicle armour {path}: {err:#}");
                Vec::new()
            });
        }
        model
    }

    /// The material of the armour a shot along `direction` meets first around `point` (both
    /// hull space, the point on the collision hull), with the joints at `joints`.
    pub fn armor_material_at(&self, joints: &[[f32; 3]], point: Vec3, direction: Vec3) -> Option<u32> {
        let direction = direction.try_normalize()?;
        // The collision hull is convex; the armour may lie a little inside or outside it.
        let origin = point - direction * ARMOR_PROBE_BEFORE;
        let transforms = self.part_transforms(joints);
        let mut nearest = (ARMOR_PROBE_LENGTH, None);
        for faces in &self.armor {
            let Some(part) = transforms.get(faces.part) else { continue };
            let inverse = part.compute_affine().inverse();
            let (o, d) = (inverse.transform_point3(origin), inverse.transform_vector3(direction));
            for (triangle, material) in faces.triangles.iter().zip(&faces.materials) {
                if let Some(t) = ray_triangle(o, d, triangle)
                    && t < nearest.0
                {
                    nearest = (t, Some(*material));
                }
            }
        }
        nearest.1
    }

    /// A part's rest orientation in hull space.
    pub fn rest_rotation(&self, part: usize) -> Quat {
        self.rest_hull.get(part).map_or(Quat::IDENTITY, |t| t.rotation)
    }

    /// How far a control surface is deflected, -1..1 of its travel about its pitch axis.
    pub fn deflection(&self, joints: &[[f32; 3]], part: usize) -> f32 {
        let (Some(joint), Some(angles)) = (
            self.desc.parts.get(part).and_then(|p| p.joint.as_ref()),
            self.joint_index.get(part).copied().flatten().and_then(|j| joints.get(j)),
        ) else {
            return 0.0;
        };
        let axis = &joint.axes[1];
        let angle = angles[1].to_degrees();
        let limit = if angle >= 0.0 { axis.max } else { -axis.min };
        if limit > 0.0 { (angle / limit).clamp(-1.0, 1.0) } else { 0.0 }
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

    /// Where a seat's occupant sits, in hull space: the seat position (the soldier's feet in
    /// the seat's pose), else its camera, else the seat part.
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

    /// Where a seat looks from, in hull space: its camera, else about head height above the
    /// seat.
    pub fn eye(&self, transforms: &[Transform], seat: usize) -> Vec3 {
        match self.desc.seats.get(seat).and_then(|s| s.camera.as_ref()) {
            Some(camera) => self.attachment(transforms, &camera.attachment).translation,
            None => self.seat_transform(transforms, seat).translation + Vec3::Y * self.head_height(seat),
        }
    }

    /// How far above `seat_transform` the occupant's head is.
    pub fn head_height(&self, seat: usize) -> f32 {
        match self.desc.seats.get(seat).and_then(|s| s.soldier.as_ref()) {
            Some(_) => 1.1,
            None => 0.6,
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

/// How far before the hit point on the collision hull the armour probe starts, and how far
/// it looks, meters.
const ARMOR_PROBE_BEFORE: f32 = 1.0;
const ARMOR_PROBE_LENGTH: f32 = 4.0;

/// Reads an armour `.glb` (`VehicleDesc::armor_mesh`): mesh `part{N}` per part, a primitive
/// per material, the glTF material named by its id.
fn load_armor(path: &Path) -> anyhow::Result<Vec<ArmorFaces>> {
    let bytes = std::fs::read(path)?;
    let gltf = gltf::Gltf::from_slice(&bytes)?;
    let blob = gltf.blob.as_deref().unwrap_or_default();
    let mut armor = Vec::new();
    for mesh in gltf.meshes() {
        let Some(part) = mesh.name().and_then(|n| n.strip_prefix("part")).and_then(|n| n.parse().ok()) else {
            continue;
        };
        let mut faces = ArmorFaces {
            part,
            triangles: Vec::new(),
            materials: Vec::new(),
        };
        for primitive in mesh.primitives() {
            let Some(material) = primitive.material().name().and_then(|n| n.parse::<u32>().ok()) else {
                continue;
            };
            let reader = primitive.reader(|_| Some(blob));
            let (Some(positions), Some(indices)) = (reader.read_positions(), reader.read_indices()) else {
                continue;
            };
            let positions: Vec<Vec3> = positions.map(Vec3::from_array).collect();
            let indices: Vec<u32> = indices.into_u32().collect();
            for t in indices.chunks_exact(3) {
                let corner = |i: u32| positions.get(i as usize).copied().unwrap_or_default();
                faces.triangles.push([corner(t[0]), corner(t[1]), corner(t[2])]);
                faces.materials.push(material);
            }
        }
        armor.push(faces);
    }
    Ok(armor)
}

/// Distance along a ray (unit `direction`) to where it crosses a triangle, either side.
fn ray_triangle(origin: Vec3, direction: Vec3, [a, b, c]: &[Vec3; 3]) -> Option<f32> {
    let (e1, e2) = (*b - *a, *c - *a);
    let p = direction.cross(e2);
    let det = e1.dot(p);
    if det.abs() < 1e-8 {
        return None;
    }
    let s = origin - *a;
    let u = s.dot(p) / det;
    if !(0.0..=1.0).contains(&u) {
        return None;
    }
    let q = s.cross(e1);
    let v = direction.dot(q) / det;
    if v < 0.0 || u + v > 1.0 {
        return None;
    }
    let t = e2.dot(q) / det;
    (t >= 0.0).then_some(t)
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

/// Simulation state carried from tick to tick (server, and the driver's prediction).
#[derive(Component, Clone, Debug, Default, PartialEq)]
pub struct VehicleSim {
    /// Suspension compression per wheel last tick, meters.
    pub compression: Vec<f32>,
    pub flight: FlightState,
    /// Sequence number of the driver's input applied this tick.
    pub driver_seq: u32,
    /// Gearbox: the gear (-1 reverse) and the seconds left of a gear change.
    pub gear: i8,
    pub shift: f32,
}

impl VehicleSim {
    pub fn new(desc: &VehicleDesc) -> Self {
        Self {
            compression: vec![0.0; desc.wheels.len()],
            flight: FlightState::new(),
            driver_seq: 0,
            gear: 0,
            shift: 0.0,
        }
    }
}

/// Server-side: a vehicle that never moves (stationary weapons); a static body whose joints
/// still aim.
#[derive(Component)]
pub struct Stationary;

/// Gives every vehicle its collider, and on the server a dynamic rigid body.
fn add_vehicle_physics(
    add: On<Add, Vehicle>,
    mut commands: Commands,
    vehicles: Query<&Vehicle>,
    mut library: ResMut<VehicleLibrary>,
    mut armory: ResMut<Armory>,
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
    // Their shells and missiles are replicated by weapon name like grenades.
    for gun in &model.guns {
        armory.weapons.entry(gun.name.clone()).or_insert_with(|| gun.clone());
    }
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
    entity.insert((
        VehicleState {
            joints: vec![[0.0; 3]; model.joint_count],
            wheels: vec![0.0; desc.wheels.len()],
            engine: 0.0,
            gear: 0,
            boost: 1.0,
        },
        VehicleSim::new(desc),
        SeatInputs(vec![None; desc.seats.len()]),
        VehicleHealth {
            current: desc.hit_points,
            max: desc.hit_points,
        },
    ));
    if desc.category == VehicleCategory::Stationary {
        entity.insert((RigidBody::Static, Stationary));
        return;
    }
    let (linear, angular) = body_damping(desc);
    entity.insert((
        RigidBody::Dynamic,
        Mass(desc.physics.mass),
        CenterOfMass(Vec3::from_array(desc.physics.center_of_mass)),
        AngularInertia::new(model.inertia),
        GravityScale(desc.physics.gravity),
        LinearDamping(linear),
        AngularDamping(angular),
        Friction::new(0.4),
        TransformInterpolation,
    ));
}

/// Compression room above a wheel's rest position, meters.
fn bump_travel(drive: DriveKind) -> f32 {
    match drive {
        DriveKind::Tracked => 0.2,
        _ => 0.25,
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
/// How far down aircraft look for the ground (landing gear, hovering), meters.
const ALTITUDE_PROBE: f32 = 200.0;

/// The level's water surface, if it has water.
pub fn water_height(level: Option<&LoadedLevel>) -> Option<f32> {
    level.and_then(|l| l.desc.water.as_ref()).map(|w| w.height)
}

#[allow(clippy::type_complexity)]
fn simulate_vehicles(
    time: Res<Time>,
    spatial: SpatialQuery,
    level: Option<Res<LoadedLevel>>,
    mut vehicles: Query<
        (
            Entity,
            &VehicleData,
            &SeatInputs,
            &mut VehicleState,
            &mut VehicleSim,
            Forces,
            Has<Sleeping>,
        ),
        Without<Stationary>,
    >,
) {
    let dt = time.delta_secs();
    if dt <= 0.0 {
        return;
    }
    let water = water_height(level.as_deref());
    for (entity, data, inputs, mut state, mut sim, mut forces, sleeping) in &mut vehicles {
        let occupied = inputs.0.iter().any(Option::is_some);
        sim.driver_seq = inputs.0.first().copied().flatten().map_or(0, |input| input.seq);
        if sleeping && !occupied {
            continue;
        }
        let body = BodyState {
            position: forces.position().0,
            rotation: forces.rotation().0,
            velocity: forces.linear_velocity(),
            angular_velocity: forces.angular_velocity(),
        };
        let mut next = state.clone();
        let push = step_vehicle(&data.0, &body, &inputs.0, &mut next, &mut sim, &spatial, entity, water, dt);
        // An empty vehicle at rest may fall asleep: don't keep it awake with its own springs.
        if occupied {
            apply_push(&mut forces, &push);
        } else {
            apply_push(&mut forces.non_waking(), &push);
        }
        state.set_if_neq(next);
    }
}

fn apply_push(forces: &mut impl WriteRigidBodyForces, push: &Push) {
    for (force, point) in &push.forces {
        forces.apply_force_at_point(*force, *point);
    }
    if push.torque != Vec3::ZERO {
        forces.apply_torque(push.torque);
    }
}

/// Stationary weapons only turn their joints towards their gunner's aim.
fn aim_stationary(
    time: Res<Time>,
    spatial: SpatialQuery,
    mut vehicles: Query<(Entity, &VehicleData, &SeatInputs, &mut VehicleState, &Position, &Rotation), With<Stationary>>,
) {
    let dt = time.delta_secs();
    for (entity, data, inputs, mut state, position, rotation) in &mut vehicles {
        if inputs.0.iter().all(Option::is_none) {
            continue;
        }
        let body = BodyState {
            position: position.0,
            rotation: rotation.0,
            ..default()
        };
        let mut next = state.clone();
        let aims = aim_points(&data.0, &body, &inputs.0, &state.joints, &spatial, entity);
        next.joints = step_joints(&data.0, &body, &aims, &state.joints, &Controls::default(), &FlightState::default(), dt).0;
        state.set_if_neq(next);
    }
}

/// One tick of a vehicle: turns its joints, and returns the forces of its suspension, tyres
/// or tracks, engines, wings, rotor and floaters. `state` and `sim` are advanced. Shared by
/// the server and the driver's prediction.
#[allow(clippy::too_many_arguments)]
pub fn step_vehicle(
    model: &VehicleModel,
    body: &BodyState,
    inputs: &[Option<InputFrame>],
    state: &mut VehicleState,
    sim: &mut VehicleSim,
    spatial: &SpatialQuery,
    entity: Entity,
    water: Option<f32>,
    dt: f32,
) -> Push {
    let desc = &model.desc;
    let body_pos = body.position;
    let body_rot = body.rotation;
    let driver = inputs.first().copied().flatten();
    let mut controls = Controls::from_input(driver.as_ref());
    if desc.category == VehicleCategory::Air
        && let Some(aero) = &desc.aero
    {
        controls.pitch = flight::limit_pitch(body, aero.stall_angle, controls.pitch);
    }
    let (throttle, steer, handbrake) = (controls.throttle, controls.steer, controls.brake);
    let velocity = body.velocity;
    let forward = body_rot * Vec3::NEG_Z;
    let forward_speed = velocity.dot(forward);
    let top = desc.engine.top_speed.max(1.0);
    let com_local = Vec3::from_array(desc.physics.center_of_mass);
    let com_world = body_pos + body_rot * com_local;
    let filter = SpatialQueryFilter::from_mask([GameLayer::World, GameLayer::Vehicle]).with_excluded_entities([entity]);

    // Height above ground or water, for the landing gear and hovering.
    let altitude = if desc.aero.is_some() {
        let ground = spatial
            .cast_ray(com_world, Dir3::NEG_Y, ALTITUDE_PROBE, true, &filter)
            .map_or(ALTITUDE_PROBE, |hit| hit.distance);
        water.map_or(ground, |w| ground.min((com_world.y - w).max(0.0)))
    } else {
        0.0
    };
    let around = Surroundings { water, altitude };

    // Joints: turrets follow their gunner's aim, steering and control surfaces the driver,
    // landing gear the gear state, rotor blades the rotor.
    let aims = aim_points(model, body, inputs, &state.joints, spatial, entity);
    let (joints, hull) = step_joints(model, body, &aims, &state.joints, &controls, &sim.flight, dt);

    // Wings, engines, rotor, floaters.
    let mut push = flight::flight_forces(model, body, &joints, &controls, &mut sim.flight, &around, dt);

    // Gearbox: the pull at full throttle and the revs (in coarse steps, so they don't
    // replicate every tick).
    let gearbox = desc.engine.gearbox.as_ref().filter(|_| desc.drive == DriveKind::Wheeled);
    let (pull, revs) = match gearbox {
        Some(gearbox) if driver.is_some() => {
            let (pull, revs) = step_gearbox(gearbox, sim, throttle, forward_speed, dt);
            (pull, (revs * 50.0).round() / 50.0)
        }
        Some(_) => (0.0, 0.0),
        None => (0.0, sim.flight.engine()),
    };

    // Suspension, tyres and tracks.
    let tracked = desc.drive == DriveKind::Tracked;
    let rolling = desc.drive == DriveKind::Rolling;
    let skids = desc.category == VehicleCategory::Helicopter;
    let up = body_rot * Vec3::Y;
    sim.compression.resize(desc.wheels.len(), 0.0);
    let mut wheel_offsets = state.wheels.clone();
    wheel_offsets.resize(desc.wheels.len(), 0.0);
    let gear_up = desc.landing_gear.is_some() && sim.flight.gear_up;
    if let Ok(down) = Dir3::new(-up)
        && !gear_up
    {
        let carrying = |w: &game_data::WheelDesc| w.contact || tracked;
        let carriers = desc.wheels.iter().filter(|w| carrying(w)).count().max(1) as f32;
        let wheel_mass = desc.physics.mass / carriers;
        let max_strength = desc.wheels.iter().map(|w| w.strength).fold(0.0, f32::max).max(1.0);
        let g = GRAVITY * desc.physics.gravity;
        let travel_up = bump_travel(desc.drive);
        let com_height = com_local.y;
        let [mu_long, mu_lat] = desc.engine.grip;
        let yaw_rate_cmd = -steer * desc.engine.turn_rate * (1.0 - 0.4 * (forward_speed.abs() / top).min(1.0));
        let track_speed_cmd = if throttle >= 0.0 {
            throttle * top
        } else {
            throttle * desc.engine.reverse_speed
        };
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
            push.forces.push((up * load, top_point));

            // Friction at the contact patch.
            let contact = top_point + *down * hit.distance;
            let normal = hit.normal;
            let heading = body_rot * (frame.rotation * Vec3::NEG_Z);
            let Ok(fwd) = Dir3::new(heading - normal * heading.dot(normal)) else {
                continue;
            };
            let side = fwd.cross(normal);
            let v = body.velocity_at(contact, com_world);
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
            } else if rolling {
                // Aircraft: skids don't roll; wheels roll freely unless braking.
                let stop = (-v_long * per_tick).clamp(-brake, brake);
                let braking = skids || driver.is_none() || handbrake || (throttle < 0.0 && forward_speed.abs() < 30.0);
                (if braking { stop } else { stop * 0.02 }, -v_lat * per_tick)
            } else {
                let stop = (-v_long * per_tick).clamp(-brake, brake);
                let long = if driver.is_none() || handbrake {
                    stop
                } else if throttle != 0.0 && forward_speed * throttle < -0.5 {
                    // Pressing against the direction of travel brakes.
                    stop
                } else if throttle != 0.0 && gearbox.is_some() {
                    pull * throttle.abs() / carriers
                } else if throttle != 0.0 {
                    let limit = if throttle > 0.0 { top } else { desc.engine.reverse_speed.max(1.0) };
                    let ratio = (forward_speed.abs() / limit).min(1.3);
                    drive * throttle * (1.0 - ratio * ratio).max(-0.3)
                } else if forward_speed.abs() < 1.0 {
                    // Standing: the brakes hold it on slopes.
                    stop
                } else if let Some(gearbox) = gearbox {
                    // Rolling resistance and engine braking.
                    let hold = gearbox.engine_brake / carriers + brake * 0.02;
                    stop.clamp(-hold, hold)
                } else {
                    // Rolling resistance and engine braking.
                    stop * 0.08
                };
                (long, -v_lat * per_tick)
            };
            // Friction ellipse: the tyre can't give more than its load allows.
            let (max_long, max_lat) = (mu_long * load, mu_lat * load);
            let usage = ((long / max_long.max(1e-3)).powi(2) + (lat / max_lat.max(1e-3)).powi(2)).sqrt();
            // A sliding tyre grips less, down to its dynamic friction.
            let sliding = 1.0 + (desc.engine.slide_grip - 1.0) * (usage - 1.0).clamp(0.0, 1.0);
            let scale = if usage > 1.0 { sliding / usage } else { 1.0 };
            let contact_height = frame.translation.y - wheel.radius - wheel_offsets[wi];
            let lift = (com_height - contact_height).max(0.0) * ANTI_ROLL;
            push.forces.push(((*fwd * long + side * lat) * scale, contact + up * lift));
        }
    } else {
        sim.compression.iter_mut().for_each(|c| *c = 0.0);
    }

    // Air and rolling drag of ground vehicles (aircraft and boats have theirs in `flight`).
    let speed = velocity.length();
    if desc.aero.is_none() && speed > 0.1 {
        push.forces.push((-velocity * speed * desc.physics.drag * 1.2, com_world));
    }

    *state = VehicleState {
        joints,
        wheels: wheel_offsets,
        engine: revs,
        gear: sim.gear,
        // Coarse steps, so the meter doesn't replicate every tick.
        boost: (sim.flight.boost * 50.0).round() / 50.0,
    };
    push
}

/// Share of the top revs up to which the engine pulls fully; the rev limiter takes the pull
/// away from there to the top revs.
const REV_LIMITER: f32 = 0.95;

/// One tick of an automatic gearbox: picks forward or reverse with the throttle, changes gear
/// at its shift points (pulling nothing while it changes) and returns the pull at full
/// throttle along the hull (negative in reverse) and the revs as a share of the top revs.
fn step_gearbox(gearbox: &GearboxDesc, sim: &mut VehicleSim, throttle: f32, forward_speed: f32, dt: f32) -> (f32, f32) {
    let top_gear = gearbox.gears.len() as i8 - 1;
    if throttle < 0.0 && forward_speed < 0.5 {
        sim.gear = -1;
    } else if (throttle > 0.0 && forward_speed > -0.5 && sim.gear < 0) || sim.gear > top_gear {
        sim.gear = 0;
    }
    sim.shift = (sim.shift - dt).max(0.0);
    let gear = |g: i8| if g < 0 { &gearbox.reverse } else { &gearbox.gears[g as usize] };
    let mut revs = forward_speed.abs() / gear(sim.gear).top_speed.max(0.1);
    if sim.gear >= 0 && sim.shift == 0.0 {
        let next = if revs > gearbox.shift_up && sim.gear < top_gear {
            sim.gear + 1
        } else if revs < gearbox.shift_down && sim.gear > 0 {
            sim.gear - 1
        } else {
            sim.gear
        };
        if next != sim.gear {
            sim.gear = next;
            sim.shift = gearbox.shift_time;
            revs = forward_speed.abs() / gear(next).top_speed.max(0.1);
        }
    }
    let pull = if sim.shift > 0.0 {
        0.0
    } else {
        let limiter = ((1.0 - revs) / (1.0 - REV_LIMITER)).clamp(0.0, 1.0);
        gear(sim.gear).force * limiter * if sim.gear < 0 { -1.0 } else { 1.0 }
    };
    (pull, revs.clamp(gearbox.idle, 1.0))
}

/// What each gunner looks at: guns converge on the point under their crosshair.
fn aim_points(
    model: &VehicleModel,
    body: &BodyState,
    inputs: &[Option<InputFrame>],
    current: &[[f32; 3]],
    spatial: &SpatialQuery,
    entity: Entity,
) -> Vec<Option<Vec3>> {
    let filter = SpatialQueryFilter::from_mask([GameLayer::World, GameLayer::Vehicle]).with_excluded_entities([entity]);
    let previous = model.part_transforms(current);
    inputs
        .iter()
        .enumerate()
        .map(|(seat, input)| {
            let input = input.as_ref().filter(|_| model.seat_aims(seat))?;
            let eye = body.position + body.rotation * model.eye(&previous, seat);
            let dir = Dir3::new(Quat::from_euler(EulerRot::YXZ, input.yaw, input.pitch, 0.0) * Vec3::NEG_Z).ok()?;
            let distance = spatial
                .cast_ray(eye, dir, AIM_RANGE, true, &filter)
                .map_or(AIM_RANGE, |hit| hit.distance.max(MIN_AIM_DISTANCE));
            Some(eye + *dir * distance)
        })
        .collect()
}

/// Turns every joint towards what its seat asks for (aimed joints towards `aim_points`, per
/// seat); returns the new angles and the parts' transforms in hull space with them.
pub fn step_joints(
    model: &VehicleModel,
    body: &BodyState,
    aim_points: &[Option<Vec3>],
    current: &[[f32; 3]],
    controls: &Controls,
    flight: &FlightState,
    dt: f32,
) -> (Vec<[f32; 3]>, Vec<Transform>) {
    let desc = &model.desc;
    let (body_pos, body_rot) = (body.position, body.rotation);
    let forward_speed = body.velocity.dot(body_rot * Vec3::NEG_Z);
    let top = desc.engine.top_speed.max(1.0);

    let mut joints = current.to_vec();
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
            // Steering eases off with speed; rudders and control surfaces don't.
            let steer_scale = if model.is_wing[i] { 1.0 } else { 1.0 - 0.6 * (forward_speed.abs() / top).min(1.0) };
            let drive = JointDrive {
                aim,
                frame,
                steer: controls.steer * steer_scale,
                pitch: controls.pitch,
                roll: controls.roll,
                gear_up: flight.gear_up,
                spin: flight.spin.max(flight.throttle.abs()),
            };
            joints[j] = step_joint(joint, joints[j], &drive, dt);
        }
        let mut local = model.rest[i];
        if let Some(angles) = model.joint_index[i].map(|j| joints[j]) {
            local.rotation *= joint_rotation(angles);
        }
        hull.push(parent * local);
    }
    (joints, hull)
}

/// What turns a joint this tick.
struct JointDrive {
    /// The seat's aim direction (world space).
    aim: Option<Vec3>,
    /// The joint's rest orientation in world space (hull rotation included).
    frame: Quat,
    steer: f32,
    pitch: f32,
    roll: f32,
    gear_up: bool,
    /// Rotor or engine speed, 0..1.
    spin: f32,
}

/// Moves one joint towards what its seat asks for.
fn step_joint(joint: &game_data::JointDesc, mut angles: [f32; 3], drive: &JointDrive, dt: f32) -> [f32; 3] {
    // The aim direction in the joint's frame.
    let aim = drive.aim.map(|d| drive.frame.inverse() * d);
    for axis in 0..3 {
        let a = &joint.axes[axis];
        let Some(kind) = a.input else {
            continue;
        };
        let (min, max) = (a.min.to_radians(), a.max.to_radians());
        let stick = |input: f32| {
            let t = (input * a.speed.signum()).clamp(-1.0, 1.0);
            Some(if t >= 0.0 { t * max } else { -t * min })
        };
        let target = match kind {
            JointInput::AimYaw => aim.map(|d| (-d.x).atan2(-d.z)),
            JointInput::AimPitch => aim.map(|d| {
                // Pitch after this joint's own yaw.
                let d = Quat::from_rotation_y(-angles[0]) * d;
                d.y.atan2(Vec2::new(d.x, d.z).length())
            }),
            JointInput::Steer => stick(drive.steer),
            JointInput::Pitch => stick(drive.pitch),
            JointInput::Roll => stick(drive.roll),
            JointInput::Gear => Some(match drive.gear_up {
                true if max != 0.0 => max,
                true => min,
                false => 0.0,
            }),
            JointInput::Spin => {
                angles[axis] = wrap_angle(angles[axis] + a.speed.to_radians() * drive.spin * dt);
                None
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
#[allow(clippy::type_complexity)]
fn record_motion(
    mut vehicles: Query<(&Position, &Rotation, &LinearVelocity, &AngularVelocity, &VehicleSim, &mut VehicleMotion)>,
) {
    for (position, rotation, velocity, angular, sim, mut motion) in &mut vehicles {
        motion.set_if_neq(VehicleMotion {
            position: position.0,
            rotation: rotation.0,
            velocity: velocity.0,
            angular_velocity: angular.0,
            ack: sim.driver_seq,
        });
    }
}

/// Linear and angular damping of a vehicle's body: aircraft keep flying through the air,
/// ground vehicles lose a little to it.
pub fn body_damping(desc: &VehicleDesc) -> (f32, f32) {
    if desc.category.flies() { (0.0, 0.05) } else { (0.02, 0.3) }
}

/// Advances a vehicle's body by one tick under `push` and gravity, like avian does (without
/// collisions): the driver's prediction on clients, where vehicles aren't simulated.
pub fn integrate(model: &VehicleModel, body: &mut BodyState, push: &Push, dt: f32) {
    let desc = &model.desc;
    let mass = desc.physics.mass;
    let com_local = Vec3::from(desc.physics.center_of_mass);
    let com = body.position + body.rotation * com_local;
    let mut force = Vec3::NEG_Y * GRAVITY * desc.physics.gravity * mass;
    let mut torque = push.torque;
    for (f, point) in &push.forces {
        force += *f;
        torque += (*point - com).cross(*f);
    }
    let rotation = Mat3::from_quat(body.rotation);
    let inverse_inertia = rotation * Mat3::from_diagonal(model.inertia.recip()) * rotation.transpose();
    let (linear, angular) = body_damping(desc);
    body.velocity = (body.velocity + force / mass * dt) / (1.0 + dt * linear);
    body.angular_velocity = (body.angular_velocity + inverse_inertia * torque * dt) / (1.0 + dt * angular);
    // The body turns about its center of mass.
    let w = body.angular_velocity * (0.5 * dt);
    let q = body.rotation;
    let spin = Quat::from_xyzw(w.x, w.y, w.z, 0.0) * q;
    body.rotation = Quat::from_xyzw(q.x + spin.x, q.y + spin.y, q.z + spin.z, q.w + spin.w).normalize();
    body.position = com + body.velocity * dt - body.rotation * com_local;
}
