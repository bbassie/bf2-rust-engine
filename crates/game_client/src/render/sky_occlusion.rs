//! How much sky soldiers, vehicles and props see: BF2 darkened them indoors with a baked
//! "ground hemi map" under the level; we measure it with a few rays up into the world's
//! collision around each object (cached per 2 m cell and refreshed a few times a second) and
//! give its meshes that sky visibility as ambient occlusion (through `MeshTag`, read by
//! `bf2_material.wgsl`). Only materials of moving kinds (bundled and skinned meshes) take
//! part; static meshes use their baked lightmaps.

use avian3d::prelude::{SpatialQuery, SpatialQueryFilter};
use bevy::{mesh::MeshTag, platform::collections::HashMap, prelude::*};
use game_shared::{level::LoadedLevel, physics::GameLayer};

use super::materials::{Bf2Layers, Bf2Material};
use crate::{camera::PlayerCamera, settings::Settings};

pub struct SkyOcclusionPlugin;

impl Plugin for SkyOcclusionPlugin {
    fn build(&self, app: &mut App) {
        app.init_resource::<SkyCells>()
            .add_observer(add_probe)
            .add_systems(
                PostUpdate,
                update_probes
                    .after(TransformSystems::Propagate)
                    .run_if(resource_exists::<LoadedLevel>),
            );
    }
}

/// `MeshTag` bit: the low byte is the mesh's sky visibility (0..255).
pub const SKY_TAG: u32 = 1 << 31;

/// Rays start this far above the mesh's origin (soldiers' and vehicles' origins are at their
/// feet or wheels).
const PROBE_HEIGHT: f32 = 1.0;
/// Size of the cells whose visibility is shared by all meshes in them.
const CELL: f32 = 2.0;
/// Seconds before a cell in use is measured again.
const REFRESH: f32 = 0.4;
/// Cells measured per frame at most.
const BUDGET: usize = 24;
/// Farther from the camera than this, meshes keep their last visibility.
const RANGE: f32 = 250.0;
/// Rays longer than this count as reaching the sky.
const RAY_LENGTH: f32 = 60.0;
/// Visibility with no sky at all (light still bounces in through doors and windows).
const FLOOR: f32 = 0.3;
/// How quickly a mesh's visibility follows a change (1/s).
const RATE: f32 = 5.0;

/// A mesh lit by its measured sky visibility.
#[derive(Component)]
struct SkyProbe {
    visibility: f32,
}

#[derive(Default)]
struct Cell {
    /// 0 (no sky) to 1 (open sky), before [`FLOOR`].
    visibility: f32,
    measured: f32,
    used: f32,
}

#[derive(Resource, Default)]
struct SkyCells {
    cells: HashMap<IVec3, Cell>,
    level: Option<String>,
}

fn add_probe(
    insert: On<Insert, MeshMaterial3d<Bf2Material>>,
    meshes: Query<&MeshMaterial3d<Bf2Material>>,
    materials: Res<Assets<Bf2Material>>,
    mut commands: Commands,
) {
    let Ok(handle) = meshes.get(insert.entity) else {
        return;
    };
    let dynamic = materials
        .get(&handle.0)
        .is_some_and(|m| m.extension.flags & Bf2Layers::DYNAMIC != 0);
    if dynamic {
        commands
            .entity(insert.entity)
            .try_insert((SkyProbe { visibility: 1.0 }, MeshTag(0)));
    }
}

/// Directions (unit vectors) and weights of the rays: straight up, and two rings.
fn rays() -> &'static [(Vec3, f32)] {
    static RAYS: std::sync::OnceLock<Vec<(Vec3, f32)>> = std::sync::OnceLock::new();
    RAYS.get_or_init(|| {
        let mut rays = vec![(Vec3::Y, 2.0)];
        for (elevation, weight, offset) in [(60f32, 1.0, 0.0), (28.0, 0.6, 45.0)] {
            for i in 0..4 {
                let azimuth = (offset + 90.0 * i as f32).to_radians();
                let (e, a) = (elevation.to_radians(), azimuth);
                rays.push((Vec3::new(e.cos() * a.cos(), e.sin(), e.cos() * a.sin()), weight));
            }
        }
        rays
    })
}

fn measure(spatial: &SpatialQuery, filter: &SpatialQueryFilter, origin: Vec3) -> f32 {
    let (mut open, mut total) = (0.0, 0.0);
    for (direction, weight) in rays() {
        let hit = Dir3::new(*direction)
            .ok()
            .and_then(|dir| spatial.cast_ray(origin, dir, RAY_LENGTH, false, filter));
        if hit.is_none() {
            open += weight;
        }
        total += weight;
    }
    open / total
}

#[allow(clippy::too_many_arguments)]
fn update_probes(
    time: Res<Time>,
    level: Res<LoadedLevel>,
    settings: Res<Settings>,
    spatial: SpatialQuery,
    mut cells: ResMut<SkyCells>,
    camera: Query<&GlobalTransform, With<PlayerCamera>>,
    mut probes: Query<(&GlobalTransform, &mut SkyProbe, &mut MeshTag)>,
) {
    let now = time.elapsed_secs();
    let cells = &mut *cells;
    if cells.level.as_deref() != Some(level.desc.name.as_str()) {
        cells.cells.clear();
        cells.level = Some(level.desc.name.clone());
    }
    if !settings.baked_ao {
        for (_, _, mut tag) in &mut probes {
            if tag.0 != 0 {
                tag.0 = 0;
            }
        }
        return;
    }
    let eye = camera.single().map_or(Vec3::ZERO, |c| c.translation());
    let filter = SpatialQueryFilter::from_mask(GameLayer::World);
    let smoothing = 1.0 - (-time.delta_secs() * RATE).exp();
    let mut budget = BUDGET;
    for (transform, mut probe, mut tag) in &mut probes {
        let origin = transform.translation() + Vec3::Y * PROBE_HEIGHT;
        if origin.distance_squared(eye) > RANGE * RANGE && tag.0 != 0 {
            continue;
        }
        let key = (origin / CELL).floor().as_ivec3();
        let cell = cells.cells.entry(key).or_insert(Cell {
            visibility: -1.0,
            ..default()
        });
        cell.used = now;
        let stale = cell.visibility < 0.0 || now - cell.measured > REFRESH;
        if stale && budget > 0 {
            budget -= 1;
            cell.visibility = measure(&spatial, &filter, origin);
            cell.measured = now;
        }
        if cell.visibility < 0.0 {
            continue;
        }
        let target = FLOOR + (1.0 - FLOOR) * cell.visibility;
        // New meshes start at their cell's value; moving ones blend.
        probe.visibility = if tag.0 == 0 {
            target
        } else {
            probe.visibility + (target - probe.visibility) * smoothing
        };
        let wanted = SKY_TAG | (probe.visibility.clamp(0.0, 1.0) * 255.0).round() as u32;
        if tag.0 != wanted {
            tag.0 = wanted;
        }
    }
    cells.cells.retain(|_, cell| now - cell.used < 5.0);
}
