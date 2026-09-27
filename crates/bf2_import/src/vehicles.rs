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
    con::{Interpreter, Template, World, parse_vec3},
    localization::Localization,
    mesh::{MeshKind, Usage, VisMesh},
};
use game_data::{
    AeroDesc, AfterburnerDesc, Attachment, DriveKind, EngineDesc, EntryPointDesc, FloaterDesc, JointAxis,
    JointDesc, JointInput, LandingGearDesc, Placement, RotorDesc, SeatCamera, SeatDesc, ThrusterDesc,
    VehicleCategory, VehicleDesc, VehiclePart, VehiclePhysics, VehicleWeaponDesc, WheelDesc, WingDesc,
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
];

/// BF2's lift, thrust and drag numbers in our units (fitted by flying, see
/// docs/ARCHITECTURE.md "Aircraft"): wing lift per `setWingLift`, flap lift per
/// `setFlapLift`, thrust acceleration per `setTorque` × `setDifferential`, drag per `drag`.
const LIFT_PER_WING_LIFT: f32 = 0.01;
const LIFT_PER_FLAP_LIFT: f32 = 0.003;
const THRUST_PER_POWER: f32 = 0.004;
const DRAG_PER_DRAG: f32 = 0.003;
/// Landing flaps' lift (`setFlapLift 3`) would lift a jet off at a walking pace.
const LANDING_FLAP_SHARE: f32 = 0.3;
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
    for name in names {
        interp.ensure_template(name);
        load_tree(interp, name, 0);
        let Some(mut desc) = build(interp, converter, name) else {
            continue;
        };
        desc.display_name = localization.resolve(&desc.display_name);
        desc.engine.top_speed = ai_max_speed(interp, name)
            .map(|s| s * 1.1)
            .unwrap_or(desc.engine.top_speed);
        if matches!(desc.drive, DriveKind::Wheeled | DriveKind::Tracked) {
            tune_engine(&mut desc);
        }
        desc.weapons = weapon_descs(interp, converter, localization, &desc, out);
        desc.sounds = crate::sounds::SoundConverter::new(converter.vfs, out).vehicle(&interp.world, name);
        game_data::write_ron(out.join("vehicles").join(format!("{name}.ron")), &desc)?;
        written.push(name.clone());
    }
    Ok(written)
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
}

#[derive(Clone, Default)]
struct Inherited {
    mesh: Option<(String, String)>,
    collision: Option<String>,
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

        // Meshes: vehicles and guns keep the third-person model in geom 1. Skinned parts
        // (swaying antennas) need their skeleton, which isn't imported for vehicles yet.
        let own_mesh = template
            .geometry
            .as_deref()
            .and_then(|g| self.world.geometry(g))
            .filter(|g| !g.ty.eq_ignore_ascii_case("SkinnedMesh"))
            .and_then(|g| g.mesh_path())
            .and_then(|path| {
                let converted = self
                    .converter
                    .convert_mesh_geom(&path, 1, "_3p")
                    .or_else(|_| self.converter.convert_mesh(&path))
                    .map_err(|e| log::debug!("vehicle mesh {path}: {e:#}"))
                    .ok()?;
                Some((path, converted))
            });
        let own_collision = template
            .collision_mesh
            .as_deref()
            .and_then(|c| self.world.collision_meshes.get(&c.to_ascii_lowercase()))
            .and_then(|path| {
                self.converter
                    .convert_collision(path)
                    .map_err(|e| log::debug!("vehicle collision {path}: {e:#}"))
                    .ok()
            });
        let geometry_part = template.get_f32("geometrypart").map(|p| p as u32);
        let collision_part = template.get_f32("collisionpart").map(|p| p as u32);
        let mesh = own_mesh
            .clone()
            .or_else(|| geometry_part.and(inherited.mesh.clone()));
        let collision = own_collision
            .clone()
            .or_else(|| collision_part.and(inherited.collision.clone()));

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
            mesh: mesh.as_ref().map(|(_, glb)| glb.clone()),
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
            source_mesh: mesh.map(|(path, _)| (path, geometry_part.unwrap_or(0))),
        });

        let inherited = Inherited {
            mesh: own_mesh.or(inherited.mesh),
            collision: own_collision.or(inherited.collision),
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

fn build(interp: &Interpreter, converter: &MeshConverter, name: &str) -> Option<VehicleDesc> {
    let world = &interp.world;
    let root = world.template(name)?;
    if !root.ty.eq_ignore_ascii_case("PlayerControlObject") {
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
    };
    builder.walk(name, None, Affine3A::IDENTITY, Affine3A::IDENTITY, Inherited::default(), None, 0);
    let Builder {
        parts,
        nodes,
        seats,
        cameras,
        entry_points,
        ..
    } = builder;

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
        _ => {
            log::debug!("{name}: nothing moves it");
            return None;
        }
    };
    // Commander assets and artillery have no way in.
    if category == VehicleCategory::Stationary && entry_points.is_empty() {
        log::debug!("{name}: stationary without an entry point");
        return None;
    }
    let drive = match (land_engine, category) {
        (Some((_, _, ty)), _) if ty == "c_ettank" => DriveKind::Tracked,
        (Some(_), _) => DriveKind::Wheeled,
        (None, VehicleCategory::Air | VehicleCategory::Helicopter) => DriveKind::Rolling,
        _ => DriveKind::None,
    };

    let mut meshes = MeshCache::default();
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
            })
        })
        .collect();

    let seat_descs: Vec<SeatDesc> = seats
        .iter()
        .enumerate()
        .filter_map(|(seat, &part)| {
            let t = world.template(&nodes[part as usize].template)?;
            let soldier = t.get("seatinformation").and_then(|args| {
                let target = args.first()?.to_ascii_lowercase();
                let part = nodes.iter().position(|n| n.template == target)? as u32;
                let position = coords::position(args.get(1).and_then(|p| parse_vec3(p))?);
                let rotation = coords::rotation_ypr(args.get(2).and_then(|r| parse_vec3(r)).unwrap_or([0.0; 3]));
                Some(Attachment {
                    part,
                    placement: Placement {
                        position,
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
            Some(WingDesc {
                part: i as u32,
                position: (Vec3::from(n.hull.translation) + offset).to_array(),
                lift: t.get_f32("setwinglift").unwrap_or(0.0) * LIFT_PER_WING_LIFT * share,
                flap_lift: t.get_f32("setflaplift").unwrap_or(0.0) * LIFT_PER_FLAP_LIFT * share,
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
                max_speed: t.get_f32("nopropellereffectatspeed").unwrap_or(150.0),
                reverse: if max > 0.0 { (-min / max).clamp(0.0, 1.0) } else { 0.0 },
                water: ty == "c_etship",
            }
        })
        .collect();
    let rotor = (category == VehicleCategory::Helicopter)
        .then(|| rotor_desc(world, root, &nodes, &engines))
        .flatten();
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

    // Direct hits land on per-face collision materials in BF2 (front, sides, rear, tracks);
    // until those are imported, one typical hull material per class.
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

    let modifier = |method: &str| root.get_vec3(method).map_or([1.0; 3], |v| v.map(f32::abs));
    let mut desc = VehicleDesc {
        name: name.to_ascii_lowercase(),
        display_name: root
            .get_str("vehiclehud.hudname")
            .map(|s| s.trim_matches('"').to_string())
            .unwrap_or_else(|| name.to_string()),
        category,
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
        wreck_mesh,
        wreck_pieces,
        parts,
        wheels,
        seats: seat_descs,
        entry_points,
        weapons: Vec::new(),
        sounds: Default::default(),
    };
    let bounds = hull_bounds(world, converter, &nodes, &desc);
    let height = bounds[1][1] - bounds[0][1];
    desc.physics.center_of_mass = match category {
        // Jets balance on their wings' lift, so they fly on hands off.
        VehicleCategory::Air => [0.0, 0.0, balance_wings(&mut desc)],
        // Helicopters are laid out around their origin under the rotor.
        VehicleCategory::Helicopter => [0.0; 3],
        // Low in the hull, like the heavy engine and chassis; BF2 doesn't say.
        _ => [0.0, bounds[0][1] + height * 0.3, (bounds[0][2] + bounds[1][2]) * 0.5],
    };
    desc.physics.bounds = bounds;
    orient_control_surfaces(&mut desc);
    Some(desc)
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
            water_drag: [2.5, 2.0, 0.12],
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
        leveling: 0.6,
        tail_position: tail.map_or([0.0, 0.0, 8.0], |(i, _, _)| nodes[*i].hull.translation.to_array()),
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

/// Forces from the top speed and mass: full speed in five (wheels) to seven (tracks)
/// seconds, braking at a little under 1 g. BF2's engine formulas aren't known
/// (gameplay-data.md §3.2).
fn tune_engine(desc: &mut VehicleDesc) {
    let mass = desc.physics.mass;
    let engine = &mut desc.engine;
    engine.reverse_speed = engine.top_speed * 0.35;
    let seconds_to_top = match desc.drive {
        DriveKind::Tracked => 7.0,
        _ => 4.5,
    };
    engine.drive_force = mass * engine.top_speed / seconds_to_top;
    engine.brake_force = mass * 8.0;
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
        // Smoke launchers, and ammo belts that only animate (velocity 0), aren't guns.
        let fire_input = t.get_str("fire.fireinput").unwrap_or("PIFire").to_ascii_lowercase();
        if fire_input == "piflarefire" || t.get_f32("velocity") == Some(0.0) {
            continue;
        }
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
        // Horns are "guns" firing harmless projectiles for their sound.
        if weapon.projectile.damage <= 0.0 && weapon.projectile.explosion_damage <= 0.0 {
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
        });
    }
    weapons
}
