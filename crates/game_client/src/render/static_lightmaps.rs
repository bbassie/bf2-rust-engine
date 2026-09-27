//! Baked sky visibility on static objects: BF2 baked a lightmap for every placed object; the
//! importer keeps their sky channel as one texture array (`EnvironmentDesc::static_lightmaps`)
//! with where each object's lightmap lies in it. Here each static mesh entity gets the
//! placement of its own lightmap in its `MeshTag`, which `bf2_material.wgsl` combines with the
//! mesh's lightmap UVs (`materials::ATTRIBUTE_LIGHTMAP_UV`) to occlude the ambient light:
//! dark interiors, alleys and undersides, as in BF2. The baked sun is never used.

use bevy::{mesh::MeshTag, platform::collections::HashMap, prelude::*};
use game_data::{LevelDesc, StaticLightmaps};
use game_shared::{
    config::GamePaths,
    level::LoadedLevel,
    statics::{Destructible, StaticMesh, StaticMeshLods},
};

pub struct StaticLightmapsPlugin;

impl Plugin for StaticLightmapsPlugin {
    fn build(&self, app: &mut App) {
        app.init_resource::<LightmapIndex>().add_systems(
            Update,
            (
                load_lightmap_index.run_if(resource_exists_and_changed::<LoadedLevel>),
                tag_static_meshes.run_if(resource_exists::<LoadedLevel>),
            )
                .chain(),
        );
    }
}

/// `MeshTag` bit: the other bits place the mesh's lightmap in the atlas array (see
/// [`encode_tag`]).
pub const LIGHTMAP_TAG: u32 = 1 << 30;

/// The level's static lightmaps (`None` without), from its `static_lightmaps` RON.
pub fn load_index(desc: &LevelDesc, paths: &GamePaths) -> Option<StaticLightmaps> {
    let path = desc.environment.static_lightmaps.as_ref()?;
    paths
        .read_ron::<StaticLightmaps>(format!("levels/{}/{path}", desc.name))
        .map_err(|e| warn!("static lightmaps: {e:#}"))
        .ok()
}

/// `MeshTag` for a lightmap at `offset` with size `scale` in atlas `layer`: the size must be
/// 1/2^n (n <= 15) on each axis and the offset a whole number of sizes (below 256), as BF2's
/// atlases place almost all lightmaps. Bits: 30 set, 24..30 layer, 20..24 and 16..20 the
/// exponents of the width and height, 8..16 and 0..8 the cell.
pub fn encode_tag(layer: u32, offset: [f32; 2], scale: [f32; 2]) -> Option<u32> {
    let exponent = |s: f32| {
        let e = -s.log2();
        ((e - e.round()).abs() < 1e-3 && (0.0..=15.0).contains(&e.round())).then(|| e.round() as u32)
    };
    let cell = |o: f32, s: f32| {
        let c = o / s;
        ((c - c.round()).abs() < 1e-3 && (0.0..256.0).contains(&c.round())).then(|| c.round() as u32)
    };
    let (ex, ey) = (exponent(scale[0])?, exponent(scale[1])?);
    let (cx, cy) = (cell(offset[0], scale[0])?, cell(offset[1], scale[1])?);
    (layer < 64).then(|| LIGHTMAP_TAG | layer << 24 | ex << 20 | ey << 16 | cx << 8 | cy)
}

/// Lightmap tags by (geometry name, geometry index, level of detail), with their positions.
#[derive(Resource, Default)]
struct LightmapIndex {
    entries: HashMap<(String, u32, u32), Vec<(Vec3, u32)>>,
}

impl LightmapIndex {
    /// The tag of the object's lightmap: BF2 names them after the position truncated to whole
    /// meters (towards zero, so the same in engine coordinates); else the closest within 2 m.
    fn find(&self, name: &str, geometry: u32, lod: u32, position: Vec3) -> Option<u32> {
        let entries = self.entries.get(&(name.to_string(), geometry, lod))?;
        let truncated = position.trunc();
        entries
            .iter()
            .find(|(p, _)| p.distance_squared(truncated) < 0.01)
            .map(|(_, tag)| *tag)
            .or_else(|| {
                entries
                    .iter()
                    .map(|(p, tag)| (p.distance_squared(position), *tag))
                    .filter(|(d, _)| *d < 4.0)
                    .min_by(|a, b| a.0.total_cmp(&b.0))
                    .map(|(_, tag)| tag)
            })
    }
}

fn load_lightmap_index(level: Res<LoadedLevel>, paths: Res<GamePaths>, mut index: ResMut<LightmapIndex>) {
    index.entries.clear();
    let Some(lightmaps) = load_index(&level.desc, &paths) else {
        return;
    };
    let mut skipped = 0;
    for entry in &lightmaps.entries {
        let Some(tag) = encode_tag(entry.layer, entry.offset, entry.scale) else {
            skipped += 1;
            continue;
        };
        index
            .entries
            .entry((entry.name.clone(), entry.geometry, entry.lod))
            .or_default()
            .push((Vec3::from_array(entry.position), tag));
    }
    info!(
        "static lightmaps: {} objects, {skipped} not placeable",
        lightmaps.entries.len() - skipped
    );
}

/// A static mesh entity whose lightmap has been looked up.
#[derive(Component)]
struct LightmapChecked;

/// The file stem of a mesh path without the LOD and wreck suffixes, and whether it's a wreck.
fn geometry_name(path: &str) -> (String, bool) {
    let stem = std::path::Path::new(path)
        .file_stem()
        .map(|s| s.to_string_lossy().to_ascii_lowercase())
        .unwrap_or_default();
    let stem = match stem.rsplit_once("_lod") {
        Some((base, n)) if !n.is_empty() && n.chars().all(|c| c.is_ascii_digit()) => base.to_string(),
        _ => stem,
    };
    match stem.strip_suffix("_wreck") {
        Some(base) => (base.to_string(), true),
        None => (stem, false),
    }
}

#[allow(clippy::type_complexity)]
fn tag_static_meshes(
    mut commands: Commands,
    index: Res<LightmapIndex>,
    meshes: Query<(Entity, &Mesh3d, &ChildOf), (Without<LightmapChecked>, Without<MeshTag>)>,
    parents: Query<(&StaticMesh, Option<&StaticMeshLods>, Option<&Destructible>, &Transform)>,
) {
    for (entity, mesh, child_of) in &meshes {
        let Ok((static_mesh, lods, destructible, transform)) = parents.get(child_of.parent()) else {
            // Not a static object's mesh (soldiers, vehicles, effects...).
            commands.entity(entity).insert(LightmapChecked);
            continue;
        };
        let mut checked = commands.entity(entity);
        checked.insert(LightmapChecked);
        if index.entries.is_empty() {
            continue;
        }
        let Some(mesh_path) = mesh.0.path().map(|p| p.path().to_string_lossy().replace('\\', "/")) else {
            continue;
        };
        let lod = if mesh_path == static_mesh.path {
            Some(0)
        } else {
            lods.and_then(|l| l.lods.iter().position(|lod| lod.mesh == mesh_path))
                .map(|i| i as u32 + 1)
        };
        let Some(lod) = lod else {
            continue;
        };
        let (name, wreck_file) = geometry_name(&static_mesh.path);
        let wreck = wreck_file || destructible.is_some_and(|d| d.wreck);
        let position = transform.translation;
        let tag = index
            .find(&name, u32::from(wreck), lod, position)
            .or_else(|| index.find(&name, 0, lod, position));
        // `BF2_LIGHTMAP_LOG=<part of a name>` logs the lookups of matching objects.
        static LOG: std::sync::OnceLock<Option<String>> = std::sync::OnceLock::new();
        if let Some(filter) = LOG.get_or_init(|| std::env::var("BF2_LIGHTMAP_LOG").ok())
            && name.contains(filter.as_str())
        {
            info!("lightmap {name} lod {lod} wreck {wreck} at {position:?} ({mesh_path}): {tag:08x?}");
        }
        if let Some(tag) = tag {
            checked.insert(MeshTag(tag));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tags() {
        assert_eq!(encode_tag(3, [0.25, 0.5], [0.125, 0.125]), Some(LIGHTMAP_TAG | 3 << 24 | 3 << 20 | 3 << 16 | 2 << 8 | 4));
        assert_eq!(encode_tag(0, [0.5, 0.0], [0.5, 1.0]), Some(LIGHTMAP_TAG | 1 << 20 | 1 << 8));
        // Not a power of two, or not on the grid.
        assert_eq!(encode_tag(0, [0.0, 0.0], [0.3, 0.3]), None);
        assert_eq!(encode_tag(0, [0.1, 0.0], [0.25, 0.25]), None);
    }

    #[test]
    fn names() {
        assert_eq!(geometry_name("a/b/house_high_06_lod2.glb"), ("house_high_06".into(), false));
        assert_eq!(geometry_name("a/b/House_wreck.glb"), ("house".into(), true));
        assert_eq!(geometry_name("a/b/lod_tower.glb"), ("lod_tower".into(), false));
    }
}
