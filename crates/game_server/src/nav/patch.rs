//! Detail patches: finer grids over big, intricate statics.
//!
//! The level grid's cells grow to 0.75 m on the big layouts, and an aircraft carrier turned
//! at any angle has ramps, doors and catwalks narrower than that: its hangar was cut off from
//! the deck. Such objects (templates named in [`DETAIL_TEMPLATES`]) get a grid of their own
//! with [`PATCH_CELL`] cells in the object's frame, so its corridors run along the cells
//! whatever its heading. It is rasterized from the same collision (everything that reaches
//! into its rectangle, the terrain too) by the same builder as the level grid, and covers the
//! object plus [`MARGIN`]. The level grid leaves the patch's rectangle out except for an
//! [`OVERLAP`] ring along its edge, where portals join each level cell to the patch cell at
//! the same spot. A patch costs a few MB (the carrier's: about 5), against about 80 MB for
//! making the whole level grid finer.
//!
//! This is one representation, not two: patch cells are [`super::NavCell`]s in the grid's
//! arrays and paths, regions, `locate` and `cells_near` treat them like any other.

use avian3d::parry::shape::TypedShape;
use bevy::{platform::collections::HashMap, prelude::*};
use game_data::{ObjectDesc, StaticInstance};
use game_shared::{ladder::Ladder, level::placement_transform};

use super::{
    CellRef, NavGrid, NavParams, NavPatch,
    build::{self, Frame, LevelGeometry, MeshInstance, Rect},
};

/// Horizontal cell size of detail patches, meters.
pub const PATCH_CELL: f32 = 0.33;
/// [`PATCH_CELL`] (tests compare others: `NAV_PATCH_CELL`).
fn patch_cell() -> f32 {
    #[cfg(test)]
    if let Some(cell) = std::env::var("NAV_PATCH_CELL").ok().and_then(|c| c.parse().ok()) {
        return cell;
    }
    PATCH_CELL
}

/// How far a patch reaches beyond its object's collision, meters.
const MARGIN: f32 = 3.0;
/// How far into the patch the level grid reaches: the ring where the two are joined.
const OVERLAP: f32 = 1.25;
/// Statics whose template names contain one of these get a patch.
const DETAIL_TEMPLATES: &[&str] = &["carrier"];
/// Detail statics this close together (and turned alike) are one object: a carrier's hull,
/// island and hangar are separate statics.
const GROUP_DISTANCE: f32 = 400.0;

/// The objects that get a patch, each as a rectangle around its collision in its own frame
/// (the frame of its first static). `template` loads a template; `meshes` are the level's
/// collision meshes, of which those placed where the objects' parts are give the extent.
pub fn detail_objects(
    statics: &[StaticInstance],
    mut template: impl FnMut(&str) -> Option<ObjectDesc>,
    meshes: &[MeshInstance],
) -> Vec<Rect> {
    struct Group {
        rotation: Quat,
        frame: Frame,
        first: Vec3,
        /// Where the parts' collision meshes are placed.
        parts: Vec<Vec3>,
    }
    let mut groups: Vec<Group> = Vec::new();
    for placed in statics {
        if !DETAIL_TEMPLATES.iter().any(|t| placed.template.contains(t)) {
            continue;
        }
        let base = placement_transform(&placed.placement);
        // Only turned about the vertical: patches are horizontal grids.
        if base.rotation.x.abs() > 1e-3 || base.rotation.z.abs() > 1e-3 {
            continue;
        }
        let Some(object) = template(&placed.template) else {
            continue;
        };
        let parts = object
            .parts
            .iter()
            .filter(|p| p.collision.is_some() && !p.ladder)
            .map(|p| (base * placement_transform(&p.placement)).translation);
        let group = groups.iter().position(|g| {
            g.rotation.dot(base.rotation).abs() > 0.9999 && g.first.distance(base.translation) < GROUP_DISTANCE
        });
        let group = match group {
            Some(g) => &mut groups[g],
            None => {
                groups.push(Group {
                    rotation: base.rotation,
                    frame: Frame::new(base.translation.xz(), base.rotation),
                    first: base.translation,
                    parts: Vec::new(),
                });
                groups.last_mut().unwrap()
            }
        };
        group.parts.extend(parts);
    }
    groups
        .iter()
        .filter_map(|g| {
            let (mut lo, mut hi) = (Vec3::MAX, Vec3::MIN);
            for mesh in meshes {
                let at = Vec3::from(mesh.transform.translation);
                if is_terrain(mesh) || !g.parts.iter().any(|p| p.distance_squared(at) < 1e-4) {
                    continue;
                }
                let (a, b) = local_aabb(mesh, g.frame);
                (lo, hi) = (lo.min(a), hi.max(b));
            }
            (lo.x <= hi.x).then(|| Rect {
                frame: g.frame,
                min: lo.xz() - MARGIN,
                max: hi.xz() + MARGIN,
            })
        })
        .collect()
}

fn is_terrain(mesh: &MeshInstance) -> bool {
    matches!(mesh.shape.as_typed_shape(), TypedShape::HeightField(_))
}

/// A mesh's bounds in a frame.
fn local_aabb(mesh: &MeshInstance, frame: Frame) -> (Vec3, Vec3) {
    let to_local = frame.world_from_local().inverse() * mesh.transform;
    build::world_aabb(&MeshInstance {
        shape: mesh.shape.clone(),
        transform: to_local,
    })
}

fn ladder_to_local(ladder: &Ladder, frame: Frame) -> Ladder {
    Ladder {
        center: frame.point_to_local(ladder.center),
        up: frame.dir_to_local(ladder.up),
        front: frame.dir_to_local(ladder.front),
        side: frame.dir_to_local(ladder.side),
        ..*ladder
    }
}

/// Builds the level grid with a patch over each of `patches` (see the module docs).
pub fn build_level(geometry: &LevelGeometry, patches: &[Rect], params: NavParams) -> NavGrid {
    if patches.is_empty() {
        return build::build(geometry, params);
    }
    let holes: Vec<Rect> = patches.iter().map(|r| r.shrunk(OVERLAP)).collect();
    // Meshes reaching into a patch go to it too; those within its hole only to it.
    let mut base_meshes = Vec::new();
    let mut patch_meshes = vec![Vec::new(); patches.len()];
    for mesh in &geometry.meshes {
        if is_terrain(mesh) {
            continue;
        }
        let mut hidden = false;
        for (i, rect) in patches.iter().enumerate() {
            let (lo, hi) = local_aabb(mesh, rect.frame);
            let overlaps = lo.x <= rect.max.x && hi.x >= rect.min.x && lo.z <= rect.max.y && hi.z >= rect.min.y;
            if !overlaps {
                continue;
            }
            patch_meshes[i].push(MeshInstance {
                shape: mesh.shape.clone(),
                transform: rect.frame.world_from_local().inverse() * mesh.transform,
            });
            let hole = holes[i];
            hidden |= lo.x >= hole.min.x && hi.x <= hole.max.x && lo.z >= hole.min.y && hi.z <= hole.max.y;
        }
        if !hidden {
            base_meshes.push(mesh.clone());
        }
    }
    let mut base_ladders = Vec::new();
    let mut patch_ladders = vec![Vec::new(); patches.len()];
    for ladder in &geometry.ladders {
        match holes.iter().position(|h| h.contains(ladder.center.xz())) {
            Some(i) => patch_ladders[i].push(ladder_to_local(ladder, patches[i].frame)),
            None => base_ladders.push(*ladder),
        }
    }
    let base = LevelGeometry {
        terrain: geometry.terrain.clone(),
        meshes: base_meshes,
        ladders: base_ladders,
        bounds: geometry.bounds,
        holes: holes.clone(),
        frame: None,
    };
    let mut grid = build::build_cells(&base, params);
    for (i, rect) in patches.iter().enumerate() {
        let local = LevelGeometry {
            terrain: geometry.terrain.clone(),
            meshes: std::mem::take(&mut patch_meshes[i]),
            ladders: std::mem::take(&mut patch_ladders[i]),
            bounds: Some((rect.min, rect.max)),
            holes: Vec::new(),
            frame: Some(rect.frame),
        };
        let patch = build::build_cells(&local, NavParams { cell: patch_cell(), ..params });
        append(&mut grid, patch, rect.frame);
        stitch(&mut grid, i, rect, &holes[i]);
    }
    build::regions(&mut grid);
    grid
}

/// Appends a patch built in `frame` to the grid.
fn append(grid: &mut NavGrid, patch: NavGrid, frame: Frame) {
    let first_cell = grid.cells.len() as u32;
    let column_base = grid.columns.len() as u32;
    grid.columns.extend(patch.columns.iter().map(|c| c + first_cell));
    grid.cells.extend_from_slice(&patch.cells);
    for ladder in &patch.ladders {
        let index = grid.ladders.len() as u16;
        let shift = |c: CellRef| CellRef {
            index: c.index + first_cell,
            ..c
        };
        let ladder = super::NavLadder {
            bottom: shift(ladder.bottom),
            top: shift(ladder.top),
            foot: frame.point_to_world(ladder.foot),
            head: frame.point_to_world(ladder.head),
            front: frame.dir_to_world(ladder.front),
            down: ladder.down,
        };
        grid.ladder_ends.entry(ladder.bottom.index).or_default().push(index);
        grid.ladder_ends.entry(ladder.top.index).or_default().push(index);
        grid.ladders.push(ladder);
    }
    grid.patches.push(NavPatch {
        frame,
        origin: patch.origin,
        cell: patch.params.cell,
        width: patch.width,
        depth: patch.depth,
        column_base,
        first_cell,
    });
}

/// Joins the level cells in the ring between a patch's hole and its edge to the patch cells
/// at the same spot, each way a soldier can go (walk, step, jump up, drop down).
fn stitch(grid: &mut NavGrid, patch: usize, rect: &Rect, hole: &Rect) {
    let p = grid.patches[patch];
    let space = NavGrid::patch_space(&p);
    let params = grid.params;
    let (lo, hi) = rect.world_aabb();
    let (Some((x0, z0)), Some((x1, z1))) = (
        grid.column_at(lo.x.max(grid.origin.x), lo.y.max(grid.origin.y)),
        grid.column_at(
            hi.x.min(grid.origin.x + grid.width as f32 * params.cell - 0.01),
            hi.y.min(grid.origin.y + grid.depth as f32 * params.cell - 0.01),
        ),
    ) else {
        return;
    };
    let allowed = |from: &super::NavCell, to: &super::NavCell| {
        let dy = to.y - from.y;
        let reach = params.walk_climb(from.slope, to.slope).max(params.jump);
        dy <= reach && dy >= -reach.max(params.drop)
    };
    let search = (0.5 * params.cell / p.cell).ceil() as i32;
    let mut portals: HashMap<u32, Vec<CellRef>> = HashMap::default();
    for z in z0..=z1 {
        for x in x0..=x1 {
            let center = grid.origin + (Vec2::new(x as f32, z as f32) + 0.5) * params.cell;
            if !rect.contains(center) || hole.contains(center) {
                continue;
            }
            let local = p.frame.to_local(center);
            let px = ((local.x - p.origin.x) / p.cell).floor() as i32;
            let pz = ((local.y - p.origin.y) / p.cell).floor() as i32;
            for index in grid.column(x, z) {
                let b = grid.cells[index as usize];
                let mut best: Option<(f32, CellRef)> = None;
                for qz in (pz - search).max(0)..=(pz + search).min(p.depth as i32 - 1) {
                    for qx in (px - search).max(0)..=(px + search).min(p.width as i32 - 1) {
                        let (qx, qz) = (qx as u32, qz as u32);
                        let d = (p.origin + (Vec2::new(qx as f32, qz as f32) + 0.5) * p.cell).distance(local);
                        for j in grid.space_column(&space, qx, qz) {
                            let c = &grid.cells[j as usize];
                            let score = (c.y - b.y).abs() + 0.5 * d;
                            if (allowed(&b, c) || allowed(c, &b)) && best.is_none_or(|(s, _)| score < s) {
                                best = Some((score, CellRef { x: qx, z: qz, index: j }));
                            }
                        }
                    }
                }
                let Some((_, to)) = best else {
                    continue;
                };
                let c = grid.cells[to.index as usize];
                if allowed(&b, &c) {
                    portals.entry(index).or_default().push(to);
                }
                if allowed(&c, &b) {
                    portals.entry(to.index).or_default().push(CellRef { x, z, index });
                }
            }
        }
    }
    for (from, to) in portals {
        grid.portals.entry(from).or_default().extend(to);
    }
}

/// A patch's frame and extent, for the cache key.
pub fn hash_rects(rects: &[Rect]) -> Vec<f32> {
    let mut words = vec![patch_cell(), MARGIN, OVERLAP];
    for r in rects {
        words.extend([r.frame.center.x, r.frame.center.y, r.frame.axis.x, r.frame.axis.y, r.min.x, r.min.y, r.max.x, r.max.y]);
    }
    words
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use avian3d::parry::shape::SharedShape;
    use bevy::math::Affine3A;
    use game_shared::{level::Heightmap, soldier::SoldierTuning};

    use super::*;

    /// A box turned by `yaw` about its middle.
    fn cuboid(center: Vec3, size: Vec3, yaw: f32) -> MeshInstance {
        MeshInstance {
            shape: SharedShape::cuboid(size.x / 2.0, size.y / 2.0, size.z / 2.0),
            transform: Affine3A::from_rotation_translation(Quat::from_rotation_y(yaw), center),
        }
    }

    /// Flat ground from -32 to 32 m, and a 3 m wall through the middle turned by `yaw`
    /// with a door `door` meters wide. Returns the geometry and the wall's frame.
    fn walled(door: f32, yaw: f32) -> (LevelGeometry, Frame) {
        let terrain = Heightmap {
            resolution: 33,
            spacing: 2.0,
            origin: Vec3::new(-32.0, 0.0, -32.0),
            heights: vec![0.0; 33 * 33],
        };
        let rotation = Quat::from_rotation_y(yaw);
        let frame = Frame::new(Vec2::ZERO, rotation);
        // Along the frame's X, on either side of the door, beyond the ground's edges.
        let half = 45.0;
        let segment = (half - door / 2.0) / 1.0;
        let meshes = [-1.0f32, 1.0]
            .iter()
            .map(|side| {
                let local = Vec3::new(side * (door / 2.0 + segment / 2.0), 1.5, 0.0);
                let world = frame.point_to_world(local) + Vec3::Y * 0.0;
                cuboid(world, Vec3::new(segment, 3.0, 0.3), yaw)
            })
            .collect();
        (
            LevelGeometry {
                terrain: Some(Arc::new(terrain)),
                meshes,
                ..default()
            },
            frame,
        )
    }

    /// Movement limits with the level grid's cells of the big layouts.
    fn params() -> NavParams {
        NavParams {
            cell: 0.75,
            ..NavParams::from_tuning(&SoldierTuning::default())
        }
    }

    #[test]
    fn frames_turn_both_ways() {
        let frame = Frame::new(Vec2::new(5.0, -3.0), Quat::from_rotation_y(0.7));
        let p = Vec2::new(1.5, -2.0);
        assert!(frame.to_local(frame.to_world(p)).distance(p) < 1e-5);
        let affine = frame.world_from_local();
        let w = affine.transform_point3(Vec3::new(p.x, 2.0, p.y));
        assert!(Vec2::new(w.x, w.z).distance(frame.to_world(p)) < 1e-5, "{w} {}", frame.to_world(p));
        let d = Vec3::new(0.3, 0.0, 0.9);
        assert!(frame.dir_to_local(frame.dir_to_world(d)).distance(d) < 1e-5);
    }

    #[test]
    fn patches_get_through_narrow_turned_doors() {
        // A 0.8 m door in a wall at 30 degrees: too narrow for 0.75 m cells turned against it,
        // wide enough for the patch's cells along it.
        let yaw = 30f32.to_radians();
        let (geometry, frame) = walled(0.8, yaw);
        let (from, to) = (frame.point_to_world(Vec3::new(0.0, 0.0, -15.0)), frame.point_to_world(Vec3::new(0.0, 0.0, 15.0)));
        let plain = build::build(&geometry, params());
        let p = plain.find_path(from, to).unwrap();
        assert!(!p.complete, "the level grid alone gets through: {p:?} {from} {to} cell {}", plain.params.cell);
        let rect = build::Rect {
            frame,
            min: Vec2::new(-14.0, -4.0),
            max: Vec2::new(14.0, 4.0),
        };
        let grid = build_level(&geometry, &[rect], params());
        assert_eq!(grid.patches().len(), 1);
        assert!(grid.patch_cell_count() > 0 && !grid.portals.is_empty());
        let path = grid.find_path(from, to).unwrap();
        assert!(path.complete, "{path:?}");
        // Through the door, not around the wall's ends.
        assert!(path.waypoints.iter().all(|w| frame.to_local(w.position.xz()).x.abs() < 6.0), "{path:?}");
        // Queries see the patch's cells.
        let door = frame.point_to_world(Vec3::ZERO);
        let at = grid.locate(door, 0.5, None).unwrap();
        assert!(at.index >= grid.base_cells && grid.position(at).xz().distance(door.xz()) < 0.4);
        assert!(grid.cells_near(door.xz(), 1.0).any(|c| c.index >= grid.base_cells));

        // Cached and loaded back, it finds the same way.
        let file = std::env::temp_dir().join(format!("navgrid_patch_{}.bin", std::process::id()));
        super::super::cache::save(&file, 3, &grid).unwrap();
        let loaded = super::super::cache::load(&file, 3, grid.params).unwrap();
        std::fs::remove_file(&file).unwrap();
        assert_eq!(loaded.patches(), grid.patches());
        assert_eq!(loaded.portals.values().map(Vec::len).sum::<usize>(), grid.portals.values().map(Vec::len).sum::<usize>());
        assert!(loaded.find_path(from, to).unwrap().complete);
    }
}
