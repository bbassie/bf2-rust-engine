//! Draws static level objects: loads the glTF mesh named by [`StaticMesh`], and the lower
//! LODs from [`StaticMeshLods`], and spawns their primitives as children once loaded, with
//! BF2 materials.
//!
//! Each LOD is drawn within its distance band from the camera (Bevy's [`VisibilityRange`],
//! cross-fading by dithering around each switch distance), and objects fade out past their
//! draw distance the way BF2 stops drawing small objects. Distances come from the import
//! (BF2's own LOD distances and cull rules at the highest geometry quality); the view
//! distance setting scales the draw distances. Zooming in reaches further like in BF2: the
//! switch distances grow with the zoom and the draw distances with its square root.
//! `BF2_STATIC_LODS=off` draws the full-detail mesh at any distance (for comparisons),
//! `BF2_LOD_SCALE` / `BF2_DRAW_SCALE` scale the LOD switch / draw distances.

use bevy::{
    asset::LoadState,
    camera::visibility::VisibilityRange,
    gltf::{Gltf, GltfAssetLabel, GltfMesh},
    prelude::*,
};
use game_shared::statics::{StaticMesh, StaticMeshLods};

use super::materials::{Bf2Material, Bf2Materials};
use crate::{camera::PlayerCamera, settings::Settings};

pub struct StaticRenderPlugin;

impl Plugin for StaticRenderPlugin {
    fn build(&self, app: &mut App) {
        app.insert_resource(LodConfig::from_env())
            .init_resource::<LodScales>()
            .add_observer(request_mesh)
            .add_systems(
                Update,
                (update_lod_scales, spawn_loaded_meshes, rescale_lod_ranges).chain(),
            );
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
    /// `BF2_LOD_DEBUG`: log what loading static meshes wait for.
    debug: bool,
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
            debug: std::env::var_os("BF2_LOD_DEBUG").is_some(),
        };
        if !config.enabled {
            info!("static mesh LODs and draw distances are off (BF2_STATIC_LODS)");
        }
        config
    }
}

/// The factors on the imported switch and draw distances right now: the environment's, the
/// view distance setting's and the camera zoom's. Vehicles and soldiers use them too.
#[derive(Resource, Clone, Copy, Debug, PartialEq)]
pub(crate) struct LodScales {
    pub(crate) lod: f32,
    pub(crate) draw: f32,
}

impl Default for LodScales {
    fn default() -> Self {
        Self { lod: 1.0, draw: 1.0 }
    }
}

/// BF2's zoom factor of the player camera, `tan(fov₀/2) / tan(fov/2)` with fov₀ the
/// unzoomed field of view, in quarter-octave steps so easing into a zoom touches the
/// ranges only a few times.
fn camera_zoom(fov: f32, unzoomed_fov: f32) -> f32 {
    let zoom = (unzoomed_fov * 0.5).tan() / (fov * 0.5).tan();
    if !zoom.is_finite() || zoom < 1.1 {
        return 1.0;
    }
    2f32.powf((zoom.log2() * 4.0).round() / 4.0)
}

fn update_lod_scales(
    config: Res<LodConfig>,
    settings: Option<Res<Settings>>,
    camera: Query<&Projection, With<PlayerCamera>>,
    mut scales: ResMut<LodScales>,
) {
    let zoom = match (camera.single(), &settings) {
        (Ok(Projection::Perspective(perspective)), Some(settings)) => {
            camera_zoom(perspective.fov, settings.field_of_view.to_radians())
        }
        _ => 1.0,
    };
    let view_distance = settings.as_ref().map_or(1.0, |s| s.view_distance.scale());
    // The graphics `lod_detail_scale` setting: lower presets switch to a cheaper LOD sooner.
    let detail = settings.as_ref().map_or(1.0, |s| s.lod_detail_scale);
    let changed = scales.set_if_neq(LodScales {
        lod: config.lod_scale * zoom * detail,
        // BF2 compares the squared cull distance times the zoom.
        draw: config.draw_scale * view_distance * zoom.sqrt(),
    });
    if changed && config.debug {
        info!("static LOD scales: zoom {zoom:.2}, {:?}", *scales);
    }
}

/// Meshes still loading: the full-detail one first, then each lower LOD.
#[derive(Component)]
struct PendingMesh {
    levels: Vec<PendingLevel>,
    draw_distance: Option<f32>,
}

struct PendingLevel {
    mesh: Handle<GltfMesh>,
    /// The whole file, held until the level is spawned: only the root `Gltf` asset keeps
    /// the glTF's `StandardMaterial`s (which the BF2 materials are built from) alive, and
    /// without it they can be dropped between the mesh arriving and its materials being
    /// read, which would reload the file over and over.
    _file: Handle<Gltf>,
    /// Distance it takes over at (0 for the full-detail mesh).
    start: f32,
    /// Its primitives with their BF2 materials, once loaded.
    ready: Option<Vec<(Handle<Mesh>, Handle<Bf2Material>)>>,
    /// A lower LOD that didn't load is left out (the one before it then reaches further).
    failed: bool,
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
    let level = |path: &str, start: f32| PendingLevel {
        mesh: asset_server.load(GltfAssetLabel::Mesh(mesh.index as usize).from_asset(format!("imported://{path}"))),
        _file: asset_server.load(format!("imported://{path}")),
        start,
        ready: None,
        failed: false,
    };
    let mut levels = vec![level(&mesh.path, 0.0)];
    let mut draw_distance = None;
    if let Some(lods) = lods.filter(|_| config.enabled) {
        levels.extend(lods.lods.iter().map(|lod| level(&lod.mesh, lod.distance)));
        draw_distance = lods.draw_distance;
    }
    commands.entity(add.entity).insert((
        PendingMesh { levels, draw_distance },
        Visibility::default(),
    ));
}

fn spawn_loaded_meshes(
    mut commands: Commands,
    mut pending: Query<(Entity, &mut PendingMesh)>,
    gltf_meshes: Res<Assets<GltfMesh>>,
    mut materials: Bf2Materials,
    asset_server: Res<AssetServer>,
    config: Res<LodConfig>,
    scales: Res<LodScales>,
    time: Res<Time<Real>>,
    mut debug_log: Local<f32>,
) {
    let started = std::time::Instant::now();
    let (mut waiting_meshes, mut waiting_materials) = (0, 0);
    for (entity, mut pending) in &mut pending {
        if let LoadState::Failed(err) = asset_server.load_state(&pending.levels[0].mesh) {
            warn!("static mesh failed to load: {err}");
            commands.entity(entity).try_remove::<PendingMesh>();
            continue;
        }
        let mut complete = true;
        for (index, level) in pending.levels.iter_mut().enumerate() {
            if level.ready.is_some() || level.failed {
                continue;
            }
            match gltf_meshes.get(&level.mesh) {
                Some(mesh) => {
                    // The glTF's own materials may not be ready yet; try again next frame.
                    level.ready = mesh
                        .primitives
                        .iter()
                        .map(|primitive| Some((primitive.mesh.clone(), materials.for_primitive(primitive)?)))
                        .collect();
                    if level.ready.is_none() {
                        waiting_materials += 1;
                        complete = false;
                    }
                }
                None if index > 0 && matches!(asset_server.load_state(&level.mesh), LoadState::Failed(_)) => {
                    level.failed = true;
                }
                None => {
                    waiting_meshes += 1;
                    complete = false;
                }
            }
        }
        if !complete {
            continue;
        }

        let levels: Vec<(f32, Vec<(Handle<Mesh>, Handle<Bf2Material>)>)> = pending
            .levels
            .iter()
            .filter_map(|level| Some((level.start, level.ready.clone()?)))
            .collect();
        let set = StaticLodSet {
            starts: levels.iter().map(|(start, _)| *start).collect(),
            draw_distance: pending.draw_distance,
        };
        let ranges = set.ranges(scales.lod, scales.draw);
        // One command on the owner, silently dropped if it is gone by the time commands are
        // applied (debris whose life ran out the frame its mesh finished loading): no
        // orphaned children, no panic.
        commands.entity(entity).queue_silenced(move |mut owner: EntityWorldMut| {
            owner.remove::<PendingMesh>();
            owner.with_children(|owner| {
                for (index, (_, primitives)) in levels.iter().enumerate() {
                    for (mesh, material) in primitives {
                        let mut child = owner.spawn((Mesh3d(mesh.clone()), MeshMaterial3d(material.clone())));
                        if let Some(range) = &ranges[index] {
                            child.insert((range.clone(), StaticLodLevel(index)));
                        }
                    }
                }
            });
            if ranges.iter().any(Option::is_some) {
                owner.insert(set);
            }
        });
    }
    let now = time.elapsed_secs();
    if config.debug && now - *debug_log > 2.0 && waiting_meshes + waiting_materials > 0 {
        *debug_log = now;
        info!(
            "static meshes: {waiting_meshes} levels waiting for meshes, {waiting_materials} for materials ({:.2} ms)",
            started.elapsed().as_secs_f64() * 1000.0,
        );
    }
}

impl StaticLodSet {
    /// The visibility range of each level (`None`: drawn at any distance), with the switch
    /// distances times `lod_scale` and the draw distance times `draw_scale`.
    fn ranges(&self, lod_scale: f32, draw_scale: f32) -> Vec<Option<VisibilityRange>> {
        lod_ranges(&self.starts, self.draw_distance, lod_scale, draw_scale)
    }
}

/// The visibility range of each level of detail that starts at `starts` (the first at 0)
/// of something drawn up to `draw_distance` (`None`: drawn at any distance), cross-fading
/// around each switch: with the switch distances times `lod_scale` and the draw distance
/// times `draw_scale`.
pub(crate) fn lod_ranges(
    starts: &[f32],
    draw_distance: Option<f32>,
    lod_scale: f32,
    draw_scale: f32,
) -> Vec<Option<VisibilityRange>> {
    let end = draw_distance.map_or(f32::INFINITY, |d| d * draw_scale);
    if starts.len() <= 1 && end.is_infinite() {
        return vec![None; starts.len()];
    }
    let count = starts.len();
    // Where each level starts (never decreasing); levels starting past the draw
    // distance are never drawn.
    let mut starts: Vec<f32> = starts.iter().map(|s| s * lod_scale).collect();
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
    (0..count)
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

/// Applies changed [`LodScales`] (view distance setting, zoom) to the spawned levels.
fn rescale_lod_ranges(
    scales: Res<LodScales>,
    mut last: Local<Option<LodScales>>,
    sets: Query<(&StaticLodSet, &Children)>,
    mut levels: Query<(&StaticLodLevel, &mut VisibilityRange)>,
) {
    if last.replace(*scales).is_none_or(|last| last == *scales) {
        return;
    }
    for (set, children) in &sets {
        let ranges = set.ranges(scales.lod, scales.draw);
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
    fn zoom_is_bf2_style_and_stepped() {
        let fov = 75f32.to_radians();
        assert_eq!(camera_zoom(fov, fov), 1.0);
        // A slight narrowing (iron sights easing in) doesn't count yet.
        assert_eq!(camera_zoom(fov * 0.95, fov), 1.0);
        // Half the FOV angle is a bit more than twice the zoom; steps of 2^(1/4).
        let z = camera_zoom(fov * 0.5, fov);
        assert!((z - 2f32.powf(1.25)).abs() < 1e-4, "{z}");
        assert_eq!(camera_zoom(0.0, fov), 1.0);
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
