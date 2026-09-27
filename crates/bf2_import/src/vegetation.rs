//! Vegetation: undergrowth (grass and small plants the client scatters around the camera)
//! and the overgrowth trees BF2 generates but doesn't list with collision.
//!
//! Undergrowth: `Undergrowth.cfg` holds the global settings, `Undergrowth.dat` the compiled
//! materials, plant types and their meshes (UVs already in `UndergrowthAtlas0.dds`), and
//! `Undergrowth.raw` a material id per heightmap sample.
//!
//! Overgrowth: `Overgrowth/Overgrowth.con` defines materials of tree types with a density,
//! `Overgrowth.raw` a material id per sample. The game scatters the trees at load time;
//! `OvergrowthCollision.con` lists the result with transforms (imported as statics), but it
//! was exported once and some levels changed their types afterwards. Types missing from it
//! are scattered here, visual only.

use std::{collections::HashMap, path::Path};

use anyhow::{Context, Result, bail, ensure};
use bf2_formats::{
    Vfs,
    con::{Interpreter, World},
    reader::Reader,
};
use game_data::{
    OvergrowthDesc, Placement, TerrainDesc, UndergrowthDesc, UndergrowthMaterial, UndergrowthMesh,
    UndergrowthType, VegetationDesc,
};
use glam::Quat;

use crate::meshes::MeshConverter;

/// Converts the level's vegetation into `vegetation.ron`. Returns its path relative to the
/// level folder, or `None` if the level has none.
pub fn import(
    interp: &mut Interpreter,
    converter: &MeshConverter,
    level_name: &str,
    level_dir: &Path,
    terrain: &TerrainDesc,
) -> Option<String> {
    let vfs = converter.vfs;
    let base = format!("levels/{level_name}");
    let undergrowth = undergrowth(vfs, &base, level_dir)
        .map_err(|e| log::warn!("{level_name}: undergrowth: {e:#}"))
        .ok()
        .flatten();
    let overgrowth = overgrowth(interp, converter, &base, level_dir, terrain)
        .map_err(|e| log::warn!("{level_name}: overgrowth: {e:#}"))
        .unwrap_or_default();
    if undergrowth.is_none() && overgrowth.is_empty() {
        return None;
    }
    let desc = VegetationDesc { undergrowth, overgrowth };
    match game_data::write_ron(level_dir.join("vegetation.ron"), &desc) {
        Ok(()) => Some("vegetation.ron".into()),
        Err(e) => {
            log::warn!("{level_name}: vegetation: {e}");
            None
        }
    }
}

fn undergrowth(vfs: &Vfs, base: &str, level_dir: &Path) -> Result<Option<UndergrowthDesc>> {
    let Ok(dat) = vfs.read(&format!("{base}/undergrowth.dat")) else {
        return Ok(None);
    };
    let materials: Vec<UndergrowthMaterial> =
        parse_dat(&dat)?.into_iter().filter(|m| !m.types.is_empty()).collect();
    if materials.is_empty() {
        return Ok(None);
    }
    let cfg = vfs.read_text(&format!("{base}/undergrowth.cfg")).unwrap_or_default();
    let setting = |key: &str, default: f32| -> f32 {
        cfg.lines()
            .map(|l| l.split_whitespace().collect::<Vec<_>>())
            .take_while(|w| w.first().is_none_or(|k| !k.eq_ignore_ascii_case("material")))
            .find(|w| w.first().is_some_and(|k| k.eq_ignore_ascii_case(key)))
            .and_then(|w| w.get(1)?.parse().ok())
            .unwrap_or(default)
    };

    let map = vfs.read(&format!("{base}/undergrowth.raw")).context("undergrowth.raw")?;
    let size = (map.len() as f64).sqrt() as usize;
    ensure!(size * size == map.len(), "undergrowth.raw is not square ({} bytes)", map.len());
    // Row 0 is south in BF2 and north (-Z) in our grids.
    let mut flipped = Vec::with_capacity(map.len());
    for row in map.chunks_exact(size).rev() {
        flipped.extend_from_slice(row);
    }
    std::fs::create_dir_all(level_dir)?;
    std::fs::write(level_dir.join("undergrowth.r8"), flipped)?;

    let atlas = vfs.read(&format!("{base}/undergrowthatlas0.dds")).context("undergrowth atlas")?;
    std::fs::write(level_dir.join("undergrowth_atlas.dds"), atlas)?;

    Ok(Some(UndergrowthDesc {
        material_map: "undergrowth.r8".into(),
        atlas: "undergrowth_atlas.dds".into(),
        view_distance: setting("ViewDistance", 50.0),
        fade: setting("ViewDistanceFadeScale", 0.5),
        sway: setting("SwayScale", 0.15),
        brightness: setting("LightingScale", 1.5),
        alpha_cutoff: setting("AlphaRef", 0.15),
        materials,
    }))
}

/// Parses `Undergrowth.dat` (version 2): the compiled `Undergrowth.cfg` with the meshes the
/// editor generated for every plant type.
///
/// ```text
/// u32 version (2), u32 material count
/// material: string name, u32 id, f32 GeneralHeight, u32 type count
/// type:     string name, string mesh (empty for CrossSize quads), string texture,
///           f32 cross width, cross height, size min, size max, ?, ?, density, variation,
///           terrain colour scale, sway scale; u8 skew
///           u32 vertex count, 24-byte vertices: f32 position[3], i16 atlas uv[2] / 32768,
///           2 bytes ?, 2 bytes padding, f32 sway weight
///           u32 index count, u16 indices, u32 0
/// ```
/// Strings are `u32 length` + bytes.
fn parse_dat(data: &[u8]) -> Result<Vec<UndergrowthMaterial>> {
    let mut r = Reader::new(data);
    let version = r.u32()?;
    if version != 2 {
        bail!("Undergrowth.dat version {version}");
    }
    let material_count = r.count(16)?;
    let mut materials = Vec::with_capacity(material_count);
    for _ in 0..material_count {
        let name = r.string()?;
        let id = r.u32()?;
        let _general_height = r.f32()?;
        let type_count = r.count(12)?;
        let mut types = Vec::with_capacity(type_count);
        for _ in 0..type_count {
            let name = r.string()?;
            let _mesh = r.string()?;
            let _texture = r.string()?;
            let mut f = [0f32; 10];
            for v in &mut f {
                *v = r.f32()?;
            }
            let skew = r.u8()? != 0;
            let vertex_count = r.count(24)?;
            let mut mesh = UndergrowthMesh::default();
            for _ in 0..vertex_count {
                let [x, y, z] = r.vec3()?;
                let u = r.i16()? as f32 / 32768.0;
                let v = r.i16()? as f32 / 32768.0;
                r.skip(4)?;
                mesh.positions.push([x, y, -z]);
                mesh.uvs.push([u, v]);
                mesh.sway.push(r.f32()?);
            }
            let index_count = r.count(2)?;
            let mut indices = Vec::with_capacity(index_count);
            for _ in 0..index_count {
                indices.push(r.u16()?);
            }
            let _ = r.u32()?;
            ensure!(
                indices.iter().all(|&i| (i as usize) < vertex_count),
                "{name}: index out of range"
            );
            // Mirroring Z flips the handedness: reverse the winding.
            for tri in indices.chunks_exact(3) {
                mesh.indices.extend_from_slice(&[tri[0], tri[2], tri[1]]);
            }
            let density = f[6];
            if mesh.indices.is_empty() || density <= 0.0 {
                continue;
            }
            types.push(UndergrowthType {
                name,
                density,
                scale: [f[2], f[3]],
                variation: f[7],
                ground_tint: f[8].clamp(0.0, 1.0),
                skew,
                mesh,
            });
        }
        materials.push(UndergrowthMaterial {
            id: id.min(255) as u8,
            name,
            types,
        });
    }
    ensure!(r.remaining() == 0, "{} trailing bytes", r.remaining());
    Ok(materials)
}

/// One overgrowth type: a tree template and its density per material.
struct OvergrowthType {
    material: u8,
    geometry: String,
    density: f32,
}

/// Square meters per unit of `OvergrowthType.density`, measured on the collision lists
/// (instances per painted area: 1100..1800 m², Karkand ~1400).
const OVERGROWTH_AREA_PER_DENSITY: f32 = 1400.0;

fn overgrowth(
    interp: &mut Interpreter,
    converter: &MeshConverter,
    base: &str,
    level_dir: &Path,
    terrain: &TerrainDesc,
) -> Result<Vec<OvergrowthDesc>> {
    let world = &interp.world;
    let mut materials: HashMap<String, u8> = HashMap::new();
    let mut types: Vec<OvergrowthType> = Vec::new();
    let mut material = None;
    for command in &world.commands {
        let arg = |i: usize| command.args.get(i).map(String::as_str);
        match command.name.as_str() {
            "overgrowth.addmaterial" => {
                if let (Some(name), Some(id)) = (arg(0), arg(1).and_then(|s| s.parse().ok())) {
                    materials.insert(name.to_ascii_lowercase(), id);
                }
            }
            "overgrowth.setactivematerial" => {
                material = arg(0).and_then(|n| materials.get(&n.to_ascii_lowercase())).copied();
            }
            "overgrowth.addtype" => {
                if let Some(material) = material {
                    types.push(OvergrowthType { material, geometry: String::new(), density: 0.0 });
                }
            }
            "overgrowthtype.geometry" => {
                if let (Some(t), Some(g)) = (types.last_mut(), arg(0)) {
                    t.geometry = g.to_ascii_lowercase();
                }
            }
            "overgrowthtype.density" => {
                if let (Some(t), Some(d)) = (types.last_mut(), arg(0).and_then(|s| s.parse().ok())) {
                    t.density = d;
                }
            }
            _ => {}
        }
    }
    if types.is_empty() {
        return Ok(Vec::new());
    }

    // Trees the collision list already places, and how high their pivots sit above the ground.
    let heights = Heights::load(level_dir, terrain)?;
    let mut listed: HashMap<String, usize> = HashMap::new();
    let mut offsets = Vec::new();
    for instance in world.instances.iter().filter(|i| i.get_str("isovergrowth").is_some()) {
        *listed.entry(instance.template.to_ascii_lowercase()).or_default() += 1;
        if let Some(m) = instance.transform {
            let p = crate::coords::position([m[3][0], m[3][1], m[3][2]]);
            offsets.push(p[1] - heights.at(p[0], p[2]));
        }
    }
    offsets.sort_by(f32::total_cmp);
    let pivot_offset = offsets.get(offsets.len() / 2).copied().unwrap_or(0.0);

    let missing: Vec<&OvergrowthType> = types
        .iter()
        .filter(|t| t.density > 0.0 && !t.geometry.is_empty() && !listed.contains_key(&t.geometry))
        .collect();
    if missing.is_empty() {
        return Ok(Vec::new());
    }
    // Templates load on first use, and nothing has used these.
    for t in &missing {
        interp.ensure_template(&t.geometry);
    }
    let world = &interp.world;
    let map = converter.vfs.read(&format!("{base}/overgrowth/overgrowth.raw")).context("overgrowth.raw")?;
    let size = (map.len() as f64).sqrt() as usize;
    ensure!(size * size == map.len() && size > 1, "overgrowth.raw is not square");
    let spacing = terrain.world_size() / (size - 1) as f32;
    let half = terrain.world_size() * 0.5;

    let mut out = Vec::new();
    for (index, t) in missing.iter().enumerate() {
        let Some(mesh) = tree_mesh(world, converter, &t.geometry) else {
            log::debug!("overgrowth type {} has no mesh", t.geometry);
            continue;
        };
        let per_cell = t.density * spacing * spacing / OVERGROWTH_AREA_PER_DENSITY;
        let mut rng = Rng(0x9E37_79B9 ^ (index as u32 + 1).wrapping_mul(0x85EB_CA6B));
        let mut instances = Vec::new();
        for (i, &id) in map.iter().enumerate() {
            if id != t.material || rng.next() >= per_cell {
                continue;
            }
            // BF2 row 0 is south; engine Z is the mirrored BF2 Z.
            let (col, row) = ((i % size) as f32, (i / size) as f32);
            let x = (col + rng.next() - 0.5) * spacing - half;
            let z = -((row + rng.next() - 0.5) * spacing - half);
            let y = heights.at(x, z) + pivot_offset;
            instances.push(Placement {
                position: [x, y, z],
                rotation: Quat::from_rotation_y(rng.next() * std::f32::consts::TAU).to_array(),
                ..Default::default()
            });
        }
        log::info!("overgrowth: scattered {} x {} (not in the collision list)", instances.len(), t.geometry);
        if !instances.is_empty() {
            out.push(OvergrowthDesc { mesh, instances });
        }
    }
    Ok(out)
}

/// The visible mesh of a tree template, converted.
fn tree_mesh(world: &World, converter: &MeshConverter, template: &str) -> Option<String> {
    let geometry = world.template(template)?.geometry.clone()?;
    let path = world.geometry(&geometry)?.mesh_path()?;
    converter
        .convert_mesh(&path)
        .map_err(|e| log::warn!("overgrowth mesh {path}: {e:#}"))
        .ok()
}

/// The imported heightmap, for placing trees.
struct Heights<'a> {
    terrain: &'a TerrainDesc,
    samples: Vec<u16>,
}

impl<'a> Heights<'a> {
    fn load(level_dir: &Path, terrain: &'a TerrainDesc) -> Result<Self> {
        let bytes = std::fs::read(level_dir.join(&terrain.heightmap))?;
        let samples = bytes.chunks_exact(2).map(|b| u16::from_le_bytes([b[0], b[1]])).collect();
        Ok(Self { terrain, samples })
    }

    /// Bilinear height at an engine-space position.
    fn at(&self, x: f32, z: f32) -> f32 {
        let t = self.terrain;
        let n = t.resolution as usize;
        let max = (n - 1) as f32;
        let fx = ((x - t.origin[0]) / t.spacing).clamp(0.0, max);
        let fz = ((z - t.origin[2]) / t.spacing).clamp(0.0, max);
        let (x0, z0) = ((fx as usize).min(n - 2), (fz as usize).min(n - 2));
        let (u, v) = (fx - x0 as f32, fz - z0 as f32);
        let h = |x: usize, z: usize| self.samples.get(z * n + x).copied().unwrap_or(0) as f32;
        let top = h(x0, z0) * (1.0 - u) + h(x0 + 1, z0) * u;
        let bottom = h(x0, z0 + 1) * (1.0 - u) + h(x0 + 1, z0 + 1) * u;
        t.origin[1] + (top * (1.0 - v) + bottom * v) * t.height_scale
    }
}

/// Small deterministic generator (xorshift), so re-imports produce the same trees.
struct Rng(u32);

impl Rng {
    /// Uniform in [0, 1).
    fn next(&mut self) -> f32 {
        self.0 ^= self.0 << 13;
        self.0 ^= self.0 >> 17;
        self.0 ^= self.0 << 5;
        (self.0 >> 8) as f32 / (1u32 << 24) as f32
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn string(out: &mut Vec<u8>, s: &str) {
        out.extend_from_slice(&(s.len() as u32).to_le_bytes());
        out.extend_from_slice(s.as_bytes());
    }

    #[test]
    fn parses_compiled_undergrowth() {
        let mut d = Vec::new();
        d.extend_from_slice(&2u32.to_le_bytes());
        d.extend_from_slice(&1u32.to_le_bytes());
        string(&mut d, "grass");
        d.extend_from_slice(&2u32.to_le_bytes());
        d.extend_from_slice(&0.3f32.to_le_bytes());
        d.extend_from_slice(&1u32.to_le_bytes());
        string(&mut d, "tuft");
        string(&mut d, "");
        string(&mut d, "grass_mix");
        for f in [1.0f32, 1.0, 0.3, 0.65, 0.0, 1.0, 3.0, 0.2, 1.0, 0.4] {
            d.extend_from_slice(&f.to_le_bytes());
        }
        d.push(1);
        d.extend_from_slice(&3u32.to_le_bytes());
        for (p, uv, sway) in [([0.0f32, 0.0, 1.0], [0i16, 16384], 0.0f32), ([1.0, 0.0, 0.0], [8192, 16384], 0.0), ([0.0, 1.0, 0.0], [0, 0], 0.4)] {
            for c in p {
                d.extend_from_slice(&c.to_le_bytes());
            }
            d.extend_from_slice(&uv[0].to_le_bytes());
            d.extend_from_slice(&uv[1].to_le_bytes());
            d.extend_from_slice(&[0, 0, 0xcd, 0xcd]);
            d.extend_from_slice(&sway.to_le_bytes());
        }
        d.extend_from_slice(&3u32.to_le_bytes());
        for i in [0u16, 1, 2] {
            d.extend_from_slice(&i.to_le_bytes());
        }
        d.extend_from_slice(&0u32.to_le_bytes());

        let materials = parse_dat(&d).unwrap();
        assert_eq!(materials.len(), 1);
        assert_eq!((materials[0].id, materials[0].name.as_str()), (2, "grass"));
        let t = &materials[0].types[0];
        assert_eq!((t.density, t.scale, t.skew), (3.0, [0.3, 0.65], true));
        assert_eq!(t.mesh.positions[0], [0.0, 0.0, -1.0]);
        assert_eq!(t.mesh.uvs[1], [0.25, 0.5]);
        assert_eq!(t.mesh.sway[2], 0.4);
        assert_eq!(t.mesh.indices, vec![0, 2, 1]);
    }
}
