//! Kits and handheld weapons: `kits/<name>.ron`, `weapons/<name>.ron`, weapon models and
//! sounds.

use std::{collections::BTreeSet, path::Path};

use anyhow::Result;
use bf2_formats::{
    con::{Interpreter, Template},
    localization::Localization,
};
use game_data::{
    DetonatorDesc, DeviationDesc, FireDesc, FireKind, FireMode, Guidance, Impact, KitDesc, LockDesc,
    OverheatDesc, ProjectileDesc, RecoilDesc, ReplenishDesc, ReplenishKind, RopeDesc, RopeKind, SmokeDesc,
    SoundDesc, TriggerBy, TriggerDesc, WeaponDesc, WeaponSounds, ZoomDesc,
};

use crate::{meshes::MeshConverter, sounds::SoundConverter};

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
            ability_restore: kit.get_f32("abilityrestorerate").unwrap_or(0.0).max(0.0),
        };
        game_data::write_ron(out.join("kits").join(format!("{}.ron", desc.name)), &desc)?;
        kit_count += 1;
    }

    let mut weapon_count = 0;
    let sounds = SoundConverter::new(converter.vfs, out);
    for name in &weapons {
        let Some(template) = interp.world.template(name).cloned() else {
            continue;
        };
        let mut desc = weapon_desc(interp, converter, &sounds, &template, out);
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

pub(crate) fn weapon_desc(
    interp: &mut Interpreter,
    converter: &MeshConverter,
    sound_converter: &SoundConverter,
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

    let projectile_template = t
        .get_str("projectiletemplate")
        .map(str::to_string)
        .and_then(|p| {
            interp.ensure_template(&p);
            interp.world.template(&p).cloned()
        });

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
    let convert = |geom: usize, lod: usize, suffix: &str| {
        mesh_path.as_deref().and_then(|p| {
            converter
                .convert_mesh_lod(p, geom, lod, suffix)
                .map_err(|e| log::debug!("weapon {name} geom {geom} LOD {lod}: {e:#}"))
                .ok()
        })
    };
    let mesh_1p = convert(0, 0, "_1p");
    let mesh_3p = convert(1, 0, "_3p");
    // While zoomed the game draws another LOD of the first-person geom: scoped weapons
    // model the view through the scope there, the others their sights up close.
    let zoom_lod = f("zoom.zoomlod", 0.0) as usize;
    let zoom = ZoomDesc {
        mesh_1p: (zoom_lod > 0).then(|| convert(0, zoom_lod, "_1p_zoom")).flatten(),
        delay: f("zoom.zoomdelay", 0.0),
        fov_delay: f("zoom.changefovdelay", 0.0),
        out_after_fire: f("zoom.zoomoutafterfire", 0.0) != 0.0,
    };
    let mut projectile = projectile_desc(interp, converter, t, projectile_template.as_ref(), mesh_3p.as_deref());
    if let Some(rope) = rope_desc(interp, t) {
        rope_projectile(&mut projectile, rope);
    }

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
    let sound = |suffix: &str| -> Option<SoundDesc> {
        let child = t
            .children
            .iter()
            .find(|c| c.template.to_ascii_lowercase().ends_with(suffix))?;
        let mut desc = sound_converter.template(&interp.world, &child.template)?;
        // BF2 loops fire sounds while the trigger is held; here every shot plays its own.
        desc.looping = false;
        Some(desc)
    };
    let fire_3p = sound("_fire3p");
    let sounds = WeaponSounds {
        fire_1p: sound("_fire1p").or_else(|| sound("_fire1p_outdoor")),
        fire_3p_distant: fire_3p.as_ref().and_then(|near| crate::sounds::distant(near, out)),
        fire_3p,
        reload_1p: sound("_reload1p"),
        reload_3p: sound("_reload3p"),
        deploy_1p: sound("_deploy1p"),
        deploy_3p: sound("_deploy3p"),
        dry_fire: sound("_triggerclick"),
        bolt: sound("_boltclick"),
        switch_fire_mode: sound("_switchfirerate"),
        zoom: sound("_zoom"),
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
        projectiles_per_shot: f("fire.batchsize", 1.0).max(1.0) as u32,
        pellet_spread: f("deviation.subprojectiledev", 0.0),
        shift_delay: if f("animation.useshiftanimation", 0.0) != 0.0 { f("animation.shiftdelay", 0.0) } else { 0.0 },
        reload_amount: f("ammo.reloadamount", 0.0) as u32,
        fire: fire_desc(t),
        worn: f("isnightvision", 0.0) != 0.0 || f("isgasmask", 0.0) != 0.0,
        detonator: detonator_desc(interp, converter, t, out),
        projectile,
        deviation,
        recoil,
        zoom_factors: t
            .get_all("zoom.addzoomfactor")
            .filter_map(|a| a.first()?.parse().ok())
            .collect(),
        zoom,
        sounds,
        replenish: replenish_desc(t, projectile_template.as_ref()),
    }
}

/// Medic and ammo bags, shock paddles and the wrench: `ReplenishingAmmoComp` on the weapon,
/// with what its thrown bags (`ReplenishDetonationComp`) or its projectile
/// (`ResurrectCollisionComp`) do.
fn replenish_desc(t: &Template, projectile: Option<&Template>) -> Option<ReplenishDesc> {
    if !has_component(t, "ReplenishingAmmoComp") {
        return None;
    }
    let f = |method: &str| t.get_f32(method).unwrap_or(0.0).max(0.0);
    let p = |method: &str| projectile.and_then(|p| p.get_f32(method)).unwrap_or(0.0).max(0.0);
    let kind = match t.get_str("ammo.replenishingtype").map(str::to_ascii_lowercase).as_deref() {
        Some("rtammo") => ReplenishKind::Ammo,
        _ => ReplenishKind::Health,
    };
    let bag = projectile.is_some_and(|p| has_component(p, "ReplenishDetonationComp"));
    let reviver = projectile.is_some_and(|p| has_component(p, "ResurrectCollisionComp"));
    Some(ReplenishDesc {
        kind,
        material: f("ammo.abilitymaterial") as u32,
        radius: f("ammo.abilityradius"),
        strength: f("ammo.abilitystrength"),
        while_firing: f("ammo.onlyactivewhilefiring") != 0.0,
        cost: f("ammo.abilitycost"),
        drain: f("ammo.abilitydrain"),
        pickup_radius: if bag { p("detonation.triggerradius") } else { 0.0 },
        pickup_strength: if bag { p("detonation.replenishingstrength") } else { 0.0 },
        revive_health: if reviver { p("collision.restorehp") } else { 0.0 },
    })
}

fn has_component(t: &Template, component: &str) -> bool {
    t.get_all("createcomponent")
        .any(|a| a.first().is_some_and(|c| c.eq_ignore_ascii_case(component)))
}

/// `x/y/z` in BF2's left-handed space to ours.
fn position(t: &Template, method: &str) -> Option<[f32; 3]> {
    let values: Vec<f32> = t.get_str(method)?.split('/').filter_map(|v| v.parse().ok()).collect();
    (values.len() == 3).then(|| [values[0], values[1], -values[2]])
}

/// The fire and target components of a weapon.
fn fire_desc(t: &Template) -> FireDesc {
    let f = |method: &str| t.get_f32(method).unwrap_or(0.0).max(0.0);
    let kind = if has_component(t, "ThrownFireComp") {
        FireKind::Thrown
    } else if has_component(t, "ExplosivesFireComp") {
        FireKind::Explosives
    } else {
        FireKind::Gun
    };
    // TV and laser guided missiles follow the gunner's sight like wire guided ones.
    let guidance = match t.get_str("target.targetsystem").map(str::to_ascii_lowercase).as_deref() {
        Some("tswireguided" | "tstvguided" | "tslaserguided") => Guidance::Wire,
        Some("tsheatseeking") => Guidance::Heat,
        _ => Guidance::None,
    };
    let lock = (guidance == Guidance::Heat).then(|| LockDesc {
        time: t.get_f32("target.lockdelay").unwrap_or(1.0),
        angle: t.get_f32("target.lockangle").unwrap_or(15.0),
        range: t.get_f32("target.maxdistance").unwrap_or(400.0),
    });
    FireDesc {
        kind,
        pull_back: f("fire.pullbacktime"),
        launch_delay: f("fire.firelaunchdelay"),
        launch_delay_soft: f("fire.firelaunchdelaysoft"),
        start_offset: position(t, "fire.projectilestartposition").unwrap_or_default(),
        max_in_world: f("fire.maxprojectilesinworld") as u32,
        guidance,
        lock,
        overheat: (f("heataddwhenfire") > 0.0).then(|| OverheatDesc {
            per_shot: f("heataddwhenfire"),
            cooling: f("cooldownpersec"),
            penalty: f("overheatpenalty"),
        }),
    }
}

/// C4's detonator (`fire.detonatorObject`): its first-person model and animation set.
fn detonator_desc(interp: &mut Interpreter, converter: &MeshConverter, t: &Template, out: &Path) -> Option<DetonatorDesc> {
    let name = t.get_str("fire.detonatorobject")?.to_ascii_lowercase();
    interp.ensure_template(&name);
    let detonator = interp.world.template(&name)?.clone();
    let mesh_1p = detonator
        .geometry
        .as_deref()
        .and_then(|g| interp.world.geometry(g))
        .and_then(|g| g.mesh_path())
        .and_then(|path| {
            converter
                .convert_mesh_lod(&path, 0, 0, "_1p")
                .map_err(|e| log::debug!("detonator {name}: {e:#}"))
                .ok()
        });
    let dir = detonator.source.split(':').next().unwrap_or_default();
    let animations_1p = dir
        .rsplit_once('/')
        .map(|(dir, _)| format!("{dir}/animations/1p.glb"))
        .filter(|path| out.join(path).exists());
    Some(DetonatorDesc { mesh_1p, animations_1p })
}

/// Smoke clouds look about this big once spread (the effect's particles fly out a few
/// meters and are up to 9 m across).
const SMOKE_RADIUS: f32 = 6.0;

fn projectile_desc(
    interp: &mut Interpreter,
    converter: &MeshConverter,
    weapon: &Template,
    projectile: Option<&Template>,
    weapon_mesh_3p: Option<&str>,
) -> ProjectileDesc {
    let velocity = weapon.get_f32("velocity").unwrap_or(300.0);
    let Some(p) = projectile else {
        return ProjectileDesc {
            velocity,
            time_to_live: 1.0,
            ..Default::default()
        };
    };
    let f = |method: &str| p.get_f32(method).unwrap_or(0.0);
    let lowercase = |method: &str| p.get_str(method).map(|s| s.trim_matches('"').to_ascii_lowercase());

    let impact = if has_component(p, "StickyCollisionComp") {
        Impact::Stick { max_angle: p.get_f32("collision.maxstickangle").unwrap_or(180.0) }
    } else if f("collision.bouncing") != 0.0 {
        Impact::Bounce
    } else {
        Impact::Stop
    };
    // Medic and ammo bags have triggers too, but replenish instead of exploding.
    let trigger = lowercase("detonation.triggertype")
        .filter(|_| has_component(p, "DefaultDetonationComp"))
        .and_then(|kind| match kind.as_str() {
            "mtypco" => Some(TriggerBy::Soldiers),
            "mtyvehicle" => Some(TriggerBy::Vehicles),
            _ => None,
        })
        .map(|by| TriggerDesc {
            by,
            radius: f("detonation.triggerradius"),
            angle: f("detonation.triggerangle"),
            min_speed: f("detonation.triggervictimminspeed"),
        });
    let detonation_effect = lowercase("detonation.endeffecttemplate");
    let explosion_damage = f("detonation.explosiondamage");
    let smoke = detonation_effect
        .as_deref()
        .filter(|e| explosion_damage <= 0.0 && (e.contains("smoke") || e.contains("teargas")))
        .map(|effect| SmokeDesc {
            radius: SMOKE_RADIUS,
            duration: effect_length(interp, effect).unwrap_or(10.0),
            gas_damage: gas_damage(interp, effect).unwrap_or(0.0),
        });
    // Rocket exhaust and grenade trails are effect bundles among the children.
    let trail_effect = p.children.iter().find_map(|child| {
        interp.ensure_template(&child.template);
        let child = interp.world.template(&child.template)?;
        child.ty.eq_ignore_ascii_case("EffectBundle").then(|| child.name.to_ascii_lowercase())
    });
    // Thrown weapons fly as their own third-person model; shells and rockets have theirs.
    let weapon_geometry = weapon.geometry.as_deref().map(str::to_ascii_lowercase);
    let mesh = match p.geometry.as_deref() {
        Some(g) if Some(g.to_ascii_lowercase()) == weapon_geometry => weapon_mesh_3p.map(str::to_string),
        Some(g) => {
            // Shared shells are declared in a file of their own, found by their name.
            interp.ensure_template(g);
            interp
                .world
                .geometry(g)
                .and_then(|g| g.mesh_path())
                .and_then(|path| {
                    converter
                        .convert_mesh(&path)
                        .map_err(|e| log::debug!("projectile {}: {e:#}", p.name))
                        .ok()
                })
        }
        None => None,
    };

    ProjectileDesc {
        velocity,
        damage: f("damage"),
        min_damage: f("mindamage"),
        falloff_start: f("disttostartlosedamage"),
        falloff_end: f("disttomindamage"),
        gravity: p.get_f32("gravitymodifier").unwrap_or(1.0),
        time_to_live: p.get_str("timetolive").and_then(crd).map_or(5.0, |v| v[0]),
        material: f("material") as u32,
        explosion_damage,
        explosion_radius: f("detonation.explosionradius"),
        explosion_material: f("detonation.explosionmaterial") as u32,
        explosion_cone: f("detonation.explosionconeangle"),
        detonation_effect,
        trail_effect,
        mesh,
        impact,
        arming_delay: f("detonation.timeuntilcandetonate").max(f("armingdelay")),
        acceleration: f("acceleration"),
        max_speed: f("maxspeed"),
        motor_delay: f("startdelay"),
        turn_rate: f("follow.maxyaw").max(f("follow.maxpitch")),
        guidance_min_distance: f("follow.mindist"),
        trigger,
        smoke,
        rope: None,
    }
}

/// BF2 SF's grappling hook throws a `GrapplingHookRope`; the zipline crossbow's shell leaves
/// a `Zipline` (its `secondaryProjectileTemplate`) where it hits.
fn rope_desc(interp: &mut Interpreter, weapon: &Template) -> Option<RopeDesc> {
    let template = |interp: &mut Interpreter, method: &str| {
        let name = weapon.get_str(method)?.trim_matches('"').to_string();
        interp.ensure_template(&name);
        let template = interp.world.template(&name).cloned();
        Some((name.to_ascii_lowercase(), template))
    };
    if let Some((name, rope)) = template(interp, "projectiletemplate")
        && (name == "grapplinghookrope" || rope.as_ref().is_some_and(|t| t.ty.eq_ignore_ascii_case("GrapplingHookRope")))
    {
        // The template is in the xpack's DummyObjectsXpack.con; its values as defaults.
        let f = |method: &str, default: f32| rope.as_ref().and_then(|t| t.get_f32(method)).unwrap_or(default);
        return Some(RopeDesc {
            kind: RopeKind::Grapple,
            max_length: f("setmaxropelength", 14.0),
            lifetime: f("timeout", 25.0),
            climb_speed: f("climbingspeed", 2.3),
        });
    }
    let (_, zipline) = template(interp, "secondaryprojectiletemplate")?;
    let zipline = zipline.filter(|t| t.ty.eq_ignore_ascii_case("Zipline"))?;
    Some(RopeDesc {
        kind: RopeKind::Zipline,
        max_length: zipline.get_f32("setmaxziplinelength").unwrap_or(75.0),
        lifetime: zipline.get_f32("timeout").unwrap_or(25.0),
        climb_speed: 0.0,
    })
}

/// How the rope's projectile flies: BF2 simulates the hook's rope as links thrown from the
/// hand; here it is a thrown hook that catches on ledges (`minYNormal 0.5`). The zipline
/// shell sticks wherever it hits.
fn rope_projectile(projectile: &mut ProjectileDesc, rope: RopeDesc) {
    match rope.kind {
        RopeKind::Grapple => {
            projectile.velocity = 20.0;
            projectile.gravity = 1.0;
            projectile.impact = Impact::Stick { max_angle: 60.0 };
        }
        RopeKind::Zipline => projectile.impact = Impact::Stick { max_angle: 180.0 },
    }
    // Long enough to land; it becomes the rope where it sticks.
    projectile.time_to_live = 5.0;
    projectile.rope = Some(rope);
}

/// Tear gas: the damage per second of the effect's gas cloud (`gasCloudType TearGas` and
/// `gasCloudDamage` on one of its particle systems).
fn gas_damage(interp: &mut Interpreter, effect: &str) -> Option<f32> {
    interp.ensure_template(effect);
    let children: Vec<String> = interp.world.template(effect)?.children.iter().map(|c| c.template.clone()).collect();
    children.iter().find_map(|child| {
        interp.ensure_template(child);
        let emitter = interp.world.template(child)?;
        emitter
            .get_str("gascloudtype")
            .filter(|kind| kind.eq_ignore_ascii_case("teargas"))
            .map(|_| emitter.get_f32("gasclouddamage").unwrap_or(0.0).max(0.01))
    })
}

/// Seconds until the last particle of an effect bundle is gone: the longest particle life
/// among its emitters (smoke grenades emit everything at once).
fn effect_length(interp: &mut Interpreter, effect: &str) -> Option<f32> {
    interp.ensure_template(effect);
    let children: Vec<String> = interp.world.template(effect)?.children.iter().map(|c| c.template.clone()).collect();
    let mut longest = 0.0f32;
    for child in children {
        interp.ensure_template(&child);
        if let Some(emitter) = interp.world.template(&child) {
            let life = emitter.get_f32("timetolive").unwrap_or(0.0) + emitter.get_f32("randomtimetolive").unwrap_or(0.0);
            longest = longest.max(life);
        }
    }
    (longest > 0.0).then_some(longest)
}
