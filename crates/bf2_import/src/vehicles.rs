//! Vehicles and stationary weapons: `vehicles/<name>.ron` plus their meshes.
//!
//! A BF2 vehicle is a `PlayerControlObject` template tree: the root is the rigid body and the
//! driver's seat, nested `PlayerControlObject`s are further seats, `RotationalBundle`s are
//! joints turned by an input (turrets, barrels, steering knuckles, rotor heads), `Spring`s
//! are wheels, `Engine`s drive (cars, tanks), push (jets, ships) or lift (helicopters),
//! `Wing`s are lifting and control surfaces, `LandingGear`s retract, `Rotor`s spin,
//! `FloatingBundle`s float, `Camera`s and `EntryPoint`s mark views and doors,
//! `GenericFireArm`s are guns. The `.con` builds the tree, the `.tweak` sets the numbers. See
//! docs/formats/gameplay-data.md §3-5.

use std::{collections::HashMap, path::Path};

use anyhow::Result;
use bf2_formats::{
    collision::{ColType, CollisionMesh},
    con::{Geometry, Interpreter, Template, World, parse_vec3},
    localization::Localization,
    mesh::{MeshKind, Usage, VisMesh},
};
use game_data::{
    AeroDesc, AfterburnerDesc, Attachment, CountermeasureDesc, DriveKind, EngineDesc, EntryPointDesc, FloaterDesc,
    GearDesc, GearboxDesc, JointAxis, JointDesc, JointInput, LandingGearDesc, MeshLod, ModelLods, Placement,
    RemoteKind, RotorDesc, SeatCamera, SeatDesc, ThrusterDesc, TrackWheelDesc, UvAnimationDesc, UvMotion,
    VehicleArmorEffect, VehicleCategory, VehicleDesc, VehiclePart, VehiclePhysics, VehicleWeaponDesc, WheelDesc,
    WingDesc,
};
use glam::{Affine3A, Quat, Vec3};

use crate::{coords, meshes::MeshConverter, weapons};

/// Template types that become parts of the vehicle tree. Effects, sounds and the like are
/// left out; cameras and entry points are recorded on their parent part.
const PART_TYPES: &[&str] = &[
    "playercontrolobject",
    "rotationalbundle",
    "bundle",
    "simpleobject",
    "spring",
    "engine",
    "genericfirearm",
    "antennaobject",
    "wing",
    "landinggear",
    "rotor",
    "floatingbundle",
    // The commander's UAV (its root).
    "uavvehicle",
];

/// BF2's lift, thrust and drag numbers in our units (fitted by flying, see
/// docs/ARCHITECTURE.md "Aircraft"): wing lift per `setWingLift`, flap lift per
/// `setFlapLift`, thrust acceleration per `setTorque` × `setDifferential`, drag per `drag`.
const LIFT_PER_WING_LIFT: f32 = 0.01;
const LIFT_PER_FLAP_LIFT: f32 = 0.003;
const THRUST_PER_POWER: f32 = 0.004;
const DRAG_PER_DRAG: f32 = 0.003;
/// BF2's land engine numbers in newtons (fitted, see [`tune_engine`]): drive force per
/// `setTorque` × `setDifferential` × gear ratio / wheel radius, brake force per
/// `brakeTorque` / radius, engine braking per `engineBrakeTorque` / radius; tanks push with
/// `setTorque` × `setDifferential` times their own factor.
const FORCE_PER_TORQUE: f32 = 4.5;
const BRAKE_PER_TORQUE: f32 = 2.5;
const ENGINE_BRAKE_PER_TORQUE: f32 = 0.4;
const TRACK_FORCE_PER_POWER: f32 = 125.0;
/// BF2's parachute canopy (converted to `.glb` beside it).
const PARACHUTE_MESH: &str = "objects/vehicles/air/parachute/meshes/animatedparachute.skinnedmesh";
/// Landing flaps' lift (`setFlapLift 3`) would lift a jet off at a walking pace.
const LANDING_FLAP_SHARE: f32 = 0.3;
/// Rudders in the water need more bite than BF2's numbers give against our water drag.
const WATER_RUDDER_SHARE: f32 = 6.0;
/// Helicopter fins and stub wings only steady them: at full strength, flying nose down
/// would press them down harder than the rotor can lift.
const HELICOPTER_WING_SHARE: f32 = 0.1;

/// Imports the given vehicle templates (those that can't be used, like parachutes, are
/// skipped). Returns the names written.
pub fn import(
    interp: &mut Interpreter,
    converter: &MeshConverter,
    localization: &Localization,
    names: &[String],
    out: &Path,
) -> Result<Vec<String>> {
    let mut written = Vec::new();
    let mut effects = Vec::new();
    let huds = crate::vehicle_hud::VehicleHuds::load(converter);
    for name in names {
        interp.ensure_template(name);
        load_tree(interp, name, 0);
        let Some((mut desc, drivetrain)) = build(interp, converter, name) else {
            continue;
        };
        desc.display_name = localization.resolve(&desc.display_name);
        desc.engine.top_speed = ai_max_speed(interp, name)
            .map(|s| s * 1.1)
            .unwrap_or(desc.engine.top_speed);
        if matches!(desc.drive, DriveKind::Wheeled | DriveKind::Tracked) {
            tune_engine(&mut desc, drivetrain.as_ref());
        }
        desc.weapons = weapon_descs(interp, converter, localization, &huds, &desc, out);
        // Smoke and flare effects live with the vehicle effects, which aren't all imported.
        for weapon in desc.weapons.iter().filter(|w| w.countermeasure.is_some()) {
            let projectile = &weapon.weapon.projectile;
            effects.extend(projectile.detonation_effect.iter().chain(&projectile.trail_effect).cloned());
        }
        desc.sounds = crate::sounds::SoundConverter::new(converter.vfs, out).vehicle(&interp.world, name);
        desc.armor_effects = armor_effects(interp, name);
        effects.extend(desc.armor_effects.iter().map(|e| e.effect.clone()));
        game_data::write_ron(out.join("vehicles").join(format!("{name}.ron")), &desc)?;
        written.push(name.clone());
    }
    let (count, _) = crate::effects::convert_named(interp, converter, effects, out);
    // The parachute soldiers bail out of aircraft with (its canopy in its bind pose).
    if let Err(err) = converter.convert_mesh(PARACHUTE_MESH) {
        log::debug!("parachute: {err:#}");
    }
    log::info!("vehicles: {} imported, {count} more damage effects", written.len());
    Ok(written)
}

/// The smoke, fire and explosions of the vehicle's damage states
/// (`armor.addArmorEffect[Spectacular] <hit points %> <effect> <position> <rotation>`).
fn armor_effects(interp: &mut Interpreter, name: &str) -> Vec<VehicleArmorEffect> {
    let Some(template) = interp.world.template(name).cloned() else {
        return Vec::new();
    };
    let mut effects = Vec::new();
    for (property, spectacular) in [("armor.addarmoreffect", false), ("armor.addarmoreffectspectacular", true)] {
        for effect in crate::destruction::armor_effects(&template, property) {
            crate::effects::load_effect(interp, &effect.template);
            effects.push(VehicleArmorEffect {
                hit_points: effect.hit_points,
                lasting: crate::effects::lasts(&interp.world, &effect.template),
                position: coords::position(effect.position),
                rotation: coords::rotation_ypr(effect.rotation).to_array(),
                effect: effect.template,
                spectacular,
            });
        }
    }
    effects
}

/// Children can be defined in other files (shared weapons, antennas): load them the way the
/// game does, by template name.
fn load_tree(interp: &mut Interpreter, name: &str, depth: u32) {
    if depth > 16 {
        return;
    }
    interp.ensure_template(name);
    let children: Vec<String> = interp
        .world
        .template(name)
        .map(|t| t.children.iter().map(|c| c.template.clone()).collect())
        .unwrap_or_default();
    for child in children {
        if !child.to_ascii_lowercase().starts_with("s_") {
            load_tree(interp, &child, depth + 1);
        }
    }
}

/// A part of the tree being built, with what we need to know about its template.
struct Node {
    template: String,
    ty: String,
    /// Rest transform in hull space.
    hull: Affine3A,
    /// Seat whose occupant controls this part.
    seat: Option<u32>,
    /// BF2 mesh file and part index, for measuring wheels.
    source_mesh: Option<(String, u32)>,
    /// BF2 collision mesh file and the template whose `mapMaterial`s name its materials.
    source_collision: Option<(String, String)>,
}

struct Builder<'a> {
    world: &'a World,
    converter: &'a MeshConverter<'a>,
    parts: Vec<VehiclePart>,
    nodes: Vec<Node>,
    /// Seat templates, in tree order.
    seats: Vec<u32>,
    cameras: Vec<(u32, String, Placement)>,
    entry_points: Vec<EntryPointDesc>,
    /// Lower LODs of the outside models met so far, with the part that owns each model and
    /// the model's radius (half the diagonal of its full-detail box).
    lods: Vec<(ModelLods, usize, f32)>,
}

/// A part's model: the BF2 mesh file, its converted outside view and interior.
#[derive(Clone)]
struct PartMesh {
    source: String,
    outside: String,
    interior: Option<String>,
}

#[derive(Clone, Default)]
struct Inherited {
    mesh: Option<PartMesh>,
    collision: Option<String>,
    source_collision: Option<(String, String)>,
}

impl Builder<'_> {
    #[allow(clippy::too_many_arguments)]
    fn walk(
        &mut self,
        name: &str,
        parent: Option<u32>,
        local: Affine3A,
        parent_hull: Affine3A,
        inherited: Inherited,
        seat: Option<u32>,
        depth: u32,
    ) {
        let Some(template) = self.world.template(name) else {
            return;
        };
        if depth > 16 {
            return;
        }
        let ty = template.ty.to_ascii_lowercase();
        let hull = parent_hull * local;
        match ty.as_str() {
            "camera" => {
                if let Some(parent) = parent {
                    self.cameras.push((parent, template.name.to_ascii_lowercase(), placement(local)));
                }
                return;
            }
            "entrypoint" => {
                self.entry_points.push(EntryPointDesc {
                    position: hull.translation.to_array(),
                    radius: template.get_f32("setentryradius").unwrap_or(3.0),
                });
                return;
            }
            t if !PART_TYPES.contains(&t) => return,
            _ => {}
        }

        // Meshes: vehicles and guns keep the third-person model in geom 1 and the interior
        // (cockpits, gunner sights) in geom 0. Skinned parts (swaying antennas) need their
        // skeleton, which isn't imported for vehicles yet.
        let geometry = template
            .geometry
            .as_deref()
            .and_then(|g| self.world.geometry(g))
            .filter(|g| !g.ty.eq_ignore_ascii_case("SkinnedMesh"));
        let own_mesh = geometry.and_then(|g| g.mesh_path()).and_then(|path| {
            let (outside, interior, geom, suffix) = match self.converter.convert_mesh_rigged(&path, 1, "_rig3p") {
                Ok(outside) => (outside, self.converter.convert_mesh_rigged(&path, 0, "_rig1p").ok(), 1, "_rig3p"),
                Err(_) => (
                    self.converter
                        .convert_mesh_rigged(&path, 0, "_rig")
                        .map_err(|e| log::debug!("vehicle mesh {path}: {e:#}"))
                        .ok()?,
                    None,
                    0,
                    "_rig",
                ),
            };
            if !self.lods.iter().any(|(l, ..)| l.mesh == outside) {
                let (lods, radius) = model_lods(self.converter, geometry, &path, geom, suffix, &outside);
                self.lods.push((lods, self.parts.len(), radius));
            }
            Some(PartMesh {
                source: path,
                outside,
                interior,
            })
        });
        let own_source_collision = template
            .collision_mesh
            .as_deref()
            .and_then(|c| self.world.collision_meshes.get(&c.to_ascii_lowercase()));
        let own_collision = own_source_collision.and_then(|path| {
            self.converter
                .convert_collision(path)
                .map_err(|e| log::debug!("vehicle collision {path}: {e:#}"))
                .ok()
        });
        let own_source_collision = own_source_collision
            .filter(|_| own_collision.is_some())
            .map(|path| (path.clone(), template.name.to_ascii_lowercase()));
        let geometry_part = template.get_f32("geometrypart").map(|p| p as u32);
        let collision_part = template.get_f32("collisionpart").map(|p| p as u32);
        let mesh = own_mesh
            .clone()
            .or_else(|| geometry_part.and(inherited.mesh.clone()));
        let collision = own_collision
            .clone()
            .or_else(|| collision_part.and(inherited.collision.clone()));
        let source_collision = own_source_collision
            .clone()
            .or_else(|| collision_part.and(inherited.source_collision.clone()));

        let index = self.parts.len() as u32;
        let seat = if ty == "playercontrolobject" {
            self.seats.push(index);
            Some(self.seats.len() as u32 - 1)
        } else {
            seat
        };
        let joint = match ty.as_str() {
            "rotationalbundle" => joint(template, seat.unwrap_or(0)),
            // Control surfaces only deflect about their pitch axis.
            "wing" => joint(template, seat.unwrap_or(0)).map(|mut j| {
                j.axes[0] = JointAxis::default();
                j.axes[2] = JointAxis::default();
                j
            }),
            "landinggear" => gear_joint(template),
            "rotor" => spin_joint(template),
            _ => None,
        };
        self.parts.push(VehiclePart {
            name: template.name.to_ascii_lowercase(),
            parent,
            placement: placement(local),
            mesh: mesh.as_ref().map(|m| m.outside.clone()),
            mesh_1p: mesh.as_ref().and_then(|m| m.interior.clone()),
            mesh_index: geometry_part.unwrap_or(0),
            collision,
            collision_part: collision_part.unwrap_or(0),
            joint,
        });
        self.nodes.push(Node {
            template: template.name.to_ascii_lowercase(),
            ty,
            hull,
            seat,
            source_mesh: mesh.map(|m| (m.source, geometry_part.unwrap_or(0))),
            source_collision,
        });

        let inherited = Inherited {
            mesh: own_mesh.or(inherited.mesh),
            collision: own_collision.or(inherited.collision),
            source_collision: own_source_collision.or(inherited.source_collision),
        };
        for child in &template.children {
            let local = Affine3A::from_rotation_translation(
                coords::rotation_ypr(child.rotation.unwrap_or([0.0; 3])),
                Vec3::from_array(coords::position(child.position.unwrap_or([0.0; 3]))),
            );
            self.walk(&child.template, Some(index), local, hull, inherited.clone(), seat, depth + 1);
        }
    }
}

fn placement(transform: Affine3A) -> Placement {
    let (scale, rotation, translation) = transform.to_scale_rotation_translation();
    // Rotations leave scale a hair off 1.
    let scale = if scale.abs_diff_eq(Vec3::ONE, 1e-4) { Vec3::ONE } else { scale };
    Placement {
        position: (translation + Vec3::ZERO).to_array(),
        rotation: rotation.normalize().to_array(),
        scale: scale.to_array(),
    }
}

/// A `RotationalBundle`'s input-driven axes, converted to engine conventions: yaw and pitch
/// flip sign (BF2 is left-handed and pitches nose-down), roll keeps it.
fn joint(t: &Template, seat: u32) -> Option<JointDesc> {
    let input = |method: &str| -> Option<JointInput> {
        match t.get_str(method)?.to_ascii_lowercase().as_str() {
            "pimouselookx" => Some(JointInput::AimYaw),
            "pimouselooky" => Some(JointInput::AimPitch),
            "piyaw" => Some(JointInput::Steer),
            "pithrottle" => Some(JointInput::Throttle),
            "pipitch" => Some(JointInput::Pitch),
            "piroll" => Some(JointInput::Roll),
            _ => None,
        }
    };
    let inputs = [input("setinputtoyaw"), input("setinputtopitch"), input("setinputtoroll")];
    if inputs.iter().all(Option::is_none) {
        return None;
    }
    let vec = |method: &str| t.get_vec3(method).unwrap_or([0.0; 3]);
    let (min, max, speed, accel) = (
        vec("setminrotation"),
        vec("setmaxrotation"),
        vec("setmaxspeed"),
        vec("setacceleration"),
    );
    let automatic_reset = t.get_f32("setautomaticreset").unwrap_or(0.0) != 0.0;
    let axes = std::array::from_fn(|i| {
        if inputs[i].is_none() {
            return JointAxis::default();
        }
        let sign = if i == 2 { 1.0 } else { -1.0 };
        let (lo, hi) = if sign > 0.0 { (min[i], max[i]) } else { (-max[i], -min[i]) };
        // `+ 0.0` turns -0 into 0.
        JointAxis {
            input: inputs[i],
            min: lo + 0.0,
            max: hi + 0.0,
            speed: speed[i] * sign + 0.0,
            acceleration: accel[i].abs(),
            automatic_reset,
        }
    });
    Some(JointDesc { axes, seat })
}

/// A `LandingGear`: every axis with a limit swings to it while the gear is up.
fn gear_joint(t: &Template) -> Option<JointDesc> {
    let vec = |method: &str| t.get_vec3(method).unwrap_or([0.0; 3]);
    let (min, max, speed) = (vec("setminrotation"), vec("setmaxrotation"), vec("setmaxspeed"));
    let axes: [JointAxis; 3] = std::array::from_fn(|i| {
        if min[i] == 0.0 && max[i] == 0.0 {
            return JointAxis::default();
        }
        let (lo, hi) = if i == 2 { (min[i], max[i]) } else { (-max[i], -min[i]) };
        JointAxis {
            input: Some(JointInput::Gear),
            min: lo + 0.0,
            max: hi + 0.0,
            speed: speed[i].abs().max(20.0),
            ..Default::default()
        }
    });
    axes.iter().any(|a| a.input.is_some()).then_some(JointDesc { axes, seat: 0 })
}

/// Rotor blades spin about their `rotationAxle` (0: up, 1: sideways) with the engine.
fn spin_joint(t: &Template) -> Option<JointDesc> {
    /// Degrees per second of a `rotationSpeedMod` of 1 at full rotor speed.
    const SPIN: f32 = 1800.0;
    let axis = match t.get_f32("rotationaxle").unwrap_or(0.0) as usize {
        1 => 1,
        2 => 2,
        _ => 0,
    };
    let mut axes: [JointAxis; 3] = Default::default();
    axes[axis] = JointAxis {
        input: Some(JointInput::Spin),
        speed: t.get_f32("rotationspeedmod").unwrap_or(1.0) * SPIN,
        ..Default::default()
    };
    Some(JointDesc { axes, seat: 0 })
}

/// Lower LODs of a rigged model: LOD 1.. of geom `geom` as `x{suffix}_lod{N}.glb`, each
/// taking over where BF2 switches (its `setSubGeometryLodDistance` or the engine's running
/// default, plus `r0`, half the diagonal of the geom's LOD 0 box). Also returns `r0`.
fn model_lods(
    converter: &MeshConverter,
    geometry: Option<&Geometry>,
    path: &str,
    geom: usize,
    suffix: &str,
    outside: &str,
) -> (ModelLods, f32) {
    let count = converter.lod_count(path, geom);
    let r0 = converter.lod0_half_diagonal(path, geom).unwrap_or(0.0);
    let starts = crate::lods::lod_starts(&crate::lods::lod_distances(geometry, geom, count));
    let mut lods = Vec::new();
    for (index, distance) in starts.into_iter().enumerate() {
        let lod = index + 1;
        match converter.convert_mesh_rigged_lod(path, geom, lod, &format!("{suffix}_lod{lod}")) {
            Ok(mesh) => lods.push(MeshLod {
                mesh,
                distance: distance * crate::lods::BUNDLED_LOD_SCALE + r0,
            }),
            Err(err) => {
                log::warn!("{path} geom {geom} LOD {lod}: {err:#}");
                break;
            }
        }
    }
    let lods = ModelLods {
        mesh: outside.to_string(),
        lods,
        draw_distance: None,
    };
    (lods, r0)
}

/// BF2's radius of an object (`Object::getRadius`) for each part of the tree: the part's
/// collision radius (the farthest corner of its collision part's box, from its first geom
/// and column that has faces), grown over its children by their distance plus their own
/// radius.
fn part_radii(world: &World, converter: &MeshConverter, nodes: &[Node], parts: &[VehiclePart]) -> Vec<f32> {
    let mut meshes: HashMap<String, Option<CollisionMesh>> = HashMap::new();
    let mut radii: Vec<f32> = nodes
        .iter()
        .map(|node| {
            let Some((path, _)) = &node.source_collision else {
                return 0.0;
            };
            let mesh = meshes
                .entry(path.clone())
                .or_insert_with(|| converter.vfs.read(path).ok().and_then(|data| CollisionMesh::parse(&data).ok()));
            let collision_part = world
                .template(&node.template)
                .and_then(|t| t.get_f32("collisionpart"))
                .unwrap_or(0.0) as usize;
            mesh.as_ref()
                .and_then(|m| m.parts.get(collision_part))
                .and_then(|part| part.geoms.iter().flat_map(|g| &g.cols).find(|c| !c.faces.is_empty()))
                .map_or(0.0, |col| crate::lods::corner_radius(col.bounds_min, col.bounds_max))
        })
        .collect();
    // Children come after their parents: fold from the leaves up.
    for i in (1..parts.len()).rev() {
        if let Some(parent) = parts[i].parent {
            let reach = Vec3::from_array(parts[i].placement.position).length() + radii[i];
            radii[parent as usize] = radii[parent as usize].max(reach);
        }
    }
    radii
}

/// Lower LODs of a vehicle's wreck (geom 2) as `x_wreck_lod{N}.glb`, laid out like its
/// wreck model, with BF2's switch distances.
fn wreck_lods(converter: &MeshConverter, geometry: Option<&Geometry>, path: &str) -> Vec<MeshLod> {
    let count = converter.lod_count(path, 2);
    let r0 = converter.lod0_half_diagonal(path, 2).unwrap_or(0.0);
    let starts = crate::lods::lod_starts(&crate::lods::lod_distances(geometry, 2, count));
    let mut lods = Vec::new();
    for (index, distance) in starts.into_iter().enumerate() {
        let lod = index + 1;
        match converter.convert_mesh_lod(path, 2, lod, &format!("_wreck_lod{lod}")) {
            Ok(mesh) => lods.push(MeshLod {
                mesh,
                distance: distance * crate::lods::BUNDLED_LOD_SCALE + r0,
            }),
            Err(err) => {
                log::warn!("{path} wreck LOD {lod}: {err:#}");
                break;
            }
        }
    }
    lods
}

/// Which remote controlled object a template is: the commander's artillery (a
/// `RemoteControlledObject` child with `rcType RCArtillery`) or the UAV (`UAVVehicle`).
fn remote_kind(world: &World, root: &Template) -> Option<RemoteKind> {
    if root.ty.eq_ignore_ascii_case("UAVVehicle") {
        return Some(RemoteKind::Uav);
    }
    root.children
        .iter()
        .filter_map(|c| world.template(&c.template))
        .any(|t| {
            t.ty.eq_ignore_ascii_case("RemoteControlledObject")
                && t.get_str("rctype").is_some_and(|r| r.eq_ignore_ascii_case("RCArtillery"))
        })
        .then_some(RemoteKind::Artillery)
}

fn build(interp: &Interpreter, converter: &MeshConverter, name: &str) -> Option<(VehicleDesc, Option<Drivetrain>)> {
    let world = &interp.world;
    let root = world.template(name)?;
    let remote = remote_kind(world, root);
    if !root.ty.eq_ignore_ascii_case("PlayerControlObject") && remote != Some(RemoteKind::Uav) {
        return None;
    }
    let mut builder = Builder {
        world,
        converter,
        parts: Vec::new(),
        nodes: Vec::new(),
        seats: Vec::new(),
        cameras: Vec::new(),
        entry_points: Vec::new(),
        lods: Vec::new(),
    };
    builder.walk(name, None, Affine3A::IDENTITY, Affine3A::IDENTITY, Inherited::default(), None, 0);
    let Builder {
        parts,
        nodes,
        seats,
        cameras,
        entry_points,
        lods: model_lods,
        ..
    } = builder;
    // How far BF2 draws the vehicle (its cull radius), and each model: with the vehicle, or on
    // its own if it is a small part (rotors).
    let radii = part_radii(world, converter, &nodes, &parts);
    let cull_scale = |node: usize| {
        world
            .template(&nodes[node].template)
            .and_then(|t| t.get_f32("cullradiusscale"))
            .unwrap_or(1.0)
            .max(0.0)
    };
    let vehicle_cull = radii.first().copied().unwrap_or(0.0) * cull_scale(0);
    let draw_distance = crate::lods::pco_draw_distance(Some(vehicle_cull));
    let lods: Vec<ModelLods> = model_lods
        .into_iter()
        .map(|(mut lods, node, model_radius)| {
            // A part with a model of its own: its radius is the model's, grown over its
            // children.
            let radius = if node == 0 { vehicle_cull } else { radii[node].max(model_radius) * cull_scale(node) };
            if node != 0 && radius < crate::lods::SMALL_PART_SHARE * vehicle_cull {
                lods.draw_distance = crate::lods::small_part_draw_distance(radius);
            }
            lods
        })
        .collect();
    // Nobody sits in remote controlled objects: the commander works them.
    let seats = if remote.is_some() { Vec::new() } else { seats };

    // What moves it: the engines in the tree, by BF2 engine type.
    let engines: Vec<(usize, &Template, String)> = nodes
        .iter()
        .enumerate()
        .filter(|(_, n)| n.ty == "engine")
        .filter_map(|(i, n)| {
            let t = world.template(&n.template)?;
            Some((i, t, t.get_str("setenginetype")?.to_ascii_lowercase()))
        })
        .collect();
    let land_engine = engines
        .iter()
        .find(|(_, _, ty)| matches!(ty.as_str(), "c_etnewcar2" | "c_etnewcar" | "c_etcar" | "c_ettank"));
    let has_engine = |kind: &str| engines.iter().any(|(_, _, ty)| ty == kind);
    let category = match root.get_str("vehiclecategory").map(str::to_ascii_lowercase).as_deref() {
        _ if root.get_f32("hasmobilephysics") == Some(0.0) => VehicleCategory::Stationary,
        Some("vcair") => VehicleCategory::Air,
        Some("vchelicopter") => VehicleCategory::Helicopter,
        Some("vcsea") => VehicleCategory::Sea,
        _ if has_engine("c_etplane") => VehicleCategory::Air,
        _ if land_engine.is_some() => VehicleCategory::Land,
        _ if has_engine("c_ethelicopter") => VehicleCategory::Helicopter,
        _ if has_engine("c_etship") => VehicleCategory::Sea,
        _ if remote == Some(RemoteKind::Uav) => VehicleCategory::Air,
        _ => {
            log::debug!("{name}: nothing moves it");
            return None;
        }
    };
    // Stationary objects without a way in (radars, trailers) aren't vehicles; the
    // commander's artillery is, worked from afar.
    if category == VehicleCategory::Stationary && entry_points.is_empty() && remote.is_none() {
        log::debug!("{name}: stationary without an entry point");
        return None;
    }
    let drive = match (land_engine, category) {
        (Some((_, _, ty)), _) if ty == "c_ettank" => DriveKind::Tracked,
        (Some(_), _) => DriveKind::Wheeled,
        _ if remote.is_some() => DriveKind::None,
        (None, VehicleCategory::Air | VehicleCategory::Helicopter) => DriveKind::Rolling,
        _ => DriveKind::None,
    };

    let mut meshes = MeshCache::default();
    // Interiors have their own, shorter part lists (and some outside models lack parts the
    // templates name): only parts a model has are drawn from it.
    let mut parts = parts;
    for (part, node) in parts.iter_mut().zip(&nodes) {
        let Some((path, index)) = &node.source_mesh else {
            continue;
        };
        let geoms = meshes.geom_count(converter, path);
        let outside_geom = if geoms > 1 { 1 } else { 0 };
        if meshes.part_count(converter, path, outside_geom).is_some_and(|count| *index >= count) {
            part.mesh = None;
        }
        if part.mesh_1p.is_some() && meshes.part_count(converter, path, 0).is_none_or(|count| *index >= count) {
            part.mesh_1p = None;
        }
    }
    let wheels: Vec<WheelDesc> = nodes
        .iter()
        .enumerate()
        .filter(|(_, n)| n.ty == "spring")
        .filter_map(|(i, n)| {
            let t = world.template(&n.template)?;
            let strength = t.get_f32("setstrength").unwrap_or(0.0);
            // Helicopter skid springs sit on the skid; their mesh part is the whole skid.
            let radius = match category {
                VehicleCategory::Helicopter => Some(0.12),
                _ => n
                    .source_mesh
                    .as_ref()
                    .and_then(|(path, part)| meshes.part_radius(converter, path, *part)),
            }
            .unwrap_or(0.45);
            Some(WheelDesc {
                part: i as u32,
                position: n.hull.translation.to_array(),
                radius,
                contact: t.get_f32("grip").unwrap_or(8.0) as u32 != 128 && strength > 0.0,
                strength,
                damping: t.get_f32("setdamping").unwrap_or(0.0),
                turns: t.get_f32("rotateuv").unwrap_or(0.0) == 0.0,
            })
        })
        .collect();

    let seat_descs: Vec<SeatDesc> = seats
        .iter()
        .enumerate()
        .filter_map(|(seat, &part)| {
            let t = world.template(&nodes[part as usize].template)?;
            let pose = seat_pose(converter, t, category, seat);
            // BF2 puts the pose's root bone (the hips) at the seat: our soldier's origin (its
            // feet) goes that far below.
            let hips = pose.as_deref().and_then(|pose| pose_root(converter, pose)).unwrap_or(Vec3::ZERO);
            let soldier = t.get("seatinformation").and_then(|args| {
                let target = args.first()?.to_ascii_lowercase();
                let part = nodes.iter().position(|n| n.template == target)? as u32;
                let position = Vec3::from(coords::position(args.get(1).and_then(|p| parse_vec3(p))?));
                let rotation = coords::rotation_ypr(args.get(2).and_then(|r| parse_vec3(r)).unwrap_or([0.0; 3]));
                Some(Attachment {
                    part,
                    placement: Placement {
                        position: (position - rotation * hips).to_array(),
                        rotation: rotation.to_array(),
                        ..Default::default()
                    },
                })
            });
            let camera = seat_camera(world, &nodes, &cameras, seat as u32);
            let exit = t
                .get("setsoldierexitlocation")
                .and_then(|a| parse_vec3(a.first()?))
                .map(coords::position)
                .unwrap_or([-2.5, 0.0, 0.0]);
            Some(SeatDesc {
                name: t.name.to_ascii_lowercase(),
                part,
                soldier,
                camera,
                exit,
                open: t.get_f32("isopenvehicle").unwrap_or(0.0) != 0.0,
                pose,
            })
        })
        .collect();

    let engine = land_engine.map(|(_, t, _)| *t);
    let get = |method: &str, default: f32| engine.and_then(|e| e.get_f32(method)).unwrap_or(default);
    let engine_desc = EngineDesc {
        top_speed: if drive == DriveKind::Tracked { 17.0 } else { 25.0 },
        turn_rate: get("trackturnspeed", 0.8),
        grip: match drive {
            DriveKind::Tracked => [1.2, 1.4],
            DriveKind::Wheeled => [
                (get("newcar2.wheellongdrivefrictionmod", 1.4) * 0.8).clamp(0.6, 1.6),
                (get("newcar2.wheellatfrictionmod", 2.0) * 0.5).clamp(0.6, 1.8),
            ],
            // Aircraft tyres and skids.
            _ => [1.0, 1.2],
        },
        brake_force: root.get_f32("mass").unwrap_or(1000.0) * 4.0,
        ..Default::default()
    };

    let floats = nodes.iter().any(|n| n.ty == "floatingbundle");
    let wings: Vec<WingDesc> = nodes
        .iter()
        .enumerate()
        .filter(|(_, n)| n.ty == "wing")
        .filter_map(|(i, n)| {
            let t = world.template(&n.template)?;
            let offset = Vec3::from(coords::position(t.get_vec3("setpositionoffset").unwrap_or([0.0; 3])));
            let landing_flap = t.get_f32("setliftregulated").unwrap_or(0.0) != 0.0;
            let share = match category {
                VehicleCategory::Helicopter => HELICOPTER_WING_SHARE,
                _ if landing_flap => LANDING_FLAP_SHARE,
                _ => 1.0,
            };
            let flap_share = if floats { WATER_RUDDER_SHARE } else { share };
            Some(WingDesc {
                part: i as u32,
                position: (Vec3::from(n.hull.translation) + offset).to_array(),
                lift: t.get_f32("setwinglift").unwrap_or(0.0) * LIFT_PER_WING_LIFT * share,
                flap_lift: t.get_f32("setflaplift").unwrap_or(0.0) * LIFT_PER_FLAP_LIFT * flap_share,
                landing_flap,
            })
        })
        .collect();
    let thrusters: Vec<ThrusterDesc> = engines
        .iter()
        .filter(|(_, _, ty)| ty == "c_etplane" || ty == "c_etship")
        .map(|(i, t, ty)| {
            let hull = nodes[*i].hull;
            let power = t.get_f32("settorque").unwrap_or(0.0) * t.get_f32("setdifferential").unwrap_or(0.0);
            let (min, max) = (
                t.get_vec3("setminrotation").map_or(0.0, |v| v[2]),
                t.get_vec3("setmaxrotation").map_or(1.0, |v| v[2]),
            );
            ThrusterDesc {
                position: hull.translation.to_array(),
                direction: hull.transform_vector3(Vec3::NEG_Z).normalize_or(Vec3::NEG_Z).to_array(),
                acceleration: thrust(power),
                // Amphibious vehicles' water jets don't say: they paddle along slowly.
                max_speed: t.get_f32("nopropellereffectatspeed").unwrap_or(if ty == "c_etship" { 7.0 } else { 150.0 }),
                reverse: if max > 0.0 { (-min / max).clamp(0.0, 1.0) } else { 0.0 },
                water: ty == "c_etship",
            }
        })
        .collect();
    let rotor = match category {
        VehicleCategory::Helicopter => rotor_desc(world, root, &nodes, &engines),
        VehicleCategory::Air => vtol_desc(&nodes, &engines),
        _ => None,
    };
    let float_lift: f32 = nodes
        .iter()
        .filter(|n| n.ty == "floatingbundle")
        .filter_map(|n| world.template(&n.template)?.get_f32("setfloatmaxlift"))
        .sum();
    let floaters: Vec<FloaterDesc> = nodes
        .iter()
        .filter(|n| n.ty == "floatingbundle")
        .filter_map(|n| {
            let t = world.template(&n.template)?;
            Some(FloaterDesc {
                position: n.hull.translation.to_array(),
                // A fully submerged hull carries twice its weight.
                lift: 2.0 * t.get_f32("setfloatmaxlift").unwrap_or(1.0) / float_lift.max(1e-3),
                depth: t.get_f32("sethullheight").unwrap_or(1.0).max(0.3),
            })
        })
        .collect();
    let landing_gear = nodes
        .iter()
        .filter(|n| n.ty == "landinggear")
        .filter_map(|n| world.template(&n.template))
        .find(|t| t.get("setgearupheight").is_some())
        .map(|t| LandingGearDesc {
            up_height: t.get_f32("setgearupheight").unwrap_or(10.0),
            up_speed: t.get_f32("setgearupspeed").unwrap_or(50.0),
            down_height: t.get_f32("setgeardownheight").unwrap_or(40.0),
            down_speed: t.get_f32("setgeardownspeed").unwrap_or(80.0),
        });
    let afterburner = root
        .get_f32("sprintfactor")
        .filter(|f| *f > 1.0 && category == VehicleCategory::Air)
        .map(|factor| AfterburnerDesc {
            factor,
            duration: root.get_f32("sprintdissipationtime").unwrap_or(10.0),
            recover: root.get_f32("sprintrecovertime").unwrap_or(30.0),
            min_charge: root.get_f32("sprintlimit").unwrap_or(0.2),
        });
    let aero = (category != VehicleCategory::Land || !floaters.is_empty())
        .then(|| aero_desc(category, root.get_f32("drag").unwrap_or(1.0)))
        .filter(|_| category != VehicleCategory::Stationary);

    // Direct hits land on per-face collision materials (`armor_mesh`); this one is for hits
    // that miss them: a typical hull material per class.
    let armor_material = match (drive, category) {
        (DriveKind::Tracked, _) => 29,
        (_, VehicleCategory::Air | VehicleCategory::Helicopter) => {
            root.get_f32("armor.defaultmaterial").unwrap_or(32.0) as u32
        }
        _ if root.get_str("setvehicletype").is_some_and(|t| t.eq_ignore_ascii_case("VTApc")) => 27,
        _ => 26,
    };
    let hull_mesh = root
        .geometry
        .as_deref()
        .and_then(|g| world.geometry(g))
        .and_then(|g| g.mesh_path());
    let wreck_mesh = hull_mesh
        .as_deref()
        .and_then(|path| converter.convert_mesh_geom(path, 2, "_wreck").ok());
    let wreck_pieces = hull_mesh
        .as_deref()
        .filter(|_| wreck_mesh.is_some())
        .and_then(|path| meshes.part_count(converter, path, 2))
        .unwrap_or(0);
    let hull_geometry = root.geometry.as_deref().and_then(|g| world.geometry(g));
    let wreck_lods = match (&wreck_mesh, &hull_mesh) {
        (Some(_), Some(path)) => wreck_lods(converter, hull_geometry, path),
        _ => Vec::new(),
    };

    let modifier = |method: &str| root.get_vec3(method).map_or([1.0; 3], |v| v.map(f32::abs));
    let mut desc = VehicleDesc {
        name: name.to_ascii_lowercase(),
        display_name: root
            .get_str("vehiclehud.hudname")
            .map(|s| s.trim_matches('"').to_string())
            .unwrap_or_else(|| name.to_string()),
        category,
        rigged: true,
        drive,
        physics: VehiclePhysics {
            mass: root.get_f32("mass").unwrap_or(1000.0),
            gravity: root.get_f32("gravitymodifier").unwrap_or(1.0),
            drag: root.get_f32("drag").unwrap_or(1.0),
            drag_modifier: modifier("dragmodifier"),
            inertia_modifier: modifier("inertiamodifier"),
            ..Default::default()
        },
        engine: engine_desc,
        wings,
        thrusters,
        rotor,
        floaters,
        landing_gear,
        afterburner,
        aero,
        hit_points: root.get_f32("armor.maxhitpoints").unwrap_or(1000.0),
        armor_material,
        blast_material: root.get_f32("armor.defaultmaterial").unwrap_or(72.0) as u32,
        armor_mesh: write_armor_mesh(world, converter, name, &nodes),
        wreck_mesh,
        wreck_pieces,
        wreck_lods,
        armor_effects: Vec::new(),
        parts,
        uv_animations: uv_animations(world, &nodes),
        track_wheels: track_wheels(world, &nodes, &wheels),
        wheels,
        seats: seat_descs,
        entry_points,
        weapons: Vec::new(),
        sounds: Default::default(),
        remote,
        repairable_wreck: root.get_f32("armor.canbedestroyed") == Some(0.0)
            && root.get_f32("armor.canberepairedwhenwreck").unwrap_or(0.0) != 0.0,
        lods,
        draw_distance,
    };
    let bounds = hull_bounds(world, converter, &nodes, &desc);
    let height = bounds[1][1] - bounds[0][1];
    desc.physics.center_of_mass = match category {
        // Jets balance on their wings' lift, so they fly on hands off.
        VehicleCategory::Air => [0.0, 0.0, balance_wings(&mut desc)],
        // Helicopters are laid out around their origin under the rotor.
        VehicleCategory::Helicopter => [0.0; 3],
        // Boats are heaviest at the keel.
        VehicleCategory::Sea => [0.0, bounds[0][1] + height * 0.15, (bounds[0][2] + bounds[1][2]) * 0.5],
        // Low in the hull, like the heavy engine and chassis; BF2 doesn't say.
        _ => [0.0, bounds[0][1] + height * 0.3, (bounds[0][2] + bounds[1][2]) * 0.5],
    };
    desc.physics.bounds = bounds;
    orient_control_surfaces(&mut desc);
    Some((desc, engine.map(Drivetrain::new)))
}

/// Where a jet's center of mass goes: on the wings' neutral point, so it keeps its attitude
/// hands off, but ahead of the main landing gear so it doesn't sit on its tail. If the
/// neutral point is behind the gear, the wings' lift points move forward with the center of
/// mass (they are only where BF2 applies the forces, tuned for its own balance).
fn balance_wings(desc: &mut VehicleDesc) -> f32 {
    let neutral = lift_center(desc);
    let main_gear = desc.wheels.iter().filter(|w| w.contact).map(|w| w.position[2]).fold(f32::MIN, f32::max);
    if main_gear == f32::MIN || neutral < main_gear - 0.8 {
        return neutral;
    }
    let com = main_gear - 0.8;
    for wing in &mut desc.wings {
        wing.position[2] -= neutral - com;
    }
    com
}

/// Where along the hull the wings' lift from pitching up acts (their neutral point): a jet
/// balanced there keeps its attitude.
fn lift_center(desc: &VehicleDesc) -> f32 {
    let transforms = rest_transforms(&desc.parts);
    let (mut sum, mut weight) = (0.0, 0.0);
    for wing in &desc.wings {
        let normal = transforms[wing.part as usize].1 * Vec3::Y;
        let w = wing.lift * normal.y * normal.y;
        sum += wing.position[2] * w;
        weight += w;
    }
    if weight > 0.0 { sum / weight } else { 0.0 }
}

/// Thrust acceleration of an engine from BF2's `setTorque` × `setDifferential`. Twin-engine
/// jets split the power the single-engine ones have in one engine, so the square root keeps
/// opposing jets (F-18 and J-10, MiG-29 and F-35B) about even.
fn thrust(power: f32) -> f32 {
    THRUST_PER_POWER * 1000.0 * (power.max(0.0) / 1000.0).sqrt()
}

/// Flight and swimming tuning by category (BF2's formulas aren't known; these are fitted by
/// flying, see docs/ARCHITECTURE.md).
fn aero_desc(category: VehicleCategory, drag: f32) -> AeroDesc {
    match category {
        VehicleCategory::Air => AeroDesc {
            drag: drag * DRAG_PER_DRAG,
            stall_angle: 18.0,
            max_load: 90.0,
            angular_damping: [1.5, 1.0, 1.5],
            speed_damping: [0.12, 0.02, 0.0],
            water_drag: [2.0, 2.0, 1.0],
        },
        // Transport helicopters (drag 2) are a little slower than attack helicopters (1).
        VehicleCategory::Helicopter => AeroDesc {
            drag: drag.sqrt() * DRAG_PER_DRAG,
            // Stub wings stall early so flying nose down doesn't press the helicopter down.
            stall_angle: 8.0,
            max_load: 40.0,
            angular_damping: [0.0; 3],
            speed_damping: [0.0; 3],
            water_drag: [2.0, 2.0, 1.0],
        },
        // Boats and amphibious vehicles: a keel against sliding sideways, a hull that damps
        // bobbing, little drag forwards.
        _ => AeroDesc {
            drag: drag * DRAG_PER_DRAG,
            stall_angle: 25.0,
            max_load: 40.0,
            angular_damping: [0.5, 0.5, 0.5],
            speed_damping: [0.0; 3],
            water_drag: [1.5, 2.0, 0.08],
        },
    }
}

/// A helicopter's main and tail rotor, from its `c_ETHelicopter` engines and rotor parts.
fn rotor_desc(world: &World, root: &Template, nodes: &[Node], engines: &[(usize, &Template, String)]) -> Option<RotorDesc> {
    let power = |t: &Template| t.get_f32("settorque").unwrap_or(0.0) * t.get_f32("setdifferential").unwrap_or(0.0);
    let helicopter: Vec<&(usize, &Template, String)> = engines.iter().filter(|(_, _, ty)| ty == "c_ethelicopter").collect();
    let is_tail = |t: &Template| {
        t.get_f32("purerotational").unwrap_or(0.0) != 0.0
            || t.get_str("setinputtoroll").is_some_and(|i| i.eq_ignore_ascii_case("piyaw"))
    };
    let main = helicopter
        .iter()
        .filter(|(_, t, _)| !is_tail(t))
        .max_by(|a, b| power(a.1).total_cmp(&power(b.1)))?;
    let tail = helicopter.iter().filter(|(_, t, _)| is_tail(t)).max_by(|a, b| power(a.1).total_cmp(&power(b.1)));
    let (_, engine, _) = main;
    // The visible main rotor is where the push comes from (the engine hangs under a 20 m
    // rotor-head lever in BF2).
    let hub = nodes
        .iter()
        .find(|n| n.ty == "rotor" && world.template(&n.template).is_some_and(|t| t.get_f32("rotationaxle").unwrap_or(0.0) == 0.0))
        .map_or(nodes[main.0].hull.translation, |n| n.hull.translation);
    // Rotor heads tilt a few degrees with the stick: steeper heads turn faster.
    let head = nodes
        .iter()
        .filter(|n| n.ty == "rotationalbundle")
        .filter_map(|n| world.template(&n.template))
        .find(|t| t.get_str("setinputtopitch").is_some_and(|i| i.eq_ignore_ascii_case("pipitch")));
    let tilt = |i: usize| {
        head.map_or(8.0, |t| {
            let lo = t.get_vec3("setminrotation").map_or(0.0, |v| v[i].abs());
            let hi = t.get_vec3("setmaxrotation").map_or(0.0, |v| v[i].abs());
            lo.max(hi)
        })
        .clamp(4.0, 12.0)
    };
    let tail_power = tail.map_or(1000.0, |(_, t, _)| power(t));
    Some(RotorDesc {
        position: hub.to_array(),
        spin_up: 6.0,
        climb_speed: [12.0, 10.0],
        lift_margin: 0.9,
        horizontal_magnifier: engine.get_f32("horizontalspeedmagnifier").unwrap_or(1.0).clamp(1.0, 3.0),
        horizontal_damping: engine.get_f32("damphorizontalvel").unwrap_or(0.0) * 0.004,
        regulation_angle: root.get_f32("maxvertregangle").unwrap_or(35.0),
        no_regulation_angle: root.get_f32("novertregangle").unwrap_or(55.0),
        turn_rates: [
            tilt(1).to_radians() * 8.0,
            0.5 + tail_power.sqrt() / 120.0,
            tilt(2).to_radians() * 10.0,
        ],
        response: 4.0,
        leveling: 0.3,
        tail_position: tail.map_or([0.0, 0.0, 8.0], |(i, _, _)| nodes[*i].hull.translation.to_array()),
    })
}

/// A jump jet's hover system (the F-35B's lift fan: a `c_ETHelicopter` engine that isn't
/// only for turning), flown like a gentle helicopter while the jet hovers: BF2 holds it level
/// with small pure-rotational engines and damps its drift (`dampHorizontalVel`).
fn vtol_desc(nodes: &[Node], engines: &[(usize, &Template, String)]) -> Option<RotorDesc> {
    let (index, engine, _) = engines
        .iter()
        .find(|(_, t, ty)| ty == "c_ethelicopter" && t.get_f32("purerotational").unwrap_or(0.0) == 0.0)?;
    Some(RotorDesc {
        position: nodes[*index].hull.translation.to_array(),
        spin_up: 2.0,
        climb_speed: [6.0, 6.0],
        lift_margin: 0.5,
        horizontal_magnifier: 1.0,
        horizontal_damping: engine.get_f32("damphorizontalvel").unwrap_or(0.0) * 0.004,
        regulation_angle: 20.0,
        no_regulation_angle: 40.0,
        turn_rates: [0.6, 0.8, 0.8],
        response: 3.0,
        leveling: 0.8,
        tail_position: [0.0, 0.0, 6.0],
    })
}

/// Makes every control surface deflect so that positive input pitches up, rolls right or
/// yaws right: BF2 says which way through the signs of its speeds and accelerations and the
/// surface's placement, which is easier to get right by checking the moment its lift makes.
fn orient_control_surfaces(desc: &mut VehicleDesc) {
    let com = Vec3::from(desc.physics.center_of_mass);
    let transforms = rest_transforms(&desc.parts);
    for wing in &desc.wings {
        let Some(joint) = desc.parts[wing.part as usize].joint.as_mut() else {
            continue;
        };
        let axis = &mut joint.axes[1];
        let wanted = match axis.input {
            Some(JointInput::Pitch) => Vec3::X,
            Some(JointInput::Roll) => Vec3::NEG_Z,
            Some(JointInput::Steer) => Vec3::NEG_Y,
            _ => continue,
        };
        // More lift along the surface's normal: what a positive deflection does.
        let normal = transforms[wing.part as usize].1 * Vec3::Y;
        let moment = (Vec3::from(wing.position) - com).cross(normal);
        let sign = if moment.dot(wanted) >= 0.0 { 1.0 } else { -1.0 };
        axis.speed = axis.speed.abs().max(1.0) * sign;
    }
}

/// Rest position and rotation of every part in hull space.
fn rest_transforms(parts: &[VehiclePart]) -> Vec<(Vec3, Quat)> {
    let mut out: Vec<(Vec3, Quat)> = Vec::with_capacity(parts.len());
    for part in parts {
        let local = (Vec3::from(part.placement.position), Quat::from_array(part.placement.rotation));
        let hull = match part.parent {
            Some(p) => {
                let (pp, pr) = out[p as usize];
                (pp + pr * local.0, pr * local.1)
            }
            None => local,
        };
        out.push(hull);
    }
    out
}

/// BF2's numbers of a land vehicle's `Engine` (gameplay-data.md §3.2).
struct Drivetrain {
    torque: f32,
    differential: f32,
    /// Forward gear ratios: the first `setNumberOfGears` of `setGearRatios`.
    ratios: Vec<f32>,
    shift_up: f32,
    shift_down: f32,
    shift_time: f32,
    idle: f32,
    brake_torque: Option<f32>,
    engine_brake_torque: Option<f32>,
    slide_grip: Option<f32>,
}

impl Drivetrain {
    fn new(t: &Template) -> Self {
        let ratios: Vec<f32> = t
            .get("setgearratios")
            .map(|args| args.iter().filter_map(|a| a.parse().ok()).collect())
            .unwrap_or_default();
        let gears = t.get_f32("setnumberofgears").map_or(ratios.len(), |n| n as usize).min(ratios.len());
        let rpm = |method: &str, default: f32| t.get_f32(method).unwrap_or(default);
        Self {
            torque: t.get_f32("settorque").unwrap_or(0.0),
            differential: t.get_f32("setdifferential").unwrap_or(0.0),
            ratios: ratios[..gears].iter().copied().filter(|r| *r > 0.0).collect(),
            shift_up: t.get_f32("setgearup").unwrap_or(0.85),
            shift_down: t.get_f32("setgeardown").unwrap_or(0.4),
            shift_time: t.get_f32("setgearchangetime").unwrap_or(0.5),
            idle: rpm("newcar2.minrpm", 1000.0) / rpm("newcar2.maxrpm", 4000.0).max(1.0),
            brake_torque: t.get_f32("newcar2.braketorque"),
            engine_brake_torque: t.get_f32("newcar2.enginebraketorque"),
            slide_grip: t.get_f32("newcar2.wheellatmindynamicfriction"),
        }
    }
}

/// Engine and brakes from BF2's drivetrain numbers, fitted to BF2's AI top speeds with one
/// factor each (BF2's own formulas aren't known):
///
/// - wheeled (`c_ETNewCar2`): each gear pulls `setTorque` × `setDifferential` × its ratio /
///   the wheel radius and tops out where the top gear reaches the top speed; reverse uses the
///   first gear. On Karkand's flat road a HMMWV reaches 50 km/h in 4 s and 88 km/h in 10 s,
///   a LAV-25 its 77 km/h in 8.5 s (vehicle2_drive). Brakes and engine braking follow
///   `brakeTorque` and `engineBrakeTorque` (a HMMWV brakes at 0.8 g and coasts down at
///   1 m/s² plus drag).
/// - tracked (`c_ETTank`): `setTorque` × `setDifferential` pushes up to the top speed, so an
///   M1A2 or T-90 needs 7 s to its top speed, the lighter M6 and Type 95 half that.
fn tune_engine(desc: &mut VehicleDesc, drivetrain: Option<&Drivetrain>) {
    let mass = desc.physics.mass;
    let radius = {
        let driven: Vec<f32> = desc.wheels.iter().filter(|w| w.contact).map(|w| w.radius).collect();
        if driven.is_empty() { 0.45 } else { driven.iter().sum::<f32>() / driven.len() as f32 }
    };
    let engine = &mut desc.engine;
    engine.reverse_speed = engine.top_speed * 0.35;
    engine.brake_force = mass * 8.0;
    let power = drivetrain.map_or(0.0, |d| d.torque * d.differential);
    match (desc.drive, drivetrain) {
        (DriveKind::Tracked, _) if power > 0.0 => engine.drive_force = TRACK_FORCE_PER_POWER * power,
        (DriveKind::Wheeled, Some(d)) if power > 0.0 && !d.ratios.is_empty() => {
            let top_ratio = d.ratios[d.ratios.len() - 1];
            let gears: Vec<GearDesc> = d
                .ratios
                .iter()
                .map(|ratio| GearDesc {
                    top_speed: engine.top_speed * top_ratio / ratio,
                    force: FORCE_PER_TORQUE * power * ratio / radius,
                })
                .collect();
            engine.drive_force = gears[0].force;
            engine.reverse_speed = gears[0].top_speed;
            if let Some(brake) = d.brake_torque {
                engine.brake_force = BRAKE_PER_TORQUE * brake / radius;
            }
            if let Some(slide) = d.slide_grip {
                engine.slide_grip = slide.clamp(0.3, 1.0);
            }
            engine.gearbox = Some(GearboxDesc {
                reverse: gears[0],
                gears,
                shift_up: d.shift_up,
                shift_down: d.shift_down,
                shift_time: d.shift_time,
                idle: d.idle.clamp(0.0, 0.9),
                engine_brake: ENGINE_BRAKE_PER_TORQUE * d.engine_brake_torque.unwrap_or(0.0) / radius,
            });
        }
        (drive, _) => {
            let seconds_to_top = if drive == DriveKind::Tracked { 7.0 } else { 4.5 };
            engine.drive_force = mass * engine.top_speed / seconds_to_top;
        }
    }
}

/// The clip a seat's occupant plays: the sitting (or still) animation of its BF2 animation
/// system, if the soldier bodies have it (they have BF2's common seat animations), else a
/// common one for the role.
fn seat_pose(converter: &MeshConverter, seat: &Template, category: VehicleCategory, index: usize) -> Option<String> {
    let common = crate::soldiers::SEAT_ANIMATIONS;
    let from_system = seat
        .get_str("seatanimationsystem")
        .and_then(|path| converter.vfs.read(&path.to_ascii_lowercase().replace('\\', "/")).ok())
        .and_then(|data| {
            let text = String::from_utf8_lossy(&data).to_ascii_lowercase();
            let clips: Vec<String> = text
                .lines()
                .filter_map(|line| line.trim().strip_prefix("animationsystem.createanimation "))
                .map(|path| path.trim().replace('\\', "/"))
                .filter(|path| path.starts_with(common) && !path.contains("_die"))
                .collect();
            let rank = |path: &String| ["static", "sit", "still"].iter().position(|w| path.contains(w)).unwrap_or(9);
            clips.into_iter().min_by_key(rank)
        })
        .and_then(|path| Some(path.rsplit('/').next()?.trim_end_matches(".baf").to_string()));
    from_system.or_else(|| {
        Some(
            match (category, index) {
                (VehicleCategory::Air | VehicleCategory::Helicopter, 0) => "3p_aircraftpilot_a_static",
                (VehicleCategory::Stationary, _) => "3p_gunturret_a_sit",
                (_, 0) => "3p_driver_a_static",
                _ => "3p_passenger_a",
            }
            .to_string(),
        )
    })
}

/// Where a seat pose (a clip of the soldier bodies) has the root bone, from the model's
/// origin, in its first frame.
fn pose_root(converter: &MeshConverter, pose: &str) -> Option<Vec3> {
    let data = converter.vfs.read(&format!("{}{pose}.baf", crate::soldiers::SEAT_ANIMATIONS)).ok()?;
    // Seat poses are body/vehicle animations; the rare version 3 format only turns up in a
    // couple of AIX weapon clips, never here, so no skeleton is needed to resolve names.
    let animation = bf2_formats::anim::Animation::parse(&data, None).ok()?;
    let t = animation.tracks.iter().find(|t| t.bone == 0)?.translations.first()?;
    Some(Vec3::new(t[0], t[1], -t[2]))
}

/// The seat's first-person camera: the first one in its part of the tree, skipping the
/// alternative (ducked) cameras.
fn seat_camera(world: &World, nodes: &[Node], cameras: &[(u32, String, Placement)], seat: u32) -> Option<SeatCamera> {
    let candidates: Vec<&(u32, String, Placement)> = cameras
        .iter()
        .filter(|(part, _, _)| nodes[*part as usize].seat == Some(seat))
        .collect();
    let (part, name, placement) = candidates
        .iter()
        .find(|(_, name, _)| world.template(name).is_some_and(|t| t.get_f32("cameraid").unwrap_or(0.0) == 0.0))
        .or_else(|| candidates.first())?;
    let t = world.template(name)?;
    let offset = t.get_vec3("chaseoffset").unwrap_or([0.0, 1.0, 7.0]);
    Some(SeatCamera {
        attachment: Attachment {
            part: *part,
            placement: *placement,
        },
        fov: t.get_f32("worldfov").unwrap_or(1.1),
        chase_distance: t.get_f32("chasedistance").unwrap_or(12.0),
        chase_offset: offset,
    })
}

/// Box around the hull's vehicle collision (the parts that don't move relative to it, plus
/// turrets).
fn hull_bounds(world: &World, converter: &MeshConverter, nodes: &[Node], desc: &VehicleDesc) -> [[f32; 3]; 2] {
    let mut min = Vec3::splat(f32::MAX);
    let mut max = Vec3::splat(f32::MIN);
    let root_collision = world
        .template(&nodes[0].template)
        .and_then(|t| t.collision_mesh.as_deref())
        .and_then(|c| world.collision_meshes.get(&c.to_ascii_lowercase()));
    let mesh = root_collision
        .and_then(|path| converter.vfs.read(path).ok())
        .and_then(|data| CollisionMesh::parse(&data).ok());
    if let Some(mesh) = mesh {
        for (index, part) in desc.parts.iter().enumerate() {
            // Attachments with their own collision mesh (missiles on the wings) are small.
            if !desc.is_hull_part(index) || part.collision.is_none() || part.collision != desc.parts[0].collision {
                continue;
            }
            let Some(cp) = mesh.parts.get(part.collision_part as usize) else {
                continue;
            };
            // Geom 1 is the third-person (outside) collision.
            let geom = cp
                .geoms
                .get(1)
                .filter(|g| !g.cols.is_empty())
                .or_else(|| cp.geoms.iter().find(|g| !g.cols.is_empty()));
            let Some(geom) = geom else { continue };
            let col = geom
                .cols
                .iter()
                .find(|c| c.col_type == ColType::Vehicle)
                .or_else(|| geom.cols.first());
            for v in col.map(|c| c.vertices.as_slice()).unwrap_or_default() {
                let p = nodes[index].hull.transform_point3(Vec3::from_array(coords::position(*v)));
                min = min.min(p);
                max = max.max(p);
            }
        }
    }
    if min.x > max.x {
        return VehiclePhysics::default().bounds;
    }
    [min.to_array(), max.to_array()]
}

/// Writes the faces direct hits land on (`VehicleDesc::armor_mesh`): each part's outside
/// projectile collision, its faces grouped by the material `mapMaterial` gives their index
/// (the part's own template's, else the collision mesh owner's), as
/// `vehicles/<name>.armor.glb`. Penetrable materials (canvas, thin sheet metal, windows the
/// shots go through) are left out. Returns the path relative to the output root.
fn write_armor_mesh(world: &World, converter: &MeshConverter, name: &str, nodes: &[Node]) -> Option<String> {
    let map_of = |template: &str| -> HashMap<u16, Option<u32>> {
        world
            .template(template)
            .map(|t| {
                t.get_all("mapmaterial")
                    .filter_map(|args| {
                        let name = args.get(1)?.to_ascii_lowercase();
                        let solid = !name.contains("penetrable") && !name.contains("pentrable");
                        Some((args.first()?.parse().ok()?, args.get(2)?.parse().ok().filter(|_| solid)))
                    })
                    .collect()
            })
            .unwrap_or_default()
    };
    let mut meshes: HashMap<String, Option<CollisionMesh>> = HashMap::new();
    let mut doc = crate::glb::Document::default();
    let mut material_slots: HashMap<u32, usize> = HashMap::new();
    for (index, node) in nodes.iter().enumerate() {
        let Some((path, owner)) = &node.source_collision else {
            continue;
        };
        let mesh = meshes
            .entry(path.clone())
            .or_insert_with(|| converter.vfs.read(path).ok().and_then(|data| CollisionMesh::parse(&data).ok()));
        let Some(mesh) = mesh else { continue };
        let collision_part = world
            .template(&node.template)
            .and_then(|t| t.get_f32("collisionpart"))
            .unwrap_or(0.0) as usize;
        let Some(part) = mesh.parts.get(collision_part) else { continue };
        // Geom 1 is the outside of vehicles, geom 0 the only one of the rest.
        let geom = part
            .geoms
            .get(1)
            .filter(|g| !g.cols.is_empty())
            .or_else(|| part.geoms.iter().find(|g| !g.cols.is_empty()));
        let Some(col) = geom.and_then(|g| g.cols.iter().find(|c| c.col_type == ColType::Projectile)) else {
            continue;
        };
        let mut map = map_of(&node.template);
        if map.is_empty() {
            map = map_of(owner);
        }
        let mut by_material: HashMap<u32, Vec<[u16; 3]>> = HashMap::new();
        for face in &col.faces {
            if let Some(Some(material)) = map.get(&face[3]) {
                by_material.entry(*material).or_default().push([face[0], face[1], face[2]]);
            }
        }
        if by_material.is_empty() {
            continue;
        }
        let positions: Vec<[f32; 3]> = col.vertices.iter().map(|&v| coords::position(v)).collect();
        let mut materials: Vec<(u32, Vec<[u16; 3]>)> = by_material.into_iter().collect();
        materials.sort_by_key(|(m, _)| *m);
        let primitives = materials
            .into_iter()
            .map(|(material, faces)| {
                let slot = *material_slots.entry(material).or_insert_with(|| {
                    doc.materials.push(crate::glb::Material {
                        name: material.to_string(),
                        base_color: None,
                        base_color_uv: 0,
                        normal: None,
                        alpha: crate::glb::AlphaMode::Opaque,
                        double_sided: true,
                        extras: serde_json::Value::Null,
                    });
                    doc.materials.len() - 1
                });
                let mut remap: HashMap<u16, u32> = HashMap::new();
                let mut primitive = crate::glb::Primitive {
                    material: Some(slot),
                    ..Default::default()
                };
                for &v in faces.iter().flatten() {
                    let index = *remap.entry(v).or_insert_with(|| {
                        primitive.positions.push(positions.get(v as usize).copied().unwrap_or_default());
                        primitive.positions.len() as u32 - 1
                    });
                    primitive.indices.push(index);
                }
                primitive
            })
            .collect();
        doc.meshes.push(crate::glb::Mesh {
            name: format!("part{index}"),
            primitives,
        });
        doc.nodes.push(crate::glb::Node {
            name: format!("part{index}"),
            mesh: Some(doc.meshes.len() - 1),
            ..Default::default()
        });
        doc.scene.push(doc.nodes.len() - 1);
    }
    if doc.meshes.is_empty() {
        return None;
    }
    let rel = format!("vehicles/{}.armor.glb", name.to_ascii_lowercase());
    doc.write(&converter.out.join(&rel))
        .map_err(|e| log::warn!("{rel}: {e:#}"))
        .ok()?;
    Some(rel)
}

/// Two numbers written `a/b`.
fn pair(t: &Template, method: &str) -> [f32; 2] {
    let mut values = t.get_str(method).unwrap_or("0/0").split('/').map(|v| v.trim().parse().unwrap_or(0.0));
    [values.next().unwrap_or(0.0), values.next().unwrap_or(0.0)]
}

/// The tracks' UV animations, from the wheels that set them up (one per matrix index).
fn uv_animations(world: &World, nodes: &[Node]) -> Vec<UvAnimationDesc> {
    let mut animations: Vec<UvAnimationDesc> = Vec::new();
    for node in nodes {
        let Some(t) = world.template(&node.template) else { continue };
        let side = node.hull.translation.x;
        let mut add = |index: Option<f32>, motion: UvMotion| {
            if let Some(index) = index.filter(|i| *i > 0.0).map(|i| i as u8)
                && !animations.iter().any(|a| a.index == index)
            {
                animations.push(UvAnimationDesc { index, side, motion });
            }
        };
        if t.get_f32("animateduvtranslation").unwrap_or(0.0) != 0.0 {
            let motion = UvMotion::Scroll {
                size: pair(t, "animateduvtranslationsize"),
                wrap: pair(t, "animateduvtranslationmax"),
            };
            add(t.get_f32("animateduvtranslationindex"), motion);
        }
        if t.get_f32("animateduvrotation").unwrap_or(0.0) != 0.0 {
            let motion = UvMotion::Spin {
                radius: t.get_f32("animateduvrotationradius").unwrap_or(0.35),
                scale: pair(t, "animateduvrotationscale").map(|v| if v == 0.0 { 1.0 } else { v }),
            };
            add(t.get_f32("animateduvrotationindex"), motion);
        }
    }
    animations.sort_by_key(|a| a.index);
    animations
}

/// Drive sprockets (`rotateAsAnimatedUV`) turn like the wheel they follow.
fn track_wheels(world: &World, nodes: &[Node], wheels: &[WheelDesc]) -> Vec<TrackWheelDesc> {
    nodes
        .iter()
        .enumerate()
        .filter_map(|(i, node)| {
            let t = world.template(&node.template)?;
            (t.get_f32("rotateasanimateduv").unwrap_or(0.0) != 0.0).then_some(())?;
            let followed = t
                .get_str("rotateasanimateduvobject")
                .and_then(|name| world.template(&name.to_ascii_lowercase()))
                .and_then(|t| t.get_f32("animateduvrotationradius"));
            let radius = followed
                .or_else(|| wheels.iter().find(|w| w.part as usize == i).map(|w| w.radius))
                .unwrap_or(0.35);
            Some(TrackWheelDesc {
                part: i as u32,
                side: node.hull.translation.x,
                radius,
            })
        })
        .collect()
}

/// Measures meshes (wheels, wreck pieces).
#[derive(Default)]
struct MeshCache(HashMap<String, Option<VisMesh>>);

impl MeshCache {
    fn mesh(&mut self, converter: &MeshConverter, path: &str) -> Option<&VisMesh> {
        self.0
            .entry(path.to_string())
            .or_insert_with(|| {
                let data = converter.vfs.read(path).ok()?;
                VisMesh::parse(&data, MeshKind::from_path(path)?).ok()
            })
            .as_ref()
    }

    fn geom_count(&mut self, converter: &MeshConverter, path: &str) -> usize {
        self.mesh(converter, path).map_or(0, |m| m.geoms.len())
    }

    /// Number of parts of a bundled mesh geom.
    fn part_count(&mut self, converter: &MeshConverter, path: &str, geom: usize) -> Option<u32> {
        Some(self.mesh(converter, path)?.geoms.get(geom)?.lods.first()?.part_count)
    }

    /// Half the largest extent of a bundled mesh part across its rolling plane (Y/Z).
    fn part_radius(&mut self, converter: &MeshConverter, path: &str, part: u32) -> Option<f32> {
        let mesh = self.mesh(converter, path)?;
        let positions = mesh.attribute::<3>(Usage::Position, 0)?;
        let blend = mesh.blend_indices()?;
        let geom = mesh.geoms.get(1).or_else(|| mesh.geoms.first())?;
        let lod = geom.lods.first()?;
        let mut min = Vec3::splat(f32::MAX);
        let mut max = Vec3::splat(f32::MIN);
        for material in &lod.materials {
            for tri in mesh.material_triangles(material) {
                for v in tri {
                    let v = v as usize;
                    if blend.get(v).is_some_and(|b| b[0] as u32 == part)
                        && let Some(p) = positions.get(v)
                    {
                        min = min.min(Vec3::from_array(*p));
                        max = max.max(Vec3::from_array(*p));
                    }
                }
            }
        }
        (min.x <= max.x).then(|| ((max.y - min.y).max(max.z - min.z) * 0.5).max(0.1))
    }
}

/// `aiTemplatePlugIn.maxSpeed` of the vehicle's `Mobile` AI plugin (`ai/Objects.ai` next to
/// its `.con`): the speed bots drive at, a good hint for the real top speed.
fn ai_max_speed(interp: &mut Interpreter, name: &str) -> Option<f32> {
    let source = interp.world.template(name)?.source.clone();
    let file = source.split(':').next().unwrap_or_default();
    let dir = file.rsplit_once('/').map_or("", |(d, _)| d);
    let start = interp.world.commands.len();
    interp.run(&format!("{dir}/ai/objects.ai"), &[]);
    let mut in_mobile = false;
    for command in &interp.world.commands[start..] {
        match command.name.as_str() {
            "aitemplateplugin.create" => {
                in_mobile = command.args.first().is_some_and(|a| a.eq_ignore_ascii_case("mobile"));
            }
            "aitemplateplugin.maxspeed" if in_mobile => return command.args.first()?.parse().ok(),
            _ => {}
        }
    }
    None
}

/// The guns: every `GenericFireArm` with a projectile, except smoke launchers.
fn weapon_descs(
    interp: &mut Interpreter,
    converter: &MeshConverter,
    localization: &Localization,
    huds: &crate::vehicle_hud::VehicleHuds,
    desc: &VehicleDesc,
    out: &Path,
) -> Vec<VehicleWeaponDesc> {
    let mut weapons = Vec::new();
    for (index, part) in desc.parts.iter().enumerate() {
        let Some(t) = interp.world.template(&part.name).cloned() else {
            continue;
        };
        if !t.ty.eq_ignore_ascii_case("GenericFireArm") || t.get_str("projectiletemplate").is_none() {
            continue;
        }
        let fire_input = t.get_str("fire.fireinput").unwrap_or("PIFire").to_ascii_lowercase();
        let countermeasure = fire_input == "piflarefire";
        // The muzzle: the child furthest forward (muzzle flash effects sit there).
        let muzzle = t
            .children
            .iter()
            .filter_map(|c| c.position)
            .max_by(|a, b| a[2].total_cmp(&b[2]))
            .filter(|p| p[2] > 0.0)
            .map(coords::position)
            .unwrap_or([0.0; 3]);
        let sounds = crate::sounds::SoundConverter::new(converter.vfs, out);
        let mut weapon = weapons::weapon_desc(interp, converter, &sounds, &t, out);
        weapon.display_name = localization.resolve(&weapon.display_name);
        let countermeasure = countermeasure.then(|| countermeasure_desc(&t, &mut weapon));
        // Horns are "guns" firing harmless projectiles for their sound; ammo belts that only
        // animate don't fire at all (velocity 0, unlike bombs, which drop).
        let harmless = weapon.projectile.damage <= 0.0 && weapon.projectile.explosion_damage <= 0.0;
        let inert = weapon.projectile.velocity <= 0.0 && !weapon.projectile.explodes();
        if countermeasure.is_none() && (harmless || inert) {
            continue;
        }
        // Vehicle guns without a deviation setting are dead accurate (the handheld default
        // doesn't apply).
        if t.get("deviation.mindev").is_none() {
            weapon.deviation.min = 0.0;
        }
        weapons.push(VehicleWeaponDesc {
            part: index as u32,
            muzzle,
            seat: desc.seat_of(index),
            alt_fire: fire_input == "pialtfire",
            weapon,
            sight: t
                .get_f32("weaponhud.guiindex")
                .filter(|_| countermeasure.is_none())
                .map_or_else(Vec::new, |index| huds.sight(index as u32)),
            countermeasure,
        });
    }
    weapons
}

/// A `PIFlareFire` launcher: its burst and barrels, and its projectile as a countermeasure.
/// Smoke grenades (a smoke effect riding on the projectile) lay their smoke where they land,
/// like hand-thrown ones; decoy flares fall and bounce, burning, as replicated objects. The
/// launcher refills `ammo.minimumTimeUntilReload` after it was emptied.
fn countermeasure_desc(t: &Template, weapon: &mut game_data::WeaponDesc) -> CountermeasureDesc {
    let projectile = &mut weapon.projectile;
    let smoke_trail = projectile.trail_effect.take_if(|e| e.contains("smoke"));
    if let Some(effect) = smoke_trail {
        projectile.smoke = Some(game_data::SmokeDesc {
            radius: weapons::SMOKE_RADIUS,
            duration: projectile.time_to_live.max(10.0),
            gas_damage: 0.0,
        });
        projectile.detonation_effect = Some(effect);
    } else {
        projectile.impact = game_data::Impact::Bounce;
    }
    if let Some(refill) = t.get_f32("ammo.minimumtimeuntilreload") {
        weapon.reload_time = weapon.reload_time.max(refill);
    }
    CountermeasureDesc {
        burst: t.get_f32("fire.burstsize").unwrap_or(1.0).max(1.0) as u32,
        barrels: t
            .children
            .iter()
            .filter(|c| c.position.is_some())
            .map(|c| Placement {
                position: coords::position(c.position.unwrap_or_default()),
                rotation: coords::rotation_ypr(c.rotation.unwrap_or_default()).to_array(),
                ..Default::default()
            })
            .collect(),
        decoy: projectile.smoke.is_none(),
    }
}
