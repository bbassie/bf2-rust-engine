//! Parked vehicles as obstacles: the cells a standing vehicle's hull takes up, which paths
//! go around (see [`super::NavGrid::find_path_avoiding`]).
//!
//! The grid is built from the level's static collision and knows nothing of vehicles; bots
//! walked into helicopters and jets parked on a carrier's deck and got stuck there. Once a
//! second, the cells under every vehicle standing still (hull within [`CLEARANCE`] of a
//! soldier standing on the cell, at knee, waist or head height) are marked in a bit set on a
//! background task. Moving vehicles are left to the bots' dodging.

use std::sync::Arc;

use avian3d::prelude::*;
use bevy::{
    prelude::*,
    tasks::{AsyncComputeTaskPool, Task, futures::check_ready},
};
use game_shared::vehicle::{Vehicle, VehicleMotion};

use super::{NavGrid, Navigation};

/// Seconds between updates.
const UPDATE_EVERY: f32 = 1.0;
/// Vehicles slower than this are obstacles, m/s.
const PARKED_SPEED: f32 = 1.0;
/// How close to a hull a soldier's middle can't get, meters (his capsule's radius and a bit).
const CLEARANCE: f32 = 0.4;
/// Heights above a cell's surface checked against the hull: knees, waist, head.
const HEIGHTS: [f32; 3] = [0.4, 1.0, 1.6];

/// Cells vehicles stand on, by cell index.
#[derive(Default, Debug)]
pub struct NavBlocked {
    bits: Vec<u64>,
    count: usize,
}

impl NavBlocked {
    pub fn contains(&self, index: u32) -> bool {
        self.bits
            .get(index as usize / 64)
            .is_some_and(|w| w >> (index % 64) & 1 != 0)
    }

    /// How many cells are blocked.
    pub fn len(&self) -> usize {
        self.count
    }

    pub fn is_empty(&self) -> bool {
        self.count == 0
    }

    fn insert(&mut self, index: u32) {
        let (word, bit) = (index as usize / 64, index % 64);
        if self.bits.len() <= word {
            self.bits.resize(word + 1, 0);
        }
        if self.bits[word] >> bit & 1 == 0 {
            self.bits[word] |= 1 << bit;
            self.count += 1;
        }
    }
}

/// The cells parked vehicles block right now, for the grid in [`Navigation`].
#[derive(Resource, Clone, Default)]
pub struct NavObstacles(pub Arc<NavBlocked>);

/// A parked vehicle: its hull and pose.
struct Parked {
    collider: Collider,
    position: Vec3,
    rotation: Quat,
}

#[derive(Default)]
pub(super) struct ObstacleUpdate {
    timer: f32,
    /// The update under way, and the grid it is for.
    task: Option<(Task<NavBlocked>, Arc<NavGrid>)>,
    /// Where the vehicles were for the last update: nothing to do while none moved.
    last: Vec<(Entity, Vec3)>,
}

pub(super) fn update_obstacles(
    mut commands: Commands,
    time: Res<Time>,
    mut state: Local<ObstacleUpdate>,
    nav: Res<Navigation>,
    vehicles: Query<(Entity, &VehicleMotion, &Collider), With<Vehicle>>,
) {
    if let Some((task, grid)) = &mut state.task
        && let Some(blocked) = check_ready(task)
    {
        // Not for a grid of a level that is gone meanwhile.
        if Arc::ptr_eq(grid, &nav.0) {
            commands.insert_resource(NavObstacles(Arc::new(blocked)));
        } else {
            state.last.clear();
        }
        state.task = None;
    }
    state.timer -= time.delta_secs();
    if state.timer > 0.0 || state.task.is_some() {
        return;
    }
    state.timer = UPDATE_EVERY;
    let mut parked = Vec::new();
    let mut now = Vec::new();
    for (entity, motion, collider) in &vehicles {
        if motion.velocity.length() > PARKED_SPEED || !motion.position.is_finite() {
            continue;
        }
        now.push((entity, (motion.position * 5.0).round() / 5.0));
        parked.push(Parked {
            collider: collider.clone(),
            position: motion.position,
            rotation: motion.rotation,
        });
    }
    now.sort_by_key(|(e, _)| *e);
    if now == state.last {
        return;
    }
    state.last = now;
    let grid = nav.0.clone();
    let for_task = grid.clone();
    state.task = Some((AsyncComputeTaskPool::get().spawn(async move { blocked_cells(&for_task, &parked) }), grid));
}

/// The cells the vehicles' hulls take up.
fn blocked_cells(grid: &NavGrid, parked: &[Parked]) -> NavBlocked {
    let mut blocked = NavBlocked::default();
    for vehicle in parked {
        let aabb = vehicle.collider.aabb(vehicle.position, vehicle.rotation);
        let (lo, hi) = (aabb.min - CLEARANCE, aabb.max + CLEARANCE);
        let center = (lo.xz() + hi.xz()) * 0.5;
        let radius = (hi.xz() - lo.xz()).length() * 0.5;
        for c in grid.cells_near(center, radius) {
            let p = grid.position(c);
            // Under the hull's box, and the box within a soldier's height of the surface.
            if p.x < lo.x || p.x > hi.x || p.z < lo.z || p.z > hi.z || p.y > hi.y || p.y + HEIGHTS[2] < lo.y {
                continue;
            }
            let hit = HEIGHTS.iter().any(|h| {
                vehicle
                    .collider
                    .distance_to_point(vehicle.position, vehicle.rotation, p + Vec3::Y * *h, true)
                    < CLEARANCE
            });
            if hit {
                blocked.insert(c.index);
            }
        }
    }
    blocked
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use game_shared::{level::Heightmap, soldier::SoldierTuning};

    use super::*;
    use crate::nav::{NavParams, build};

    /// Paths go around a parked vehicle, and to its door.
    #[test]
    fn paths_go_around_parked_vehicles() {
        let terrain = Heightmap {
            resolution: 33,
            spacing: 2.0,
            origin: Vec3::new(-32.0, 0.0, -32.0),
            heights: vec![0.0; 33 * 33],
        };
        let geometry = build::LevelGeometry {
            terrain: Some(Arc::new(terrain)),
            ..default()
        };
        let grid = build::build(&geometry, NavParams::from_tuning(&SoldierTuning::default()));
        // A 3 m wide, 10 m long hull standing on the ground across the way, turned a little.
        let parked = Parked {
            collider: Collider::cuboid(10.0, 2.0, 3.0),
            position: Vec3::new(0.0, 1.2, 0.0),
            rotation: Quat::from_rotation_y(0.2),
        };
        let blocked = blocked_cells(&grid, &[parked]);
        assert!(blocked.len() > 50, "{}", blocked.len());
        let (from, to) = (Vec3::new(0.0, 0.0, -10.0), Vec3::new(0.0, 0.0, 10.0));
        let straight = grid.find_path(from, to).unwrap();
        assert_eq!(straight.waypoints.len(), 2, "{straight:?}");
        let around = grid.find_path_avoiding(from, to, Some(&blocked)).unwrap();
        assert!(around.complete, "{around:?}");
        // Every leg keeps off the hull.
        for leg in around.waypoints.windows(2) {
            for i in 0..=20 {
                let p = leg[0].position.lerp(leg[1].position, i as f32 / 20.0);
                let local = Quat::from_rotation_y(-0.2) * p;
                assert!(local.x.abs() > 5.0 || local.z.abs() > 1.5, "through the vehicle at {p}: {around:?}");
            }
        }
        assert!(!grid.walkable_line_avoiding(from, to, Some(&blocked)));
        // Its door, beside the hull: the path gets there without walking through it.
        let door = grid.find_path_avoiding(from, Vec3::new(0.0, 0.0, 0.0), Some(&blocked)).unwrap();
        let end = door.waypoints.last().unwrap().position;
        assert!(door.complete && end.distance(Vec3::ZERO) < 3.5, "{door:?}");
    }

    #[test]
    fn bit_set() {
        let mut b = NavBlocked::default();
        assert!(!b.contains(5) && !b.contains(1000));
        b.insert(5);
        b.insert(1000);
        b.insert(1000);
        assert!(b.contains(5) && b.contains(1000) && !b.contains(6));
        assert_eq!(b.len(), 2);
    }
}
