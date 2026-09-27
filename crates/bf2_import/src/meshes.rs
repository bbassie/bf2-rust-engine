//! Converts BF2 meshes to `.glb` files and copies the textures they use.

use std::{
    collections::HashMap,
    path::PathBuf,
    sync::{Arc, Mutex, OnceLock},
};

use anyhow::{Context, Result};
use bf2_formats::{
    Vfs,
    collision::{ColType, CollisionMesh},
    mesh::{AlphaMode, MeshKind, Usage, VisMesh},
    vfs::normalize,
};
use serde_json::json;

use crate::{
    coords,
    glb::{self, Document},
};

/// Shared state for converting many meshes: output folder and already-copied textures.
pub struct MeshConverter<'a> {
    pub vfs: &'a Vfs,
    pub out: PathBuf,
    textures: Mutex<HashMap<String, Option<String>>>,
    /// Each output file is produced once, even when several threads ask for it.
    jobs: Mutex<HashMap<String, Arc<OnceLock<Result<String, String>>>>>,
}

impl<'a> MeshConverter<'a> {
    pub fn new(vfs: &'a Vfs, out: impl Into<PathBuf>) -> Self {
        Self {
            vfs,
            out: out.into(),
            textures: Mutex::default(),
            jobs: Mutex::default(),
        }
    }

    fn once(&self, key: &str, work: impl FnOnce() -> Result<String>) -> Result<String> {
        let cell = self
            .jobs
            .lock()
            .unwrap()
            .entry(key.to_string())
            .or_default()
            .clone();
        cell.get_or_init(|| work().map_err(|e| format!("{e:#}")))
            .clone()
            .map_err(anyhow::Error::msg)
    }

    /// Copies a texture referenced by a mesh into the output folder. Returns its path relative
    /// to the output root, or `None` if the game data doesn't contain it.
    pub fn texture(&self, reference: &str) -> Option<String> {
        let key = normalize(reference);
        if let Some(done) = self.textures.lock().unwrap().get(&key) {
            return done.clone();
        }
        let result = self.copy_texture(&key);
        if result.is_none() {
            log::debug!("texture not found: {reference}");
        }
        self.textures.lock().unwrap().insert(key, result.clone());
        result
    }

    /// Copies any file (sounds, ...) from the game data into the output folder as-is.
    pub fn file(&self, reference: &str) -> Option<String> {
        let key = normalize(reference.trim_matches('"'));
        if !self.vfs.exists(&key) {
            log::debug!("file not found: {reference}");
            return None;
        }
        let target = self.out.join(&key);
        if !target.exists() {
            let data = self.vfs.read(&key).ok()?;
            std::fs::create_dir_all(target.parent()?).ok()?;
            std::fs::write(&target, data).ok()?;
        }
        Some(key)
    }

    fn copy_texture(&self, key: &str) -> Option<String> {
        // Absolute DICE build paths: keep what follows `/mods/<mod>/`.
        let key = match key.find("/mods/") {
            Some(i) => key[i + 6..].split_once('/').map_or(key, |(_, rest)| rest),
            None => key,
        };
        let candidates = [key.to_string(), format!("{key}.dds")];
        let path = candidates.into_iter().find(|c| self.vfs.exists(c))?;
        let data = self.vfs.read(&path).ok()?;
        let target = self.out.join(&path);
        if !target.exists() {
            std::fs::create_dir_all(target.parent()?).ok()?;
            std::fs::write(&target, data).ok()?;
        }
        Some(path)
    }

    /// Largest distance of any vertex of the first geom/LOD from the origin.
    pub fn mesh_radius(&self, mesh_path: &str) -> Option<f32> {
        let key = normalize(mesh_path);
        let mesh = VisMesh::parse(&self.vfs.read(&key).ok()?, MeshKind::from_path(&key)?).ok()?;
        let lod = mesh.geoms.first()?.lods.first()?;
        let corner = |a: [f32; 3], b: [f32; 3]| {
            (0..3).map(|i| a[i].abs().max(b[i].abs()).powi(2)).sum::<f32>().sqrt()
        };
        Some(corner(lod.bounds_min, lod.bounds_max))
    }

    /// Converts a visible mesh (`objects/.../meshes/x.staticmesh`) to `objects/.../meshes/x.glb`.
    /// Returns the output path relative to the output root.
    pub fn convert_mesh(&self, mesh_path: &str) -> Result<String> {
        self.convert_mesh_geom(mesh_path, 0, "")
    }

    /// Converts one geom of a mesh to `x{suffix}.glb` (weapons: geom 0 is first person,
    /// geom 1 third person).
    pub fn convert_mesh_geom(&self, mesh_path: &str, geom: usize, suffix: &str) -> Result<String> {
        self.convert_mesh_lod(mesh_path, geom, 0, suffix)
    }

    /// Converts one LOD of one geom to `x{suffix}.glb` (weapons: first-person LOD 1 is the
    /// zoomed view).
    pub fn convert_mesh_lod(&self, mesh_path: &str, geom: usize, lod: usize, suffix: &str) -> Result<String> {
        let key = normalize(mesh_path);
        let job = format!("{key}#{geom}.{lod}{suffix}");
        let suffix = suffix.to_string();
        self.once(&job, || self.convert_mesh_inner(key, geom, lod, &suffix))
    }

    fn convert_mesh_inner(&self, key: String, geom: usize, lod: usize, suffix: &str) -> Result<String> {
        let kind = MeshKind::from_path(&key).context("not a mesh file")?;
        let out_rel = format!("{}{suffix}.glb", key.rsplit_once('.').map_or(key.as_str(), |(s, _)| s));
        if self.out.join(&out_rel).exists() {
            return Ok(out_rel);
        }
        let data = self.vfs.read(&key)?;
        let mesh = VisMesh::parse(&data, kind).with_context(|| format!("parsing {key}"))?;
        anyhow::ensure!(
            mesh.geoms.get(geom).is_some_and(|g| lod < g.lods.len()),
            "{key} has no geom {geom} LOD {lod}"
        );
        let doc = self.build_document(&mesh, geom, lod, &out_rel);
        doc.write(&self.out.join(&out_rel))
            .with_context(|| format!("writing {out_rel}"))?;
        Ok(out_rel)
    }

    fn build_document(&self, mesh: &VisMesh, geom: usize, lod: usize, out_rel: &str) -> Document {
        let mut doc = Document::default();
        let mut image_index: HashMap<String, usize> = HashMap::new();
        let mut image = |doc: &mut Document, texture: &str| -> usize {
            *image_index.entry(texture.to_string()).or_insert_with(|| {
                doc.images.push(glb::relative_uri(out_rel, texture));
                doc.images.len() - 1
            })
        };

        let positions = mesh.attribute::<3>(Usage::Position, 0).unwrap_or_default();
        let normals = mesh.attribute::<3>(Usage::Normal, 0);
        let tangents = mesh.attribute::<3>(Usage::Tangent, 0);
        let blend = mesh.blend_indices();
        let uv_sets: Vec<Vec<[f32; 2]>> = (0..mesh.texcoord_sets())
            .map(|i| mesh.attribute::<2>(Usage::TexCoord, i).unwrap_or_default())
            .collect();

        let Some(lod) = mesh.geoms.get(geom).and_then(|g| g.lods.get(lod)) else {
            return doc;
        };

        // Bundled meshes: one glTF mesh per part (vertices are part-local and placed by the
        // template hierarchy). Other kinds: a single mesh.
        let part_count = match mesh.kind {
            MeshKind::Bundled => lod.part_count.max(1) as usize,
            _ => 1,
        };
        let mut part_primitives: Vec<Vec<glb::Primitive>> = (0..part_count).map(|_| Vec::new()).collect();

        for (material_index, material) in lod.materials.iter().enumerate() {
            let maps: Vec<(String, Option<String>)> = material
                .texture_maps()
                .into_iter()
                .map(|m| (m.to_string(), self.texture(m)))
                .collect();
            let (base_slot, base_uv) = base_color_slot(mesh.kind, &material.technique);
            let base = maps.get(base_slot).and_then(|(_, t)| t.clone());
            let normal = match mesh.kind {
                MeshKind::Bundled => maps
                    .iter()
                    .find(|(m, _)| m.to_ascii_lowercase().trim_end_matches(".dds").ends_with("_b"))
                    .and_then(|(_, t)| t.clone()),
                _ => None,
            };
            let alpha = match material.alpha_mode {
                AlphaMode::Opaque => glb::AlphaMode::Opaque,
                AlphaMode::Test => glb::AlphaMode::Mask(0.5),
                AlphaMode::Blend => glb::AlphaMode::Blend,
            };
            let base_image = base.as_ref().map(|t| image(&mut doc, t));
            let normal_image = normal.as_ref().map(|t| image(&mut doc, t));
            doc.materials.push(glb::Material {
                name: format!("{}_{material_index}", material.technique),
                base_color: base_image,
                base_color_uv: base_uv.min(1),
                normal: normal_image,
                alpha,
                double_sided: alpha != glb::AlphaMode::Opaque,
                extras: json!({
                    "bf2": {
                        "technique": material.technique,
                        "alpha_mode": format!("{:?}", material.alpha_mode),
                        "maps": maps.iter().map(|(m, t)| json!({ "ref": m, "path": t })).collect::<Vec<_>>(),
                    }
                }),
            });
            let material_id = doc.materials.len() - 1;

            // Remap the material's vertices into compact per-part primitives.
            let mut remap: Vec<HashMap<u32, u32>> = vec![HashMap::new(); part_count];
            let mut prims: Vec<glb::Primitive> = (0..part_count)
                .map(|_| glb::Primitive {
                    material: Some(material_id),
                    uvs: vec![Vec::new(); uv_sets.len().min(2)],
                    ..Default::default()
                })
                .collect();
            for tri in mesh.material_triangles(material) {
                if tri.iter().any(|&v| v as usize >= positions.len()) {
                    continue;
                }
                let part = match (&blend, mesh.kind) {
                    (Some(b), MeshKind::Bundled) => (b[tri[0] as usize][0] as usize).min(part_count - 1),
                    _ => 0,
                };
                let prim = &mut prims[part];
                // Reverse winding: the Z mirror flips handedness.
                for &v in [tri[0], tri[2], tri[1]].iter() {
                    let index = *remap[part].entry(v).or_insert_with(|| {
                        let vi = v as usize;
                        prim.positions.push(coords::position(positions[vi]));
                        if let Some(n) = &normals {
                            prim.normals.push(normalize_or_up(coords::direction(n[vi])));
                        }
                        if let Some(t) = &tangents {
                            let flip = blend.as_ref().map_or(0, |b| b[vi][2]);
                            let t = coords::direction(t[vi]);
                            let w = if flip == 0 { 1.0 } else { -1.0 };
                            prim.tangents.push([t[0], t[1], t[2], w]);
                        }
                        for (set, uv) in prim.uvs.iter_mut().enumerate() {
                            // UV set 0 = BF2 channel 0; set 1 = the base color channel if it
                            // isn't 0 (detail), so the glTF base color texCoord works.
                            let channel = if set == 1 { base_uv.max(1) as usize } else { 0 };
                            let value = uv_sets.get(channel).and_then(|s| s.get(vi)).copied().unwrap_or([0.0, 0.0]);
                            uv.push(sanitize_uv(value));
                        }
                        (prim.positions.len() - 1) as u32
                    });
                    prim.indices.push(index);
                }
            }
            for (part, prim) in prims.into_iter().enumerate() {
                if !prim.indices.is_empty() {
                    part_primitives[part].push(prim);
                }
            }
        }

        // Tangents are unreliable in old files; drop them if any are degenerate.
        for prims in &mut part_primitives {
            for prim in prims {
                if prim.tangents.iter().any(|t| t[0] * t[0] + t[1] * t[1] + t[2] * t[2] < 0.5) {
                    prim.tangents.clear();
                }
            }
        }

        for (part, primitives) in part_primitives.into_iter().enumerate() {
            doc.meshes.push(glb::Mesh {
                name: format!("part{part}"),
                primitives,
            });
            doc.nodes.push(glb::Node {
                name: format!("part{part}"),
                mesh: Some(part),
                ..Default::default()
            });
            doc.scene.push(part);
        }
        doc
    }

    /// Converts a `.collisionmesh` to `x.collision.glb`: one mesh per part and col type,
    /// named `part{N}_{type}`. Returns the path relative to the output root.
    pub fn convert_collision(&self, path: &str) -> Result<String> {
        let key = normalize(path);
        self.once(&key.clone(), || self.convert_collision_inner(key))
    }

    fn convert_collision_inner(&self, key: String) -> Result<String> {
        let stem = key.rsplit_once('.').map_or(key.as_str(), |(s, _)| s);
        let out_rel = format!("{stem}.collision.glb");
        if self.out.join(&out_rel).exists() {
            return Ok(out_rel);
        }
        let data = self.vfs.read(&key)?;
        let collision = CollisionMesh::parse(&data).with_context(|| format!("parsing {key}"))?;
        let mut doc = Document::default();
        for (part_index, part) in collision.parts.iter().enumerate() {
            // Geom 0 (vehicles: 1 is 3rd person, but statics only have one).
            let geom = part
                .geoms
                .get(1)
                .filter(|g| !g.cols.is_empty() && part.geoms[0].cols.is_empty())
                .or_else(|| part.geoms.first());
            let Some(geom) = geom else { continue };
            for col in &geom.cols {
                let kind = match col.col_type {
                    ColType::Projectile => "projectile",
                    ColType::Vehicle => "vehicle",
                    ColType::Soldier => "soldier",
                    ColType::Ai => "ai",
                    ColType::Unknown(_) => continue,
                };
                let primitive = glb::Primitive {
                    positions: col.vertices.iter().map(|&v| coords::position(v)).collect(),
                    // Collision winding points inward in BF2; the mirror makes it outward CCW.
                    indices: col
                        .faces
                        .iter()
                        .flat_map(|f| [f[0] as u32, f[1] as u32, f[2] as u32])
                        .collect(),
                    ..Default::default()
                };
                let mesh_index = doc.meshes.len();
                doc.meshes.push(glb::Mesh {
                    name: format!("part{part_index}_{kind}"),
                    primitives: vec![primitive],
                });
                doc.nodes.push(glb::Node {
                    name: format!("part{part_index}_{kind}"),
                    mesh: Some(mesh_index),
                    ..Default::default()
                });
                doc.scene.push(doc.nodes.len() - 1);
            }
        }
        doc.write(&self.out.join(&out_rel))?;
        Ok(out_rel)
    }
}

/// Which texture map is the main color and which UV channel it uses.
///
/// Static meshes list maps in technique-layer order (Base, Detail, Dirt, ...), bundled and
/// skinned meshes start with the color map. All of these put the first map on UV0. Proper
/// BF2 layering (Base × Detail × Dirt, cracks, lightmaps) needs a custom material; the full
/// map list is kept in the material extras for that.
fn base_color_slot(_kind: MeshKind, _technique: &str) -> (usize, u32) {
    (0, 0)
}

fn normalize_or_up(v: [f32; 3]) -> [f32; 3] {
    let len = (v[0] * v[0] + v[1] * v[1] + v[2] * v[2]).sqrt();
    if len > 1e-6 && len.is_finite() {
        [v[0] / len, v[1] / len, v[2] / len]
    } else {
        [0.0, 1.0, 0.0]
    }
}

fn sanitize_uv(uv: [f32; 2]) -> [f32; 2] {
    if uv[0].is_finite() && uv[1].is_finite() {
        uv
    } else {
        [0.0, 0.0]
    }
}
