//! Draws static level objects: loads the glTF mesh named by [`StaticMesh`], and the lower
//! LODs from [`StaticMeshLods`], and spawns their primitives as children once loaded, with
//! BF2 materials.
//!
//! Each LOD is drawn within its distance band from the camera (Bevy's [`VisibilityRange`],
//! cross-fading by dithering around each switch distance), and objects fade out past their
//! draw distance the way BF2 stops drawing small objects. Distances come from the import
//! (BF2's own LOD distances and cull rules at the highest geometry quality); the view
//! distance setting scales the draw distances. `BF2_STATIC_LODS=off` draws the full-detail
//! mesh at any distance (for comparisons), `BF2_LOD_SCALE` / `BF2_DRAW_SCALE` scale the LOD
//! switch / draw distances.

use bevy::{
    asset::LoadState,
    camera::visibility::VisibilityRange,
    gltf::{GltfAssetLabel, GltfMesh},
    prelude::*,
};
use game_shared::statics::{StaticMesh, StaticMeshLods};

use super::materials::Bf2Materials;
use crate::settings::Settings;

pub struct StaticRenderPlugin;

impl Plugin for StaticRenderPlugin {
    fn build(&self, app: &mut App) {
        app.insert_resource(LodConfig::from_env())
            .add_observer(request_mesh)
            .add_systems(Update, (spawn_loaded_meshes, rescale_draw_distances).chain());
    }
}

/// How much of a switch distance on either side the LODs cross-fade over.
const CROSSFADE: f32 = 0.1;

/// Global LOD tuning, from the environment (see the module docs).
#[derive(Resource, Clone, Copy, Debug)]
struct LodConfig {
    enabled: bool,
    lod_scale: f32,
    draw_scale: f32,
}

impl LodConfig {
    fn from_env() -> Self {
        let number = |name: &str| {
            std::env::var(name)
                .ok()
                .and_then(|v| v.parse::<f32>().ok())
                .filter(|v| v.is_finite() && *v > 0.0)
                .unwrap_or(1.0)
        };
        let enabled = !matches!(
            std::env::var("BF2_STATIC_LODS").as_deref().map(str::to_ascii_lowercase).as_deref(),
            Ok("off" | "0" | "false" | "no")
        );
        let config = Self {
            enabled,
            lod_scale: number("BF2_LOD_SCALE"),
            draw_scale: number("BF2_DRAW_SCALE"),
        };
        if !config.enabled {
            info!("static mesh LODs and draw distances are off (BF2_STATIC_LODS)");
        }
        config
    }
}

/// Meshes still loading: the full-detail one first, then each lower LOD with the distance
/// it takes over at.
#[derive(Component)]
struct PendingMesh {
    levels: Vec<(Handle<GltfMesh>, f32)>,
    draw_distance: Option<f32>,
}

/// On a static part whose meshes are drawn by distance: where each spawned level starts
/// (the first at 0) and how far the object is drawn, before scaling.
#[derive(Component, Clone, Debug)]
struct StaticLodSet {
    starts: Vec<f32>,
    draw_distance: Option<f32>,
}

/// A primitive of level `.0` of its parent's [`StaticLodSet`].
#[derive(Component, Clone, Copy, Debug)]
struct StaticLodLevel(usize);

fn request_mesh(
    add: On<Add, StaticMesh>,
    meshes: Query<(&StaticMesh, Option<&StaticMeshLods>)>,
    config: Res<LodConfig>,
    asset_server: Res<AssetServer>,
    mut commands: Commands,
) {
    let Ok((mesh, lods)) = meshes.get(add.entity) else {
        return;
    };
    let load = |path: &str| {
        asset_server.load(GltfAssetLabel::Mesh(mesh.index as usize).from_asset(format!("imported://{path}")))
    };
    let mut levels = vec![(load(&mesh.path), 0.0)];
    let mut draw_distance = None;
    if let Some(lods) = lods.filter(|_| config.enabled) {
        levels.extend(lods.lods.iter().map(|lod| (load(&lod.mesh), lod.distance)));
        draw_distance = lods.draw_distance;
    }
    commands.entity(add.entity).insert((
        PendingMesh { levels, draw_distance },
        Visibility::default(),
    ));
}

fn spawn_loaded_meshes(
    mut commands: Commands,
    pending: Query<(Entity, &PendingMesh)>,
    gltf_meshes: Res<Assets<GltfMesh>>,
    mut materials: Bf2Materials,
    asset_server: Res<AssetServer>,
    config: Res<LodConfig>,
    settings: Option<Res<Settings>>,
) {
    let draw_scale = config.draw_scale * settings.as_ref().map_or(1.0, |s| s.view_distance.scale());
    'entities: for (entity, pending) in &pending {
        let (full_detail, _) = &pending.levels[0];
        if let LoadState::Failed(err) = asset_server.load_state(full_detail) {
            warn!("static mesh failed to load: {err}");
            commands.entity(entity).remove::<PendingMesh>();
            continue;
        }
        // Wait for every level; a lower LOD that fails is left out (the one before it
        // then reaches further).
        let mut levels = Vec::with_capacity(pending.levels.len());
        for (index, (handle, start)) in pending.levels.iter().enumerate() {
            match gltf_meshes.get(handle) {
                Some(mesh) => levels.push((mesh, *start)),
                None if index > 0 && matches!(asset_server.load_state(handle), LoadState::Failed(_)) => {}
                None => continue 'entities,
            }
        }
        // The glTF's own materials may not be ready yet; try again next frame.
        let Some(level_materials) = levels
            .iter()
            .map(|(mesh, _)| {
                mesh.primitives
                    .iter()
                    .map(|primitive| materials.for_primitive(primitive))
                    .collect::<Option<Vec<_>>>()
            })
            .collect::<Option<Vec<_>>>()
        else {
            continue;
        };

        let set = StaticLodSet {
            starts: levels.iter().map(|(_, start)| *start).collect(),
            draw_distance: pending.draw_distance,
        };
        let ranges = set.ranges(config.lod_scale, draw_scale);
        for (index, ((mesh, _), primitive_materials)) in levels.iter().zip(level_materials).enumerate() {
            for (primitive, material) in mesh.primitives.iter().zip(primitive_materials) {
                let mut child = commands.spawn((
                    Mesh3d(primitive.mesh.clone()),
                    MeshMaterial3d(material),
                    ChildOf(entity),
                ));
                if let Some(range) = &ranges[index] {
                    child.insert((range.clone(), StaticLodLevel(index)));
                }
            }
        }
        let mut parent = commands.entity(entity);
        parent.remove::<PendingMesh>();
        if ranges.iter().any(Option::is_some) {
            parent.insert(set);
        }
    }
}

impl StaticLodSet {
    /// The visibility range of each level (`None`: drawn at any distance), with the switch
    /// distances times `lod_scale` and the draw distance times `draw_scale`.
    fn ranges(&self, lod_scale: f32, draw_scale: f32) -> Vec<Option<VisibilityRange>> {
        let end = self.draw_distance.map_or(f32::INFINITY, |d| d * draw_scale);
        if self.starts.len() <= 1 && end.is_infinite() {
            return vec![None; self.starts.len()];
        }
        // Where each level starts (never decreasing); levels starting past the draw
        // distance are never drawn.
        let mut starts: Vec<f32> = self.starts.iter().map(|s| s * lod_scale).collect();
        starts[0] = 0.0;
        for i in 1..starts.len() {
            starts[i] = starts[i].max(starts[i - 1]);
        }
        let shown = starts.iter().take_while(|s| **s < end).count().max(1);
        // Level k is drawn from bounds[k] to bounds[k + 1].
        let mut bounds = starts[..shown].to_vec();
        bounds.push(end);
        // Half the cross-fade band around each bound, never reaching past a neighbour's
        // midpoint so the bands of successive levels don't overlap.
        let widths: Vec<f32> = (0..bounds.len())
            .map(|i| {
                if i == 0 || bounds[i].is_infinite() {
                    return 0.0;
                }
                let below = (bounds[i] - bounds[i - 1]) * 0.5;
                let above = bounds.get(i + 1).map_or(f32::INFINITY, |next| (next - bounds[i]) * 0.5);
                (bounds[i] * CROSSFADE).min(below).min(above).max(0.0)
            })
            .collect();
        let band = |i: usize| {
            if bounds[i].is_infinite() {
                f32::INFINITY..f32::INFINITY
            } else {
                (bounds[i] - widths[i])..(bounds[i] + widths[i])
            }
        };
        (0..self.starts.len())
            .map(|k| {
                Some(if k >= shown {
                    VisibilityRange::abrupt(0.0, 0.0)
                } else {
                    VisibilityRange {
                        start_margin: if k == 0 { 0.0..0.0 } else { band(k) },
                        end_margin: band(k + 1),
                        use_aabb: false,
                    }
                })
            })
            .collect()
    }
}

/// The view distance setting scales how far objects are drawn.
fn rescale_draw_distances(
    config: Res<LodConfig>,
    settings: Option<Res<Settings>>,
    mut last_scale: Local<Option<f32>>,
    sets: Query<(&StaticLodSet, &Children)>,
    mut levels: Query<(&StaticLodLevel, &mut VisibilityRange)>,
) {
    let draw_scale = config.draw_scale * settings.as_ref().map_or(1.0, |s| s.view_distance.scale());
    if last_scale.replace(draw_scale).is_none_or(|last| last == draw_scale) {
        return;
    }
    for (set, children) in &sets {
        let ranges = set.ranges(config.lod_scale, draw_scale);
        for child in children.iter() {
            if let Ok((level, mut range)) = levels.get_mut(child)
                && let Some(Some(new)) = ranges.get(level.0)
            {
                range.set_if_neq(new.clone());
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn set(starts: &[f32], draw: Option<f32>) -> StaticLodSet {
        StaticLodSet {
            starts: starts.to_vec(),
            draw_distance: draw,
        }
    }

    #[test]
    fn single_mesh_without_draw_distance_has_no_range() {
        assert!(set(&[0.0], None).ranges(1.0, 1.0).iter().all(Option::is_none));
    }

    #[test]
    fn levels_cross_fade_at_their_switch_distances() {
        let ranges = set(&[0.0, 50.0, 100.0], Some(400.0)).ranges(1.0, 1.0);
        let r: Vec<_> = ranges.into_iter().map(Option::unwrap).collect();
        assert_eq!(r[0].start_margin, 0.0..0.0);
        assert_eq!(r[0].end_margin, 45.0..55.0);
        assert_eq!(r[1].start_margin, 45.0..55.0);
        assert_eq!(r[1].end_margin, 90.0..110.0);
        assert_eq!(r[2].start_margin, 90.0..110.0);
        assert_eq!(r[2].end_margin, 360.0..440.0);
    }

    #[test]
    fn close_switches_and_short_draw_distances_stay_consistent() {
        // Switches 2 m apart: the bands shrink to 1 m on each side.
        let r: Vec<_> = set(&[0.0, 30.0, 32.0], None).ranges(1.0, 1.0).into_iter().map(Option::unwrap).collect();
        assert_eq!(r[1].start_margin, 29.0..31.0);
        assert_eq!(r[1].end_margin, 31.0..33.0);
        assert_eq!(r[2].end_margin, f32::INFINITY..f32::INFINITY);
        // Drawn only to 80 m: the LOD from 100 m on never shows.
        let r: Vec<_> = set(&[0.0, 50.0, 100.0], Some(80.0)).ranges(1.0, 1.0).into_iter().map(Option::unwrap).collect();
        assert!(r[2].is_culled(85.0) && r[2].is_culled(0.0) && r[2].is_culled(200.0));
        assert!(!r[1].is_culled(70.0));
        assert_eq!(r[1].end_margin, 72.0..88.0);
        for range in &r {
            assert!(range.start_margin.end <= range.end_margin.start);
        }
    }
}
