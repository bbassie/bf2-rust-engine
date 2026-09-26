//! Importing a whole level.

use std::{
    collections::{HashMap, HashSet},
    path::Path,
    sync::Mutex,
};

use anyhow::{Context, Result};
use bf2_formats::{
    Bf2Install, LevelInfo, Side,
    con::{Instance, Interpreter, Template, World, parse_vec3},
};
use game_data::{
    ControlPointDesc, EnvironmentDesc, GameModeDesc, LevelDesc, ObjectDesc, ObjectPart, Placement,
    SpawnPointDesc, StaticInstance, VehicleSpawnerDesc,
};
use glam::{Affine3A, Vec3};
use rayon::prelude::*;

use crate::{coords, meshes::MeshConverter, terrain};

pub struct LevelReport {
    pub statics: usize,
    pub templates: usize,
    pub meshes: usize,
    pub failed_meshes: Vec<String>,
    pub missing_templates: Vec<String>,
    pub game_modes: Vec<String>,
}

pub fn import_level(install: &Bf2Install, level: &LevelInfo, out: &Path) -> Result<LevelReport> {
    let vfs = install.level_vfs(level, Side::Both)?;
    let base = format!("levels/{}", level.name);
    let mut interp = Interpreter::new(&vfs);

    // The same order the game loads a level in.
    interp.run(&format!("{base}/init.con"), &[]);
    interp.run(&format!("{base}/staticobjects.con"), &[]);
    if vfs.exists(&format!("{base}/triggerables.con")) {
        interp.run(&format!("{base}/triggerables.con"), &[]);
    }
    let static_range = 0..interp.world.instances.len();

    let desc_text = vfs
        .read_text(&format!("{base}/info/{}.desc", level.name))
        .unwrap_or_default();
    let display_name = xml_text(&desc_text, "name").unwrap_or_else(|| level.name.replace('_', " "));
    let mut layouts = Vec::new();
    for (mode, size) in parse_modes(&desc_text) {
        let path = format!("{base}/gamemodes/{mode}/{size}/gameplayobjects.con");
        if !vfs.exists(&path) {
            continue;
        }
        let start = interp.world.instances.len();
        interp.run(&path, &["host".to_string()]);
        layouts.push((mode, size, start..interp.world.instances.len()));
    }

    let name = level.name.to_lowercase();
    let level_dir = out.join("levels").join(&name);
    let (terrain, water) = terrain::import(&vfs, &interp.world, &level.name, &level_dir)
        .context("importing terrain")?;
    let world = &interp.world;

    // Static objects and the meshes they need.
    let converter = MeshConverter::new(&vfs, out);
    let static_instances: Vec<&Instance> = world.instances[static_range]
        .iter()
        .filter(|i| world.template(&i.template).is_some_and(is_visible_static))
        .collect();
    let template_names: HashSet<String> = static_instances
        .iter()
        .map(|i| i.template.to_ascii_lowercase())
        .collect();

    let failed = Mutex::new(Vec::new());
    let mesh_count = Mutex::new(0usize);
    let objects: HashMap<String, ObjectDesc> = template_names
        .par_iter()
        .map(|name| {
            let object = build_object(world, name, &converter, &failed, &mesh_count);
            (name.clone(), object)
        })
        .collect();
    for (name, object) in &objects {
        if !object.parts.is_empty() {
            game_data::write_ron(out.join("templates").join(format!("{name}.ron")), object)?;
        }
    }

    let statics: Vec<StaticInstance> = static_instances
        .iter()
        .filter(|i| objects.get(&i.template.to_ascii_lowercase()).is_some_and(|o| !o.parts.is_empty()))
        .filter_map(|i| {
            Some(StaticInstance {
                template: i.template.to_ascii_lowercase(),
                placement: instance_placement(i)?,
            })
        })
        .collect();

    let game_modes: Vec<GameModeDesc> = layouts
        .iter()
        .map(|(mode, size, range)| build_game_mode(world, mode, *size, &world.instances[range.clone()]))
        .collect();

    let desc = LevelDesc {
        name: name.clone(),
        display_name,
        terrain: Some(terrain),
        water,
        environment: environment(world),
        statics,
        game_modes,
    };
    game_data::write_ron(level_dir.join("level.ron"), &desc)?;

    Ok(LevelReport {
        statics: desc.statics.len(),
        templates: objects.values().filter(|o| !o.parts.is_empty()).count(),
        meshes: *mesh_count.lock().unwrap(),
        failed_meshes: failed.into_inner().unwrap(),
        missing_templates: interp.missing_templates.keys().cloned().collect(),
        game_modes: desc
            .game_modes
            .iter()
            .map(|g| format!("{}/{}", g.mode, g.size))
            .collect(),
    })
}

/// Template types that show up as static scenery.
fn is_visible_static(t: &Template) -> bool {
    matches!(
        t.ty.to_ascii_lowercase().as_str(),
        "simpleobject" | "bundle" | "destroyableobject" | "ladder" | "animatedbundle" | "rotationalbundle"
    )
}

fn instance_placement(instance: &Instance) -> Option<Placement> {
    if let Some(m) = instance.transform {
        let (scale, rotation, translation) = coords::matrix(m);
        return Some(Placement {
            position: translation.to_array(),
            rotation: rotation.normalize().to_array(),
            scale: scale.to_array(),
        });
    }
    let position = coords::position(instance.position?);
    let rotation = coords::rotation_ypr(instance.rotation.unwrap_or([0.0; 3]));
    Some(Placement {
        position,
        rotation: rotation.to_array(),
        ..Default::default()
    })
}

/// Flattens a template and its children into mesh/collision parts, converting meshes.
fn build_object(
    world: &World,
    name: &str,
    converter: &MeshConverter,
    failed: &Mutex<Vec<String>>,
    mesh_count: &Mutex<usize>,
) -> ObjectDesc {
    let mut parts = Vec::new();
    let kind = world.template(name).map(|t| t.ty.clone()).unwrap_or_default();
    let mut visited = HashSet::new();
    flatten(
        world,
        name,
        Affine3A::IDENTITY,
        Inherited::default(),
        converter,
        failed,
        mesh_count,
        &mut parts,
        &mut visited,
        0,
    );
    ObjectDesc {
        name: name.to_string(),
        kind,
        parts,
    }
}

/// Geometry and collision a child without its own inherits from its parent.
#[derive(Default, Clone)]
struct Inherited {
    mesh: Option<String>,
    collision: Option<String>,
}

#[allow(clippy::too_many_arguments)]
fn flatten(
    world: &World,
    name: &str,
    transform: Affine3A,
    inherited: Inherited,
    converter: &MeshConverter,
    failed: &Mutex<Vec<String>>,
    mesh_count: &Mutex<usize>,
    parts: &mut Vec<ObjectPart>,
    visited: &mut HashSet<String>,
    depth: u32,
) {
    let Some(template) = world.template(name) else {
        return;
    };
    if depth > 16 {
        return;
    }

    let convert = |path: &str, is_collision: bool| -> Option<String> {
        let result = if is_collision {
            converter.convert_collision(path)
        } else {
            converter.convert_mesh(path)
        };
        match result {
            Ok(rel) => {
                *mesh_count.lock().unwrap() += 1;
                Some(rel)
            }
            Err(err) => {
                failed.lock().unwrap().push(format!("{path}: {err:#}"));
                None
            }
        }
    };

    let own_mesh = template
        .geometry
        .as_deref()
        .and_then(|g| world.geometry(g))
        .and_then(|g| g.mesh_path())
        .and_then(|path| convert(&path, false));
    let own_collision = template
        .collision_mesh
        .as_deref()
        .and_then(|c| world.collision_meshes.get(&c.to_ascii_lowercase()))
        .and_then(|path| convert(path, true));
    let has_geometry_part = template.get("geometrypart").is_some();
    let has_collision_part = template.get("collisionpart").is_some();
    let mesh = own_mesh.clone().or_else(|| has_geometry_part.then(|| inherited.mesh.clone()).flatten());
    let collision = own_collision
        .clone()
        .or_else(|| has_collision_part.then(|| inherited.collision.clone()).flatten());

    if mesh.is_some() || collision.is_some() {
        let (scale, rotation, translation) = transform.to_scale_rotation_translation();
        parts.push(ObjectPart {
            mesh,
            mesh_index: template.get_f32("geometrypart").unwrap_or(0.0) as u32,
            collision,
            collision_part: template.get_f32("collisionpart").unwrap_or(0.0) as u32,
            placement: Placement {
                position: translation.to_array(),
                rotation: rotation.to_array(),
                scale: scale.to_array(),
            },
        });
    }

    let inherited = Inherited {
        mesh: own_mesh.or(inherited.mesh),
        collision: own_collision.or(inherited.collision),
    };
    visited.insert(name.to_ascii_lowercase());
    for child in &template.children {
        let key = child.template.to_ascii_lowercase();
        if visited.contains(&key) {
            continue;
        }
        let local = Affine3A::from_rotation_translation(
            coords::rotation_ypr(child.rotation.unwrap_or([0.0; 3])),
            Vec3::from_array(coords::position(child.position.unwrap_or([0.0; 3]))),
        );
        flatten(
            world,
            &key,
            transform * local,
            inherited.clone(),
            converter,
            failed,
            mesh_count,
            parts,
            visited,
            depth + 1,
        );
    }
    visited.remove(&name.to_ascii_lowercase());
}

fn build_game_mode(world: &World, mode: &str, size: u32, instances: &[Instance]) -> GameModeDesc {
    let mut layout = GameModeDesc {
        mode: mode.to_string(),
        size,
        ..Default::default()
    };
    for instance in instances {
        let Some(template) = world.template(&instance.template) else {
            continue;
        };
        let Some(placement) = instance_placement(instance) else {
            continue;
        };
        match template.ty.to_ascii_lowercase().as_str() {
            "controlpoint" => layout.control_points.push(ControlPointDesc {
                id: template
                    .get_str("controlpointid")
                    .unwrap_or(&template.name)
                    .to_string(),
                name: template
                    .get_str("setcontrolpointname")
                    .unwrap_or(&template.name)
                    .to_string(),
                position: placement.position,
                initial_team: template.get_f32("team").unwrap_or(0.0) as u8,
                radius: template.get_f32("radius").unwrap_or(10.0),
                uncapturable: template.get_f32("unabletochangeteam").unwrap_or(0.0) != 0.0,
            }),
            "spawnpoint" => layout.spawn_points.push(SpawnPointDesc {
                control_point: template
                    .get_str("setcontrolpointid")
                    .unwrap_or_default()
                    .to_string(),
                placement,
            }),
            "objectspawner" => {
                let mut templates: [Option<String>; 2] = [None, None];
                for args in template.get_all("setobjecttemplate") {
                    if let (Some(team), Some(name)) = (args.first(), args.get(1)) {
                        match team.as_str() {
                            "1" => templates[0] = Some(name.to_ascii_lowercase()),
                            "2" => templates[1] = Some(name.to_ascii_lowercase()),
                            _ => {}
                        }
                    }
                }
                layout.vehicle_spawners.push(VehicleSpawnerDesc {
                    control_point: instance.get_str("setcontrolpointid").map(str::to_string),
                    templates,
                    placement,
                    min_respawn_seconds: template.get_f32("minspawndelay").unwrap_or(0.0),
                    max_respawn_seconds: template.get_f32("maxspawndelay").unwrap_or(0.0),
                });
            }
            _ => {}
        }
    }
    layout
}

fn environment(world: &World) -> EnvironmentDesc {
    let defaults = EnvironmentDesc::default();
    let color = |name: &str| world.setting(name).and_then(parse_vec3).map(terrain::normalize_color);
    let fog = world
        .setting("renderer.fogstartendandbase")
        .and_then(|s| bf2_formats::con::parse_vec(s));
    let view_distance = world
        .setting("gamelogic.maximumlevelviewdistance")
        .and_then(|s| s.parse::<f32>().ok());
    let fog_color = color("renderer.fogcolor").unwrap_or(defaults.fog_color);
    let fog_range = match fog.as_deref() {
        Some([start, end, ..]) if end > start => [*start, *end],
        _ => defaults.fog_range,
    };
    EnvironmentDesc {
        sun_direction: world
            .setting("lightmanager.sundirection")
            .and_then(parse_vec3)
            .map(coords::direction)
            .unwrap_or(defaults.sun_direction),
        sun_color: color("lightmanager.suncolor").unwrap_or(defaults.sun_color),
        ambient_color: color("lightmanager.ambientcolor").unwrap_or(defaults.ambient_color),
        sky_color: fog_color,
        fog_color,
        fog_range,
        view_distance: view_distance.unwrap_or(fog_range[1]).max(fog_range[1]),
    }
}

/// `(mode, players)` pairs from an Info `.desc`.
pub fn parse_modes(desc: &str) -> Vec<(String, u32)> {
    let mut modes = Vec::new();
    let mut current_mode: Option<String> = None;
    for tag in desc.split('<').skip(1) {
        let tag_body = tag.split('>').next().unwrap_or("");
        let lower = tag_body.to_ascii_lowercase();
        if lower.starts_with("mode ") {
            current_mode = attribute(tag_body, "type");
        } else if lower.starts_with("maptype")
            && let (Some(mode), Some(players)) = (&current_mode, attribute(tag_body, "players"))
            && let Ok(players) = players.parse()
        {
            modes.push((mode.to_ascii_lowercase(), players));
        }
    }
    modes
}

fn attribute(tag: &str, name: &str) -> Option<String> {
    let lower = tag.to_ascii_lowercase();
    let start = lower.find(&format!("{name}=\""))? + name.len() + 2;
    let end = tag[start..].find('"')? + start;
    Some(tag[start..end].to_string())
}

fn xml_text(desc: &str, tag: &str) -> Option<String> {
    let lower = desc.to_ascii_lowercase();
    let start = lower.find(&format!("<{tag}>"))? + tag.len() + 2;
    let end = lower[start..].find(&format!("</{tag}>"))? + start;
    Some(desc[start..end].trim().to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_modes_from_desc() {
        let desc = r#"<map><modes><mode type="gpm_cq">
            <maptype ai="1" players="16" type="assault">x</maptype>
            <maptype players="64" type="assault">x</maptype></mode>
            <mode type="gpm_coop"><maptype ai="1" players="16">x</maptype></mode></modes></map>"#;
        assert_eq!(
            parse_modes(desc),
            vec![("gpm_cq".into(), 16), ("gpm_cq".into(), 64), ("gpm_coop".into(), 16)]
        );
    }
}
