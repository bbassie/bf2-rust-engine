//! The commander's assets to `levels/<name>/commander.ron` (see `game_data::commander`).
//!
//! BF2's game mode layouts spawn the assets with `ObjectSpawner`s like vehicles: the
//! artillery guns (`ars_d30`, `usart_lw155`), the UAV trailer (`aircontroltower*`) and the
//! radar for satellite scans (`mobileradar_*`). They are `PlayerControlObject`s with an
//! `Armor` that turns them into repairable wrecks (`canBeDestroyed 0`), and their `Radio`
//! component says which asset they are: `destroyedMessage artillery_destroyed`,
//! `uav_destroyed`, `satellite_destroyed`. Here they become destroyable objects: their
//! templates get the armor's hit points and destruction effects.
//!
//! Each gun fires `fire.burstSize` shells at `fire.roundsPerMinute`, landing within
//! `deviation.radius` of the target, with its projectile's detonation. The UAV is
//! `UAVControlObject`'s `uav_pred` circling `uavVehicleRadius` meters around the target, the
//! supply crate `supply_crate`'s `SupplyObject` settings.

use std::{collections::HashSet, path::Path};

use anyhow::Result;
use bf2_formats::con::{Interpreter, Template, World};
use game_data::{
    ArmorDesc, AssetKind, CommanderDesc, EffectPlacement, ExplosionDesc, ObjectDesc,
};

use crate::{coords, meshes::MeshConverter, sounds::SoundConverter};

/// Which asset a template is, by its `Radio.destroyedMessage`.
fn asset_kind(template: &Template) -> Option<AssetKind> {
    match template.get_str("radio.destroyedmessage")?.to_ascii_lowercase().as_str() {
        "artillery_destroyed" => Some(AssetKind::Artillery),
        "uav_destroyed" => Some(AssetKind::Uav),
        "satellite_destroyed" => Some(AssetKind::Radar),
        _ => None,
    }
}

/// Loads what the level's object spawners spawn; returns the asset templates (lowercase).
pub fn load_assets(interp: &mut Interpreter) -> HashSet<String> {
    let spawned: HashSet<String> = interp
        .world
        .templates
        .values()
        .filter(|t| t.ty.eq_ignore_ascii_case("objectspawner"))
        .flat_map(|t| t.get_all("setobjecttemplate").filter_map(|args| args.get(1)))
        .map(|name| name.to_ascii_lowercase())
        .collect();
    spawned
        .into_iter()
        .filter(|name| {
            interp.ensure_template(name);
            interp.world.template(name).and_then(asset_kind).is_some()
        })
        .collect()
}

/// The UAV's template (`UAVControlObject.uavVehicleTemplate`), to be imported as a vehicle.
pub fn uav_vehicle(interp: &mut Interpreter) -> Option<String> {
    interp.ensure_template("uavcontrolobject");
    let vehicle = interp
        .world
        .template("uavcontrolobject")?
        .get_str("uavvehicletemplate")?
        .to_ascii_lowercase();
    interp.ensure_template(&vehicle);
    interp.world.template(&vehicle).is_some().then_some(vehicle)
}

/// A template's first descendant (or itself) that `matches`.
fn find<'a>(world: &'a World, name: &str, matches: &dyn Fn(&Template) -> bool, depth: u32) -> Option<&'a Template> {
    let template = world.template(name)?;
    if matches(template) {
        return Some(template);
    }
    if depth > 8 {
        return None;
    }
    template.children.iter().find_map(|c| find(world, &c.template, matches, depth + 1))
}

/// Gives the asset templates' objects their armor and writes `commander.ron`. Returns the
/// number of asset templates.
pub fn import(
    interp: &mut Interpreter,
    converter: &MeshConverter,
    templates: &HashSet<String>,
    level_dir: &Path,
    out: &Path,
) -> Result<usize> {
    let mut desc = CommanderDesc::default();
    for name in templates {
        let Some(template) = interp.world.template(name).cloned() else { continue };
        let Some(kind) = asset_kind(&template) else { continue };
        desc.assets.insert(name.clone(), kind);
        add_armor(&template, out)?;
        if kind == AssetKind::Artillery {
            artillery(interp, converter, name, out, &mut desc);
        }
    }

    // The UAV and its model.
    interp.ensure_template("uavcontrolobject");
    if let Some(control) = interp.world.template("uavcontrolobject") {
        let uav = &mut desc.uav;
        uav.radius = control.get_f32("uavvehicleradius").unwrap_or(uav.radius);
        uav.height = control.get_f32("uavvehicleflightheight").unwrap_or(uav.height);
        uav.speed = control.get_f32("uavvehiclespeed").unwrap_or(uav.speed);
        if let Some(vehicle) = control.get_str("uavvehicletemplate").map(str::to_ascii_lowercase) {
            interp.ensure_template(&vehicle);
            uav.mesh = mesh(&interp.world, converter, &vehicle);
            uav.vehicle = out.join("vehicles").join(format!("{vehicle}.ron")).exists().then_some(vehicle);
        }
    }
    // The supply crate.
    interp.ensure_template("supply_crate");
    if let Some(crate_template) = interp.world.template("supply_crate") {
        let supply = &mut desc.supply;
        supply.radius = crate_template.get_f32("radius").unwrap_or(supply.radius);
        supply.heal = crate_template.get_f32("healspeed").unwrap_or(supply.heal);
        supply.ammo = crate_template.get_f32("refillammospeed").unwrap_or(supply.ammo);
        supply.storage = crate_template.get_f32("sharedstoragesize").unwrap_or(supply.storage);
        supply.mesh = mesh(&interp.world, converter, "supply_crate");
    }
    let settings = |name: &str| interp.world.setting(name).and_then(|v| v.parse::<f32>().ok());
    desc.supply.drop_height = settings("gamelogic.supplydropheight").unwrap_or(desc.supply.drop_height);
    desc.supply.lifetime = settings("gamelogic.supplydropnumsecstolive").unwrap_or(desc.supply.lifetime);

    let count = desc.assets.len();
    game_data::write_ron(level_dir.join("commander.ron"), &desc)?;
    Ok(count)
}

/// The template's visible mesh as a `.glb` (first geom, best LOD).
fn mesh(world: &World, converter: &MeshConverter, name: &str) -> Option<String> {
    let path = world.template(name)?.geometry.as_deref().and_then(|g| world.geometry(g))?.mesh_path()?;
    converter
        .convert_mesh(&path)
        .map_err(|e| log::debug!("{name} mesh: {e:#}"))
        .ok()
}

/// The asset's armor on its object (written by the level import): hit points, material, the
/// blast and the effects when it goes.
fn add_armor(template: &Template, out: &Path) -> Result<()> {
    let path = out.join("templates").join(format!("{}.ron", template.name.to_ascii_lowercase()));
    if !path.exists() {
        return Ok(());
    }
    let mut object: ObjectDesc = game_data::read_ron(&path)?;
    let get = |property: &str| template.get_f32(&format!("armor.{property}"));
    let mut armor = ArmorDesc {
        hit_points: get("maxhitpoints").or(get("hitpoints")).unwrap_or(1000.0),
        material: get("defaultmaterial").unwrap_or(98.0) as u32,
        explosion: match (get("explosiondamage"), get("explosionradius")) {
            (Some(damage), Some(radius)) if damage > 0.0 && radius > 0.0 => Some(ExplosionDesc {
                damage,
                radius,
                material: get("explosionmaterial").unwrap_or(0.0) as u32,
            }),
            _ => None,
        },
        effect: Default::default(),
    };
    for args in template.get_all("armor.addarmoreffect") {
        let (Some(hit_points), Some(effect)) = (args.first().and_then(|a| a.parse::<f32>().ok()), args.get(1)) else {
            continue;
        };
        let effect = effect.to_ascii_lowercase();
        if hit_points > 0.0 || !out.join("effects").join(format!("{effect}.ron")).exists() {
            continue;
        }
        let vec = |i: usize| args.get(i).and_then(|s| bf2_formats::con::parse_vec3(s)).unwrap_or_default();
        armor.effect.effects.push(EffectPlacement {
            name: effect,
            position: coords::position(vec(2)),
            rotation: coords::rotation_ypr(vec(3)).to_array(),
        });
    }
    object.armor = Some(armor);
    game_data::write_ron(&path, &object)?;
    Ok(())
}

/// What an artillery gun fires: its barrel's burst and its projectile's detonation.
fn artillery(interp: &mut Interpreter, converter: &MeshConverter, gun: &str, out: &Path, desc: &mut CommanderDesc) {
    let world = &interp.world;
    let Some(barrel) = find(world, gun, &|t| t.get("fire.burstsize").is_some(), 0).cloned() else {
        return;
    };
    let artillery = &mut desc.artillery;
    artillery.shells = barrel.get_f32("fire.burstsize").map_or(artillery.shells, |s| s.max(1.0) as u32);
    if let Some(rpm) = barrel.get_f32("fire.roundsperminute").filter(|r| *r > 0.0) {
        artillery.interval = 60.0 / rpm;
    }
    artillery.spread = barrel.get_f32("deviation.radius").unwrap_or(artillery.spread);
    let Some(projectile) = barrel.get_str("projectiletemplate").map(str::to_ascii_lowercase) else {
        return;
    };
    interp.ensure_template(&projectile);
    let Some(projectile) = interp.world.template(&projectile).cloned() else {
        return;
    };
    let artillery = &mut desc.artillery;
    let get = |property: &str| projectile.get_f32(&format!("detonation.{property}"));
    artillery.damage = get("explosiondamage").unwrap_or(artillery.damage);
    artillery.radius = get("explosionradius").unwrap_or(artillery.radius);
    artillery.material = get("explosionmaterial").map_or(artillery.material, |m| m as u32);
    artillery.effect = projectile
        .get_str("detonation.endeffecttemplate")
        .map(str::to_ascii_lowercase)
        .filter(|e| out.join("effects").join(format!("{e}.ron")).exists());
    let sounds = SoundConverter::new(converter.vfs, out);
    artillery.incoming = projectile
        .children
        .iter()
        .find(|c| c.template.to_ascii_lowercase().ends_with("_projectile_looping"))
        .and_then(|c| sounds.template(&interp.world, &c.template))
        .map(|mut s| {
            s.looping = false;
            s
        });
}
