//! Land vehicles: `vehicles/<name>.ron` plus their meshes.
//!
//! A BF2 vehicle is a `PlayerControlObject` template tree: the root is the rigid body and the
//! driver's seat, nested `PlayerControlObject`s are further seats, `RotationalBundle`s are
//! joints turned by an input (turrets, barrels, steering knuckles), `Spring`s are wheels
//! under an `Engine`, `Camera`s and `EntryPoint`s mark views and doors, `GenericFireArm`s are
//! guns. The `.con` builds the tree, the `.tweak` sets the numbers. See
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
    Attachment, DriveKind, EngineDesc, EntryPointDesc, JointAxis, JointDesc, JointInput, Placement,
    SeatCamera, SeatDesc, VehicleDesc, VehiclePart, VehiclePhysics, VehicleWeaponDesc, WheelDesc,
};
use glam::{Affine3A, Vec3};

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
];

/// Imports the given vehicle templates (those that aren't land vehicles are skipped).
/// Returns the names written.
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
        tune_engine(&mut desc);
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
        let joint = (ty == "rotationalbundle")
            .then(|| joint(template, seat.unwrap_or(0)))
            .flatten();
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

    // Land vehicles only (for now): a car or tank engine somewhere in the tree.
    let engine = nodes.iter().filter(|n| n.ty == "engine").find_map(|n| {
        let t = world.template(&n.template)?;
        let ty = t.get_str("setenginetype")?.to_ascii_lowercase();
        matches!(ty.as_str(), "c_etnewcar2" | "c_etnewcar" | "c_etcar" | "c_ettank").then_some((t, ty))
    });
    let Some((engine, engine_type)) = engine else {
        log::debug!("{name}: not a land vehicle");
        return None;
    };
    let drive = if engine_type == "c_ettank" {
        DriveKind::Tracked
    } else {
        DriveKind::Wheeled
    };

    let mut meshes = MeshCache::default();
    let wheels: Vec<WheelDesc> = nodes
        .iter()
        .enumerate()
        .filter(|(_, n)| n.ty == "spring")
        .filter_map(|(i, n)| {
            let t = world.template(&n.template)?;
            let strength = t.get_f32("setstrength").unwrap_or(0.0);
            let radius = n
                .source_mesh
                .as_ref()
                .and_then(|(path, part)| meshes.part_radius(converter, path, *part))
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

    let engine_desc = EngineDesc {
        top_speed: if drive == DriveKind::Tracked { 17.0 } else { 25.0 },
        turn_rate: engine.get_f32("trackturnspeed").unwrap_or(0.8),
        grip: match drive {
            DriveKind::Tracked => [1.2, 1.4],
            DriveKind::Wheeled => [
                (engine.get_f32("newcar2.wheellongdrivefrictionmod").unwrap_or(1.4) * 0.8).clamp(0.6, 1.6),
                (engine.get_f32("newcar2.wheellatfrictionmod").unwrap_or(2.0) * 0.5).clamp(0.6, 1.8),
            ],
        },
        ..Default::default()
    };

    // Direct hits land on per-face collision materials in BF2 (front, sides, rear, tracks);
    // until those are imported, one typical hull material per class.
    let armor_material = match drive {
        DriveKind::Tracked => 29,
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

    let mut desc = VehicleDesc {
        name: name.to_ascii_lowercase(),
        display_name: root
            .get_str("vehiclehud.hudname")
            .map(|s| s.trim_matches('"').to_string())
            .unwrap_or_else(|| name.to_string()),
        drive,
        physics: VehiclePhysics {
            mass: root.get_f32("mass").unwrap_or(1000.0),
            gravity: root.get_f32("gravitymodifier").unwrap_or(1.0),
            drag: root.get_f32("drag").unwrap_or(1.0),
            ..Default::default()
        },
        engine: engine_desc,
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
    // Low in the hull, like the heavy engine and chassis; BF2 doesn't say.
    desc.physics.center_of_mass = [0.0, bounds[0][1] + height * 0.3, (bounds[0][2] + bounds[1][2]) * 0.5];
    desc.physics.bounds = bounds;
    Some(desc)
}

/// Forces from the top speed and mass: full speed in five (wheels) to seven (tracks)
/// seconds, braking at a little under 1 g. BF2's engine formulas aren't known
/// (gameplay-data.md §3.2).
fn tune_engine(desc: &mut VehicleDesc) {
    let mass = desc.physics.mass;
    let engine = &mut desc.engine;
    engine.reverse_speed = engine.top_speed * 0.35;
    let seconds_to_top = match desc.drive {
        DriveKind::Wheeled => 4.5,
        DriveKind::Tracked => 7.0,
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
            if !desc.is_hull_part(index) || part.collision.is_none() {
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
