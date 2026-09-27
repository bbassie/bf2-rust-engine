//! Levels of detail of static objects, vehicles and soldiers, and how far away BF2 draws
//! them at all.
//!
//! Neither is stored in the mesh files; both come from the scripts and from constants in
//! the engine (found in `RendDX9.dll` and `BF2.exe` 1.5):
//!
//! * **LOD switch distances**: `GeometryTemplate.setSubGeometryLodDistance <geom> <lod> <m>`
//!   is the camera distance at which LOD `lod` hands over to `lod + 1`. When the mesh loads,
//!   LODs without one get a running default that starts at 50 m and grows by 50 m per LOD
//!   (and by the set distance where one is set). At the highest geometry quality nothing
//!   scales them (`renderer.globalStaticMeshLodDistanceScale` 1 × quality factor 1); the
//!   camera distance is divided by the zoom (1 unzoomed).
//! * **Culling**: the object manager fades an object out around the distance
//!   `C = max(sqrt(10 π) · distanceCullConst · r, minCullDistance)` (measured to the cull
//!   sphere: `distance² − r²`, fading between 0.8 C² and 1.2 C²), where the cull radius
//!   `r = 0.8 · bounding radius · ObjectTemplate.cullRadiusScale`. At the highest geometry
//!   quality the renderer sets `distanceCullConst` 8 (10 for soldiers and vehicles) and
//!   `minCullDistance` 80 m.
//!
//! Vehicles (bundled meshes) and soldiers (skinned meshes) follow the rules found later in
//! the same binaries (see docs/formats/meshes.md §2.12):
//!
//! * **LOD switch distances**: like the statics' (`setSubGeometryLodDistance`, the running
//!   default, scale 1 at every quality), but compared with the camera distance divided by the
//!   zoom *minus* `r0`, half the diagonal of the geom's LOD 0 bounding box: LOD `k` takes over
//!   at its switch distance plus `r0`.
//! * **Culling** (`distanceCullConstPCO` for `PlayerControlObject`s and soldiers): the object
//!   is hidden once `distance² − r² > C²` with `C = max(√(10π) · 10 · r, 80)` on high and
//!   `r = Object::getRadius · cullRadiusScale` (no 0.8 factor): the collision radius of each
//!   part grown over its children, 0.98 for a soldier (its built-in collision box, cull radius
//!   scale 2.5). Parts with a model of their own (rotors, carried weapons) smaller than 0.8 of
//!   the whole are culled on their own with the "small part" constants: `C = max(√(5π) · 8 ·
//!   r, 40)`.

use std::collections::HashMap;

use bf2_formats::{
    con::{Geometry, Template},
    mesh::{MeshKind, VisMesh},
    vfs::normalize,
};
use game_data::MeshLod;

use crate::meshes::MeshConverter;

/// First default switch distance, and how much each further default adds (m).
const DEFAULT_LOD_STEP: f32 = 50.0;
/// `renderer.distanceCullConst` at the highest geometry quality (4 by default, 8 on high).
const DISTANCE_CULL_CONST: f32 = 8.0;
/// `renderer.minCullDistance` at the highest geometry quality (100 by default, 80 on high).
const MIN_CULL_DISTANCE: f32 = 80.0;
/// The object manager's cull radius is this much of the object's bounding radius.
const CULL_RADIUS_FACTOR: f32 = 0.8;
/// `renderer.globalBundleMeshLodDistanceScale` at the highest geometry quality: the factor on
/// bundled meshes' (vehicles') switch distances.
pub const BUNDLED_LOD_SCALE: f32 = 1.0;
/// `renderer.globalSkinnedMeshLodDistanceScale` at the highest geometry quality: the factor on
/// skinned meshes' (soldiers') switch distances.
pub const SKINNED_LOD_SCALE: f32 = 1.0;
/// `renderer.distanceCullConstPCO` at the highest geometry quality (5 by default, 10 on
/// high): the cull constant of player control objects (vehicles, soldiers).
const DISTANCE_CULL_CONST_PCO: f32 = 10.0;
/// A soldier's radius for culling: its collision box, which the engine builds itself (x and z
/// within ±0.4 m, y from −0.6 to 0.8 m).
pub const SOLDIER_RADIUS: f32 = 0.98;
/// Parts of an object smaller than this share of it (radius times cull radius scale) are
/// culled on their own, with the small part constants.
pub const SMALL_PART_SHARE: f32 = 0.8;

/// Lower LODs of one visible mesh as used by a static object (the geom drawn in the world
/// and its wreck), plus the mesh's bounding radius.
#[derive(Default, Clone, Debug)]
pub struct ObjectLods {
    pub lods: Vec<MeshLod>,
    pub wreck_lods: Vec<MeshLod>,
    /// Largest distance of the full-detail mesh from its origin (m).
    pub radius: f32,
    /// Trees and bushes: no LODs or draw distance here, the renderer's vegetation code
    /// handles their distance.
    pub vegetation: bool,
}

/// Converts LOD 1.. of the intact and destroyed geoms of a static object's mesh to
/// `x{suffix}_lod{N}.glb` (the suffix of the full-detail `.glb`: none, `_3p` or `_wreck`),
/// with their switch distances. Vegetation is left alone (its LODs are drawn differently).
pub fn object_lods(converter: &MeshConverter, geometry: Option<&Geometry>, mesh_path: &str) -> ObjectLods {
    let key = normalize(mesh_path);
    let Some(kind) = MeshKind::from_path(&key) else {
        return ObjectLods::default();
    };
    let Some(mesh) = converter
        .vfs
        .read(&key)
        .ok()
        .and_then(|data| VisMesh::parse(&data, kind).ok())
    else {
        return ObjectLods::default();
    };
    let radius = mesh
        .geoms
        .first()
        .and_then(|g| g.lods.first())
        .map_or(0.0, |lod| corner_radius(lod.bounds_min, lod.bounds_max));
    if key.contains("vegitation") {
        return ObjectLods {
            radius,
            vegetation: true,
            ..Default::default()
        };
    }
    // The same geoms `destruction::convert_object_mesh`/`convert_wreck_mesh` draw.
    let intact = usize::from(kind == MeshKind::Bundled && mesh.geoms.len() > 1);
    let wreck = (intact + 1 < mesh.geoms.len()).then_some(intact + 1);
    let intact_suffix = if intact == 0 { "" } else { "_3p" };
    let convert = |geom: usize, suffix: &str| -> Vec<MeshLod> {
        let lod_count = mesh.geoms.get(geom).map_or(0, |g| g.lods.len());
        let starts = lod_starts(&lod_distances(geometry, geom, lod_count));
        let mut lods = Vec::new();
        for (index, distance) in starts.into_iter().enumerate() {
            let lod = index + 1;
            // An empty LOD draws nothing: BF2 hides the object from there on. Keep it so the
            // previous LOD ends there too; an empty .glb is written for it.
            match converter.convert_mesh_lod(mesh_path, geom, lod, &format!("{suffix}_lod{lod}")) {
                Ok(path) => lods.push(MeshLod { mesh: path, distance }),
                Err(err) => {
                    log::warn!("{mesh_path} geom {geom} LOD {lod}: {err:#}");
                    break;
                }
            }
        }
        lods
    };
    ObjectLods {
        lods: convert(intact, intact_suffix),
        wreck_lods: wreck.map(|geom| convert(geom, "_wreck")).unwrap_or_default(),
        radius,
        vegetation: false,
    }
}

/// BF2's switch distances of a geom with `lod_count` LODs: entry `i` is where LOD `i` hands
/// over to `i + 1`. Explicit distances from `setSubGeometryLodDistance`, the rest the
/// engine's running default. (The engine fills gaps below a set LOD with 0, which would
/// never show the more detailed LOD; those gaps get the default instead.)
pub fn lod_distances(geometry: Option<&Geometry>, geom: usize, lod_count: usize) -> Vec<f32> {
    let mut explicit: HashMap<usize, f32> = HashMap::new();
    for (method, args) in geometry.map_or(&[][..], |g| g.props.as_slice()) {
        if method != "setsubgeometryloddistance" || args.len() < 3 {
            continue;
        }
        let (Ok(g), Ok(lod), Ok(distance)) =
            (args[0].parse::<usize>(), args[1].parse::<usize>(), args[2].parse::<f32>())
        else {
            continue;
        };
        if g == geom && distance >= 0.0 {
            explicit.insert(lod, distance);
        }
    }
    let mut running = DEFAULT_LOD_STEP;
    (0..lod_count.saturating_sub(1))
        .map(|lod| match explicit.get(&lod) {
            Some(&distance) => {
                running += distance;
                distance
            }
            None => {
                let distance = running;
                running += DEFAULT_LOD_STEP;
                distance
            }
        })
        .collect()
}

/// The distance each LOD from 1 on starts at. BF2 shows LOD `k` once any switch distance
/// from `k - 1` on is passed, so an out-of-order list skips LODs: LOD `k` starts at the
/// smallest switch distance from `k - 1` on (never decreasing).
pub fn lod_starts(distances: &[f32]) -> Vec<f32> {
    let mut starts: Vec<f32> = (0..distances.len())
        .map(|k| distances[k..].iter().copied().fold(f32::INFINITY, f32::min))
        .collect();
    // Suffix minima never decrease; make it explicit against NaN.
    for i in 1..starts.len() {
        starts[i] = starts[i].max(starts[i - 1]);
    }
    starts
}

/// How far from its origin BF2 draws an object with this bounding radius (fully faded out
/// at about 1.1 times this). `None` for objects that reach the view distance anyway.
pub fn draw_distance(template: Option<&Template>, radius: f32) -> Option<f32> {
    if radius <= 0.0 || !radius.is_finite() {
        return None;
    }
    let scale = template.and_then(|t| t.get_f32("cullradiusscale")).unwrap_or(1.0).max(0.0);
    let cull_radius = CULL_RADIUS_FACTOR * radius * scale;
    let cull = ((10.0 * std::f32::consts::PI).sqrt() * DISTANCE_CULL_CONST * cull_radius).max(MIN_CULL_DISTANCE);
    // Measured to the cull sphere: distance² − r² = cull².
    let distance = (cull * cull + cull_radius * cull_radius).sqrt();
    // Past a few kilometres the fog has long hidden it.
    (distance < 4000.0).then_some(distance)
}

/// How far from its origin BF2 draws a player control object (vehicle, soldier) of this cull
/// radius (`getRadius` times `cullRadiusScale`): the object manager's rule with
/// `distanceCullConstPCO` at the highest quality. `None` for those that reach the view
/// distance anyway.
pub fn pco_draw_distance(cull_radius: Option<f32>) -> Option<f32> {
    let r = cull_radius.filter(|r| *r > 0.0 && r.is_finite())?;
    let cull = ((10.0 * std::f32::consts::PI).sqrt() * DISTANCE_CULL_CONST_PCO * r).max(MIN_CULL_DISTANCE);
    // Measured to the cull sphere: distance² − r² = cull².
    let distance = (cull * cull + r * r).sqrt();
    (distance < 4000.0).then_some(distance)
}

/// How far BF2 draws a small part of an object (a rotor, a carried weapon) of this cull radius
/// on its own: `C² = max(π r² · 0.5 · distanceCullConst² · 10, (minCullDistance / 2)²)`.
pub fn small_part_draw_distance(cull_radius: f32) -> Option<f32> {
    let r = Some(cull_radius).filter(|r| *r > 0.0 && r.is_finite())?;
    let cull = ((5.0 * std::f32::consts::PI).sqrt() * DISTANCE_CULL_CONST * r).max(MIN_CULL_DISTANCE * 0.5);
    let distance = (cull * cull + r * r).sqrt();
    (distance < 4000.0).then_some(distance)
}

/// Half the diagonal of a bounding box: `r0` of the LOD selection (BF2 compares the camera
/// distance minus it with the switch distances).
pub fn half_diagonal(min: [f32; 3], max: [f32; 3]) -> f32 {
    (0..3).map(|i| (max[i] - min[i]).powi(2)).sum::<f32>().sqrt() * 0.5
}

/// Distance from the origin to the farthest corner of a bounding box.
pub fn corner_radius(min: [f32; 3], max: [f32; 3]) -> f32 {
    (0..3).map(|i| min[i].abs().max(max[i].abs()).powi(2)).sum::<f32>().sqrt()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn geometry(lines: &[(&str, &str, &str)]) -> Geometry {
        Geometry {
            ty: "StaticMesh".into(),
            name: "test".into(),
            dir: String::new(),
            props: lines
                .iter()
                .map(|(g, l, d)| {
                    (
                        "setsubgeometryloddistance".to_string(),
                        vec![g.to_string(), l.to_string(), d.to_string()],
                    )
                })
                .collect(),
        }
    }

    #[test]
    fn default_lod_distances_step_by_50() {
        assert_eq!(lod_distances(None, 0, 4), vec![50.0, 100.0, 150.0]);
        assert_eq!(lod_distances(None, 0, 1), Vec::<f32>::new());
        assert_eq!(lod_distances(None, 0, 0), Vec::<f32>::new());
    }

    #[test]
    fn explicit_lod_distances_feed_the_running_default() {
        // billboard_highway_01: all set.
        let g = geometry(&[("0", "0", "10"), ("0", "1", "23"), ("0", "2", "90")]);
        assert_eq!(lod_distances(Some(&g), 0, 4), vec![10.0, 23.0, 90.0]);
        // lrg_trash_dumpster: only LOD 0 set, LOD 1 continues from 50 + 30.
        let g = geometry(&[("0", "0", "30")]);
        assert_eq!(lod_distances(Some(&g), 0, 3), vec![30.0, 80.0]);
        // Other geoms don't count.
        let g = geometry(&[("1", "0", "30")]);
        assert_eq!(lod_distances(Some(&g), 0, 3), vec![50.0, 100.0]);
        // mosque: LOD 0 unset below set ones gets the default.
        let g = geometry(&[("0", "1", "100"), ("0", "2", "200")]);
        assert_eq!(lod_distances(Some(&g), 0, 4), vec![50.0, 100.0, 200.0]);
    }

    #[test]
    fn out_of_order_distances_skip_lods() {
        assert_eq!(lod_starts(&[10.0, 23.0, 90.0]), vec![10.0, 23.0, 90.0]);
        // LOD 1 would start at 60 but LOD 2 already at 40: LOD 1 is never shown.
        assert_eq!(lod_starts(&[60.0, 40.0]), vec![40.0, 40.0]);
    }

    #[test]
    fn player_control_objects_use_their_own_cull_constant() {
        // A soldier: 0.98 m times cull radius scale 2.5 is drawn to about 137 m.
        let d = pco_draw_distance(Some(SOLDIER_RADIUS * 2.5)).unwrap();
        assert!((d - 137.3).abs() < 0.5, "{d}");
        // A tank of radius 5 m: 56 m per meter of radius.
        let d = pco_draw_distance(Some(5.0)).unwrap();
        assert!((d - 280.3).abs() < 0.5, "{d}");
        // Small ones: the 80 m minimum.
        let d = pco_draw_distance(Some(1.0)).unwrap();
        assert!((d - 80.006).abs() < 0.01, "{d}");
        // A carried weapon (half diagonal 0.51 m): the small part minimum, 40 m.
        let d = small_part_draw_distance(0.51).unwrap();
        assert!((d - 40.003).abs() < 0.01, "{d}");
        assert!((half_diagonal([-1.0, 0.0, -2.0], [1.0, 2.0, 2.0]) - 2.449).abs() < 0.01);
    }

    #[test]
    fn small_objects_are_culled_at_the_minimum_distance() {
        // A 1 m crate: 35.9 m from the radius, so the 80 m minimum.
        let d = draw_distance(None, 1.0).unwrap();
        assert!((d - 80.004).abs() < 0.01, "{d}");
        // A house of radius 13 m: 0.8 · 13 · 8 · sqrt(10π) = 466 m (plus the sphere).
        let d = draw_distance(None, 13.0).unwrap();
        assert!((d - 466.8).abs() < 1.0, "{d}");
        assert_eq!(draw_distance(None, 0.0), None);
        assert_eq!(draw_distance(None, 200.0), None);
    }
}
