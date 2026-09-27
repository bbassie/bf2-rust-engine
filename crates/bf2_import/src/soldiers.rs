//! Soldier bodies: skinned mesh + `3p_setup` skeleton + third-person animations, one `.glb`
//! per soldier model.

use std::path::Path;

use anyhow::{Context, Result};
use bf2_formats::{
    Bf2Install, Side, Vfs,
    anim::{Animation, Skeleton},
    mesh::{MeshKind, Usage, VisMesh},
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

/// Every `.baf` directly inside `dir`, named by file stem.
fn load_clips(vfs: &Vfs, dir: &str) -> Vec<(String, Animation)> {
    let mut clips: Vec<(String, Animation)> = vfs
        .list(dir)
        .filter(|p| p.ends_with(".baf") && !p[dir.len()..].contains('/'))
        .filter_map(|p| {
            let name = p.rsplit('/').next()?.trim_end_matches(".baf").to_string();
            let anim = Animation::parse(&vfs.read(p).ok()?)
                .map_err(|e| log::warn!("{p}: {e}"))
                .ok()?;
            Some((name, anim))
        })
        .collect();
    clips.sort_by(|a, b| a.0.cmp(&b.0));
    clips
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
        let mut clips = load_clips(vfs, &dir);
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

/// Imports every soldier body of every installed mod. Returns the soldier names.
pub fn import_all(install: &Bf2Install, out: &Path) -> Result<Vec<String>> {
    let mut done = Vec::new();
    for mod_name in install.mods() {
        let mut vfs = Vfs::new();
        install.mount_mod(&mut vfs, &mod_name, Side::Both)?;
        let Ok(skeleton_data) = vfs.read(SKELETON) else {
            continue;
        };
        let skeleton = Skeleton::parse(&skeleton_data).context("parsing 3p_setup.ske")?;

        let mut clips = load_clips(&vfs, ANIMATIONS);
        clips.extend(load_clips(&vfs, SEAT_ANIMATIONS));
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
                Ok(glb_path) => {
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
    let idle = Animation::parse(&vfs.read(&format!("{FLAGS}animations/flag_idle.baf"))?)?;
    let clips = [("idle".to_string(), idle)];
    let meshes: Vec<String> = vfs
        .list(FLAGS)
        .filter(|p| p.ends_with(".skinnedmesh"))
        .map(str::to_string)
        .collect();
    for mesh_path in &meshes {
        let out_rel = format!("{}.glb", mesh_path.trim_end_matches(".skinnedmesh"));
        export_skinned(vfs, converter, mesh_path, 0, &skeleton, "flag", &clips, &out_rel, out)?;
    }
    Ok(meshes.len())
}

/// Third-person body (geom 1) with the `3p_setup` skeleton and movement clips.
fn export(
    vfs: &Vfs,
    converter: &MeshConverter,
    mesh_path: &str,
    skeleton: &Skeleton,
    clips: &[(String, Animation)],
    out: &Path,
) -> Result<String> {
    let out_rel = format!("{}.glb", mesh_path.trim_end_matches(".skinnedmesh"));
    export_skinned(vfs, converter, mesh_path, 1, skeleton, "soldier", clips, &out_rel, out)?;
    Ok(out_rel)
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
    export_skinned(vfs, converter, mesh_path, 0, skeleton, "firstperson", &[("rest".into(), rest)], &out_rel, out)?;
    Ok(out_rel)
}

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
) -> Result<()> {
    let mesh = VisMesh::parse(&vfs.read(mesh_path)?, MeshKind::Skinned)?;
    let lod = mesh
        .geoms
        .get(geom)
        .and_then(|g| g.lods.first())
        .with_context(|| format!("mesh has no geom {geom}"))?;
    let out_rel = out_rel.to_string();

    let mut doc = glb::Document::default();

    let bone_count = skeleton.bones.len();
    let world = add_skeleton(&mut doc, skeleton);
    // The bind pose is the skeleton's rest pose.
    doc.skins.push(glb::Skin {
        joints: (0..bone_count).collect(),
        inverse_bind_matrices: world.iter().map(|m| m.inverse().to_cols_array()).collect(),
    });

    // Skinned primitives, one per material.
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
                doc.images.push(glb::relative_uri(&out_rel, &t));
                doc.images.len() - 1
            })
        };
        let color = image(maps.first().and_then(|m| converter.texture(m)));
        let normal = image(maps.get(1).and_then(|m| converter.texture(m)));
        // Without `tangent` in the technique the normal map is in object (bind pose) space.
        // The game rebuilds the object-to-world rotation per pixel from the vertex frame
        // before skinning, packed as COLOR_0 = (tangent, normal.x), TEXCOORD_1 = normal.yz.
        let object_space = normal.is_some() && !material.technique.to_ascii_lowercase().contains("tangent");
        let tangents = normal.map(|_| crate::meshes::tangent_frames(&mesh, material, 0));
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
        name: "body".into(),
        primitives,
    });
    doc.nodes.push(glb::Node {
        name: "body".into(),
        mesh: Some(0),
        skin: Some(0),
        ..Default::default()
    });
    let body = doc.nodes.len() - 1;
    add_named_root(&mut doc, skeleton, &[body], root_name);
    add_clips(&mut doc, clips, bone_count);

    doc.write(&out.join(&out_rel))?;
    Ok(())
}
