//! Compiled road meshes to `.glb` decals.

use std::path::Path;

use anyhow::{Context, Result};
use bf2_formats::{
    Vfs,
    con::World,
    road::{CompiledRoad, RoadTextures},
    vfs::normalize,
};
use game_data::RoadDesc;
use serde_json::json;

use crate::{coords, glb, meshes::MeshConverter};

/// Lift above the terrain so the decal doesn't z-fight with it.
const LIFT: f32 = 0.04;

/// Converts every road placed by the level's `CompiledRoads.con`.
pub fn import(
    vfs: &Vfs,
    world: &World,
    converter: &MeshConverter,
    level: &str,
    out: &Path,
) -> Vec<RoadDesc> {
    let mut roads = Vec::new();
    for instance in &world.instances {
        let Some(mesh_path) = instance
            .props
            .iter()
            .rev()
            .find(|(method, _)| method == "geometry.loadmesh")
            .and_then(|(_, args)| args.first())
        else {
            continue;
        };
        match convert(vfs, converter, &instance.template, mesh_path, level, out) {
            Ok(road) => roads.push(road),
            Err(err) => log::warn!("road {mesh_path}: {err:#}"),
        }
    }
    roads
}

fn convert(
    vfs: &Vfs,
    converter: &MeshConverter,
    template: &str,
    mesh_path: &str,
    level: &str,
    out: &Path,
) -> Result<RoadDesc> {
    let road = CompiledRoad::parse(&vfs.read(mesh_path)?)?;
    let dat = format!("objects/roads/splines/{}_compiled.dat", template.to_ascii_lowercase());
    let textures = RoadTextures::parse(&vfs.read(&dat).with_context(|| format!("reading {dat}"))?)?;

    let stem = normalize(mesh_path);
    let stem = stem.rsplit('/').next().unwrap_or(&stem);
    let stem = stem.strip_suffix(".mesh").unwrap_or(stem);
    let out_rel = format!("levels/{level}/roads/{stem}.glb");

    let mut doc = glb::Document::default();
    let primary = converter.texture(&textures.primary);
    let secondary = converter.texture(&textures.secondary);
    if let Some(texture) = &primary {
        doc.images.push(glb::relative_uri(&out_rel, texture));
    }
    doc.materials.push(glb::Material {
        name: template.to_string(),
        base_color: primary.as_ref().map(|_| 0),
        base_color_uv: 0,
        normal: None,
        alpha: glb::AlphaMode::Blend,
        double_sided: true,
        extras: json!({
            "bf2": {
                "technique": "Road",
                "maps": [
                    { "ref": textures.primary, "path": primary },
                    { "ref": textures.secondary, "path": secondary },
                ],
                "blend": textures.blend,
            }
        }),
    });

    let mut primitive = glb::Primitive {
        material: Some(0),
        uvs: vec![Vec::new(), Vec::new()],
        ..Default::default()
    };
    for v in &road.vertices {
        let p = coords::position(v.position);
        primitive.positions.push([p[0], p[1] + LIFT, p[2]]);
        primitive.normals.push([0.0, 1.0, 0.0]);
        primitive.uvs[0].push(v.uv0);
        primitive.uvs[1].push(v.uv1);
        primitive.colors.push([1.0, 1.0, 1.0, v.alpha.clamp(0.0, 1.0)]);
    }
    // Reverse winding for the mirror.
    for tri in road.indices.chunks_exact(3) {
        primitive
            .indices
            .extend_from_slice(&[tri[0] as u32, tri[2] as u32, tri[1] as u32]);
    }
    doc.meshes.push(glb::Mesh {
        name: stem.to_string(),
        primitives: vec![primitive],
    });
    doc.nodes.push(glb::Node {
        name: stem.to_string(),
        mesh: Some(0),
        ..Default::default()
    });
    doc.scene.push(0);
    doc.write(&out.join(&out_rel))?;

    Ok(RoadDesc {
        mesh: out_rel,
        position: coords::position(road.position),
    })
}
