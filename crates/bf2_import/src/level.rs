//! Importing a whole level.

use std::{
    collections::{HashMap, HashSet},
    path::Path,
    sync::Mutex,
};

use anyhow::{Context, Result};
use bf2_formats::{
    Bf2Install, LevelInfo, Side, Vfs,
    localization::Localization,
    mesh::{MeshKind, Usage, VisMesh},
    con::{Instance, Interpreter, Template, World, parse_vec3},
};
use game_data::{
    ControlPointDesc, EnvironmentDesc, FlagModels, GameModeDesc, KitSlot, LevelDesc, ObjectDesc, ObjectPart,
    Placement, SkyDesc, SpawnPointDesc, StaticInstance, TeamDesc, TeamVoice, TerrainDesc, VehicleSpawnerDesc,
};
use glam::{Affine3A, Vec3};
use rayon::prelude::*;

use crate::{audio, coords, destruction, lods, meshes::MeshConverter, roads, terrain, vehicles, weapons};

pub struct LevelReport {
    pub statics: usize,
    pub roads: usize,
    pub kits: usize,
    pub weapons: usize,
    pub vehicles: usize,
    pub templates: usize,
    pub meshes: usize,
    pub failed_meshes: Vec<String>,
    pub missing_templates: Vec<String>,
    pub game_modes: Vec<String>,
}

pub fn import_level(
    install: &Bf2Install,
    level: &LevelInfo,
    localization: &Localization,
    out: &Path,
) -> Result<LevelReport> {
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
        let range = start..interp.world.instances.len();
        // Layouts reuse template names with other ids and teams (the game only ever loads
        // one), and a later layout's `ObjectTemplate.create` replaces the template: keep
        // this layout's own (Operation Blue Pearl 16 got the 64 player Market and Yard).
        let templates: HashMap<String, Template> = interp.world.instances[range.clone()]
            .iter()
            .filter_map(|i| {
                let template = interp.world.template(&i.template)?;
                Some((i.template.to_ascii_lowercase(), template.clone()))
            })
            .collect();
        layouts.push((mode, size, range, templates));
    }

    let name = level.name.to_lowercase();
    let level_dir = out.join("levels").join(&name);
    let converter = MeshConverter::new(&vfs, out);

    // Kits of both teams and the weapons they carry.
    let mut level_teams = teams(&interp.world);
    for team in &mut level_teams {
        team.voice = team_voice(&vfs, &team.language, out);
        team.icons = crate::ui_icons::team_icons(&vfs, out, &team.name);
    }
    let languages: Vec<String> = level_teams.iter().map(|t| t.language.clone()).collect();
    crate::sounds::import_radio(&vfs, localization, &languages, out);
    let kit_names: Vec<String> = level_teams
        .iter()
        .flat_map(|t| t.kits.iter().map(|k| k.kit.clone()))
        .collect();
    let (kit_count, weapon_count) =
        weapons::import(&mut interp, &converter, localization, &kit_names, out)?;
    let flag_models = flag_models(&mut interp, &vfs, &converter, out);

    let (terrain, water) = terrain::import(&vfs, &interp.world, &converter, &level.name, &level_dir)
        .context("importing terrain")?;
    let vegetation = crate::vegetation::import(&mut interp, &converter, &name, &level_dir, &terrain);
    let static_templates: HashSet<String> = interp.world.instances[static_range.clone()]
        .iter()
        .map(|i| i.template.to_ascii_lowercase())
        .collect();
    destruction::load_effects(&mut interp, &static_templates);
    crate::effects::import(&mut interp, &converter, &kit_names, &static_templates, out);
    if let Err(err) = destruction::import_materials(&vfs, out) {
        log::warn!("damage table: {err:#}");
    }
    let assets = crate::commander::load_assets(&mut interp);
    let world = &interp.world;

    // Static objects and the meshes they need.
    let static_instances: Vec<&Instance> = world.instances[static_range]
        .iter()
        .filter(|i| world.template(&i.template).is_some_and(is_visible_static))
        .collect();
    let mut template_names: HashSet<String> = static_instances
        .iter()
        .map(|i| i.template.to_ascii_lowercase())
        .collect();
    // The commander's assets (spawned by the layouts) are objects too.
    template_names.extend(assets.iter().cloned());

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
    if let Err(err) = crate::effects::import_surfaces(world, &converter, &objects, &level_dir) {
        log::warn!("surfaces: {err:#}");
    }
    if let Err(err) = crate::sounds::import_level(&vfs, world, &level_dir, out) {
        log::warn!("sounds: {err:#}");
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

    let roads = roads::import(&vfs, world, &converter, &name, out);

    let game_modes: Vec<GameModeDesc> = layouts
        .iter()
        .map(|(mode, size, range, templates)| {
            build_game_mode(templates, localization, mode, *size, &world.instances[range.clone()])
        })
        .collect();
    // The top-down map BF2 shows in game, copied as-is (north up, not flipped).
    let minimap = converter.file(&format!("{base}/hud/minimap/ingamemap.dds"));
    // The cube map `EnvMap` materials reflect; the game finds it by this path.
    converter.file(&format!("{base}/envmaps/envmap0.dds"));
    let ticket_loss_at_end_per_minute = world
        .commands
        .iter()
        .rev()
        .find(|c| c.name == "gamelogic.setticketlossatendpermin")
        .and_then(|c| c.args.first()?.parse().ok())
        .unwrap_or(200.0);

    let mut environment = environment(world, &converter, &terrain, &level_dir);
    environment.static_lightmaps = crate::lighting::static_lightmaps(&vfs, &level.name, &level_dir)
        .map_err(|e| log::warn!("{name}: static lightmaps: {e:#}"))
        .ok()
        .flatten();
    let desc = LevelDesc {
        name: name.clone(),
        display_name,
        terrain: Some(terrain),
        water,
        environment,
        statics,
        roads,
        game_modes,
        teams: level_teams,
        minimap,
        flag_models,
        neutral_icons: crate::ui_icons::neutral_icons(&vfs, out),
        vehicle_icons: Default::default(),
        ticket_loss_at_end_per_minute,
        vegetation,
    };
    game_data::write_ron(level_dir.join("level.ron"), &desc)?;
    match crate::ai::import(&mut interp, &vfs, &base, &kit_names, &desc.game_modes, &level_dir, out) {
        Ok((areas, weapons)) => log::info!("{name}: {areas} strategic areas, {weapons} weapon templates for bots"),
        Err(err) => log::warn!("ai: {err:#}"),
    }

    // Vehicles the spawners of any layout create.
    let mut vehicle_names: Vec<String> = desc
        .game_modes
        .iter()
        .flat_map(|g| &g.vehicle_spawners)
        .flat_map(|s| s.templates.iter().flatten().cloned())
        .collect();
    vehicle_names.sort();
    vehicle_names.dedup();
    let missing_templates = interp.missing_templates.keys().cloned().collect();
    let vehicles = vehicles::import(&mut interp, &converter, localization, &vehicle_names, out)?;
    // The vehicles' map icons, now that their templates are loaded.
    let mut desc = desc;
    desc.vehicle_icons = crate::ui_icons::vehicle_icons(&mut interp, &vfs, out, &vehicle_names);
    game_data::write_ron(level_dir.join("level.ron"), &desc)?;
    crate::effects::import_vehicle_weapons(&interp.world, out);
    if let Err(err) = crate::commander::import(&mut interp, &converter, &assets, &level_dir, out) {
        log::warn!("commander assets: {err:#}");
    }

    Ok(LevelReport {
        statics: desc.statics.len(),
        roads: desc.roads.len(),
        kits: kit_count,
        weapons: weapon_count,
        vehicles: vehicles.len(),
        templates: objects.values().filter(|o| !o.parts.is_empty()).count(),
        meshes: *mesh_count.lock().unwrap(),
        failed_meshes: failed.into_inner().unwrap(),
        missing_templates,
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
    let mut bounds = ObjectBounds::default();
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
        &mut bounds,
    );
    ObjectDesc {
        name: name.to_string(),
        kind,
        parts,
        armor: world.template(name).and_then(|t| destruction::armor(world, t, converter)),
        draw_distance: if bounds.vegetation {
            None
        } else {
            lods::draw_distance(world.template(name), bounds.radius)
        },
    }
}

/// Geometry and collision a child without its own inherits from its parent.
#[derive(Default, Clone)]
struct Inherited {
    mesh: Option<String>,
    collision: Option<String>,
    /// Lower LODs of `mesh` and its radius.
    lods: lods::ObjectLods,
}

/// How far the visible parts of an object reach from its origin, for its draw distance.
#[derive(Default)]
struct ObjectBounds {
    radius: f32,
    vegetation: bool,
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
    bounds: &mut ObjectBounds,
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
            destruction::convert_object_mesh(converter, path)
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

    let mesh_source = template
        .geometry
        .as_deref()
        .and_then(|g| world.geometry(g))
        .and_then(|g| g.mesh_path());
    let collision_source = template
        .collision_mesh
        .as_deref()
        .and_then(|c| world.collision_meshes.get(&c.to_ascii_lowercase()).cloned())
        .or_else(|| destruction::uncreated_collision_mesh(converter, template));
    let own_mesh = mesh_source.as_deref().and_then(|path| convert(path, false));
    let own_collision = collision_source.as_deref().and_then(|path| convert(path, true));
    let has_geometry_part = template.get("geometrypart").is_some();
    let has_collision_part = template.get("collisionpart").is_some();
    // Lower LODs of the mesh this part draws (its own or, for a geometry part, the parent's).
    let own_lods = own_mesh.as_ref().and(mesh_source.as_deref()).map(|path| {
        let geometry = template.geometry.as_deref().and_then(|g| world.geometry(g));
        lods::object_lods(converter, geometry, path)
    });
    let part_lods = match (&own_lods, &own_mesh) {
        (Some(own), _) => Some(own.clone()),
        (None, None) if has_geometry_part && inherited.mesh.is_some() => Some(inherited.lods.clone()),
        _ => None,
    };
    let mesh = own_mesh.clone().or_else(|| has_geometry_part.then(|| inherited.mesh.clone()).flatten());
    let collision = own_collision
        .clone()
        .or_else(|| has_collision_part.then(|| inherited.collision.clone()).flatten());
    let collision_part = template.get_f32("collisionpart").unwrap_or(0.0) as u32;

    // What a destroyable object leaves behind, and what its surface is made of.
    let (wreck_mesh, (hit_material, wreck_collision)) = if destruction::is_destroyable(template) {
        (
            mesh_source.as_deref().and_then(|path| destruction::convert_wreck_mesh(converter, path)),
            collision_source.as_deref().map_or((None, None), |path| {
                destruction::collision_info(converter, path, collision_part as usize, template)
            }),
        )
    } else {
        (None, (None, None))
    };

    if mesh.is_some() || collision.is_some() {
        let (scale, rotation, translation) = transform.to_scale_rotation_translation();
        let part_lods = part_lods.filter(|_| mesh.is_some()).unwrap_or_default();
        if mesh.is_some() {
            bounds.vegetation |= part_lods.vegetation;
            bounds.radius = bounds
                .radius
                .max(translation.length() + part_lods.radius * scale.max_element());
        }
        let wreck_mesh_set = wreck_mesh.is_some();
        parts.push(ObjectPart {
            mesh,
            mesh_index: template.get_f32("geometrypart").unwrap_or(0.0) as u32,
            collision,
            collision_part,
            wreck_mesh,
            wreck_collision,
            hit_material,
            ladder: template.ty.eq_ignore_ascii_case("ladder"),
            wreck_lods: if wreck_mesh_set { part_lods.wreck_lods.clone() } else { Vec::new() },
            lods: part_lods.lods,
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
        lods: own_lods.unwrap_or(inherited.lods),
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
            bounds,
        );
    }
    visited.remove(&name.to_ascii_lowercase());
}

/// A layout from its instances and the templates they had when its script ran (by
/// lowercased name).
fn build_game_mode(
    templates: &HashMap<String, Template>,
    localization: &Localization,
    mode: &str,
    size: u32,
    instances: &[Instance],
) -> GameModeDesc {
    let mut layout = GameModeDesc {
        mode: mode.to_string(),
        size,
        ..Default::default()
    };
    for instance in instances {
        let Some(template) = templates.get(&instance.template.to_ascii_lowercase()) else {
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
                name: localization.resolve(
                    template.get_str("setcontrolpointname").unwrap_or(&template.name),
                ),
                position: placement.position,
                initial_team: template.get_f32("team").unwrap_or(0.0) as u8,
                radius: template.get_f32("radius").unwrap_or(10.0),
                uncapturable: template.get_f32("unabletochangeteam").unwrap_or(0.0) != 0.0,
                area_value: [
                    template.get_f32("areavalueteam1").unwrap_or(0.0),
                    template.get_f32("areavalueteam2").unwrap_or(0.0),
                ],
                time_to_get_control: template.get_f32("timetogetcontrol").unwrap_or(10.0),
                time_to_lose_control: template.get_f32("timetolosecontrol").unwrap_or(10.0),
                only_takeable_by_team: template.get_f32("onlytakeablebyteam").unwrap_or(0.0) as u8,
                enemy_ticket_loss_when_captured: template
                    .get_f32("enemyticketlosswhencaptured")
                    .unwrap_or(0.0),
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

fn environment(world: &World, converter: &MeshConverter, terrain: &TerrainDesc, level_dir: &Path) -> EnvironmentDesc {
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
    let sun_direction = world
        .setting("lightmanager.sundirection")
        .and_then(parse_vec3)
        .map(coords::direction)
        .unwrap_or(defaults.sun_direction);
    EnvironmentDesc {
        sun_direction,
        sun_color: color("lightmanager.suncolor").unwrap_or(defaults.sun_color),
        ambient_color: color("lightmanager.ambientcolor").unwrap_or(defaults.ambient_color),
        sky_color: fog_color,
        fog_color,
        fog_range,
        view_distance: view_distance.unwrap_or(fog_range[1]).max(fog_range[1]),
        sky: sky(world, converter),
        lighting: crate::lighting::level_lighting(world, Some(terrain), level_dir, sun_direction),
        ground_albedo: crate::lighting::ground_albedo(level_dir, terrain),
        static_lightmaps: None,
    }
}

/// The sky dome: `Skydome.skyTemplate` names the dome object, `Skydome.skyTexture` the
/// level's sky picture.
fn sky(world: &World, converter: &MeshConverter) -> Option<SkyDesc> {
    let template = world.template(world.setting("skydome.skytemplate")?)?;
    let geometry = world.geometry(template.geometry.as_deref()?)?;
    let mesh_path = geometry.mesh_path()?;
    let mesh = converter
        .convert_mesh(&mesh_path)
        .map_err(|e| log::warn!("sky dome: {e:#}"))
        .ok()?;
    let radius = converter.mesh_radius(&mesh_path).unwrap_or(500.0);
    let texture = converter.texture(world.setting("skydome.skytexture")?)?;
    Some(SkyDesc {
        mesh,
        radius,
        texture,
        rotation: world
            .setting("skydome.domerotation")
            .and_then(|s| s.parse().ok())
            .unwrap_or(0.0),
    })
}

/// The flag pole (converted here, with its height) and the flags of neutral, team 1 and
/// team 2 from `gameLogic.setTeamFlag` (their models are exported with the soldiers).
fn flag_models(interp: &mut Interpreter, vfs: &Vfs, converter: &MeshConverter, out: &Path) -> FlagModels {
    let flags: Vec<(usize, String)> = interp
        .world
        .commands
        .iter()
        .filter(|c| c.name == "gamelogic.setteamflag")
        .filter_map(|c| {
            let team: usize = c.args.first()?.parse().ok()?;
            Some((team, c.args.get(1)?.trim_matches('"').to_ascii_lowercase()))
        })
        .collect();
    let mut mesh_path = |template: &str| -> Option<String> {
        interp.ensure_template(template);
        let world = &interp.world;
        let geometry = world.template(template)?.geometry.clone()?;
        world.geometry(&geometry)?.mesh_path()
    };
    let mut models = FlagModels::default();
    if let Some(pole) = mesh_path("flagpole") {
        models.pole_height = vfs
            .read(&pole)
            .ok()
            .and_then(|data| VisMesh::parse(&data, MeshKind::Static).ok())
            .and_then(|mesh| mesh.attribute::<3>(Usage::Position, 0))
            .map_or(0.0, |positions| positions.iter().map(|p| p[1]).fold(0.0, f32::max));
        models.pole = converter
            .convert_mesh(&pole)
            .map_err(|e| log::warn!("flag pole: {e:#}"))
            .ok();
    }
    for (team, template) in flags {
        let glb = mesh_path(&template).map(|p| format!("{}.glb", p.trim_end_matches(".skinnedmesh")));
        if let Some(slot) = models.flags.get_mut(team) {
            *slot = glb.filter(|p| out.join(p).exists());
        }
    }
    // The pole's effect bundle loops a flapping sound.
    models.sound = crate::sounds::SoundConverter::new(vfs, out).template(&interp.world, "s_flagpole_sfxbundle_start");
    models
}

/// The commander's rule announcements in `language` (`common/sound/<language>/commander/`).
fn team_voice(vfs: &Vfs, language: &str, out: &Path) -> TeamVoice {
    let dir = format!("common/sound/{}/commander/filter/", language.to_ascii_lowercase());
    let lines = |name: &str| -> Vec<String> {
        let prefix = format!("{dir}auto_rules_{name}");
        let mut files: Vec<String> = vfs
            .list(&dir)
            .filter(|p| p.starts_with(&prefix) && p.ends_with(".ogg"))
            .filter_map(|p| audio::sound(vfs, p, out))
            .collect();
        files.sort();
        files
    };
    TeamVoice {
        we_captured: lines("wecapturedacp"),
        we_lost: lines("welostacp"),
        enemy_captured: lines("enemycapturedacp"),
        bleed_start: lines("ticketbleedstart"),
        bleed_end: lines("ticketbleedend"),
        low_tickets: audio::sound(vfs, "common/sound/hud/lowontickets.wav", out).into_iter().collect(),
    }
}

/// Team names, kits and ticket rules from the level's `gameLogic.*` settings.
fn teams(world: &World) -> Vec<TeamDesc> {
    let mut teams = vec![TeamDesc::default(), TeamDesc::default()];
    let team = |arg: Option<&String>| -> Option<usize> {
        match arg.map(String::as_str) {
            Some("1") => Some(0),
            Some("2") => Some(1),
            _ => None,
        }
    };
    let unquote = |s: &String| s.trim_matches('"').to_string();
    for command in &world.commands {
        let a = &command.args;
        match command.name.as_str() {
            "gamelogic.setteamname" => {
                if let (Some(t), Some(name)) = (team(a.first()), a.get(1)) {
                    teams[t].name = unquote(name);
                }
            }
            "gamelogic.setkit" => {
                if let (Some(t), Some(slot), Some(kit), Some(soldier)) =
                    (team(a.first()), a.get(1).and_then(|s| s.parse::<usize>().ok()), a.get(2), a.get(3))
                {
                    let kits = &mut teams[t].kits;
                    if kits.len() <= slot {
                        kits.resize(slot + 1, KitSlot { kit: String::new(), soldier: String::new() });
                    }
                    kits[slot] = KitSlot {
                        kit: unquote(kit).to_ascii_lowercase(),
                        soldier: unquote(soldier).to_ascii_lowercase(),
                    };
                }
            }
            "gamelogic.setdefaultnumberofticketsex" => {
                if let (Some(size), Some(t), Some(tickets)) = (
                    a.first().and_then(|s| s.parse().ok()),
                    team(a.get(1)),
                    a.get(2).and_then(|s| s.parse().ok()),
                ) {
                    teams[t].tickets.retain(|(s, _)| *s != size);
                    teams[t].tickets.push((size, tickets));
                }
            }
            "gamelogic.setteamlanguage" => {
                if let (Some(t), Some(language)) = (team(a.first()), a.get(1)) {
                    teams[t].language = unquote(language);
                }
            }
            "gamelogic.setticketlosspermin" => {
                if let (Some(t), Some(loss)) = (team(a.first()), a.get(1).and_then(|s| s.parse().ok())) {
                    teams[t].ticket_loss_per_minute = loss;
                }
            }
            _ => {}
        }
    }
    teams
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
