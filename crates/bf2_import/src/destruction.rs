//! Destroyable objects and the damage table.
//!
//! BF2 `DestroyableObject` templates carry an `Armor` component: hit points, the material
//! explosions are checked against, an optional blast and the effects that play at 0 hit
//! points (`armor.addArmorEffect`: effect bundles whose emitters throw meshes, plus sounds and
//! sprite particles). Their meshes may have a second geom for the destroyed state. How much
//! a projectile hurts which surface comes from `Common/Material/materialManagerSettings.con`.

use std::{
    collections::{HashMap, HashSet},
    path::Path,
    sync::Mutex,
};

use anyhow::{Context, Result};
use bf2_formats::{
    Vfs,
    collision::{ColType, CollisionMesh},
    con::{Interpreter, Template, World, parse_vec3},
    mesh::{MeshKind, VisMesh},
    vfs::normalize,
};
use game_data::{ArmorDesc, DebrisPiece, DestructionEffect, EffectPlacement, ExplosionDesc, MaterialTable, Spread};
use glam::{Affine3A, Vec3};

use crate::{coords, glb, meshes::MeshConverter};

/// Hit points of an `Armor` component that sets none (chairs, cones) [inferred: they break
/// on any damage].
const DEFAULT_HIT_POINTS: f32 = 1.0;

/// Converts the material definitions and damage cells to `materials.ron`. Returns the number
/// of damage factors.
pub fn import_materials(vfs: &Vfs, out: &Path) -> Result<usize> {
    let mut interp = Interpreter::new(vfs);
    interp.run("common/material/materialmanagersettings.con", &[]);
    let mut table = MaterialTable::default();
    let (mut material, mut cell) = (None, None);
    for command in &interp.world.commands {
        let arg = |i: usize| command.args.get(i).map(|a| a.trim_matches('"'));
        let number = |i: usize| arg(i).and_then(|a| a.parse::<f32>().ok());
        match command.name.as_str() {
            "material.active" => material = number(0).map(|id| id as u32),
            "material.name" => {
                if let (Some(id), Some(name)) = (material, arg(0)) {
                    table.names.insert(id, name.to_string());
                }
            }
            "materialmanager.createcell" => {
                cell = number(0).zip(number(1)).map(|(a, b)| (a as u32, b as u32));
            }
            "materialmanager.damagemod" => {
                if let (Some(cell), Some(factor)) = (cell, number(0)) {
                    table.damage.insert(cell, factor);
                }
            }
            _ => {}
        }
    }
    anyhow::ensure!(!table.damage.is_empty(), "no material damage table in this mod");
    game_data::write_ron(out.join("materials.ron"), &table)?;
    Ok(table.damage.len())
}

pub fn is_destroyable(template: &Template) -> bool {
    template.ty.eq_ignore_ascii_case("destroyableobject")
}

/// Loads the effect templates the destroyable objects among `templates` use: the level's
/// scripts don't, and effects only name their children.
pub fn load_effects(interp: &mut Interpreter, templates: &HashSet<String>) {
    let mut pending: Vec<String> = templates
        .iter()
        .filter_map(|name| interp.world.template(name))
        .filter(|t| is_destroyable(t))
        .flat_map(|t| armor_effects(t, "armor.addarmoreffect").map(|e| e.template).collect::<Vec<_>>())
        .collect();
    let mut seen = HashSet::new();
    while let Some(name) = pending.pop() {
        if !seen.insert(name.clone()) {
            continue;
        }
        interp.ensure_template(&name);
        if let Some(template) = interp.world.template(&name) {
            pending.extend(template.children.iter().map(|c| c.template.to_ascii_lowercase()));
            pending.extend(template.get_str("template").map(str::to_ascii_lowercase));
        }
    }
}

/// The armor of a destroyable template, with its effects converted.
pub fn armor(world: &World, template: &Template, converter: &MeshConverter) -> Option<ArmorDesc> {
    if !is_destroyable(template) {
        return None;
    }
    let get = |property: &str| template.get_f32(&format!("armor.{property}"));
    let explosion = match (get("explosiondamage"), get("explosionradius")) {
        (Some(damage), Some(radius)) if damage > 0.0 && radius > 0.0 => Some(ExplosionDesc {
            damage,
            radius,
            material: get("explosionmaterial").unwrap_or(0.0) as u32,
        }),
        _ => None,
    };
    let mut effect = DestructionEffect::default();
    for armor_effect in armor_effects(template, "armor.addarmoreffect").filter(|e| e.hit_points <= 0.0) {
        let transform = Affine3A::from_rotation_translation(
            coords::rotation_ypr(armor_effect.rotation),
            Vec3::from_array(coords::position(armor_effect.position)),
        );
        collect_effect(world, &armor_effect.template, transform, converter, &mut effect, 0);
        effect.effects.push(EffectPlacement {
            name: armor_effect.template.clone(),
            position: transform.translation.to_array(),
            rotation: coords::rotation_ypr(armor_effect.rotation).to_array(),
        });
    }
    Some(ArmorDesc {
        hit_points: get("maxhitpoints").or(get("hitpoints")).unwrap_or(DEFAULT_HIT_POINTS),
        material: get("defaultmaterial").unwrap_or(0.0) as u32,
        explosion,
        effect,
    })
}

pub struct ArmorEffect {
    pub hit_points: f32,
    pub template: String,
    pub position: [f32; 3],
    pub rotation: [f32; 3],
}

/// `armor.addArmorEffect <hit points> <effect> <position> <rotation>` (or `property`'s
/// variant): effects that start when the hit points drop to the threshold.
pub fn armor_effects<'a>(template: &'a Template, property: &'a str) -> impl Iterator<Item = ArmorEffect> + 'a {
    template.get_all(property).filter_map(|args| {
        let vec = |i: usize| args.get(i).and_then(|s| parse_vec3(s)).unwrap_or_default();
        Some(ArmorEffect {
            hit_points: args.first()?.parse().ok()?,
            template: args.get(1)?.to_ascii_lowercase(),
            position: vec(2),
            rotation: vec(3),
        })
    })
}

/// Walks an effect bundle: emitters of mesh particles become debris, sounds are copied.
fn collect_effect(
    world: &World,
    name: &str,
    transform: Affine3A,
    converter: &MeshConverter,
    effect: &mut DestructionEffect,
    depth: u32,
) {
    let Some(template) = world.template(name) else {
        return;
    };
    if depth > 8 {
        return;
    }
    match template.ty.to_ascii_lowercase().as_str() {
        "effectbundle" => {
            for child in &template.children {
                let local = Affine3A::from_rotation_translation(
                    coords::rotation_ypr(child.rotation.unwrap_or_default()),
                    Vec3::from_array(coords::position(child.position.unwrap_or_default())),
                );
                collect_effect(world, &child.template, transform * local, converter, effect, depth + 1);
            }
        }
        "emitter" => effect.debris.extend(debris_piece(world, template, transform, converter)),
        "sound" => {
            let sound = crate::sounds::SoundConverter::new(converter.vfs, &converter.out).template(world, name);
            if let Some(sound) = sound.filter(|s| !effect.sounds.contains(s)) {
                effect.sounds.push(sound);
            }
        }
        "spriteparticlesystem" | "particlesystememitter" => effect.dust = true,
        _ => {}
    }
}

/// An emitter that throws one mesh particle (`ObjectTemplate.template` names the particle).
fn debris_piece(
    world: &World,
    emitter: &Template,
    transform: Affine3A,
    converter: &MeshConverter,
) -> Option<DebrisPiece> {
    let particle = world.template(emitter.get_str("template")?)?;
    if !particle.ty.eq_ignore_ascii_case("particle") {
        return None;
    }
    let path = world.geometry(particle.geometry.as_deref()?)?.mesh_path()?;
    let mesh = convert_object_mesh(converter, &path)
        .map_err(|e| log::warn!("debris mesh {path}: {e:#}"))
        .ok()?;
    let spread = |property: &str| emitter.get_str(property).map_or_else(Spread::default, parse_spread);
    // BF2 right/up/forward are +X/+Y/+Z; the engine mirrors Z.
    let axes = |kind: &str| {
        [
            spread(&format!("{kind}speedinright")),
            spread(&format!("{kind}speedinup")),
            negate(spread(&format!("{kind}speedindof"))),
        ]
    };
    Some(DebrisPiece {
        mesh,
        mesh_index: particle.get_f32("geometrypart").unwrap_or(0.0) as u32,
        position: transform.translation.to_array(),
        velocity: axes("positional"),
        spin: axes("rotational"),
        life: particle
            .get_str("timetolive")
            .map_or(Spread { min: 2.0, max: 2.0, mirror: false }, parse_spread),
    })
}

/// `CRD_UNIFORM/min/max/mirror`, `CRD_NONE/value/_/_` or `CRD_NORMAL/mean/deviation/mirror`.
fn parse_spread(text: &str) -> Spread {
    let fields: Vec<&str> = text.split('/').collect();
    let number = |i: usize| fields.get(i).and_then(|v| v.parse::<f32>().ok()).unwrap_or(0.0);
    let (a, b) = (number(1), number(2));
    let (min, max) = match fields[0].to_ascii_uppercase().as_str() {
        "CRD_UNIFORM" => (a.min(b), a.max(b)),
        "CRD_NORMAL" => (a - b, a + b),
        _ => (a, a),
    };
    Spread {
        min,
        max,
        mirror: number(3) != 0.0,
    }
}

fn negate(spread: Spread) -> Spread {
    Spread {
        min: -spread.max,
        max: -spread.min,
        ..spread
    }
}

/// Some destroyable templates (the wooden `fence`) name a collision mesh that no
/// `CollisionManager.createTemplate` created; the file sits next to the template.
pub fn uncreated_collision_mesh(converter: &MeshConverter, template: &Template) -> Option<String> {
    if !is_destroyable(template) {
        return None;
    }
    let dir = template.source.rsplit_once('/')?.0;
    let name = template.collision_mesh.as_deref()?.to_ascii_lowercase();
    let path = normalize(&format!("{dir}/meshes/{name}.collisionmesh"));
    converter.vfs.exists(&path).then_some(path)
}

/// The geoms of an object's visible mesh: intact and, if there is one, destroyed. Static
/// meshes put the destroyed state in geom 1; bundled meshes with several geoms follow the
/// vehicle layout (0 first person, 1 third person, 2 wreck).
pub fn object_geoms(vfs: &Vfs, path: &str) -> (usize, Option<usize>) {
    let key = normalize(path);
    let Some(kind) = MeshKind::from_path(&key) else {
        return (0, None);
    };
    let geoms = vfs
        .read(&key)
        .ok()
        .and_then(|data| VisMesh::parse(&data, kind).ok())
        .map_or(1, |mesh| mesh.geoms.len());
    let intact = usize::from(kind == MeshKind::Bundled && geoms > 1);
    (intact, (intact + 1 < geoms).then_some(intact + 1))
}

/// Converts the geom of a static object's mesh that is drawn in the world.
pub fn convert_object_mesh(converter: &MeshConverter, path: &str) -> Result<String> {
    if !normalize(path).ends_with(".bundledmesh") {
        return converter.convert_mesh(path);
    }
    match object_geoms(converter.vfs, path) {
        (0, _) => converter.convert_mesh(path),
        (geom, _) => converter.convert_mesh_geom(path, geom, "_3p"),
    }
}

/// Converts the destroyed geom of a mesh to `x_wreck.glb`, if it has one.
pub fn convert_wreck_mesh(converter: &MeshConverter, path: &str) -> Option<String> {
    let (_, wreck) = object_geoms(converter.vfs, path);
    converter
        .convert_mesh_geom(path, wreck?, "_wreck")
        .map_err(|e| log::warn!("wreck mesh {path}: {e:#}"))
        .ok()
}

/// What the collision mesh says about a destroyable part: the material most of its
/// projectile surface is made of (mapped through the template's `mapMaterial`), and the
/// destroyed geom converted to `x_wreck.collision.glb` if it has one.
pub fn collision_info(
    converter: &MeshConverter,
    path: &str,
    part: usize,
    template: &Template,
) -> (Option<u32>, Option<String>) {
    let key = normalize(path);
    let Some(collision) = converter
        .vfs
        .read(&key)
        .ok()
        .and_then(|data| CollisionMesh::parse(&data).ok())
    else {
        return (None, None);
    };
    let hit_material = collision
        .parts
        .get(part)
        .and_then(|p| p.geoms.iter().find(|g| !g.cols.is_empty()))
        .and_then(|geom| geom.cols.iter().find(|c| c.col_type == ColType::Projectile))
        .and_then(|col| dominant_material(col, template));
    let wreck = convert_wreck_collision(converter, &key, &collision)
        .map_err(|e| log::warn!("wreck collision {key}: {e:#}"))
        .ok()
        .flatten();
    (hit_material, wreck)
}

/// The material with the largest area, by `mapMaterial <index> <name> <id>`.
fn dominant_material(col: &bf2_formats::collision::Col, template: &Template) -> Option<u32> {
    let ids: HashMap<u16, u32> = template
        .get_all("mapmaterial")
        .filter_map(|args| Some((args.first()?.parse().ok()?, args.get(2)?.parse().ok()?)))
        .collect();
    let mut areas: HashMap<u16, f32> = HashMap::new();
    for face in &col.faces {
        let [a, b, c] = [face[0], face[1], face[2]].map(|i| {
            Vec3::from_array(col.vertices.get(i as usize).copied().unwrap_or_default())
        });
        *areas.entry(face[3]).or_default() += (b - a).cross(c - a).length() * 0.5;
    }
    let (index, _) = areas.into_iter().max_by(|a, b| a.1.total_cmp(&b.1))?;
    ids.get(&index).copied()
}

/// Several templates can share a collision mesh and are built in parallel.
static WRECK_COLLISION: Mutex<()> = Mutex::new(());

/// The geom after the intact one, as `part{N}_{type}` meshes like the intact collision.
fn convert_wreck_collision(
    converter: &MeshConverter,
    key: &str,
    collision: &CollisionMesh,
) -> Result<Option<String>> {
    let wreck_geoms: Vec<_> = collision
        .parts
        .iter()
        .map(|part| {
            let intact = part.geoms.iter().position(|g| !g.cols.is_empty())?;
            part.geoms.get(intact + 1).filter(|g| !g.cols.is_empty())
        })
        .collect();
    if wreck_geoms.iter().all(Option::is_none) {
        return Ok(None);
    }
    let stem = key.rsplit_once('.').map_or(key, |(s, _)| s);
    let out_rel = format!("{stem}_wreck.collision.glb");
    let _guard = WRECK_COLLISION.lock().unwrap();
    if converter.out.join(&out_rel).exists() {
        return Ok(Some(out_rel));
    }
    let mut doc = glb::Document::default();
    for (part, geom) in wreck_geoms.iter().enumerate() {
        for col in geom.iter().flat_map(|g| &g.cols) {
            let kind = match col.col_type {
                ColType::Projectile => "projectile",
                ColType::Vehicle => "vehicle",
                ColType::Soldier => "soldier",
                ColType::Ai => "ai",
                ColType::Unknown(_) => continue,
            };
            let name = format!("part{part}_{kind}");
            doc.meshes.push(glb::Mesh {
                name: name.clone(),
                primitives: vec![glb::Primitive {
                    positions: col.vertices.iter().map(|&v| coords::position(v)).collect(),
                    indices: col.faces.iter().flat_map(|f| [f[0] as u32, f[1] as u32, f[2] as u32]).collect(),
                    ..Default::default()
                }],
            });
            doc.nodes.push(glb::Node {
                name,
                mesh: Some(doc.meshes.len() - 1),
                ..Default::default()
            });
            doc.scene.push(doc.nodes.len() - 1);
        }
    }
    doc.write(&converter.out.join(&out_rel))
        .with_context(|| format!("writing {out_rel}"))?;
    Ok(Some(out_rel))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_spreads() {
        assert_eq!(
            parse_spread("CRD_UNIFORM/2/-2/0"),
            Spread { min: -2.0, max: 2.0, mirror: false }
        );
        assert_eq!(
            parse_spread("CRD_UNIFORM/0/3/1"),
            Spread { min: 0.0, max: 3.0, mirror: true }
        );
        assert_eq!(parse_spread("CRD_NONE/5/0/0"), Spread { min: 5.0, max: 5.0, mirror: false });
    }
}
