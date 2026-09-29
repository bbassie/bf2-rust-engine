//! Soldier bodies: skinned mesh + `3p_setup` skeleton + third-person animations, one `.glb`
//! per soldier model, with BF2's lower levels of detail of the body as further meshes on the
//! same skeleton (`body_lod1`, ...).

use std::path::Path;

use anyhow::{Context, Result};
use bf2_formats::{
    Bf2Install, Side, Vfs,
    anim::{Animation, Skeleton},
    con::Interpreter,
    mesh::{Lod, MeshKind, Usage, VisMesh},
};
use game_data::SoldierDesc;
use glam::{Mat4, Quat, Vec3};
use serde_json::json;

use crate::{
    glb::{self, ChannelValues},
    meshes::MeshConverter,
};

const SKELETON: &str = "objects/soldiers/common/animations/3p_setup.ske";
const SKELETON_1P: &str = "objects/soldiers/common/animations/1p_setup.ske";
const ANIMATIONS: &str = "objects/soldiers/common/animations/3p/";
/// Sitting and standing poses in vehicle seats (drivers, pilots, gunners, passengers).
pub const SEAT_ANIMATIONS: &str = "objects/vehicles/common/animations/3p/";
/// Climbing ladders (BF2 plays these through the ladder's seat animation system).
const LADDER_ANIMATIONS: &str = "objects/common/ladder/animations/";
/// Climbing a grappling rope and hanging from a zipline (Special Forces; their "seats").
const ROPE_ANIMATIONS: [&str; 2] = [
    "objects/vehicles/xpak_vehicles/xpak_grapplehook/3p/",
    "objects/vehicles/xpak_vehicles/xpak_zipline/3p/",
];
const FLAGS: &str = "objects/common/flags/";

/// Z-mirror of a (true, un-conjugated) BF2 rotation.
fn rotation(r: [f32; 4]) -> Quat {
    Quat::from_xyzw(-r[0], -r[1], r[2], r[3]).normalize()
}

fn translation(t: [f32; 3]) -> Vec3 {
    Vec3::new(t[0], t[1], -t[2])
}

/// Adds the skeleton as nodes (node index == bone index). Returns rest-pose world matrices.
fn add_skeleton(doc: &mut glb::Document, skeleton: &Skeleton) -> Vec<Mat4> {
    let bone_count = skeleton.bones.len();
    for bone in &skeleton.bones {
        doc.nodes.push(glb::Node {
            name: bone.name.clone(),
            translation: translation(bone.translation).to_array(),
            rotation: rotation(bone.rotation).to_array(),
            ..Default::default()
        });
    }
    let mut world = vec![Mat4::IDENTITY; bone_count];
    for (i, bone) in skeleton.bones.iter().enumerate() {
        let local = Mat4::from_rotation_translation(rotation(bone.rotation), translation(bone.translation));
        world[i] = match bone.parent {
            Some(p) => {
                doc.nodes[p].children.push(i);
                world[p] * local
            }
            None => local,
        };
    }
    world
}

/// One root over the skeleton roots (and `extra` nodes), so a soldier is a single animation
/// hierarchy. Animation sets use the same root name (`soldier` for third person,
/// `firstperson` for first person), so their clips target the same bones by name path.
fn add_named_root(doc: &mut glb::Document, skeleton: &Skeleton, extra: &[usize], name: &str) {
    let mut children: Vec<usize> = skeleton
        .bones
        .iter()
        .enumerate()
        .filter(|(_, b)| b.parent.is_none())
        .map(|(i, _)| i)
        .collect();
    children.extend_from_slice(extra);
    doc.nodes.push(glb::Node {
        name: name.into(),
        children,
        ..Default::default()
    });
    doc.scene.push(doc.nodes.len() - 1);
}

fn add_clips(doc: &mut glb::Document, clips: &[(String, Animation)], bone_count: usize) {
    for (name, clip) in clips {
        let times: Vec<f32> = (0..clip.frame_count).map(|f| f as f32 / Animation::FPS).collect();
        let mut channels = Vec::new();
        for track in clip.tracks.iter().filter(|t| t.bone < bone_count) {
            channels.push(glb::Channel {
                node: track.bone,
                times: times.clone(),
                values: ChannelValues::Rotation(track.rotations.iter().map(|&r| rotation(r).to_array()).collect()),
            });
            channels.push(glb::Channel {
                node: track.bone,
                times: times.clone(),
                values: ChannelValues::Translation(track.translations.iter().map(|&t| translation(t).to_array()).collect()),
            });
        }
        doc.animations.push(glb::Animation {
            name: name.clone(),
            channels,
        });
    }
}

/// Every `.baf` directly inside `dir`, named by file stem. `skeleton` resolves version 3's
/// inline bone names (see `bf2_formats::anim`); pass the clips' own skeleton (body or weapon).
fn load_clips(vfs: &Vfs, dir: &str, skeleton: Option<&Skeleton>) -> Vec<(String, Animation)> {
    let mut clips: Vec<(String, Animation)> = vfs
        .list(dir)
        .filter(|p| p.ends_with(".baf") && !p[dir.len()..].contains('/'))
        .filter_map(|p| {
            let name = p.rsplit('/').next()?.trim_end_matches(".baf").to_string();
            let anim = Animation::parse(&vfs.read(p).ok()?, skeleton)
                .map_err(|e| log::warn!("{p}: {e}"))
                .ok()?;
            Some((name, anim))
        })
        .collect();
    clips.sort_by(|a, b| a.0.cmp(&b.0));
    clips
}

/// Some weapons name their standing reload clip something other than `reload` (BF2's own
/// underslung grenade launchers, e.g. the US `usrgl_m203`'s `3p_M203GL_reloadGrenade.baf` /
/// `1p_M203GL_reloadGrenade.baf`, where every other faction's launcher, e.g. `rurgl_gp30`,
/// uses plain `.../reload.baf`; both still have a distinct `pronereload`). The render code
/// (`render::soldiers`, `render::viewmodel`) looks up the one-shot by the exact name `reload`,
/// so without this the clip exists but is never found: no animation plays at all for a
/// standing reload, first or third person. Register the odd one under `reload` too (keeping
/// its original name) rather than hard-coding the M203's file name, so any weapon with the
/// same naming quirk is covered.
fn alias_reload_clip(clips: &mut Vec<(String, Animation)>) {
    if clips.iter().any(|(n, _)| n == "reload") {
        return;
    }
    if let Some(clip) = clips
        .iter()
        .find(|(n, _)| n != "pronereload" && n.starts_with("reload"))
        .map(|(_, c)| c.clone())
    {
        clips.push(("reload".to_string(), clip));
    }
}

/// Third-person upper-body animations of every handheld weapon, as animation sets
/// (`objects/weapons/handheld/<weapon>/animations/3p.glb`). Clips are named by state:
/// `3p_ak47_crouchstill` becomes `crouchstill`. Returns how many sets were written.
fn export_weapon_sets(vfs: &Vfs, skeleton: &Skeleton, view: &str, out: &Path) -> Result<usize> {
    let marker = format!("/animations/{view}/");
    let mut dirs: Vec<String> = vfs
        .list("objects/weapons/handheld/")
        .filter(|p| p.ends_with(".baf") && p.contains(&marker))
        .filter_map(|p| p.rsplit_once('/').map(|(dir, _)| format!("{dir}/")))
        .collect();
    dirs.sort();
    dirs.dedup();
    let mut written = 0;
    for dir in dirs {
        let mut clips = load_clips(vfs, &dir, Some(skeleton));
        if clips.is_empty() {
            continue;
        }
        // Strip `3p_`/`1p_` and the weapon token shared by all file names.
        let view_prefix = format!("{view}_");
        let stems: Vec<&str> = clips.iter().map(|(n, _)| n.trim_start_matches(view_prefix.as_str())).collect();
        let prefix_len = stems
            .first()
            .map(|first| {
                let common = stems.iter().fold(first.len(), |len, s| {
                    first.bytes().zip(s.bytes()).take(len).take_while(|(a, b)| a == b).count()
                });
                first[..common].rfind('_').map_or(0, |i| i + 1)
            })
            .unwrap_or(0);
        for (name, _) in &mut clips {
            let stem = name.trim_start_matches(view_prefix.as_str()).to_string();
            *name = stem.get(prefix_len..).unwrap_or(&stem).to_string();
        }
        alias_reload_clip(&mut clips);
        let mut doc = glb::Document::default();
        add_skeleton(&mut doc, skeleton);
        add_named_root(&mut doc, skeleton, &[], if view == "1p" { "firstperson" } else { "soldier" });
        add_clips(&mut doc, &clips, skeleton.bones.len());
        let out_rel = format!("{}.glb", dir.trim_end_matches('/'));
        doc.write(&out.join(&out_rel))?;
        written += 1;
    }
    Ok(written)
}

/// Imports every soldier body of `mods` (installed BF2 mod folder names, e.g. `bf2`, `AIX2`).
/// Returns the soldier names.
pub fn import_all(install: &Bf2Install, out: &Path, mods: &[String]) -> Result<Vec<String>> {
    let mut done = Vec::new();
    for mod_name in mods {
        let mod_name = mod_name.as_str();
        let mut vfs = Vfs::new();
        // One broken mod (missing/unreadable archives) must not abort the others' soldiers.
        if let Err(err) = install.mount_mod(&mut vfs, mod_name, Side::Both) {
            log::warn!("mounting {mod_name}: {err:#}");
            continue;
        }
        let Ok(skeleton_data) = vfs.read(SKELETON) else {
            continue;
        };
        let skeleton = Skeleton::parse(&skeleton_data).context("parsing 3p_setup.ske")?;

        let mut clips = load_clips(&vfs, ANIMATIONS, Some(&skeleton));
        clips.extend(load_clips(&vfs, LADDER_ANIMATIONS, Some(&skeleton)));
        clips.extend(load_clips(&vfs, SEAT_ANIMATIONS, Some(&skeleton)));
        for dir in ROPE_ANIMATIONS {
            clips.extend(load_clips(&vfs, dir, Some(&skeleton)));
        }
        clips.sort_by(|a, b| a.0.cmp(&b.0));
        let skeleton_1p = vfs
            .read(SKELETON_1P)
            .ok()
            .and_then(|d| Skeleton::parse(&d).map_err(|e| log::warn!("1p_setup.ske: {e}")).ok());
        for (view, skeleton) in [("3p", Some(&skeleton)), ("1p", skeleton_1p.as_ref())] {
            let Some(skeleton) = skeleton else { continue };
            match export_weapon_sets(&vfs, skeleton, view, out) {
                Ok(count) => log::info!("{mod_name}: {count} {view} weapon animation sets"),
                Err(err) => log::warn!("{mod_name} {view} weapon animations: {err:#}"),
            }
        }

        let converter = MeshConverter::new(&vfs, out);
        let mut interp = bf2_formats::con::Interpreter::new(&vfs);
        match export_flags(&vfs, &converter, out) {
            Ok(0) => {}
            Ok(count) => log::info!("{mod_name}: {count} flags"),
            Err(err) => log::warn!("{mod_name} flags: {err:#}"),
        }
        let mut meshes: Vec<String> = vfs
            .list("objects/soldiers/")
            .filter(|p| p.ends_with(".skinnedmesh"))
            .map(str::to_string)
            .collect();
        meshes.sort();
        for mesh_path in meshes {
            let name = mesh_path.rsplit('/').next().unwrap_or(&mesh_path).trim_end_matches(".skinnedmesh").to_string();
            if done.contains(&name) {
                continue;
            }
            match export(&vfs, &converter, &mesh_path, &skeleton, &clips, out) {
                Ok((glb_path, lod_count)) => {
                    let mesh_1p = skeleton_1p.as_ref().and_then(|skeleton| {
                        export_arms(&vfs, &converter, &mesh_path, skeleton, out)
                            .map_err(|e| log::warn!("1p arms {mesh_path}: {e:#}"))
                            .ok()
                    });
                    let desc = SoldierDesc {
                        name: name.clone(),
                        mesh: glb_path,
                        animations: clips.iter().map(|(n, _)| n.clone()).collect(),
                        mesh_1p,
                        hit_zones: crate::hitzones::hit_zones(&mut interp, &vfs, &skeleton, &name),
                        lods: body_lod_distances(&interp, &name, lod_count, converter.lod0_half_diagonal(&mesh_path, 1)),
                        draw_distance: crate::lods::pco_draw_distance(Some(soldier_cull_radius(&interp, &name))),
                        cull_radius: soldier_cull_radius(&interp, &name),
                    };
                    game_data::write_ron(out.join("soldiers").join(format!("{name}.ron")), &desc)?;
                    done.push(name);
                }
                Err(err) => log::warn!("soldier {mesh_path}: {err:#}"),
            }
        }
    }
    Ok(done)
}

/// Control point flags (`objects/common/flags/flag_*/meshes/flag_*.glb`): skinned cloth with
/// the waving `idle` clip.
fn export_flags(vfs: &Vfs, converter: &MeshConverter, out: &Path) -> Result<usize> {
    let Ok(data) = vfs.read(&format!("{FLAGS}flag_setup.ske")) else {
        return Ok(0);
    };
    let skeleton = Skeleton::parse(&data).context("parsing flag_setup.ske")?;
    let idle = Animation::parse(&vfs.read(&format!("{FLAGS}animations/flag_idle.baf"))?, Some(&skeleton))?;
    let clips = [("idle".to_string(), idle)];
    let meshes: Vec<String> = vfs
        .list(FLAGS)
        .filter(|p| p.ends_with(".skinnedmesh"))
        .map(str::to_string)
        .collect();
    for mesh_path in &meshes {
        let out_rel = format!("{}.glb", mesh_path.trim_end_matches(".skinnedmesh"));
        export_skinned(vfs, converter, mesh_path, 0, &skeleton, "flag", &clips, &out_rel, out, false)?;
    }
    Ok(meshes.len())
}

/// Third-person body (geom 1, all its LODs) with the `3p_setup` skeleton and movement clips.
/// Returns the file and the number of LODs in it.
fn export(
    vfs: &Vfs,
    converter: &MeshConverter,
    mesh_path: &str,
    skeleton: &Skeleton,
    clips: &[(String, Animation)],
    out: &Path,
) -> Result<(String, usize)> {
    let out_rel = format!("{}.glb", mesh_path.trim_end_matches(".skinnedmesh"));
    let lods = export_skinned(vfs, converter, mesh_path, 1, skeleton, "soldier", clips, &out_rel, out, true)?;
    Ok((out_rel, lods))
}

/// A soldier's cull radius: its built-in collision radius times its template's
/// `cullRadiusScale` (2.5 for all of them); BF2 draws it by the rule of player control
/// objects.
fn soldier_cull_radius(interp: &bf2_formats::con::Interpreter, name: &str) -> f32 {
    let scale = interp.world.template(name).and_then(|t| t.get_f32("cullradiusscale")).unwrap_or(1.0);
    crate::lods::SOLDIER_RADIUS * scale
}

/// Where the body's LODs 1.. take over (m): the soldier geometry template's
/// `setSubGeometryLodDistance` for geom 1 (or the engine's running default) times the
/// skinned mesh scale, plus `r0`, half the diagonal of the body's full-detail box.
fn body_lod_distances(interp: &bf2_formats::con::Interpreter, name: &str, lod_count: usize, r0: Option<f32>) -> Vec<f32> {
    let world = &interp.world;
    let geometry = world
        .template(name)
        .and_then(|t| t.geometry.as_deref())
        .and_then(|g| world.geometry(g))
        .or_else(|| world.geometry(name));
    crate::lods::lod_starts(&crate::lods::lod_distances(geometry, 1, lod_count))
        .into_iter()
        .map(|d| d * crate::lods::SKINNED_LOD_SCALE + r0.unwrap_or(0.0))
        .collect()
}

/// First-person arms (geom 0) with the `1p_setup` skeleton. A one-frame rest clip makes the
/// root an animation root, so weapon animation sets can drive every bone.
fn export_arms(
    vfs: &Vfs,
    converter: &MeshConverter,
    mesh_path: &str,
    skeleton: &Skeleton,
    out: &Path,
) -> Result<String> {
    let rest = Animation {
        frame_count: 2,
        tracks: vec![bf2_formats::anim::BoneTrack {
            bone: 0,
            rotations: vec![skeleton.bones[0].rotation; 2],
            translations: vec![skeleton.bones[0].translation; 2],
        }],
    };
    let out_rel = format!("{}_1p.glb", mesh_path.trim_end_matches(".skinnedmesh"));
    export_skinned(vfs, converter, mesh_path, 0, skeleton, "firstperson", &[("rest".into(), rest)], &out_rel, out, false)?;
    Ok(out_rel)
}

/// A skinned mesh (one geom) with its skeleton and clips as `out_rel`: mesh `body` is LOD 0;
/// with `lods`, the geom's further LODs follow as `body_lod1`, ... on the same skin. Returns
/// the number of LODs written.
#[allow(clippy::too_many_arguments)]
fn export_skinned(
    vfs: &Vfs,
    converter: &MeshConverter,
    mesh_path: &str,
    geom: usize,
    skeleton: &Skeleton,
    root_name: &str,
    clips: &[(String, Animation)],
    out_rel: &str,
    out: &Path,
    lods: bool,
) -> Result<usize> {
    let mesh = VisMesh::parse(&vfs.read(mesh_path)?, MeshKind::Skinned)?;
    let geom_lods = mesh
        .geoms
        .get(geom)
        .map(|g| g.lods.as_slice())
        .filter(|l| !l.is_empty())
        .with_context(|| format!("mesh has no geom {geom}"))?;
    let geom_lods = if lods { geom_lods } else { &geom_lods[..1] };
    let out_rel = out_rel.to_string();

    let mut doc = glb::Document::default();

    let bone_count = skeleton.bones.len();
    let world = add_skeleton(&mut doc, skeleton);
    // The bind pose is the skeleton's rest pose.
    doc.skins.push(glb::Skin {
        joints: (0..bone_count).collect(),
        inverse_bind_matrices: world.iter().map(|m| m.inverse().to_cols_array()).collect(),
    });

    let mut images: std::collections::HashMap<String, usize> = std::collections::HashMap::new();
    let mut bodies = Vec::new();
    for (lod_index, lod) in geom_lods.iter().enumerate() {
        let name = match lod_index {
            0 => "body".to_string(),
            n => format!("body_lod{n}"),
        };
        bodies.push(add_lod_node(&mut doc, &mesh, lod, converter, bone_count, &mut images, &name, &out_rel));
    }
    add_named_root(&mut doc, skeleton, &bodies, root_name);
    add_clips(&mut doc, clips, bone_count);

    doc.write(&out.join(&out_rel))?;
    Ok(bodies.len())
}

/// One LOD's skinned primitives (one per material) as a mesh + node named `name`, added to
/// `doc` (already holding the skeleton and its single skin). Returns the node's index. Shared
/// by [`export_skinned`] (a body's own LOD chain) and [`export_with_gear`] (a kit's gear geom,
/// which has no LODs of its own).
#[allow(clippy::too_many_arguments)]
fn add_lod_node(
    doc: &mut glb::Document,
    mesh: &VisMesh,
    lod: &Lod,
    converter: &MeshConverter,
    bone_count: usize,
    images: &mut std::collections::HashMap<String, usize>,
    name: &str,
    out_rel: &str,
) -> usize {
    let positions = mesh.attribute::<3>(Usage::Position, 0).unwrap_or_default();
    let normals = mesh.attribute::<3>(Usage::Normal, 0).unwrap_or_default();
    let uvs = mesh.attribute::<2>(Usage::TexCoord, 0).unwrap_or_default();
    let weights = mesh.attribute::<1>(Usage::BlendWeight, 0).unwrap_or_default();
    let blend = mesh.blend_indices().unwrap_or_default();
    let mut primitives = Vec::new();
    for (index, material) in lod.materials.iter().enumerate() {
        let Some(rig) = lod.rigs.get(index).or_else(|| lod.rigs.last()) else {
            continue;
        };
        let joint = |local: u8| -> u16 {
            rig.get(local as usize)
                .map(|b| b.ske_index as u16)
                .filter(|&j| (j as usize) < bone_count)
                .unwrap_or(0)
        };
        let maps = material.texture_maps();
        let mut image = |texture: Option<String>| {
            texture.map(|t| {
                *images.entry(t.clone()).or_insert_with(|| {
                    doc.images.push(glb::relative_uri(out_rel, &t));
                    doc.images.len() - 1
                })
            })
        };
        let color = image(maps.first().and_then(|m| converter.texture(m)));
        let normal = image(maps.get(1).and_then(|m| converter.texture(m)));
        // Without `tangent` in the technique the normal map is in object (bind pose) space.
        // The game rebuilds the object-to-world rotation per pixel from the vertex frame
        // before skinning, packed as COLOR_0 = (tangent, normal.x), TEXCOORD_1 = normal.yz.
        let object_space = normal.is_some() && !material.technique.to_ascii_lowercase().contains("tangent");
        let tangents = normal.map(|_| crate::meshes::tangent_frames(mesh, material, 0));
        doc.materials.push(glb::Material {
            name: material.technique.clone(),
            base_color: color,
            base_color_uv: 0,
            normal,
            alpha: if material.technique.to_ascii_lowercase().contains("alpha_test") {
                glb::AlphaMode::Mask(0.5)
            } else {
                glb::AlphaMode::Opaque
            },
            double_sided: false,
            extras: json!({ "bf2": { "kind": "skinned", "technique": material.technique, "maps": maps } }),
        });

        let mut primitive = glb::Primitive {
            material: Some(doc.materials.len() - 1),
            uvs: vec![Vec::new(); if object_space { 2 } else { 1 }],
            ..Default::default()
        };
        let mut remap = std::collections::HashMap::new();
        for tri in mesh.material_triangles(material) {
            if tri.iter().any(|&v| v as usize >= positions.len()) {
                continue;
            }
            for &v in [tri[0], tri[2], tri[1]].iter() {
                let index = *remap.entry(v).or_insert_with(|| {
                    let vi = v as usize;
                    let normal = translation(normals.get(vi).copied().unwrap_or([0.0, 1.0, 0.0])).normalize_or(Vec3::Y);
                    primitive.positions.push(translation(positions[vi]).to_array());
                    primitive.normals.push(normal.to_array());
                    primitive.uvs[0].push(uvs.get(vi).copied().unwrap_or_default());
                    if let Some(tangents) = &tangents {
                        let t = tangents.get(&v).copied().unwrap_or([1.0, 0.0, 0.0, 1.0]);
                        primitive.tangents.push(t);
                        if object_space {
                            primitive.colors.push([t[0], t[1], t[2], normal.x]);
                            primitive.uvs[1].push([normal.y, normal.z]);
                        }
                    }
                    let b = blend.get(vi).copied().unwrap_or_default();
                    let w = weights.get(vi).map_or(1.0, |w| w[0]).clamp(0.0, 1.0);
                    primitive.joints.push([joint(b[0]), joint(b[1]), 0, 0]);
                    primitive.weights.push([w, 1.0 - w, 0.0, 0.0]);
                    (primitive.positions.len() - 1) as u32
                });
                primitive.indices.push(index);
            }
        }
        if !primitive.indices.is_empty() {
            primitives.push(primitive);
        }
    }
    doc.meshes.push(glb::Mesh {
        name: name.to_string(),
        primitives,
    });
    doc.nodes.push(glb::Node {
        name: name.to_string(),
        mesh: Some(doc.meshes.len() - 1),
        skin: Some(0),
        ..Default::default()
    });
    doc.nodes.len() - 1
}

/// A soldier body plus its equipped kit's gear (vest, pack, helmet, ghillie suit, radio, ...),
/// both skinned to the same `3p_setup` skeleton so they animate together as one model: BF2
/// layers a kit's own mesh (a shared per-faction skinnedmesh, e.g. `us_kits.skinnedmesh`, one
/// geom per kit class) over the plain body (`ObjectTemplate.geometry.kit N` on the `Kit`
/// template picks the geom). The gear is exported as its own node (`gear`) at LOD 0 only: it
/// doesn't fade with distance the way the body's own LODs do, which is an acceptable
/// simplification given how small a soldier's gear silhouette is at range. Returns the number
/// of body LODs written (same convention as [`export`]).
fn export_with_gear(
    vfs: &Vfs,
    converter: &MeshConverter,
    body_mesh_path: &str,
    gear_mesh_path: &str,
    gear_geom: usize,
    skeleton: &Skeleton,
    clips: &[(String, Animation)],
    out_rel: &str,
    out: &Path,
) -> Result<usize> {
    let body_mesh = VisMesh::parse(&vfs.read(body_mesh_path)?, MeshKind::Skinned)?;
    let body_lods = body_mesh
        .geoms
        .get(1)
        .map(|g| g.lods.as_slice())
        .filter(|l| !l.is_empty())
        .with_context(|| format!("{body_mesh_path} has no geom 1"))?;
    let gear_mesh = VisMesh::parse(&vfs.read(gear_mesh_path)?, MeshKind::Skinned)?;
    let gear_lod = gear_mesh
        .geoms
        .get(gear_geom)
        .and_then(|g| g.lods.first())
        .with_context(|| format!("{gear_mesh_path} has no geom {gear_geom}"))?;

    let mut doc = glb::Document::default();
    let bone_count = skeleton.bones.len();
    let world = add_skeleton(&mut doc, skeleton);
    doc.skins.push(glb::Skin {
        joints: (0..bone_count).collect(),
        inverse_bind_matrices: world.iter().map(|m| m.inverse().to_cols_array()).collect(),
    });

    let mut images: std::collections::HashMap<String, usize> = std::collections::HashMap::new();
    let mut bodies = Vec::new();
    for (lod_index, lod) in body_lods.iter().enumerate() {
        let name = match lod_index {
            0 => "body".to_string(),
            n => format!("body_lod{n}"),
        };
        bodies.push(add_lod_node(&mut doc, &body_mesh, lod, converter, bone_count, &mut images, &name, out_rel));
    }
    bodies.push(add_lod_node(&mut doc, &gear_mesh, gear_lod, converter, bone_count, &mut images, "gear", out_rel));

    add_named_root(&mut doc, skeleton, &bodies, "soldier");
    add_clips(&mut doc, clips, bone_count);
    doc.write(&out.join(out_rel))?;
    Ok(body_lods.len())
}

/// A kit's worn-gear attachment: the shared per-faction kit skinnedmesh and which geom is this
/// kit's look (`ObjectTemplate.geometry` / `.geometry.kit` on the `Kit` template).
struct KitGear {
    mesh_path: String,
    geom: usize,
}

/// Reads every `Kit` template of a mounted mod (`objects/kits/**/*.con`) and its gear attachment, by
/// lowercased kit name (e.g. `us_assault`, matching `gameLogic.setKit`'s kit argument).
fn kit_gear(vfs: &Vfs) -> std::collections::HashMap<String, KitGear> {
    let mut interp = Interpreter::new(vfs);
    let mut paths: Vec<String> = vfs.list("objects/kits/").filter(|p| p.ends_with(".con")).map(str::to_string).collect();
    paths.sort();
    for path in &paths {
        interp.run(path, &[]);
    }
    interp
        .world
        .templates
        .values()
        .filter(|t| t.ty.eq_ignore_ascii_case("Kit"))
        .filter_map(|t| {
            let geometry = t.geometry.as_deref()?;
            let geom = t.get_f32("geometry.kit")? as usize;
            let mesh_path = interp.world.geometry(geometry)?.mesh_path()?;
            Some((t.name.to_ascii_lowercase(), KitGear { mesh_path, geom }))
        })
        .collect()
}

/// Combines each of `teams`' kit slots' soldier body with that kit's gear, writing
/// `soldiers/<body>__<kit>.ron` and pointing the slot at it, so each class looks like it does
/// in BF2 instead of every kit sharing the plain body. Kits whose gear can't be resolved (a
/// mod with no `Kit` template, or a level whose kit names don't match one) keep the plain body
/// they already had (`fix_missing_kit_soldiers` runs before this).
pub fn import_kit_gear(vfs: &Vfs, converter: &MeshConverter, teams: &mut [game_data::TeamDesc], out: &Path) {
    let Ok(skeleton_data) = vfs.read(SKELETON) else {
        return;
    };
    let Ok(skeleton) = Skeleton::parse(&skeleton_data) else {
        return;
    };
    let clips = load_clips(vfs, ANIMATIONS, Some(&skeleton));
    let gear = kit_gear(vfs);
    let body_meshes: std::collections::HashMap<String, String> = vfs
        .list("objects/soldiers/")
        .filter(|p| p.ends_with(".skinnedmesh"))
        .map(|p| (p.rsplit('/').next().unwrap_or(p).trim_end_matches(".skinnedmesh").to_ascii_lowercase(), p.to_string()))
        .collect();
    for team in teams.iter_mut() {
        for slot in &mut team.kits {
            let Some(gear) = gear.get(&slot.kit) else {
                log::debug!("kit gear: no gear template for kit `{}`", slot.kit);
                continue;
            };
            let Some(body_mesh_path) = body_meshes.get(&slot.soldier) else {
                log::debug!("kit gear: no body mesh for `{}` (kit `{}`)", slot.soldier, slot.kit);
                continue;
            };
            let key = format!("{}__{}", slot.soldier, slot.kit);
            let ron_path = out.join("soldiers").join(format!("{key}.ron"));
            if !ron_path.exists() {
                // The plain body's own `.ron` (written by `import_all`) carries the
                // first-person arms and, as a fallback, its hit zones/LODs: reuse its arms
                // (gear isn't worn in first person; kit gear only changes the third-person
                // look) so `render::viewmodel::load_arms` still finds a model.
                let plain = game_data::read_ron::<SoldierDesc>(out.join("soldiers").join(format!("{}.ron", slot.soldier))).ok();
                let out_rel = format!("objects/soldiers/kits/{key}.glb");
                match export_with_gear(vfs, converter, body_mesh_path, &gear.mesh_path, gear.geom, &skeleton, &clips, &out_rel, out) {
                    Ok(lod_count) => {
                        let mut interp = Interpreter::new(vfs);
                        let desc = SoldierDesc {
                            name: key.clone(),
                            mesh: out_rel,
                            animations: clips.iter().map(|(n, _)| n.clone()).collect(),
                            mesh_1p: plain.as_ref().and_then(|p| p.mesh_1p.clone()),
                            hit_zones: crate::hitzones::hit_zones(&mut interp, vfs, &skeleton, &slot.soldier),
                            lods: body_lod_distances(&interp, &slot.soldier, lod_count, converter.lod0_half_diagonal(body_mesh_path, 1)),
                            draw_distance: crate::lods::pco_draw_distance(Some(soldier_cull_radius(&interp, &slot.soldier))),
                            cull_radius: soldier_cull_radius(&interp, &slot.soldier),
                        };
                        if let Err(err) = game_data::write_ron(&ron_path, &desc) {
                            log::warn!("kit gear {key}: writing soldier desc: {err:#}");
                            continue;
                        }
                    }
                    Err(err) => {
                        log::debug!("kit gear {key}: {err:#}");
                        continue;
                    }
                }
            }
            slot.soldier = key;
        }
    }
}
