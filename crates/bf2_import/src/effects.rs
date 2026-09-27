//! Particle effects: BF2 effect bundles to `effects/<name>.ron`, and the tables that pick
//! them: `effects/impacts.ron`, `effects/weapons.ron`, `effects/decals.ron` and
//! `levels/<name>/surfaces.ron`.
//!
//! BF2 effects are `EffectBundle` templates (`objects/effects/**/e_*.con`). Their children
//! are placed like any template's: `SpriteParticleSystem` (camera-facing sprites),
//! `NonScreenAlignedParticleSystem` (sprites lying flat), `MeshParticleSystem` (tiny meshes:
//! sparks, shards; drawn as sprites here), `Emitter`s throwing one `Particle` (a mesh:
//! muzzle flash stars, planks), `LightSource` and `Sound`. Sprite graphs such as
//! `sizeGraph a/b/c/d` are cubics `a t³ + b t² + c t + d` over the particle's life (the
//! editor's defaults `0/0/0/1` are constant 1). Sprite textures are packed into
//! `objects/effects/textures/atlas/particleatlas0.dds`, indexed by `particleatlas.tai`.
//!
//! Weapons add their muzzle flash as a child (`e_muzz_*`, placed at the muzzle), projectiles
//! name a `detonation.endEffectTemplate`, destroyable objects list theirs with
//! `armor.addArmorEffect`, and bullet impacts come from the material manager's cells
//! (`MaterialManager.createCell <projectile> <surface>` then `setEffectTemplate 0 <effect>`,
//! and `setDecalTemplate 0 <decal>` for the mark it leaves: a `Decal` template with a
//! texture, size and random variations).

use std::{
    collections::{BTreeMap, HashMap, HashSet},
    path::Path,
    sync::Mutex,
};

use anyhow::{Context, Result};
use bf2_formats::{
    Vfs,
    collision::{ColType, CollisionMesh},
    con::{Interpreter, Template, World, parse_vec3},
    mesh::{MeshKind, Usage, VisMesh},
    vfs::normalize,
};
use game_data::{
    Blend, Curve, DebrisPiece, DecalDesc, DecalTable, EffectDesc, EffectSound, EmitShape,
    EmitterDesc, Facing, FlashMeshDesc, Frames, ImpactTable, LightDesc, ObjectDesc, Spread,
    SurfaceMap, Views, WeaponEffectTable, WeaponEffects,
};
use glam::{Affine3A, Vec3};

use crate::{coords, destruction, meshes::MeshConverter, sounds::SoundConverter};

/// Folders under `objects/effects/` whose bundles are all imported.
const EFFECT_FOLDERS: [&str; 3] = ["objects/effects/impacts/", "objects/effects/weapons/", "objects/effects/misc/"];
const ATLAS_INDEX: &str = "objects/effects/textures/atlas/particleatlas.tai";
/// Lights of effects that live as long as their owner (muzzle flashes) last this long.
const DEFAULT_LIGHT_LIFE: f32 = 0.06;

/// Effects already written by this run (`level --all` imports many levels).
static WRITTEN: Mutex<Option<HashSet<String>>> = Mutex::new(None);

/// Converts every effect of `objects/effects/{impacts,weapons,misc}` plus those the level's
/// weapons and destroyable statics use, the impact table and the weapons' effects.
pub fn import(
    interp: &mut Interpreter,
    converter: &MeshConverter,
    kits: &[String],
    static_templates: &HashSet<String>,
    out: &Path,
) {
    let vfs = converter.vfs;
    let mut names: Vec<String> = vfs
        .list("objects/effects/")
        .filter(|p| EFFECT_FOLDERS.iter().any(|f| p.starts_with(f)) && p.ends_with(".con"))
        .filter_map(|p| p.rsplit('/').next()?.strip_suffix(".con"))
        .filter(|n| n.starts_with("e_"))
        .map(str::to_string)
        .collect();

    let weapons = weapon_effects(interp, kits);
    for effects in weapons.weapons.values() {
        names.extend(effects.muzzle.iter().chain(&effects.detonation).cloned());
    }
    let (impacts, decal_cells) = material_cells(vfs);
    if impacts.effects.is_empty() {
        log::warn!("no impact effects in the material manager");
    }
    names.extend(impacts.effects.values().cloned());
    for template in static_templates.iter().filter_map(|n| interp.world.template(n)) {
        for args in template.get_all("armor.addarmoreffect") {
            names.extend(args.get(1).map(|n| n.to_ascii_lowercase()));
        }
    }

    names.sort();
    names.dedup();
    let atlas = Atlas::load(vfs);
    let sounds = SoundConverter::new(vfs, &converter.out);
    let mut written = WRITTEN.lock().unwrap();
    let written = written.get_or_insert_with(HashSet::new);
    let (mut count, mut failed) = (0, 0);
    for name in &names {
        if written.contains(name) {
            continue;
        }
        load_effect(interp, name);
        match convert_effect(&interp.world, name, converter, &sounds, &atlas) {
            Some(effect) => match game_data::write_ron(out.join("effects").join(format!("{name}.ron")), &effect) {
                Ok(()) => count += 1,
                Err(err) => log::warn!("{name}: {err}"),
            },
            None => failed += 1,
        }
        written.insert(name.clone());
    }
    if let Err(err) = game_data::write_ron(out.join("effects/impacts.ron"), &impacts) {
        log::warn!("impacts.ron: {err}");
    }
    if let Err(err) = merge_weapon_effects(out, weapons) {
        log::warn!("weapons.ron: {err:#}");
    }
    let decals = decal_table(interp, converter, decal_cells);
    if let Err(err) = game_data::write_ron(out.join("effects/decals.ron"), &decals) {
        log::warn!("decals.ron: {err}");
    }
    log::info!(
        "effects: {count} converted, {failed} empty or missing, {} impact cells, {} decals",
        impacts.effects.len(),
        decals.decals.len()
    );
}

/// Loads an effect bundle and every template it names.
fn load_effect(interp: &mut Interpreter, name: &str) {
    let mut pending = vec![name.to_string()];
    let mut seen = HashSet::new();
    while let Some(name) = pending.pop() {
        if !seen.insert(name.clone()) {
            continue;
        }
        interp.ensure_template(&name);
        let Some(template) = interp.world.template(&name) else {
            continue;
        };
        pending.extend(template.children.iter().map(|c| c.template.to_ascii_lowercase()));
        pending.extend(template.get_str("template").map(str::to_ascii_lowercase));
        // Mesh particles name geometry templates, which live in files of the same name.
        let geometry = template.geometry.clone();
        if let Some(geometry) = geometry
            && interp.world.geometry(&geometry).is_none()
        {
            interp.ensure_template(&geometry);
        }
    }
}

/// Muzzle flash (a child effect bundle `e_muzz*`) and detonation of every kit weapon.
fn weapon_effects(interp: &mut Interpreter, kits: &[String]) -> WeaponEffectTable {
    let mut table = WeaponEffectTable::default();
    for kit in kits {
        interp.ensure_template(kit);
        let Some(kit) = interp.world.template(kit).cloned() else {
            continue;
        };
        for child in &kit.children {
            let name = child.template.to_ascii_lowercase();
            interp.ensure_template(&name);
            if let Some(projectile) = interp.world.template(&name).and_then(|w| w.get_str("projectiletemplate")) {
                let projectile = projectile.to_string();
                interp.ensure_template(&projectile);
            }
            if let Some(effects) = interp.world.template(&name).and_then(|w| weapon_entry(&interp.world, w)) {
                table.weapons.insert(name, effects);
            }
        }
    }
    table
}

/// Adds the guns of the vehicles loaded by now (every `GenericFireArm` template) to
/// `effects/weapons.ron`. Their muzzle flashes are among the imported effects already.
pub fn import_vehicle_weapons(world: &World, out: &Path) {
    let mut table = WeaponEffectTable::default();
    for (name, template) in &world.templates {
        if let Some(effects) = weapon_entry(world, template) {
            table.weapons.insert(name.clone(), effects);
        }
    }
    if let Err(err) = merge_weapon_effects(out, table) {
        log::warn!("weapons.ron: {err:#}");
    }
}

/// The muzzle flash (a child effect bundle `e_muzz*`) and detonation of a weapon template.
fn weapon_entry(world: &World, weapon: &Template) -> Option<WeaponEffects> {
    if !weapon.ty.eq_ignore_ascii_case("genericfirearm") {
        return None;
    }
    let mut effects = WeaponEffects::default();
    if let Some(muzzle) = weapon
        .children
        .iter()
        .find(|c| c.template.to_ascii_lowercase().starts_with("e_muzz"))
    {
        effects.muzzle = Some(muzzle.template.to_ascii_lowercase());
        effects.muzzle_offset = coords::position(muzzle.position.unwrap_or_default());
    }
    effects.detonation = weapon
        .get_str("projectiletemplate")
        .and_then(|p| world.template(p))
        .and_then(|p| p.get_str("detonation.endeffecttemplate"))
        .map(str::to_ascii_lowercase);
    Some(effects)
}

fn merge_weapon_effects(out: &Path, table: WeaponEffectTable) -> Result<()> {
    let path = out.join("effects/weapons.ron");
    let mut merged: WeaponEffectTable = game_data::read_ron(&path).unwrap_or_default();
    merged.weapons.extend(table.weapons);
    game_data::write_ron(&path, &merged)?;
    Ok(())
}

/// The first effect and the first decal of every material manager cell.
fn material_cells(vfs: &Vfs) -> (ImpactTable, BTreeMap<(u32, u32), String>) {
    let mut interp = Interpreter::new(vfs);
    interp.run("common/material/materialmanagersettings.con", &[]);
    let mut table = ImpactTable::default();
    let mut decals = BTreeMap::new();
    let mut cell = None;
    for command in &interp.world.commands {
        let arg = |i: usize| command.args.get(i).map(|a| a.trim_matches('"'));
        match command.name.as_str() {
            "materialmanager.createcell" => {
                cell = arg(0)
                    .and_then(|a| a.parse::<u32>().ok())
                    .zip(arg(1).and_then(|b| b.parse::<u32>().ok()));
            }
            "materialmanager.seteffecttemplate" if arg(0) == Some("0") => {
                if let (Some(cell), Some(effect)) = (cell, arg(1)) {
                    table.effects.insert(cell, effect.to_ascii_lowercase());
                }
            }
            "materialmanager.setdecaltemplate" if arg(0) == Some("0") => {
                if let (Some(cell), Some(decal)) = (cell, arg(1)) {
                    decals.insert(cell, decal.to_ascii_lowercase());
                }
            }
            _ => {}
        }
    }
    (table, decals)
}

/// The decals the cells name. BF2's decal `size` is taken as the radius.
fn decal_table(interp: &mut Interpreter, converter: &MeshConverter, cells: BTreeMap<(u32, u32), String>) -> DecalTable {
    let mut table = DecalTable::default();
    let mut names: Vec<&String> = cells.values().collect();
    names.sort();
    names.dedup();
    for name in names {
        interp.ensure_template(name);
        let Some(t) = interp.world.template(name).filter(|t| t.ty.eq_ignore_ascii_case("decal")) else {
            continue;
        };
        let Some(texture) = t.get_str("decaltexturename").and_then(|n| converter.texture(n)) else {
            continue;
        };
        let (size, random_size) = (f(t, "size", 0.05), f(t, "randomsize", 0.0).abs());
        let count = f(t, "animationframecount", 0.0) as u32;
        let columns = f(t, "animationframecountx", 0.0) as u32;
        let frames = (count > 1 && columns > 0).then(|| {
            let rows = count.div_ceil(columns);
            let size = [f(t, "setanimationframewidthrelative", 0.0), f(t, "setanimationframeheightrelative", 0.0)];
            Frames {
                count,
                columns,
                frame_size: [
                    if size[0] > 0.0 { size[0] } else { 1.0 / columns as f32 },
                    if size[1] > 0.0 { size[1] } else { 1.0 / rows as f32 },
                ],
                fps: 0.0,
                once: false,
                random_start: true,
            }
        });
        let color = t.get_str("color").and_then(parse_vec3).unwrap_or([1.0; 3]);
        table.decals.insert(
            name.clone(),
            DecalDesc {
                texture,
                frames,
                size: [2.0 * (size - random_size).max(0.005), 2.0 * (size + random_size)],
                rotation: f(t, "randomrotation", 0.0).abs(),
                color,
            },
        );
    }
    table.cells = cells.into_iter().filter(|(_, name)| table.decals.contains_key(name)).collect();
    table
}

/// Sprites packed into shared textures: `<texture> <atlas>, <index>, <u>, <v>, <w>, <h>`.
struct Atlas(HashMap<String, (String, [f32; 4])>);

impl Atlas {
    fn load(vfs: &Vfs) -> Self {
        let text = vfs.read_text(ATLAS_INDEX).unwrap_or_default();
        let entries = text
            .lines()
            .filter(|l| !l.trim_start().starts_with('#'))
            .filter_map(|line| {
                let (texture, rest) = line.trim().split_once(char::is_whitespace)?;
                let fields: Vec<&str> = rest.split(',').map(str::trim).collect();
                let number = |i: usize| fields.get(i).and_then(|f| f.parse::<f32>().ok());
                let rect = [number(2)?, number(3)?, number(4)?, number(5)?];
                Some((texture_key(texture), (normalize(fields[0]), rect)))
            })
            .collect();
        Self(entries)
    }

    /// The texture to use for `name` (as a `textureName`) and the sprite's part of it.
    fn texture(&self, converter: &MeshConverter, name: &str) -> Option<(String, [f32; 4])> {
        if let Some((atlas, rect)) = self.0.get(&texture_key(name))
            && let Some(path) = converter.texture(atlas)
        {
            return Some((path, *rect));
        }
        Some((converter.texture(name)?, [0.0, 0.0, 1.0, 1.0]))
    }
}

fn texture_key(name: &str) -> String {
    let key = normalize(name);
    key.strip_suffix(".dds").map_or(key.clone(), str::to_string)
}

/// Converts one effect bundle; `None` if it doesn't exist or shows nothing.
fn convert_effect(
    world: &World,
    name: &str,
    converter: &MeshConverter,
    sounds: &SoundConverter,
    atlas: &Atlas,
) -> Option<EffectDesc> {
    let bundle = world.template(name)?;
    let mut effect = EffectDesc {
        name: name.to_string(),
        ..Default::default()
    };
    let life = bundle
        .get_str("timetolive")
        .map(|s| parse_spread(s).max)
        .filter(|l| *l > 0.0)
        .unwrap_or(DEFAULT_LIGHT_LIFE);
    let context = EffectBuilder {
        world,
        converter,
        sounds,
        atlas,
        light_life: life,
        muzzle: name.starts_with("e_muzz"),
    };
    context.collect(bundle, Affine3A::IDENTITY, &mut effect, 0);
    let empty = effect.emitters.is_empty()
        && effect.lights.is_empty()
        && effect.meshes.is_empty()
        && effect.debris.is_empty()
        && effect.sounds.is_empty();
    (!empty).then_some(effect)
}

struct EffectBuilder<'a> {
    world: &'a World,
    converter: &'a MeshConverter<'a>,
    sounds: &'a SoundConverter<'a>,
    atlas: &'a Atlas,
    light_life: f32,
    /// Weapons play their own fire sounds.
    muzzle: bool,
}

impl EffectBuilder<'_> {
    fn collect(&self, template: &Template, transform: Affine3A, effect: &mut EffectDesc, depth: u32) {
        if depth > 8 {
            return;
        }
        for child in &template.children {
            let Some(child_template) = self.world.template(&child.template) else {
                continue;
            };
            let local = Affine3A::from_rotation_translation(
                coords::rotation_ypr(child.rotation.unwrap_or_default()),
                Vec3::from_array(coords::position(child.position.unwrap_or_default())),
            );
            let transform = transform * local;
            match child_template.ty.to_ascii_lowercase().as_str() {
                "effectbundle" => self.collect(child_template, transform, effect, depth + 1),
                "spriteparticlesystem" => effect.emitters.extend(self.sprites(child_template, transform, Facing::Camera)),
                "nonscreenalignedparticlesystem" => {
                    effect.emitters.extend(self.sprites(child_template, transform, Facing::Horizontal))
                }
                "meshparticlesystem" => effect.emitters.extend(self.mesh_sprites(child_template, transform)),
                "emitter" => self.mesh_emitter(child_template, transform, effect),
                "lightsource" => effect.lights.extend(self.light(child_template, transform)),
                "sound" if !self.muzzle => effect.sounds.extend(self.sound(child_template)),
                _ => {}
            }
        }
    }

    /// A sprite particle system. `facing` is overridden by `alignRotationToSpeed`.
    fn sprites(&self, t: &Template, transform: Affine3A, facing: Facing) -> Option<EmitterDesc> {
        let (texture, uv_rect) = self.atlas.texture(self.converter, t.get_str("texturename")?)?;
        let mut emitter = self.emitter_base(t, transform, texture, uv_rect)?;
        emitter.facing = if flag(t, "alignrotationtospeed") { Facing::Velocity } else { facing };
        if emitter.facing == Facing::Velocity {
            emitter.stretch = 0.02;
        }
        let count = f(t, "animationframecount", 0.0) as u32;
        let columns = f(t, "animationframecountx", 0.0) as u32;
        if count > 1 && columns > 0 {
            let rows = count.div_ceil(columns);
            let size = [
                f(t, "setanimationframewidthrelative", 0.0),
                f(t, "setanimationframeheightrelative", 0.0),
            ];
            emitter.frames = Some(Frames {
                count,
                columns,
                frame_size: [
                    if size[0] > 0.0 { size[0] } else { 1.0 / columns as f32 },
                    if size[1] > 0.0 { size[1] } else { 1.0 / rows as f32 },
                ],
                fps: if flag(t, "animationenable") { f(t, "animationspeed", 0.0) } else { 0.0 },
                once: flag(t, "animationplayonce"),
                random_start: flag(t, "animationrandomizedstartframe"),
            });
        }
        Some(emitter)
    }

    /// Mesh particles (sparks, rock and wood shards) become sprites of about their size.
    fn mesh_sprites(&self, t: &Template, transform: Affine3A) -> Option<EmitterDesc> {
        let geometry = t.geometry.as_deref()?.to_ascii_lowercase();
        let mesh = self.world.geometry(&geometry).and_then(|g| mesh_file(self.converter.vfs, g));
        let radius = mesh.and_then(|m| self.converter.mesh_radius(&m)).unwrap_or(0.02);
        let (texture, frames, blend, facing) = if geometry.contains("spark") {
            ("objects/effects/textures/streaks/streak", None, Blend::Additive, Facing::Velocity)
        } else if geometry.contains("shockwave") || geometry.contains("waterfall") || geometry.contains("frog") {
            return None;
        } else {
            let frames = Frames {
                count: 8,
                columns: 4,
                frame_size: [0.25, 0.25],
                fps: 0.0,
                once: false,
                random_start: true,
            };
            ("objects/effects/textures/animated/anim_debris", Some(frames), Blend::Alpha, Facing::Camera)
        };
        let (texture, uv_rect) = self.atlas.texture(self.converter, texture)?;
        let mut emitter = self.emitter_base(t, transform, texture, uv_rect)?;
        emitter.frames = frames;
        emitter.blend = if t.get_str("blendmode").is_some_and(|b| b.eq_ignore_ascii_case("additive")) {
            Blend::Additive
        } else {
            blend
        };
        emitter.facing = facing;
        if facing == Facing::Velocity {
            emitter.stretch = 0.02 * f(t, "directionalscale", 1.0).max(0.25);
        }
        // The size is a scale of the mesh.
        let diameter = radius * 2.0;
        emitter.size = emitter.size.map(|s| s * diameter);
        Some(emitter)
    }

    fn emitter_base(&self, t: &Template, transform: Affine3A, texture: String, uv_rect: [f32; 4]) -> Option<EmitterDesc> {
        let rate = f(t, "emitfrequency", 0.0);
        if rate <= 0.0 {
            return None;
        }
        let life = f(t, "timetolive", 1.0);
        let random_life = f(t, "randomtimetolive", 0.0).abs();
        let speed = f(t, "emitspeed", 0.0);
        let random_speed = f(t, "randomspeed", 0.0).abs();
        let size = f(t, "particlemaxsize", 1.0);
        let random_size = f(t, "randomsize", 0.0).abs();
        let spin = f(t, "rotationspeed", 0.0);
        let random_spin = f(t, "randomrotationspeed", 0.0).abs();
        let direction = Vec3::from_array(coords::direction(vec3(t, "emitdirection")));
        let direction = transform.transform_vector3(direction).normalize_or_zero();
        let extent = vec3(t, "emitradius").map(f32::abs);
        let color = |name: &str| {
            let c = vec3(t, name);
            if c == [0.0; 3] && t.get_str(name).is_none() { [1.0; 3] } else { c }
        };
        Some(EmitterDesc {
            texture,
            uv_rect,
            frames: None,
            blend: if t.get_str("particletype").is_some_and(|p| p.eq_ignore_ascii_case("additive")) {
                Blend::Additive
            } else {
                Blend::Alpha
            },
            facing: Facing::Camera,
            stretch: 0.0,
            views: views(t),
            position: transform.translation.to_array(),
            shape: match t.get_str("emittertype").map(str::to_ascii_lowercase).as_deref() {
                Some("radialdirection") => EmitShape::Ring,
                Some("radial") => EmitShape::Sphere,
                _ => EmitShape::Box,
            },
            extent,
            delay: f(t, "emitdelay", 0.0).max(0.0),
            duration: f(t, "emittime", 0.0).max(0.0),
            rate,
            burst: f(t, "prewarmtime", 0.0).max(0.0),
            looping: flag(t, "islooping"),
            life: [(life - random_life).max(0.01), (life + random_life).max(0.01)],
            direction: direction.to_array(),
            spread: vec3(t, "randomdirectionangle").map(f32::abs),
            speed: [speed - random_speed, speed + random_speed],
            speed_curve: curve(t, "emitspeedgraph"),
            gravity: f(t, "gravity", 0.0),
            gravity_curve: curve(t, "gravitygraph"),
            drag: f(t, "airresistance", 0.0).max(0.0),
            drag_curve: curve(t, "airresistancegraph"),
            size: [(size - random_size).max(0.0), size + random_size],
            size_curve: curve(t, "sizegraph"),
            opacity_curve: curve(t, "transparencygraph"),
            colors: [color("color1"), color("color2")],
            color_curve: t.get_str("colorblendgraph").and_then(parse_curve).unwrap_or(Curve([0.0; 4])),
            brightness_jitter: f(t, "randomintensity", 0.0).clamp(0.0, 1.0),
            rotation: f(t, "randomrotation", 0.0).abs(),
            spin: [spin - random_spin, spin + random_spin],
            spin_curve: curve(t, "rotationgraph"),
        })
    }

    /// An `Emitter` throws one `Particle`: a flash mesh if its geometry is a
    /// `MeshParticleMesh`, otherwise debris.
    fn mesh_emitter(&self, emitter: &Template, transform: Affine3A, effect: &mut EffectDesc) {
        let Some(particle) = emitter.get_str("template").and_then(|p| self.world.template(p)) else {
            return;
        };
        let Some(geometry) = particle.geometry.as_deref().and_then(|g| self.world.geometry(g)) else {
            return;
        };
        let life = particle.get_str("timetolive").map_or(Spread { min: 2.0, max: 2.0, mirror: false }, parse_spread);
        let Some(path) = mesh_file(self.converter.vfs, geometry) else {
            return;
        };
        if geometry.ty.eq_ignore_ascii_case("meshparticlemesh") {
            let scale = particle.get_str("size").map_or(Spread { min: 1.0, max: 1.0, mirror: false }, parse_spread);
            if let Some(mut mesh) = flash_mesh(self.converter, &path) {
                mesh.position = transform.translation.to_array();
                mesh.scale = [scale.min, scale.max];
                mesh.roll_step = f(emitter, "zrotationsnap", 0.0);
                mesh.life = life.max.max(0.02);
                mesh.views = views(emitter);
                effect.meshes.push(mesh);
            }
            return;
        }
        let Ok(mesh) = destruction::convert_object_mesh(self.converter, &path)
            .map_err(|e| log::debug!("debris mesh {path}: {e:#}"))
        else {
            return;
        };
        let spread = |property: &str| emitter.get_str(property).map_or_else(Spread::default, parse_spread);
        let axes = |kind: &str| {
            [
                spread(&format!("{kind}speedinright")),
                spread(&format!("{kind}speedinup")),
                negate(spread(&format!("{kind}speedindof"))),
            ]
        };
        effect.debris.push(DebrisPiece {
            mesh,
            mesh_index: particle.get_f32("geometrypart").unwrap_or(0.0) as u32,
            position: transform.translation.to_array(),
            velocity: axes("positional"),
            spin: axes("rotational"),
            life,
        });
    }

    fn light(&self, t: &Template, transform: Affine3A) -> Option<LightDesc> {
        let intensity = f(t, "intensity", 1.0);
        let color = vec3(t, "color").map(|c| c * intensity);
        let radius = f(t, "attenuationrange2", 0.0).max(f(t, "attenuationrange1", 0.0));
        (radius > 0.0 && color.iter().any(|&c| c > 0.0)).then(|| LightDesc {
            position: transform.translation.to_array(),
            color,
            radius,
            life: self.light_life,
            views: views(t),
        })
    }

    fn sound(&self, t: &Template) -> Option<EffectSound> {
        let sound = self.sounds.desc(&t.name, &t.props)?;
        Some(EffectSound {
            files: sound.files,
            volume: sound.volume,
            pitch: sound.pitch,
            falloff: sound.falloff,
            views: Views::Both,
        })
    }
}

/// `<dir>/meshes/<name>.<kind>` of a geometry template, including mesh particle meshes.
fn mesh_file(vfs: &Vfs, geometry: &bf2_formats::con::Geometry) -> Option<String> {
    if let Some(path) = geometry.mesh_path() {
        return Some(path);
    }
    let name = geometry.name.to_ascii_lowercase();
    ["bundledmesh", "staticmesh"]
        .iter()
        .map(|ext| normalize(&format!("{}/meshes/{name}.{ext}", geometry.dir)))
        .find(|p| vfs.exists(p))
}

/// The first geom and LOD of a small mesh, inline, with its color texture.
fn flash_mesh(converter: &MeshConverter, path: &str) -> Option<FlashMeshDesc> {
    let key = normalize(path);
    let mesh = VisMesh::parse(&converter.vfs.read(&key).ok()?, MeshKind::from_path(&key)?).ok()?;
    let lod = mesh.geoms.first()?.lods.first()?;
    let positions_all = mesh.attribute::<3>(Usage::Position, 0)?;
    let uvs_all = mesh.attribute::<2>(Usage::TexCoord, 0)?;
    let material = lod.materials.first()?;
    let texture = material.texture_maps().first().and_then(|t| converter.texture(t))?;
    let mut remap = HashMap::new();
    let (mut positions, mut uvs, mut indices) = (Vec::new(), Vec::new(), Vec::new());
    for material in &lod.materials {
        for triangle in mesh.material_triangles(material) {
            for vertex in triangle {
                let index = *remap.entry(vertex).or_insert_with(|| {
                    let v = vertex as usize;
                    positions.push(coords::position(positions_all.get(v).copied().unwrap_or_default()));
                    uvs.push(uvs_all.get(v).copied().unwrap_or_default());
                    positions.len() as u32 - 1
                });
                indices.push(index);
            }
        }
    }
    (!indices.is_empty()).then_some(FlashMeshDesc {
        texture,
        positions,
        uvs,
        indices,
        position: [0.0; 3],
        scale: [1.0, 1.0],
        roll_step: 0.0,
        life: 0.02,
        intensity: 1.0,
        views: Views::Both,
    })
}

fn f(t: &Template, name: &str, default: f32) -> f32 {
    t.get_f32(name).unwrap_or(default)
}

fn flag(t: &Template, name: &str) -> bool {
    t.get_f32(name).is_some_and(|v| v != 0.0)
}

fn vec3(t: &Template, name: &str) -> [f32; 3] {
    t.get_str(name).and_then(parse_vec3).unwrap_or_default()
}

fn curve(t: &Template, name: &str) -> Curve {
    t.get_str(name).and_then(parse_curve).unwrap_or_default()
}

/// `a/b/c/d` → the cubic `a t³ + b t² + c t + d`.
fn parse_curve(text: &str) -> Option<Curve> {
    let values: Vec<f32> = text.split('/').filter_map(|v| v.trim().parse().ok()).collect();
    (values.len() == 4).then(|| Curve([values[0], values[1], values[2], values[3]]))
}

fn views(t: &Template) -> Views {
    let first = t.get_f32("showinfirstperson").is_none_or(|v| v != 0.0);
    let third = t.get_f32("showinthirdperson").is_none_or(|v| v != 0.0);
    match (first, third) {
        (true, false) => Views::FirstPerson,
        (false, true) => Views::ThirdPerson,
        _ => Views::Both,
    }
}

/// `CRD_UNIFORM/min/max/mirror`, `CRD_NONE/value/_/_` or `CRD_NORMAL/mean/deviation/mirror`.
fn parse_spread(text: &str) -> Spread {
    let fields: Vec<&str> = text.split('/').collect();
    let number = |i: usize| fields.get(i).and_then(|v| v.parse::<f32>().ok()).unwrap_or(0.0);
    let (a, b) = (number(1), number(2));
    let (min, max) = match fields[0].to_ascii_uppercase().as_str() {
        "CRD_UNIFORM" => (a.min(b), a.max(b)),
        "CRD_NORMAL" => (a - b, a + b),
        "CRD_NONE" => (a, a),
        _ => (number(0), number(0)),
    };
    Spread { min, max, mirror: number(3) != 0.0 }
}

fn negate(spread: Spread) -> Spread {
    Spread { min: -spread.max, max: -spread.min, ..spread }
}

/// What the level's surfaces are made of: the terrain's material map, and the material
/// covering most of each static mesh part's projectile collision surface.
pub fn import_surfaces(
    world: &World,
    converter: &MeshConverter,
    objects: &HashMap<String, ObjectDesc>,
    level_dir: &Path,
) -> Result<()> {
    let mut surfaces = SurfaceMap {
        terrain: terrain_materials(world, converter.vfs, level_dir)
            .map_err(|e| log::warn!("terrain materials: {e:#}"))
            .ok()
            .flatten(),
        ..Default::default()
    };
    let mut collisions = HashMap::new();
    for name in objects.keys() {
        let mut walk = Walk { world, converter, collisions: &mut collisions, surfaces: &mut surfaces.meshes };
        walk.template(name, None, None, None, 0);
    }
    game_data::write_ron(level_dir.join("surfaces.ron"), &surfaces).context("writing surfaces.ron")
}

/// `heightmap.loadMaterialData` of the primary heightmap, rows flipped like the heightmap.
fn terrain_materials(world: &World, vfs: &Vfs, level_dir: &Path) -> Result<Option<String>> {
    let (mut primary, mut size, mut data) = (false, None, None);
    for command in &world.commands {
        let arg = |i: usize| command.args.get(i).map(String::as_str);
        match command.name.as_str() {
            "heightmapcluster.addheightmap" => primary = arg(1) == Some("0") && arg(2) == Some("0"),
            "heightmap.setsize" if primary => size = arg(0).and_then(|s| s.parse::<usize>().ok()),
            "heightmap.loadmaterialdata" if primary => data = arg(0).map(str::to_string),
            _ => {}
        }
    }
    let (Some(size), Some(data)) = (size, data) else {
        return Ok(None);
    };
    let raw = vfs.read(&data).with_context(|| format!("reading {data}"))?;
    anyhow::ensure!(raw.len() == size * size, "{data}: {} bytes for {size}x{size}", raw.len());
    let flipped: Vec<u8> = raw.chunks_exact(size).rev().flatten().copied().collect();
    std::fs::write(level_dir.join("terrain_materials.r8"), flipped)?;
    Ok(Some("terrain_materials.r8".into()))
}

/// Walks a template tree like the level import does, noting each part's material.
struct Walk<'a, 'b> {
    world: &'a World,
    converter: &'a MeshConverter<'a>,
    collisions: &'b mut HashMap<String, Option<CollisionMesh>>,
    surfaces: &'b mut BTreeMap<(String, u32), u32>,
}

impl Walk<'_, '_> {
    fn template(
        &mut self,
        name: &str,
        mesh: Option<&str>,
        collision: Option<&str>,
        materials: Option<&Template>,
        depth: u32,
    ) {
        let world = self.world;
        let Some(template) = world.template(name) else {
            return;
        };
        if depth > 16 {
            return;
        }
        let own_mesh = template.geometry.as_deref().and_then(|g| world.geometry(g)).and_then(|g| g.mesh_path());
        let own_collision = template
            .collision_mesh
            .as_deref()
            .and_then(|c| world.collision_meshes.get(&c.to_ascii_lowercase()).cloned())
            .or_else(|| destruction::uncreated_collision_mesh(self.converter, template));
        let mesh = own_mesh.as_deref().or(mesh.filter(|_| template.get("geometrypart").is_some()));
        let collision = own_collision
            .as_deref()
            .or(collision.filter(|_| template.get("collisionpart").is_some()));
        let materials = if template.get("mapmaterial").is_some() { Some(template) } else { materials };
        if let (Some(mesh), Some(collision), Some(materials)) = (mesh, collision, materials) {
            let part = template.get_f32("collisionpart").unwrap_or(0.0) as usize;
            if let Some(material) = self.material(collision, part, materials) {
                let index = template.get_f32("geometrypart").unwrap_or(0.0) as u32;
                self.surfaces.insert((object_mesh_path(self.converter.vfs, mesh), index), material);
            }
        }
        let (mesh, collision) = (mesh.map(str::to_string), collision.map(str::to_string));
        for child in &template.children {
            self.template(&child.template, mesh.as_deref(), collision.as_deref(), materials, depth + 1);
        }
    }

    fn material(&mut self, path: &str, part: usize, materials: &Template) -> Option<u32> {
        let key = normalize(path);
        let vfs = self.converter.vfs;
        let collision = self
            .collisions
            .entry(key.clone())
            .or_insert_with(|| vfs.read(&key).ok().and_then(|d| CollisionMesh::parse(&d).ok()))
            .as_ref()?;
        let col = collision
            .parts
            .get(part)?
            .geoms
            .iter()
            .find(|g| !g.cols.is_empty())?
            .cols
            .iter()
            .find(|c| c.col_type == ColType::Projectile)?;
        let ids: HashMap<u16, u32> = materials
            .get_all("mapmaterial")
            .filter_map(|args| Some((args.first()?.parse().ok()?, args.get(2)?.parse().ok()?)))
            .collect();
        let mut areas: HashMap<u16, f32> = HashMap::new();
        for face in &col.faces {
            let [a, b, c] = [face[0], face[1], face[2]]
                .map(|i| Vec3::from_array(col.vertices.get(i as usize).copied().unwrap_or_default()));
            *areas.entry(face[3]).or_default() += (b - a).cross(c - a).length() * 0.5;
        }
        let (index, _) = areas.into_iter().max_by(|a, b| a.1.total_cmp(&b.1))?;
        ids.get(&index).copied()
    }
}

/// The `.glb` the level import writes for an object's visible mesh (see
/// `destruction::convert_object_mesh`).
fn object_mesh_path(vfs: &Vfs, path: &str) -> String {
    let key = normalize(path);
    let stem = key.rsplit_once('.').map_or(key.as_str(), |(s, _)| s);
    let geom = if key.ends_with(".bundledmesh") { destruction::object_geoms(vfs, &key).0 } else { 0 };
    if geom == 0 { format!("{stem}.glb") } else { format!("{stem}_3p.glb") }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_curves() {
        let curve = parse_curve("0/-0.29/-0.7/1").unwrap();
        assert_eq!(curve.at(0.0), 1.0);
        assert!((curve.at(1.0) - 0.01).abs() < 1e-5);
        assert!(parse_curve("1/2").is_none());
    }

    #[test]
    fn parses_spreads() {
        assert_eq!(parse_spread("CRD_UNIFORM/14/18/0"), Spread { min: 14.0, max: 18.0, mirror: false });
        assert_eq!(parse_spread("CRD_NONE/-1/0/0").max, -1.0);
    }
}
