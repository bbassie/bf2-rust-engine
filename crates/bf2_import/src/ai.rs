//! BF2's AI hints: strategic areas per layout (`gamemodes/<mode>/<size>/ai/StrategicAreas.ai`)
//! and weapon templates (`objects/weapons/**/ai/Weapons.ai`), converted to
//! [`game_data::LevelAiDesc`] and [`game_data::AiWeaponsDesc`].
//!
//! Most of BF2's AI setup (`mods/bf2/AI/AIBehaviours.ai`, `AIDefaultStrategies.ai`) binds
//! engine behaviours and is not data worth importing; the server implements those rules
//! itself. Only BF2's single player layouts have strategic areas, so the server derives
//! areas for the others from their control points.

use std::{collections::HashMap, path::Path};

use anyhow::{Context, Result};
use bf2_formats::{
    Bf2Install, LevelInfo, Side, Vfs,
    con::{Interpreter, lex, parse_vec},
};
use game_data::{
    AiWeaponDesc, AiWeaponsDesc, FiringPose, GameModeDesc, KitDesc, LevelAiDesc, LevelDesc, StrategicAreaDesc,
    StrategicLayoutDesc, StrategicRouteDesc,
};

use crate::coords;

/// Strategic areas and weapon templates for a level being imported (after its kits and
/// weapons). Returns the number of areas and of weapons with a template.
pub fn import(
    interp: &mut Interpreter,
    vfs: &Vfs,
    base: &str,
    kits: &[String],
    game_modes: &[GameModeDesc],
    level_dir: &Path,
    out: &Path,
) -> Result<(usize, usize)> {
    let areas = import_level(vfs, base, game_modes, level_dir)?;
    let weapons = import_weapons(interp, vfs, kits, out)?;
    Ok((areas, weapons))
}

/// `bf2-import ai`: only the AI hints of an already imported level.
pub fn import_only(install: &Bf2Install, level: &LevelInfo, out: &Path) -> Result<(usize, usize)> {
    let name = level.name.to_lowercase();
    let level_dir = out.join("levels").join(&name);
    let desc: LevelDesc = game_data::read_ron(level_dir.join("level.ron"))
        .with_context(|| format!("{name} isn't imported yet"))?;
    let vfs = install.level_vfs(level, Side::Both)?;
    let mut interp = Interpreter::new(&vfs);
    let kits: Vec<String> = desc.teams.iter().flat_map(|t| t.kits.iter().map(|k| k.kit.clone())).collect();
    let base = format!("levels/{}", level.name);
    import(&mut interp, &vfs, &base, &kits, &desc.game_modes, &level_dir, out)
}

/// How far an area made from bounds may be from the control point it is about, meters.
const MATCH_DISTANCE: f32 = 30.0;

/// Writes `levels/<name>/ai.ron` from the strategic areas of the level's layouts. `base` is
/// the level's folder in the VFS. Returns the number of areas.
pub fn import_level(vfs: &Vfs, base: &str, game_modes: &[GameModeDesc], level_dir: &Path) -> Result<usize> {
    let mut desc = LevelAiDesc::default();
    for layout in game_modes {
        let path = format!("{base}/gamemodes/{}/{}/ai/strategicareas.ai", layout.mode, layout.size);
        let Ok(text) = vfs.read_text(&path) else {
            continue;
        };
        let strategic = parse_strategic_areas(&text, layout);
        if !strategic.areas.is_empty() {
            desc.layouts.push(strategic);
        }
    }
    let count = desc.layouts.iter().map(|l| l.areas.len()).sum();
    let path = level_dir.join("ai.ron");
    if count > 0 {
        game_data::write_ron(&path, &desc)?;
    } else if path.exists() {
        std::fs::remove_file(&path)?;
    }
    Ok(count)
}

/// Parses one `StrategicAreas.ai`, tying areas to the layout's control points.
fn parse_strategic_areas(text: &str, layout: &GameModeDesc) -> StrategicLayoutDesc {
    let mut out = StrategicLayoutDesc {
        mode: layout.mode.clone(),
        size: layout.size,
        ..Default::default()
    };
    let mut control_point_flag: Vec<bool> = Vec::new();
    let mut active: Option<usize> = None;
    let mut routes: Vec<(String, String, Vec<[f32; 3]>)> = Vec::new();
    let position = |arg: &str| -> Option<[f32; 3]> {
        let v = parse_vec(arg)?;
        (v.len() == 3).then(|| coords::position([v[0], v[1], v[2]]))
    };
    for stmt in lex(text) {
        let args: Vec<&str> = stmt.tokens.iter().map(|t| t.text.as_str()).collect();
        let Some((command, args)) = args.split_first() else {
            continue;
        };
        let find = |areas: &[StrategicAreaDesc], name: &str| {
            areas.iter().position(|a| a.name.eq_ignore_ascii_case(name))
        };
        match command.to_ascii_lowercase().as_str() {
            // `createFromControlPoint <name> <control point id> <?>`
            "aistrategicarea.createfromcontrolpoint" => {
                let (Some(name), Some(id)) = (args.first(), args.get(1)) else {
                    continue;
                };
                let Some(cp) = layout.control_points.iter().find(|cp| cp.id == *id) else {
                    log::debug!("strategic area {name}: no control point {id}");
                    continue;
                };
                out.areas.push(StrategicAreaDesc {
                    name: name.to_string(),
                    control_point: Some(cp.id.clone()),
                    position: cp.position,
                    infantry_position: None,
                    neighbours: Vec::new(),
                });
                control_point_flag.push(true);
            }
            // `create <name> <x0/z0> <x1/z1> <y> <height>`
            "aistrategicarea.create" => {
                let (Some(name), Some(min), Some(max), Some(y)) =
                    (args.first(), args.get(1), args.get(2), args.get(3))
                else {
                    continue;
                };
                let (Some(min), Some(max), Ok(y)) = (parse_vec(min), parse_vec(max), y.parse::<f32>()) else {
                    continue;
                };
                if min.len() < 2 || max.len() < 2 {
                    continue;
                }
                let center = [(min[0] + max[0]) / 2.0, y, (min[1] + max[1]) / 2.0];
                out.areas.push(StrategicAreaDesc {
                    name: name.to_string(),
                    control_point: None,
                    position: coords::position(center),
                    infantry_position: None,
                    neighbours: Vec::new(),
                });
                control_point_flag.push(false);
            }
            "aistrategicarea.setactive" => {
                active = args.first().and_then(|name| find(&out.areas, name));
            }
            "aistrategicarea.addneighbour" => {
                if let (Some(index), Some(name)) = (active, args.first())
                    && !out.areas[index].neighbours.iter().any(|n| n.eq_ignore_ascii_case(name))
                {
                    out.areas[index].neighbours.push(name.to_string());
                }
            }
            "aistrategicarea.addobjecttypeflag" => {
                if let (Some(index), Some(flag)) = (active, args.first())
                    && flag.eq_ignore_ascii_case("controlpoint")
                {
                    control_point_flag[index] = true;
                }
            }
            // `setOrderPosition Infantry x/y/z`
            "aistrategicarea.setorderposition" => {
                if let (Some(index), Some(kind), Some(at)) = (active, args.first(), args.get(1))
                    && kind.eq_ignore_ascii_case("infantry")
                    && let Some(at) = position(at)
                    && at != [0.0; 3]
                {
                    out.areas[index].infantry_position = Some(at);
                }
            }
            // `addWayPoint x/y/z Infantry <from> <to>` (not tied to the active area)
            "aistrategicarea.addwaypoint" => {
                let (Some(at), Some(kind), Some(from), Some(to)) =
                    (args.first(), args.get(1), args.get(2), args.get(3))
                else {
                    continue;
                };
                let Some(at) = position(at).filter(|_| kind.eq_ignore_ascii_case("infantry")) else {
                    continue;
                };
                match routes.iter_mut().find(|(f, t, _)| f.eq_ignore_ascii_case(from) && t.eq_ignore_ascii_case(to)) {
                    Some((_, _, waypoints)) => waypoints.push(at),
                    None => routes.push((from.to_string(), to.to_string(), vec![at])),
                }
            }
            _ => {}
        }
    }

    // Areas made from bounds that are about a control point: the closest one.
    for (area, is_cp) in out.areas.iter_mut().zip(&control_point_flag) {
        if area.control_point.is_some() || !is_cp {
            continue;
        }
        let distance = |p: [f32; 3]| {
            let (dx, dz) = (p[0] - area.position[0], p[2] - area.position[2]);
            (dx * dx + dz * dz).sqrt()
        };
        area.control_point = layout
            .control_points
            .iter()
            .map(|cp| (cp, distance(cp.position)))
            .filter(|(_, d)| *d < MATCH_DISTANCE)
            .min_by(|a, b| a.1.total_cmp(&b.1))
            .map(|(cp, _)| cp.id.clone());
    }
    let names: Vec<String> = out.areas.iter().map(|a| a.name.to_ascii_lowercase()).collect();
    for area in &mut out.areas {
        area.neighbours.retain(|n| names.contains(&n.to_ascii_lowercase()));
    }
    out.routes = routes
        .into_iter()
        .filter(|(from, to, _)| names.contains(&from.to_ascii_lowercase()) && names.contains(&to.to_ascii_lowercase()))
        .map(|(from, to, waypoints)| StrategicRouteDesc { from, to, waypoints })
        .collect();
    out
}

/// Adds the AI templates of the weapons of `kits` to `ai/weapons.ron` (shared by all levels).
/// The weapon templates must be loaded by `interp` (or loadable). Returns how many weapons
/// have one.
pub fn import_weapons(interp: &mut Interpreter, vfs: &Vfs, kits: &[String], out: &Path) -> Result<usize> {
    let templates = weapon_templates(vfs);
    let path = out.join("ai").join("weapons.ron");
    let mut desc: AiWeaponsDesc = game_data::read_ron(&path).unwrap_or_default();
    let mut count = 0;
    for kit in kits {
        let Ok(kit) = game_data::read_ron::<KitDesc>(out.join("kits").join(format!("{kit}.ron"))) else {
            continue;
        };
        for weapon in &kit.weapons {
            interp.ensure_template(weapon);
            let ai = interp
                .world
                .template(weapon)
                .and_then(|t| t.get_str("aitemplate"))
                .and_then(|name| templates.get(&name.to_ascii_lowercase()));
            if let Some(ai) = ai {
                desc.weapons.insert(weapon.clone(), ai.clone());
                count += 1;
            }
        }
    }
    if count > 0 {
        game_data::write_ron(&path, &desc)?;
    }
    Ok(count)
}

/// Every `weaponTemplate` in the weapons' `ai/weapons.ai` files, by lowercased name.
fn weapon_templates(vfs: &Vfs) -> HashMap<String, AiWeaponDesc> {
    let mut templates = HashMap::new();
    let files: Vec<&str> = vfs
        .list("objects/weapons")
        .filter(|p| p.ends_with("/ai/weapons.ai"))
        .collect();
    for file in files {
        let Ok(text) = vfs.read_text(file) else {
            continue;
        };
        parse_weapon_templates(&text, &mut templates);
    }
    templates
}

fn parse_weapon_templates(text: &str, templates: &mut HashMap<String, AiWeaponDesc>) {
    let mut active: Option<(String, AiWeaponDesc, f32)> = None;
    let mut finish = |active: Option<(String, AiWeaponDesc, f32)>| {
        if let Some((name, mut weapon, percentage)) = active {
            weapon.optimal_range = weapon.min_range + (weapon.max_range - weapon.min_range) * percentage / 100.0;
            templates.insert(name, weapon);
        }
    };
    for stmt in lex(text) {
        let args: Vec<&str> = stmt.tokens.iter().map(|t| t.text.as_str()).collect();
        let Some((command, args)) = args.split_first() else {
            continue;
        };
        let command = command.to_ascii_lowercase();
        if command == "weapontemplate.create" {
            finish(active.take());
            if let Some(name) = args.first() {
                let weapon = AiWeaponDesc {
                    min_range: 0.0,
                    max_range: 50.0,
                    optimal_range: 0.0,
                    pose: FiringPose::Standing,
                    infantry_strength: 0.0,
                    thrown: false,
                    explosion_radius: 0.0,
                };
                active = Some((name.to_ascii_lowercase(), weapon, 50.0));
            }
            continue;
        }
        let Some((_, weapon, percentage)) = &mut active else {
            continue;
        };
        let number = |i: usize| args.get(i).and_then(|a| a.parse::<f32>().ok());
        match command.as_str() {
            "weapontemplate.minrange" => weapon.min_range = number(0).unwrap_or(weapon.min_range),
            "weapontemplate.maxrange" => weapon.max_range = number(0).unwrap_or(weapon.max_range),
            "weapontemplate.optimalrangepercentage" => *percentage = number(0).unwrap_or(*percentage),
            "weapontemplate.isthrown" => weapon.thrown = number(0).is_some_and(|v| v != 0.0),
            "weapontemplate.setexplosionradius" => weapon.explosion_radius = number(0).unwrap_or(0.0),
            "weapontemplate.setfiringpose" => {
                weapon.pose = match args.first().map(|a| a.to_ascii_lowercase()).as_deref() {
                    Some("crouching") => FiringPose::Crouching,
                    Some("lying") => FiringPose::Prone,
                    _ => FiringPose::Standing,
                }
            }
            "weapontemplate.setstrength" => {
                if args.first().is_some_and(|a| a.eq_ignore_ascii_case("infantry")) {
                    weapon.infantry_strength = number(1).unwrap_or(0.0);
                }
            }
            _ => {}
        }
    }
    finish(active);
}

#[cfg(test)]
mod tests {
    use game_data::ControlPointDesc;

    use super::*;

    fn control_point(id: &str, position: [f32; 3]) -> ControlPointDesc {
        ControlPointDesc {
            id: id.into(),
            name: id.into(),
            position,
            initial_team: 1,
            radius: 10.0,
            uncapturable: false,
            area_value: [0.0; 2],
            time_to_get_control: 10.0,
            time_to_lose_control: 10.0,
            only_takeable_by_team: 0,
            enemy_ticket_loss_when_captured: 0.0,
        }
    }

    #[test]
    fn parses_strategic_areas() {
        let text = "\
rem *** Create strategic areas ***
aiStrategicArea.createFromControlPoint CPNAME_gasstation 45 1
aiStrategicArea.layer 5
aiStrategicArea.create 32_CP_Hotel -205.957/0.949 -195.957/10.949 155.8 50
aiStrategicArea.create LEFTFLANK -253.42/-159.966 -243.42/-149.966 163.399 50

aiStrategicArea.setActive CPNAME_gasstation
AIStrategicArea.addNeighbour 32_CP_Hotel
aiStrategicArea.addObjectTypeFlag ControlPoint
AIStrategicArea.setOrderPosition Infantry -158.22/161.298/-248.308
AIStrategicArea.setOrderPosition Vehicle -160.02/161.298/-247.841

aiStrategicArea.setActive 32_CP_Hotel
AIStrategicArea.addNeighbour CPNAME_gasstation
AIStrategicArea.addNeighbour LEFTFLANK
AIStrategicArea.addNeighbour NOWHERE
aiStrategicArea.addObjectTypeFlag ControlPoint
AIStrategicArea.setOrderPosition Infantry 0/0/0

aiStrategicArea.addWayPoint -196.571/156.718/-69.2323 Infantry CPNAME_gasstation 32_CP_Hotel
aiStrategicArea.addWayPoint -219.118/157.888/-72.7266 Infantry CPNAME_gasstation 32_CP_Hotel
";
        let layout = GameModeDesc {
            mode: "gpm_cq".into(),
            size: 32,
            control_points: vec![
                control_point("45", [-161.0, 161.3, 262.8]),
                control_point("301", [-194.4, 158.2, -11.3]),
            ],
            ..Default::default()
        };
        let parsed = parse_strategic_areas(text, &layout);
        assert_eq!(parsed.areas.len(), 3);
        let [gas, hotel, flank] = &parsed.areas[..] else { unreachable!() };
        assert_eq!(gas.control_point.as_deref(), Some("45"));
        assert_eq!(gas.infantry_position, Some([-158.22, 161.298, 248.308]));
        assert_eq!(gas.neighbours, ["32_CP_Hotel"]);
        // Bounds in BF2 coordinates, matched to the control point near their center.
        assert_eq!(hotel.control_point.as_deref(), Some("301"));
        assert!((hotel.position[2] + 5.949).abs() < 1e-3, "{:?}", hotel.position);
        assert_eq!(hotel.infantry_position, None);
        assert_eq!(hotel.neighbours, ["CPNAME_gasstation", "LEFTFLANK"]);
        assert_eq!(flank.control_point, None);
        assert_eq!(parsed.routes.len(), 1);
        assert_eq!(parsed.routes[0].waypoints.len(), 2);
    }

    #[test]
    fn parses_weapon_templates() {
        let text = "\
rem *** Add Usrif_m16a2 ***
weaponTemplate.create usrif_m16a2
weaponTemplate.minRange 0.0
weaponTemplate.maxRange 72.0
weaponTemplate.optimalRangePercentage 35
weaponTemplate.setFiringPose Crouching
weaponTemplate.setStrength Infantry    5.0
weaponTemplate.setStrength LightArmour 1.0
weaponTemplate.create ushgr_m67_AI
weaponTemplate.isThrown 1
weaponTemplate.minRange 15.0
weaponTemplate.maxRange 55.0
weaponTemplate.optimalRangePercentage 85
weaponTemplate.setExplosionRadius 10.0
";
        let mut templates = HashMap::new();
        parse_weapon_templates(text, &mut templates);
        let rifle = &templates["usrif_m16a2"];
        assert_eq!(rifle.pose, FiringPose::Crouching);
        assert_eq!(rifle.infantry_strength, 5.0);
        assert!((rifle.optimal_range - 25.2).abs() < 1e-3);
        let grenade = &templates["ushgr_m67_ai"];
        assert!(grenade.thrown);
        assert_eq!(grenade.explosion_radius, 10.0);
        assert!((grenade.optimal_range - 49.0).abs() < 1e-3);
    }
}
