//! The lamps of a level: BF2's `LightSource` templates placed in it, for real-time lights.
//!
//! BF2 places light sources as objects of their own (a `Bundle` whose child is the
//! `LightSource`, e.g. `NF_ONE` in the Special Forces levels' `StaticLights.con` and
//! `StaticObjects.con`, or `baselight` in Operation Clean Sweep) or as children of lamp
//! objects (the ceiling light `xp1_daylightglow` adds `xp1_dayglowlight`). Their light is baked
//! into the lightmaps' red channel, which the shaders add times the level's lamp colour
//! (`lightmap.r x singlePointColor`, `RaShaderSTM.fx` and the Special Forces'
//! `TerrainShader_Shared.fx`), so their own colour is mostly white.
//!
//! What a light source gives:
//! - `lightType Point` / `Spot` (`Directional` ones are editor ambience, skipped), `enabled`.
//! - `color`, `intensity`: the light is `color x intensity` up to `attenuationRange1` meters and
//!   fades to nothing at `attenuationRange2` (the baked light saturates at 1, so strengths above
//!   1 light a wider area fully).
//! - `direction` and `coneAngle1/2` for spot lights.
//! - `castsDynamicShadow`: the Special Forces night levels place a second light (the `*DY_*`
//!   templates, much stronger and mostly shorter) next to most static ones; those lit soldiers
//!   and vehicles and cast their shadows. Here both would light everything, so a flagged light
//!   within [`MERGE_DISTANCE`] of an unflagged one is merged into it (which then becomes a
//!   shadow candidate).
//!
//! Street lamps on day levels have no light source (only the Special Forces' `*_lit` variants
//! add a glow). For night versions of those levels, the lamp objects in [`LAMP_OBJECTS`] get a
//! lamp at their head (`unlit`): the top of their mesh, or the ends of its arms.

use std::collections::{HashMap, HashSet};

use bf2_formats::{
    Vfs,
    con::{Instance, Template, World},
    mesh::{MeshKind, Usage, VisMesh},
};
use game_data::LampDesc;
use glam::{Affine3A, Vec3};

use crate::coords;

/// A lamp flagged for dynamic shadows this close to another is the same lamp (Night Flight: half
/// of them within 1.3 m, three quarters within 3.4 m).
const MERGE_DISTANCE: f32 = 3.0;
/// Strength a flagged duplicate that stands alone keeps at most: theirs (10 to 100) were meant
/// for BF2's dynamic lighting, not the lightmaps.
const MAX_ALONE_STRENGTH: f32 = 2.0;

/// Lamp objects without a light source, and how far their light reaches (m).
const LAMP_OBJECTS: &[(&str, f32)] = &[
    ("lamp_post", 14.0),
    ("lamppost_highway_01", 20.0),
    ("lamppost_highway_02", 20.0),
    ("xp2_streetlight_01", 16.0),
    ("xp2_highway_light", 20.0),
    ("stoneroad_light_xp2", 12.0),
    ("floodlight", 28.0),
];
/// A lamp's head: vertices this close to the top of the mesh...
const HEAD_DEPTH: f32 = 0.7;
/// ...and, on lamps with arms, this close to the end of an arm.
const ARM_END: f32 = 0.6;
/// The light sits this far below the head (under the housing).
const BELOW_HEAD: f32 = 0.3;

/// The lamps among `instances` (and their children), merged as described in the module docs,
/// and lamps at the heads of lamp objects without one (`unlit`).
pub fn level_lamps(world: &World, vfs: &Vfs, instances: &[Instance]) -> Vec<LampDesc> {
    let mut lamps = Vec::new();
    let mut heads: HashMap<String, Vec<Vec3>> = HashMap::new();
    let mut unlit = Vec::new();
    for instance in instances {
        let Some(transform) = instance_transform(instance) else {
            continue;
        };
        let mut visited = HashSet::new();
        let before = lamps.len();
        collect(world, &instance.template, transform, 0, &mut visited, &mut lamps);
        let name = instance.template.to_ascii_lowercase();
        let Some((_, range)) = LAMP_OBJECTS.iter().find(|(n, _)| *n == name) else {
            continue;
        };
        if lamps.len() > before {
            continue;
        }
        let local = heads.entry(name.clone()).or_insert_with(|| lamp_heads(world, vfs, &name));
        for head in local.iter() {
            unlit.push(LampDesc {
                position: transform.transform_point3(*head).to_array(),
                direction: None,
                color: [1.0; 3],
                strength: 1.0,
                range: [1.0, *range],
                cone: [0.0; 2],
                shadows: false,
                unlit: true,
            });
        }
    }
    let mut lamps = merge(lamps);
    lamps.extend(unlit);
    lamps
}

/// Where the lights of a lamp object without a light source sit, in its own space: below the
/// top of its mesh, or below the ends of its arms if the top reaches out from the pole.
fn lamp_heads(world: &World, vfs: &Vfs, name: &str) -> Vec<Vec3> {
    let Some(path) = world
        .template(name)
        .and_then(|t| t.geometry.as_deref())
        .and_then(|g| world.geometry(g))
        .and_then(|g| g.mesh_path())
    else {
        return Vec::new();
    };
    let mesh = MeshKind::from_path(&path)
        .and_then(|kind| VisMesh::parse(&vfs.read(&path).ok()?, kind).ok());
    let Some(mesh) = mesh else {
        return Vec::new();
    };
    let (Some(lod), Some(positions)) = (
        mesh.geoms.first().and_then(|g| g.lods.first()),
        mesh.attribute::<3>(Usage::Position, 0),
    ) else {
        return Vec::new();
    };
    let mut used = HashSet::new();
    let mut points = Vec::new();
    for material in &lod.materials {
        for triangle in mesh.material_triangles(material) {
            for vertex in triangle {
                if used.insert(vertex)
                    && let Some(p) = positions.get(vertex as usize)
                {
                    points.push(Vec3::from_array(coords::position(*p)));
                }
            }
        }
    }
    heads_of(&points)
}

/// The heads of a lamp mesh's vertices (see [`lamp_heads`]).
fn heads_of(points: &[Vec3]) -> Vec<Vec3> {
    let Some(top) = points.iter().map(|p| p.y).reduce(f32::max) else {
        return Vec::new();
    };
    let head: Vec<Vec3> = points.iter().copied().filter(|p| p.y > top - HEAD_DEPTH).collect();
    let reach = |p: &Vec3| Vec3::new(p.x, 0.0, p.z).length();
    let centre = |ps: &[Vec3]| ps.iter().copied().sum::<Vec3>() / ps.len().max(1) as f32;
    let below = |p: Vec3| p - Vec3::Y * BELOW_HEAD;
    let far = head.iter().map(reach).fold(0.0, f32::max);
    if far < 1.0 {
        return vec![below(centre(&head))];
    }
    // Arms: the vertices near each far end, taking ends one after another.
    let mut rest: Vec<Vec3> = head.into_iter().filter(|p| reach(p) > far * 0.5).collect();
    let mut heads = Vec::new();
    while let Some(end) = rest.iter().copied().max_by(|a, b| reach(a).total_cmp(&reach(b))) {
        let (near, others): (Vec<Vec3>, Vec<Vec3>) = rest.into_iter().partition(|p| p.distance(end) < ARM_END * 2.0);
        heads.push(below(centre(&near)));
        rest = others.into_iter().filter(|p| heads.iter().all(|h| p.distance(*h) > ARM_END * 3.0)).collect();
        if heads.len() >= 4 {
            break;
        }
    }
    heads
}

fn instance_transform(instance: &Instance) -> Option<Affine3A> {
    if let Some(m) = instance.transform {
        let (scale, rotation, translation) = coords::matrix(m);
        return Some(Affine3A::from_scale_rotation_translation(scale, rotation.normalize(), translation));
    }
    let position = Vec3::from_array(coords::position(instance.position?));
    let rotation = coords::rotation_ypr(instance.rotation.unwrap_or([0.0; 3]));
    Some(Affine3A::from_rotation_translation(rotation, position))
}

fn collect(
    world: &World,
    name: &str,
    transform: Affine3A,
    depth: u32,
    visited: &mut HashSet<String>,
    lamps: &mut Vec<LampDesc>,
) {
    let Some(template) = world.template(name) else {
        return;
    };
    if depth > 8 {
        return;
    }
    if template.ty.eq_ignore_ascii_case("lightsource") {
        lamps.extend(lamp(template, transform));
        return;
    }
    let key = name.to_ascii_lowercase();
    if !visited.insert(key.clone()) {
        return;
    }
    for child in &template.children {
        let local = Affine3A::from_rotation_translation(
            coords::rotation_ypr(child.rotation.unwrap_or([0.0; 3])),
            Vec3::from_array(coords::position(child.position.unwrap_or([0.0; 3]))),
        );
        collect(world, &child.template, transform * local, depth + 1, visited, lamps);
    }
    visited.remove(&key);
}

/// One light source placed at `transform`.
fn lamp(template: &Template, transform: Affine3A) -> Option<LampDesc> {
    let kind = template.get_str("lighttype").unwrap_or("Point").to_ascii_lowercase();
    // Numbers as in the editor's enum: 0 point, 1 spot, 2 directional.
    let spot = match kind.as_str() {
        "point" | "0" => false,
        "spot" | "1" => true,
        _ => return None,
    };
    if template.get_f32("enabled") == Some(0.0) {
        return None;
    }
    let near = template.get_f32("attenuationrange1").unwrap_or(0.0).max(0.0);
    let far = template.get_f32("attenuationrange2").unwrap_or(10.0).max(near);
    let strength = template.get_f32("intensity").unwrap_or(1.0);
    let color = template.get_vec3("color").unwrap_or([1.0; 3]);
    if far <= 0.0 || strength <= 0.0 || color.iter().all(|c| *c <= 0.0) {
        return None;
    }
    let direction = spot.then(|| {
        let local = template
            .get_vec3("direction")
            .map(coords::direction)
            .unwrap_or([0.0, -1.0, 0.0]);
        let world = transform.transform_vector3(Vec3::from_array(local)).normalize_or(Vec3::NEG_Y);
        world.to_array()
    });
    let cone = if spot {
        let inner = template.get_f32("coneangle1").unwrap_or(30.0);
        let outer = template.get_f32("coneangle2").unwrap_or(inner).max(inner);
        [inner, outer]
    } else {
        [0.0; 2]
    };
    Some(LampDesc {
        position: transform.translation.to_array(),
        direction,
        color: color.map(|c| c.clamp(0.0, 1.0)),
        strength,
        range: [near, far],
        cone,
        shadows: template.get_f32("castsdynamicshadow").is_some_and(|v| v != 0.0),
        unlit: false,
    })
}

/// Merges each lamp flagged for shadows into an unflagged one within [`MERGE_DISTANCE`], and
/// drops exact duplicates.
fn merge(lamps: Vec<LampDesc>) -> Vec<LampDesc> {
    let (flagged, mut plain): (Vec<LampDesc>, Vec<LampDesc>) = lamps.into_iter().partition(|l| l.shadows);
    let position = |l: &LampDesc| Vec3::from_array(l.position);
    let mut alone = Vec::new();
    for lamp in flagged {
        let nearest = plain
            .iter_mut()
            .map(|p| (position(p).distance(position(&lamp)), p))
            .filter(|(d, _)| *d < MERGE_DISTANCE)
            .min_by(|a, b| a.0.total_cmp(&b.0));
        match nearest {
            Some((_, p)) => p.shadows = true,
            None => alone.push(LampDesc {
                strength: lamp.strength.min(MAX_ALONE_STRENGTH),
                ..lamp
            }),
        }
    }
    plain.extend(alone);
    let mut out: Vec<LampDesc> = Vec::with_capacity(plain.len());
    for lamp in plain {
        if let Some(same) = out.iter_mut().find(|o| position(o).distance(position(&lamp)) < 0.05) {
            same.strength = same.strength.max(lamp.strength);
            same.range[1] = same.range[1].max(lamp.range[1]);
            same.shadows |= lamp.shadows;
        } else {
            out.push(lamp);
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn lamp_at(x: f32, strength: f32, shadows: bool) -> LampDesc {
        LampDesc {
            position: [x, 0.0, 0.0],
            direction: None,
            color: [1.0; 3],
            strength,
            range: [0.5, 10.0],
            cone: [0.0; 2],
            shadows,
            unlit: false,
        }
    }

    #[test]
    fn flagged_duplicates_merge() {
        let lamps = merge(vec![
            lamp_at(0.0, 1.0, false),
            lamp_at(1.0, 10.0, true),
            lamp_at(50.0, 10.0, true),
            lamp_at(50.0, 1.0, false),
            lamp_at(100.0, 20.0, true),
        ]);
        assert_eq!(lamps.len(), 3);
        assert!(lamps[0].shadows && lamps[0].strength == 1.0);
        assert!(lamps[1].shadows && lamps[1].strength == 1.0);
        // Alone: kept, weakened.
        assert_eq!(lamps[2].position[0], 100.0);
        assert_eq!(lamps[2].strength, MAX_ALONE_STRENGTH);
    }

    #[test]
    fn light_sources_in_bundles() {
        let dir = std::env::temp_dir().join(format!("bf2_lamps_test_{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(
            dir.join("test.con"),
            "ObjectTemplate.create Bundle nf_one\n\
             ObjectTemplate.addTemplate nf_one_light\n\
             ObjectTemplate.setPosition 0/2/1\n\
             ObjectTemplate.create LightSource nf_one_light\n\
             ObjectTemplate.lightType Point\n\
             ObjectTemplate.attenuationRange1 0.5\n\
             ObjectTemplate.attenuationRange2 20\n\
             ObjectTemplate.color 1/1/0.5\n\
             ObjectTemplate.intensity 1.1\n\
             ObjectTemplate.create LightSource ambience\n\
             ObjectTemplate.lightType Directional\n\
             Object.create nf_one\n\
             Object.absolutePosition 10/20/30\n\
             Object.create ambience\n\
             Object.absolutePosition 0/0/0\n",
        )
        .unwrap();
        let mut vfs = bf2_formats::vfs::Vfs::default();
        vfs.mount_dir(&dir, "").unwrap();
        let mut interp = bf2_formats::con::Interpreter::new(&vfs);
        interp.run("test.con", &[]);
        let _ = std::fs::remove_dir_all(&dir);
        let lamps = level_lamps(&interp.world, &vfs, &interp.world.instances);
        assert_eq!(lamps.len(), 1);
        let lamp = &lamps[0];
        assert_eq!(lamp.position, [10.0, 22.0, -31.0]);
        assert_eq!(lamp.range, [0.5, 20.0]);
        assert_eq!(lamp.color, [1.0, 1.0, 0.5]);
        assert!((lamp.strength - 1.1).abs() < 1e-6);
        assert!(lamp.direction.is_none() && !lamp.shadows);
    }

    #[test]
    fn heads_of_lamp_meshes() {
        // A pole 6 m high: one head just below its top.
        let pole: Vec<Vec3> = (0..=12).map(|i| Vec3::new(0.05, i as f32 * 0.5, 0.0)).collect();
        let heads = heads_of(&pole);
        assert_eq!(heads.len(), 1);
        assert!((heads[0].y - (6.0 - 0.25 - BELOW_HEAD)).abs() < 0.3, "{heads:?}");
        // Two arms reaching 2 m out either side at the top.
        let mut double = pole.clone();
        for x in [-2.0, -1.9, -1.5, -1.0, 1.0, 1.5, 1.9, 2.0] {
            double.push(Vec3::new(x, 5.9, 0.0));
        }
        let heads = heads_of(&double);
        assert_eq!(heads.len(), 2, "{heads:?}");
        assert!(heads.iter().all(|h| h.x.abs() > 1.5));
    }
}
