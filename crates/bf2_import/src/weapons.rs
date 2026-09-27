//! Kits and handheld weapons: `kits/<name>.ron`, `weapons/<name>.ron`, weapon models and
//! fire sounds.

use std::{collections::BTreeSet, path::Path};

use anyhow::Result;
use bf2_formats::{
    con::{Interpreter, Template},
    localization::Localization,
};
use game_data::{
    DeviationDesc, FireMode, KitDesc, ProjectileDesc, RecoilDesc, WeaponDesc, WeaponSounds,
};

use crate::meshes::MeshConverter;

/// Imports the given kits and every weapon they carry. Returns (kits, weapons) written.
pub fn import(
    interp: &mut Interpreter,
    converter: &MeshConverter,
    localization: &Localization,
    kits: &[String],
    out: &Path,
) -> Result<(usize, usize)> {
    let mut weapons = BTreeSet::new();
    let mut kit_count = 0;
    for kit_name in kits {
        interp.ensure_template(kit_name);
        let Some(kit) = interp.world.template(kit_name).cloned() else {
            log::warn!("kit template {kit_name} not found");
            continue;
        };
        let mut items = Vec::new();
        for child in &kit.children {
            let name = child.template.to_ascii_lowercase();
            interp.ensure_template(&name);
            if interp
                .world
                .template(&name)
                .is_some_and(|t| t.ty.eq_ignore_ascii_case("GenericFireArm"))
            {
                items.push(name);
            }
        }
        weapons.extend(items.iter().cloned());
        let desc = KitDesc {
            name: kit_name.to_ascii_lowercase(),
            kind: kit.get_str("kittype").unwrap_or_default().to_string(),
            weapons: items,
        };
        game_data::write_ron(out.join("kits").join(format!("{}.ron", desc.name)), &desc)?;
        kit_count += 1;
    }

    let mut weapon_count = 0;
    for name in &weapons {
        let Some(template) = interp.world.template(name).cloned() else {
            continue;
        };
        let mut desc = weapon_desc(interp, converter, &template, out);
        desc.display_name = localization.resolve(&desc.display_name);
        game_data::write_ron(out.join("weapons").join(format!("{name}.ron")), &desc)?;
        weapon_count += 1;
    }
    Ok((kit_count, weapon_count))
}

/// `CRD_UNIFORM/0.1/0.6/0` → (0.1, 0.6); `CRD_NONE/6/0/0` → (6, 6); `6` → (6, 6).
fn crd(value: &str) -> Option<[f32; 2]> {
    let parts: Vec<&str> = value.split('/').collect();
    if parts[0].to_ascii_uppercase().starts_with("CRD_") {
        let a: f32 = parts.get(1)?.parse().ok()?;
        let b: f32 = parts.get(2).and_then(|b| b.parse().ok()).unwrap_or(a);
        return Some(if parts[0].eq_ignore_ascii_case("CRD_NONE") { [a, a] } else { [a.min(b), a.max(b)] });
    }
    let v: f32 = value.parse().ok()?;
    Some([v, v])
}

fn floats<const N: usize>(template: &Template, method: &str) -> Option<[f32; N]> {
    let args = template.get(method)?;
    let values: Vec<f32> = args.iter().filter_map(|a| a.parse().ok()).collect();
    (values.len() >= N).then(|| std::array::from_fn(|i| values[i]))
}

fn weapon_desc(
    interp: &mut Interpreter,
    converter: &MeshConverter,
    t: &Template,
    out: &Path,
) -> WeaponDesc {
    let name = t.name.to_ascii_lowercase();
    let f = |method: &str, default: f32| t.get_f32(method).unwrap_or(default);

    let fire_modes = {
        let modes: Vec<FireMode> = t
            .get_all("fire.addfirerate")
            .filter_map(|a| match a.first().map(String::as_str) {
                Some("0") => Some(FireMode::Single),
                Some("1") => Some(FireMode::Burst),
                Some("2") => Some(FireMode::Auto),
                _ => None,
            })
            .collect();
        if modes.is_empty() { vec![FireMode::Single] } else { modes }
    };

    let projectile = t
        .get_str("projectiletemplate")
        .map(str::to_string)
        .and_then(|p| {
            interp.ensure_template(&p);
            interp.world.template(&p).cloned()
        });
    let projectile = match &projectile {
        Some(p) => ProjectileDesc {
            velocity: f("velocity", 300.0),
            damage: p.get_f32("damage").unwrap_or(0.0),
            min_damage: p.get_f32("mindamage").unwrap_or(0.0),
            falloff_start: p.get_f32("disttostartlosedamage").unwrap_or(0.0),
            falloff_end: p.get_f32("disttomindamage").unwrap_or(0.0),
            gravity: p.get_f32("gravitymodifier").unwrap_or(1.0),
            time_to_live: p.get_str("timetolive").and_then(crd).map_or(5.0, |v| v[0]),
            material: p.get_f32("material").unwrap_or(0.0) as u32,
            explosion_damage: p.get_f32("detonation.explosiondamage").unwrap_or(0.0),
            explosion_radius: p.get_f32("detonation.explosionradius").unwrap_or(0.0),
        },
        None => ProjectileDesc {
            velocity: f("velocity", 300.0),
            time_to_live: 1.0,
            ..Default::default()
        },
    };

    let deviation = DeviationDesc {
        min: f("deviation.mindev", 0.5),
        stand: f("deviation.devmodstand", 1.0),
        crouch: f("deviation.devmodcrouch", 1.0),
        prone: f("deviation.devmodlie", 1.0),
        zoom: f("deviation.devmodzoom", 1.0),
        fire: floats(t, "deviation.setfiredev").unwrap_or([0.0; 3]),
        speed: floats(t, "deviation.setspeeddev").unwrap_or([0.0; 4]),
        misc: floats(t, "deviation.setmiscdev").unwrap_or([0.0; 3]),
    };
    let recoil = RecoilDesc {
        up: t.get_str("recoil.recoilforceup").and_then(crd).unwrap_or([0.0; 2]),
        left_right: t.get_str("recoil.recoilforceleftright").and_then(crd).unwrap_or([0.0; 2]),
        zoom_modifier: f("recoil.zoommodifier", 1.0),
    };

    // Models: geom 0 is first person, geom 1 third person.
    let mesh_path = t
        .geometry
        .as_deref()
        .and_then(|g| interp.world.geometry(g))
        .and_then(|g| g.mesh_path());
    let convert = |geom: usize, suffix: &str| {
        mesh_path.as_deref().and_then(|p| {
            converter
                .convert_mesh_geom(p, geom, suffix)
                .map_err(|e| log::debug!("weapon {name} geom {geom}: {e:#}"))
                .ok()
        })
    };
    let mesh_1p = convert(0, "_1p");
    let mesh_3p = convert(1, "_3p");

    // Third-person animations were exported per weapon folder by the soldier import.
    let dir = t.source.split(':').next().unwrap_or_default();
    let dir = dir.rsplit_once('/').map_or("", |(d, _)| d);
    // Many weapons borrow another weapon's animations: follow the `.baf` paths referenced
    // by their animation system (`animationSystem1P ...AnimationSystem1p.inc`).
    let animation_set = |view: &str| -> Option<String> {
        let method = format!("animationsystem{view}");
        let marker = format!("/animations/{view}/");
        let from_system = t
            .get_str(&method)
            .and_then(|inc| converter.vfs.read_text(inc).ok())
            .and_then(|text| {
                text.split_whitespace()
                    .map(|token| bf2_formats::vfs::normalize(token.trim_matches('"')))
                    .find(|token| token.ends_with(".baf") && token.contains(&marker))
            })
            .and_then(|baf| baf.split_once("/animations/").map(|(d, _)| d.to_string()));
        [from_system, Some(dir.to_string())]
            .into_iter()
            .flatten()
            .map(|d| format!("{d}/animations/{view}.glb"))
            .find(|path| out.join(path).exists())
    };
    let animations_3p = animation_set("3p");
    let animations_1p = animation_set("1p");

    // Sounds are child templates like `S_usrif_m16a2_Fire3P`.
    let sound = |suffix: &str| -> Option<String> {
        let child = t
            .children
            .iter()
            .find(|c| c.template.to_ascii_lowercase().ends_with(suffix))?;
        let file = interp.world.template(&child.template)?.get_str("soundfilename")?;
        converter.file(file)
    };
    let sounds = WeaponSounds {
        fire_1p: sound("_fire1p").or_else(|| sound("_fire1p_outdoor")),
        fire_3p: sound("_fire3p"),
        reload_1p: sound("_reload1p"),
    };

    WeaponDesc {
        name: name.clone(),
        display_name: t
            .get_str("weaponhud.hudname")
            .map(|s| s.trim_matches('"').to_string())
            .unwrap_or_else(|| name.clone()),
        slot: f("itemindex", 0.0) as u32,
        mesh_1p,
        mesh_3p,
        animations_3p,
        animations_1p,
        // Unset in several rifles; the engine default is presumably a typical 600.
        rounds_per_minute: f("fire.roundsperminute", 600.0),
        fire_modes,
        magazine_size: f("ammo.magsize", 30.0) as u32,
        magazines: f("ammo.nrofmags", 6.0) as u32,
        reload_time: f("ammo.reloadtime", 3.0),
        deploy_time: f("delaytouse", 1.0),
        projectiles_per_shot: 1,
        projectile,
        deviation,
        recoil,
        zoom_factors: t
            .get_all("zoom.addzoomfactor")
            .filter_map(|a| a.first()?.parse().ok())
            .collect(),
        sounds,
    }
}
