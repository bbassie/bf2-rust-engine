//! Converts BF2 meshes to `.glb` files and copies the textures they use.

use std::{
    collections::HashMap,
    io::Read,
    path::{Path, PathBuf},
    sync::{Arc, Mutex, OnceLock},
};

use anyhow::{Context, Result};
use bf2_formats::{
    Vfs,
    collision::{ColType, CollisionMesh},
    mesh::{AlphaMode, Material, MeshKind, Usage, VisMesh},
    vfs::normalize,
};
use glam::{Vec2, Vec3};
use serde_json::json;

use crate::{
    coords,
    glb::{self, Document},
};

/// Version of the visible mesh conversion; older `.glb` files are converted again.
/// 3: static meshes carry their lightmap UVs (`_LIGHTMAP_UV`).
const MESH_VERSION: u64 = 3;
/// Version of rigged (vehicle) meshes, which also split BF2's UV-animated faces.
const RIGGED_MESH_VERSION: u64 = 6;

/// glTF vertex attribute with a static mesh's lightmap UVs (BF2's last UV set, as stored:
/// no V flip), for meshes with at least 3 UV sets. The client registers it as a custom
/// attribute.
pub const LIGHTMAP_UV_ATTRIBUTE: &str = "_LIGHTMAP_UV";

/// The UV set holding a mesh's lightmap UVs: the last of 3 or more sets of a static mesh
/// that isn't vegetation (`out_rel` is the output path, which keeps the source path).
fn lightmap_uv_set(kind: MeshKind, uv_sets: usize, out_rel: &str) -> Option<usize> {
    (kind == MeshKind::Static && uv_sets >= 3 && !out_rel.to_ascii_lowercase().contains("vegitation"))
        .then(|| uv_sets - 1)
}

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
        self.once(&job, || self.convert_mesh_inner(key, geom, lod, &suffix, false))
    }

    /// Converts one geom of a bundled mesh to `x{suffix}.glb` as a single skinned mesh whose
    /// joint `n` is part `n` (vehicles): each vertex follows its own part, so triangles
    /// spanning parts (track belts between hull and wheels) stretch between them instead of
    /// tearing. The joints' inverse bind matrices are identities: vertices are part-local.
    pub fn convert_mesh_rigged(&self, mesh_path: &str, geom: usize, suffix: &str) -> Result<String> {
        let key = normalize(mesh_path);
        let job = format!("{key}#{geom}.rig{suffix}");
        let suffix = suffix.to_string();
        self.once(&job, || self.convert_mesh_inner(key, geom, 0, &suffix, true))
    }

    fn convert_mesh_inner(&self, key: String, geom: usize, lod: usize, suffix: &str, rigged: bool) -> Result<String> {
        let kind = MeshKind::from_path(&key).context("not a mesh file")?;
        let out_rel = format!("{}{suffix}.glb", key.rsplit_once('.').map_or(key.as_str(), |(s, _)| s));
        let version = if rigged { RIGGED_MESH_VERSION } else { MESH_VERSION };
        if mesh_version(&self.out.join(&out_rel)).is_some_and(|v| v >= version) {
            return Ok(out_rel);
        }
        let data = self.vfs.read(&key)?;
        let mesh = VisMesh::parse(&data, kind).with_context(|| format!("parsing {key}"))?;
        anyhow::ensure!(
            mesh.geoms.get(geom).is_some_and(|g| lod < g.lods.len()),
            "{key} has no geom {geom} LOD {lod}"
        );
        let doc = self.build_document(&mesh, geom, lod, &out_rel, rigged && mesh.kind == MeshKind::Bundled);
        doc.write(&self.out.join(&out_rel))
            .with_context(|| format!("writing {out_rel}"))?;
        Ok(out_rel)
    }

    fn build_document(&self, mesh: &VisMesh, geom: usize, lod: usize, out_rel: &str, rigged: bool) -> Document {
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
        let blend = mesh.blend_indices();
        let uv_sets: Vec<Vec<[f32; 2]>> = (0..mesh.texcoord_sets())
            .map(|i| mesh.attribute::<2>(Usage::TexCoord, i).unwrap_or_default())
            .collect();
        let uv = |channel: usize, vertex: usize| -> [f32; 2] {
            let set = uv_sets.get(channel).or(uv_sets.last());
            sanitize_uv(set.and_then(|s| s.get(vertex)).copied().unwrap_or([0.0, 0.0]))
        };
        // Static meshes with 3+ UV sets keep their lightmap UVs (the last set) as
        // `_LIGHTMAP_UV`, for the client's baked lighting. Vegetation has none.
        let lightmap_set = lightmap_uv_set(mesh.kind, uv_sets.len(), out_rel);

        let Some(lod) = mesh.geoms.get(geom).and_then(|g| g.lods.get(lod)) else {
            return doc;
        };
        doc.extras = json!({ "bf2_mesh_version": if rigged { RIGGED_MESH_VERSION } else { MESH_VERSION } });

        // Bundled meshes: one glTF mesh per part (vertices are part-local and placed by the
        // template hierarchy). Other kinds: a single mesh.
        let part_count = match mesh.kind {
            MeshKind::Bundled => lod.part_count.max(1) as usize,
            _ => 1,
        };
        // Rigged: all parts in one mesh, each vertex skinned to its part.
        let buckets = if rigged { 1 } else { part_count };
        let mut part_primitives: Vec<Vec<glb::Primitive>> = (0..buckets).map(|_| Vec::new()).collect();

        let bands = match &blend {
            Some(b) if rigged => uv_animation_bands(mesh, lod, b, &positions),
            _ => HashMap::new(),
        };
        for (material_index, material) in lod.materials.iter().enumerate() {
            let maps: Vec<(String, Option<String>)> = material
                .texture_maps()
                .into_iter()
                .map(|m| (m.to_string(), self.texture(m)))
                .collect();
            let (base_slot, base_uv) = base_color_slot(mesh.kind, &material.technique);
            let base = maps.get(base_slot).and_then(|(_, t)| t.clone());
            let layers = (mesh.kind == MeshKind::Static).then(|| StaticLayers::parse(&material.technique));
            let normal = match mesh.kind {
                MeshKind::Bundled => maps
                    .iter()
                    .find(|(m, _)| m.to_ascii_lowercase().trim_end_matches(".dds").ends_with("_b"))
                    .and_then(|(_, t)| t.clone()),
                _ => None,
            };
            // Normal maps follow the UV set their tangents are built for: static detail
            // normals the detail set, everything else the base set.
            let tangent_uv = match &layers {
                Some(layers) => layers.normal_mapped().then_some(1),
                None => normal.is_some().then_some(0),
            };
            let tangents = tangent_uv.map(|channel| tangent_frames(mesh, material, channel));
            let color_uvs = layers.as_ref().and_then(StaticLayers::color_uvs);
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
                        "kind": match mesh.kind {
                            MeshKind::Static => "static",
                            MeshKind::Bundled => "bundled",
                            MeshKind::Skinned => "skinned",
                        },
                        "technique": material.technique,
                        "alpha_mode": format!("{:?}", material.alpha_mode),
                        "maps": maps.iter().map(|(m, t)| json!({ "ref": m, "path": t })).collect::<Vec<_>>(),
                    }
                }),
            });
            let material_id = doc.materials.len() - 1;
            let triangles: Vec<[u32; 3]> = mesh
                .material_triangles(material)
                .into_iter()
                .filter(|tri| tri.iter().all(|&v| (v as usize) < positions.len()))
                .collect();
            // Rigged: faces BF2 animates through a UV matrix (tracks, wheel hubs) get a
            // material of their own per matrix, so each can be scrolled.
            let animated = match &blend {
                Some(b) if rigged && material.technique.to_ascii_lowercase().contains("animateduv") => {
                    Some(uv_animation_groups(b, &triangles, &positions, &uv_sets, &bands))
                }
                _ => None,
            };
            let mut group_materials: Vec<usize> = vec![material_id];
            let mut group_of: HashMap<u16, usize> = HashMap::from([(0, 0)]);
            for (key, animation) in animated.iter().flat_map(|a| &a.animations) {
                let mut extras = doc.materials[material_id].extras.clone();
                extras["bf2"]["uv_animation"] = animation.clone();
                let base = &doc.materials[material_id];
                let copy = glb::Material {
                    name: format!("{}_uv{key}", base.name),
                    base_color: base.base_color,
                    base_color_uv: base.base_color_uv,
                    normal: base.normal,
                    alpha: base.alpha,
                    double_sided: base.double_sided,
                    extras,
                };
                doc.materials.push(copy);
                group_of.insert(*key, group_materials.len());
                group_materials.push(doc.materials.len() - 1);
            }
            let groups = group_materials.len();

            // Remap the material's vertices into compact per-part (and per UV matrix) primitives.
            let mut remap: Vec<HashMap<u32, u32>> = vec![HashMap::new(); buckets * groups];
            let mut prims: Vec<glb::Primitive> = (0..buckets * groups)
                .map(|slot| glb::Primitive {
                    material: Some(group_materials[slot % groups]),
                    uvs: vec![Vec::new(); uv_sets.len().min(2)],
                    extra_vec2: lightmap_set
                        .map(|_| vec![(LIGHTMAP_UV_ATTRIBUTE.to_string(), Vec::new())])
                        .unwrap_or_default(),
                    ..Default::default()
                })
                .collect();
            for (t, tri) in triangles.iter().enumerate() {
                let part_of = |v: u32| match (&blend, mesh.kind) {
                    (Some(b), MeshKind::Bundled) => (b[v as usize][0] as usize).min(part_count - 1),
                    _ => 0,
                };
                let part = if rigged { 0 } else { part_of(tri[0]) };
                let group = animated.as_ref().and_then(|a| group_of.get(&a.triangle_groups[t]).copied()).unwrap_or(0);
                let slot = part * groups + group;
                let prim = &mut prims[slot];
                // Reverse winding: the Z mirror flips handedness.
                for &v in [tri[0], tri[2], tri[1]].iter() {
                    let index = *remap[slot].entry(v).or_insert_with(|| {
                        let vi = v as usize;
                        prim.positions.push(coords::position(positions[vi]));
                        if rigged {
                            prim.joints.push([part_of(v) as u16, 0, 0, 0]);
                            prim.weights.push([1.0, 0.0, 0.0, 0.0]);
                        }
                        if let Some(n) = &normals {
                            prim.normals.push(normalize_or_up(coords::direction(n[vi])));
                        }
                        if let Some(tangents) = &tangents {
                            prim.tangents.push(tangents.get(&v).copied().unwrap_or([1.0, 0.0, 0.0, 1.0]));
                        }
                        for (set, value) in prim.uvs.iter_mut().enumerate() {
                            // UV set 0 = BF2 channel 0; set 1 = the base color channel if it
                            // isn't 0 (detail), so the glTF base color texCoord works.
                            let channel = if set == 1 { base_uv.max(1) as usize } else { 0 };
                            value.push(uv(channel, vi));
                        }
                        if let Some([a, b]) = color_uvs {
                            let (a, b) = (uv(a, vi), uv(b, vi));
                            prim.colors.push([a[0], a[1], b[0], b[1]]);
                        }
                        if let (Some(set), Some((_, values))) = (lightmap_set, prim.extra_vec2.first_mut()) {
                            values.push(uv(set, vi));
                        }
                        (prim.positions.len() - 1) as u32
                    });
                    prim.indices.push(index);
                }
            }
            for (slot, prim) in prims.into_iter().enumerate() {
                if !prim.indices.is_empty() {
                    part_primitives[slot / groups].push(prim);
                }
            }
        }

        if rigged {
            for part in 0..part_count {
                doc.nodes.push(glb::Node {
                    name: format!("part{part}"),
                    ..Default::default()
                });
                doc.scene.push(part);
            }
            const IDENTITY: [f32; 16] = [1.0, 0.0, 0.0, 0.0, 0.0, 1.0, 0.0, 0.0, 0.0, 0.0, 1.0, 0.0, 0.0, 0.0, 0.0, 1.0];
            doc.skins.push(glb::Skin {
                joints: (0..part_count).collect(),
                inverse_bind_matrices: vec![IDENTITY; part_count],
            });
            doc.meshes.push(glb::Mesh {
                name: "parts".into(),
                primitives: part_primitives.pop().unwrap_or_default(),
            });
            doc.nodes.push(glb::Node {
                name: "parts".into(),
                mesh: Some(0),
                skin: Some(0),
                ..Default::default()
            });
            doc.scene.push(part_count);
            return doc;
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
/// skinned meshes start with the color map. All of these put the first map on UV0. The
/// other layers are applied by the game's BF2 material from the map list in the extras.
fn base_color_slot(_kind: MeshKind, _technique: &str) -> (usize, u32) {
    (0, 0)
}

/// `bf2_mesh_version` of an exported `.glb` (`None` if missing or older than versioning).
fn mesh_version(path: &Path) -> Option<u64> {
    let mut file = std::fs::File::open(path).ok()?;
    let mut header = [0u8; 20];
    file.read_exact(&mut header).ok()?;
    let len = u32::from_le_bytes([header[12], header[13], header[14], header[15]]) as usize;
    let mut json = vec![0; len];
    file.read_exact(&mut json).ok()?;
    serde_json::from_slice::<serde_json::Value>(&json).ok()?["extras"]["bf2_mesh_version"].as_u64()
}

/// Which layers of a static mesh technique (`BaseDetailDirtCrackNDetailNCrack`, ...) need
/// more than the base and detail UV sets.
struct StaticLayers {
    dirt: bool,
    crack: bool,
    /// Detail or crack normal maps, or parallax (which needs the detail normal map's height).
    normal: bool,
}

impl StaticLayers {
    fn parse(technique: &str) -> Self {
        let t = technique.to_ascii_lowercase();
        let colors = t.replace("ndetail", "").replace("ncrack", "");
        Self {
            dirt: colors.contains("dirt"),
            crack: colors.contains("crack"),
            normal: t.contains("ndetail") || t.contains("ncrack") || t.contains("parallax"),
        }
    }

    fn normal_mapped(&self) -> bool {
        self.normal
    }

    /// BF2 UV channels packed into `COLOR_0`: dirt (xy) and crack (zw). The crack takes the
    /// dirt's channel when there is no dirt layer.
    fn color_uvs(&self) -> Option<[usize; 2]> {
        (self.dirt || self.crack).then_some([2, if self.dirt { 3 } else { 2 }])
    }
}

/// Tangents (engine space, `w` = binormal sign) of a material's vertices for normal maps on
/// UV channel `channel`. BF2's own tangents where usable; zero or broken ones (old files)
/// are rebuilt from the UV layout the way BF2's are made: tangent along +u, binormal
/// (`cross(T, N) * w` in BF2 space) along -v.
pub fn tangent_frames(mesh: &VisMesh, material: &Material, channel: u8) -> HashMap<u32, [f32; 4]> {
    let vec3 = |usage: Usage, v: u32| {
        mesh.element(usage, 0).map(|e| {
            let f = mesh.read_floats(v as usize, e);
            Vec3::new(f[0], f[1], f[2])
        })
    };
    let uv_element = mesh.element(Usage::TexCoord, channel).or_else(|| mesh.element(Usage::TexCoord, 0));
    let uv = |v: u32| {
        uv_element.map_or(Vec2::ZERO, |e| {
            let f = mesh.read_floats(v as usize, e);
            Vec2::new(f[0], f[1])
        })
    };
    let flip = |v: u32| {
        mesh.element(Usage::BlendIndices, 0)
            .is_some_and(|e| mesh.read_bytes4(v as usize, e)[2] != 0)
    };
    let triangles: Vec<[u32; 3]> = mesh
        .material_triangles(material)
        .into_iter()
        .filter(|t| t.iter().all(|&v| (v as usize) < mesh.vertex_count))
        .collect();

    // Accumulated position derivatives along u and v per vertex.
    let mut derivatives: HashMap<u32, (Vec3, Vec3)> = HashMap::new();
    for tri in &triangles {
        let p = tri.map(|v| vec3(Usage::Position, v).unwrap_or_default());
        let t = tri.map(uv);
        let (e1, e2) = (p[1] - p[0], p[2] - p[0]);
        let (d1, d2) = (t[1] - t[0], t[2] - t[0]);
        let det = d1.x * d2.y - d2.x * d1.y;
        if det.abs() < 1e-12 || !det.is_finite() {
            continue;
        }
        let dpdu = (e1 * d2.y - e2 * d1.y) / det;
        let dpdv = (e2 * d1.x - e1 * d2.x) / det;
        if !dpdu.is_finite() || !dpdv.is_finite() {
            continue;
        }
        for &v in tri {
            let d = derivatives.entry(v).or_default();
            d.0 += dpdu;
            d.1 += dpdv;
        }
    }

    let mut frames = HashMap::new();
    for &v in triangles.iter().flatten() {
        if frames.contains_key(&v) {
            continue;
        }
        let n = vec3(Usage::Normal, v).and_then(Vec3::try_normalize).unwrap_or(Vec3::Y);
        let stored = vec3(Usage::Tangent, v)
            .map(|t| t - n * n.dot(t))
            .filter(|t| t.is_finite() && t.length_squared() > 0.01);
        let (t, w) = match stored {
            Some(t) => (t.normalize(), if flip(v) { -1.0 } else { 1.0 }),
            None => {
                let (dpdu, dpdv) = derivatives.get(&v).copied().unwrap_or_default();
                let t = (dpdu - n * n.dot(dpdu))
                    .try_normalize()
                    .unwrap_or_else(|| n.any_orthonormal_vector());
                (t, if t.cross(n).dot(-dpdv) < 0.0 { -1.0 } else { 1.0 })
            }
        };
        let t = coords::direction(t.to_array());
        frames.insert(v, [t[0], t[1], t[2], w]);
    }
    frames
}

fn normalize_or_up(v: [f32; 3]) -> [f32; 3] {
    let len = (v[0] * v[0] + v[1] * v[1] + v[2] * v[2]).sqrt();
    if len > 1e-6 && len.is_finite() {
        [v[0] / len, v[1] / len, v[2] / len]
    } else {
        [0.0, 1.0, 0.0]
    }
}

/// A material's faces by the UV matrix BF2 animates them with (the fourth blend index), and
/// for each group what the game needs to animate it like BF2 (`bf2.uv_animation` in the
/// material extras): the matrix `index`; for scrolling faces (tracks, tread on wheel rims)
/// the sign per UV axis that moves the texture backwards along the bottom (`flow`, from which
/// way the UV runs along the length of the lowest faces; belts repeat their texture per link,
/// so per face); for turning faces (wheel hubs, whose second UV set is the offset from the
/// centre in the first) the `center` and the sign that turns them forwards (`spin`, from
/// whether the UV mapping mirrors the side view). Turning faces are grouped by their centre,
/// as a hub's rings turn about centres of their own.
struct UvAnimationGroups {
    /// Per triangle: its group's key, 0 for faces that don't move.
    triangle_groups: Vec<u16>,
    animations: Vec<(u16, serde_json::Value)>,
}

/// UV centres closer than this are one.
const UV_CENTER_TOLERANCE: f32 = 0.003;

/// The heights (lowest, highest) of the faces each UV matrix animates over the whole model
/// (those on the hull where the belt runs there): a belt's lower run is at the bottom.
fn uv_animation_bands(mesh: &VisMesh, lod: &bf2_formats::mesh::Lod, blend: &[[u8; 4]], positions: &[[f32; 3]]) -> HashMap<u8, (f32, f32)> {
    let mut corners: HashMap<u8, (Vec<f32>, Vec<f32>)> = HashMap::new();
    for material in lod.materials.iter().filter(|m| m.technique.to_ascii_lowercase().contains("animateduv")) {
        for tri in mesh.material_triangles(material) {
            if tri.iter().any(|&v| v as usize >= positions.len()) {
                continue;
            }
            let [a, b, c] = tri.map(|v| blend[v as usize][3]);
            let index = if b == c { b } else { a };
            if index == 0 {
                continue;
            }
            let entry = corners.entry(index).or_default();
            for &v in &tri {
                let y = coords::position(positions[v as usize])[1];
                if blend[v as usize][0] == 0 { entry.0.push(y) } else { entry.1.push(y) }
            }
        }
    }
    corners
        .into_iter()
        .map(|(index, (hull, parts))| {
            let heights = if hull.len() >= 10 { hull } else { parts };
            let band = heights.iter().fold((f32::MAX, f32::MIN), |(lo, hi), &y| (lo.min(y), hi.max(y)));
            (index, band)
        })
        .collect()
}

fn uv_animation_groups(
    blend: &[[u8; 4]],
    triangles: &[[u32; 3]],
    positions: &[[f32; 3]],
    uv_sets: &[Vec<[f32; 2]>],
    bands: &HashMap<u8, (f32, f32)>,
) -> UvAnimationGroups {
    let uv = |set: usize, v: usize| uv_sets.get(set).and_then(|s| s.get(v)).copied().map_or([0.0; 2], sanitize_uv);
    let offset = |v: usize| uv(1, v).iter().any(|c| c.abs() > 1e-4);
    // Turning corners: the centres (first UV set) they turn about, per matrix index.
    let mut centers: Vec<(u8, [f32; 2])> = Vec::new();
    let mut center_of = |index: u8, c: [f32; 2]| -> usize {
        let near = |(i, k): &(u8, [f32; 2])| {
            *i == index && (k[0] - c[0]).abs() < UV_CENTER_TOLERANCE && (k[1] - c[1]).abs() < UV_CENTER_TOLERANCE
        };
        match centers.iter().position(near) {
            Some(n) => n,
            None => {
                centers.push((index, c));
                centers.len() - 1
            }
        }
    };
    // Each face goes with the matrix most of its corners use; turning faces with the centre
    // of their first turning corner. Keys: the index for scrolling, 16 + centre for turning.
    let triangle_groups: Vec<u16> = triangles
        .iter()
        .map(|tri| {
            let [a, b, c] = tri.map(|v| blend[v as usize][3]);
            let index = if b == c { b } else { a };
            if index == 0 {
                return 0;
            }
            match tri.iter().map(|&v| v as usize).find(|&v| blend[v][3] == index && offset(v)) {
                Some(v) => 16 + center_of(index, uv(0, v)) as u16,
                None => index as u16,
            }
        })
        .collect();
    let mut keys: Vec<u16> = triangle_groups.iter().copied().filter(|k| *k != 0).collect();
    keys.sort_unstable();
    keys.dedup();
    let mut animations = Vec::new();
    for key in keys {
        let mut vertices: Vec<usize> = triangles
            .iter()
            .zip(&triangle_groups)
            .filter(|(_, g)| **g == key)
            .flat_map(|(tri, _)| tri.map(|v| v as usize))
            .collect();
        vertices.sort_unstable();
        vertices.dedup();
        // Side view in our coordinates: z along the hull (forward is -z), y up.
        let side = |v: usize| {
            let p = coords::position(positions[v]);
            (p[2], p[1])
        };
        let part = |v: usize| blend[v][0];
        let animation = if key >= 16 {
            let (index, center) = centers[(key - 16) as usize];
            // Offsets from the centre against positions around each part's middle.
            let mut middles: HashMap<u8, ((f32, f32), f32)> = HashMap::new();
            for &v in vertices.iter().filter(|&&v| offset(v)) {
                let (z, y) = side(v);
                let entry = middles.entry(part(v)).or_default();
                entry.0 = (entry.0.0 + z, entry.0.1 + y);
                entry.1 += 1.0;
            }
            let mut m = [[0.0f32; 2]; 2];
            for &v in vertices.iter().filter(|&&v| offset(v)) {
                let ((sz, sy), n) = middles[&part(v)];
                let (z, y) = side(v);
                let (dz, dy) = (z - sz / n, y - sy / n);
                let o = uv(1, v);
                m[0][0] += o[0] * dz;
                m[0][1] += o[0] * dy;
                m[1][0] += o[1] * dz;
                m[1][1] += o[1] * dy;
            }
            let det = m[0][0] * m[1][1] - m[0][1] * m[1][0];
            json!({
                "index": index,
                "center": center,
                "spin": if det < 0.0 { -1.0 } else { 1.0 },
            })
        } else {
            // The lowest faces: those on the hull if the belt runs there, else each part's.
            let faces: Vec<[usize; 3]> = triangles
                .iter()
                .zip(&triangle_groups)
                .filter(|(_, g)| **g == key)
                .map(|(tri, _)| tri.map(|v| v as usize))
                .collect();
            let on_hull: Vec<[usize; 3]> = faces.iter().copied().filter(|f| f.iter().all(|&v| part(v) == 0)).collect();
            let chosen = if on_hull.len() >= 10 { on_hull } else { faces };
            let (low, high) = bands.get(&(key as u8)).copied().unwrap_or((f32::MIN, f32::MAX));
            let quarter = (high - low) * 0.25;
            let within = |f: &[usize; 3], below: bool| {
                f.iter().all(|&v| if below { side(v).1 <= low + quarter } else { side(v).1 >= high - quarter })
            };
            // Along the bottom the texture must move backwards; faces only on top (the upper
            // run of a belt) move forwards.
            let on_bottom = chosen.iter().any(|f| within(f, true));
            let reverse = if on_bottom { 1.0 } else { -1.0 };
            let bottom = |f: &&[usize; 3]| within(f, on_bottom);
            // How the UV changes along z within each face (x runs across the belt).
            let mut along = [0.0f32; 2];
            for face in chosen.iter().filter(bottom) {
                let p = face.map(|v| coords::position(positions[v]));
                let (dz1, dx1, dz2, dx2) = (p[1][2] - p[0][2], p[1][0] - p[0][0], p[2][2] - p[0][2], p[2][0] - p[0][0]);
                let det = dz1 * dx2 - dz2 * dx1;
                if det.abs() < 1e-6 {
                    continue;
                }
                let t = face.map(|v| uv(0, v));
                for (axis, total) in along.iter_mut().enumerate() {
                    let (du1, du2) = (t[1][axis] - t[0][axis], t[2][axis] - t[0][axis]);
                    *total += (du1 * dx2 - du2 * dx1) / det * det.abs();
                }
            }
            // The texture must move towards +z (backwards) along the bottom.
            let flow = |a: f32| if a > 0.0 { -reverse } else { reverse };
            json!({
                "index": key,
                "flow": [flow(along[0]), flow(along[1])],
            })
        };
        animations.push((key, animation));
    }
    UvAnimationGroups { triangle_groups, animations }
}

fn sanitize_uv(uv: [f32; 2]) -> [f32; 2] {
    if uv[0].is_finite() && uv[1].is_finite() {
        uv
    } else {
        [0.0, 0.0]
    }
}

#[cfg(test)]
mod tests {
    use bf2_formats::mesh::{DeclType, VertexElement};

    use super::*;

    #[test]
    fn static_layers_need_extra_uvs_and_tangents() {
        let layers = StaticLayers::parse("BaseDetailDirtCrackNDetailNCrack");
        assert_eq!(layers.color_uvs(), Some([2, 3]));
        assert!(layers.normal_mapped());
        let layers = StaticLayers::parse("BaseDetailCrackNDetailNCrack");
        assert_eq!(layers.color_uvs(), Some([2, 2]));
        let layers = StaticLayers::parse("BaseDetailNDetailparallaxdetail");
        assert_eq!(layers.color_uvs(), None);
        assert!(layers.normal_mapped());
        assert!(!StaticLayers::parse("BaseDetail").normal_mapped());
    }

    #[test]
    fn lightmap_uvs_are_the_last_of_three_or_more_static_sets() {
        let house = "objects/staticobjects/x/house/meshes/house_lod1.glb";
        assert_eq!(lightmap_uv_set(MeshKind::Static, 3, house), Some(2));
        assert_eq!(lightmap_uv_set(MeshKind::Static, 5, house), Some(4));
        assert_eq!(lightmap_uv_set(MeshKind::Static, 2, house), None);
        assert_eq!(lightmap_uv_set(MeshKind::Bundled, 4, house), None);
        let tree = "objects/vegitation/mideast/me_palmtree01/meshes/me_palmtree01.glb";
        assert_eq!(lightmap_uv_set(MeshKind::Static, 3, tree), None);
    }

    /// The attribute reaches the file with one value per vertex.
    #[test]
    fn extra_vec2_attributes_are_written() {
        let primitive = glb::Primitive {
            positions: vec![[0.0; 3], [1.0, 0.0, 0.0], [0.0, 1.0, 0.0]],
            indices: vec![0, 1, 2],
            extra_vec2: vec![(LIGHTMAP_UV_ATTRIBUTE.to_string(), vec![[0.0, 0.0], [0.5, 0.0], [0.0, 0.5]])],
            ..Default::default()
        };
        let doc = Document {
            meshes: vec![glb::Mesh {
                name: "m".into(),
                primitives: vec![primitive],
            }],
            ..Default::default()
        };
        let path = std::env::temp_dir().join(format!("bf2_lightmap_uv_{}.glb", std::process::id()));
        doc.write(&path).unwrap();
        let data = std::fs::read(&path).unwrap();
        let _ = std::fs::remove_file(&path);
        let json_len = u32::from_le_bytes(data[12..16].try_into().unwrap()) as usize;
        let json: serde_json::Value = serde_json::from_slice(&data[20..20 + json_len]).unwrap();
        let attributes = &json["meshes"][0]["primitives"][0]["attributes"];
        let accessor = &json["accessors"][attributes[LIGHTMAP_UV_ATTRIBUTE].as_u64().unwrap() as usize];
        assert_eq!(accessor["type"], "VEC2");
        assert_eq!(accessor["count"], 3);
    }

    /// A quad in the XY plane facing -Z (BF2 space), u along +X and v along -Y, with zero
    /// tangents: the rebuilt tangent runs along +u and the binormal `cross(T, N) * w` along -v.
    #[test]
    fn rebuilds_missing_tangents_like_bf2() {
        let element = |offset, ty, usage| VertexElement {
            offset,
            ty,
            usage,
            usage_index: 0,
        };
        let vertices: [([f32; 3], [f32; 2]); 4] = [
            ([0.0, 0.0, 0.0], [0.0, 1.0]),
            ([1.0, 0.0, 0.0], [1.0, 1.0]),
            ([1.0, 1.0, 0.0], [1.0, 0.0]),
            ([0.0, 1.0, 0.0], [0.0, 0.0]),
        ];
        let mut vertex_data = Vec::new();
        for (position, uv) in vertices {
            for f in position.into_iter().chain([0.0, 0.0, -1.0]).chain(uv).chain([0.0; 3]) {
                vertex_data.extend_from_slice(&f32::to_le_bytes(f));
            }
        }
        let mesh = VisMesh {
            kind: MeshKind::Bundled,
            version: 10,
            elements: vec![
                element(0, DeclType::Float3, Usage::Position),
                element(12, DeclType::Float3, Usage::Normal),
                element(24, DeclType::Float2, Usage::TexCoord),
                element(32, DeclType::Float3, Usage::Tangent),
            ],
            stride: 44,
            vertex_count: 4,
            vertex_data,
            indices: vec![0, 2, 1, 0, 3, 2],
            geoms: Vec::new(),
        };
        let material = Material {
            alpha_mode: AlphaMode::Opaque,
            fx_file: String::new(),
            technique: String::new(),
            maps: Vec::new(),
            vstart: 0,
            istart: 0,
            inum: 6,
            vnum: 4,
        };
        let frames = tangent_frames(&mesh, &material, 0);
        for v in 0..4 {
            let [x, y, z, w] = frames[&v];
            assert!((Vec3::new(x, y, z) - Vec3::X).length() < 1e-5, "{:?}", frames[&v]);
            // cross(T, N) * w in BF2 space: cross(+X, -Z) = +Y is already -v, so w = 1.
            assert_eq!(w, 1.0);
        }
    }
}
