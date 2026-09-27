//! Path finding on a [`NavGrid`]: A* over the cells, then string pulling.

use std::{cmp::Ordering, collections::BinaryHeap, f32::consts::SQRT_2};

use bevy::{platform::collections::HashMap, prelude::*};

use super::{CellRef, NavCell, NavGrid};

/// Nodes A* may expand before giving up with the best partial path.
const MAX_EXPANDED: usize = 120_000;
/// The same when the goal isn't reachable and the search only gets as close as it can.
const MAX_EXPANDED_UNREACHABLE: usize = 30_000;
/// How far a goal snaps to a reachable cell, meters.
const GOAL_SNAP: f32 = 8.0;
/// Heuristic weight: slightly greedy search expands far fewer nodes; string pulling
/// straightens out the result anyway.
const HEURISTIC_WEIGHT: f32 = 1.5;
/// Extra cost of a jump, in meters of walking.
const JUMP_COST: f32 = 3.0;
/// Extra cost of dropping down a ledge, in meters of walking.
const DROP_COST: f32 = 1.0;
/// How far string pulling looks ahead, in path cells.
const MAX_LOOKAHEAD: usize = 120;
/// Straight lines cross cells this far from the edge of the walkable area (half cells)
/// anywhere; cells closer to walls and ledges only near their middle, where the soldier's
/// capsule still fits (see [`NavGrid::straight_walk`]).
const CLEARANCE: u8 = 2;
/// How close to a cell's middle a straight line must pass to cross an edge cell, in cells.
const CENTER_TOLERANCE: f32 = 0.2;

/// A path for a soldier to walk.
#[derive(Clone, Debug)]
pub struct NavPath {
    /// Corners to walk through, starting at the start cell.
    pub waypoints: Vec<Waypoint>,
    /// Whether the path reaches the goal; otherwise it gets as close as it could.
    pub complete: bool,
}

#[derive(Clone, Copy, Debug)]
pub struct Waypoint {
    /// On the walkable surface.
    pub position: Vec3,
    /// Reaching this waypoint needs a jump up a ledge.
    pub jump: bool,
}

#[derive(Clone, Copy)]
struct Node {
    g: f32,
    parent: u32,
    x: u32,
    z: u32,
    closed: bool,
}

#[derive(PartialEq)]
struct Open {
    f: f32,
    index: u32,
}

impl Eq for Open {}

impl Ord for Open {
    fn cmp(&self, other: &Self) -> Ordering {
        // Smallest f first.
        other.f.total_cmp(&self.f)
    }
}

impl PartialOrd for Open {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

impl NavGrid {
    /// A path from a soldier's feet at `from` towards `to`. The goal snaps to the nearest
    /// reachable cell within a few meters; if there is none, the path gets as close as it
    /// can. `None` if `from` isn't near any walkable cell.
    pub fn find_path(&self, from: Vec3, to: Vec3) -> Option<NavPath> {
        let start = self
            .locate(from, 2.5, None)
            .or_else(|| self.locate(from, 6.0, None))?;
        let region = self.cell(start).region;
        let goal = self.locate(to, GOAL_SNAP, Some(region));
        let target = goal.map_or(to, |g| self.position(g));
        let (cells, complete) = self.search(start, goal, target);
        Some(NavPath {
            waypoints: self.smooth(&cells),
            complete,
        })
    }

    /// A* from `start` to `goal`, or towards `target` if there is no goal cell. Returns the
    /// cells of the path and whether it reached the goal.
    fn search(&self, start: CellRef, goal: Option<CellRef>, target: Vec3) -> (Vec<CellRef>, bool) {
        let cell = self.params.cell;
        let (tx, tz) = self
            .column_at(target.x, target.z)
            .map_or((-1.0, -1.0), |(x, z)| (x as f32, z as f32));
        let heuristic = |x: u32, z: u32| {
            let (dx, dz) = ((x as f32 - tx).abs(), (z as f32 - tz).abs());
            (dx.max(dz) + (SQRT_2 - 1.0) * dx.min(dz)) * cell * HEURISTIC_WEIGHT
        };
        let mut nodes: HashMap<u32, Node> = HashMap::with_capacity(4096);
        let mut open = BinaryHeap::new();
        nodes.insert(
            start.index,
            Node {
                g: 0.0,
                parent: u32::MAX,
                x: start.x,
                z: start.z,
                closed: false,
            },
        );
        open.push(Open {
            f: heuristic(start.x, start.z),
            index: start.index,
        });
        let mut closest = (heuristic(start.x, start.z), start.index);
        let mut end = None;
        let mut expanded = 0;
        let budget = if goal.is_some() {
            MAX_EXPANDED
        } else {
            MAX_EXPANDED_UNREACHABLE
        };
        while let Some(Open { index, .. }) = open.pop() {
            let node = nodes[&index];
            if node.closed {
                continue;
            }
            nodes.get_mut(&index).unwrap().closed = true;
            if goal.is_some_and(|g| g.index == index) {
                end = Some(index);
                break;
            }
            let h = heuristic(node.x, node.z);
            if h < closest.0 {
                closest = (h, index);
            }
            expanded += 1;
            if expanded > budget {
                break;
            }
            let here = CellRef {
                x: node.x,
                z: node.z,
                index,
            };
            let a = *self.cell(here);
            let straight: [Option<CellRef>; 4] =
                std::array::from_fn(|dir| self.neighbour(here, dir));
            let mut moves: [(Option<CellRef>, f32); 8] = [(None, 0.0); 8];
            for dir in 0..4 {
                moves[dir] = (straight[dir], 1.0);
                // Diagonal between this direction and the next, if both ways around lead to
                // the same cell without jumps.
                let next = (dir + 1) % 4;
                if let (Some(p), Some(q)) = (straight[dir], straight[next]) {
                    let via_p = self.neighbour(p, next);
                    let via_q = self.neighbour(q, dir);
                    if let (Some(d1), Some(d2)) = (via_p, via_q)
                        && d1.index == d2.index
                    {
                        let (pc, qc, dc) = (self.cell(p), self.cell(q), self.cell(d1));
                        let smooth = [(&a, pc), (&a, qc), (pc, dc), (qc, dc)]
                            .iter()
                            .all(|(u, v)| (v.y - u.y).abs() <= self.walk_climb(u, v));
                        if smooth {
                            moves[4 + dir] = (Some(d1), SQRT_2);
                        }
                    }
                }
            }
            for (to, length) in moves {
                let Some(to) = to else {
                    continue;
                };
                let b = self.cell(to);
                let mut cost = length * cell * wall_penalty(b);
                let dy = b.y - a.y;
                if dy.abs() > self.walk_climb(&a, b) {
                    cost += if dy > 0.0 { JUMP_COST } else { DROP_COST };
                }
                let g = node.g + cost;
                let better = nodes.get(&to.index).is_none_or(|n| !n.closed && g < n.g);
                if better {
                    nodes.insert(
                        to.index,
                        Node {
                            g,
                            parent: index,
                            x: to.x,
                            z: to.z,
                            closed: false,
                        },
                    );
                    open.push(Open {
                        f: g + heuristic(to.x, to.z),
                        index: to.index,
                    });
                }
            }
        }
        let complete = end.is_some();
        let mut index = end.unwrap_or(closest.1);
        let mut cells = Vec::new();
        while index != u32::MAX {
            let node = nodes[&index];
            cells.push(CellRef {
                x: node.x,
                z: node.z,
                index,
            });
            index = node.parent;
        }
        cells.reverse();
        (cells, complete)
    }

    /// String pulling: keeps only the cells where the path has to turn, i.e. where a
    /// straight walk over the grid to a later cell would leave the walkable area, graze a
    /// wall, or cross a jump or drop.
    fn smooth(&self, cells: &[CellRef]) -> Vec<Waypoint> {
        let mut waypoints = vec![Waypoint {
            position: self.position(cells[0]),
            jump: false,
        }];
        let walkable = |a: CellRef, b: CellRef| {
            let (a, b) = (self.cell(a), self.cell(b));
            (b.y - a.y).abs() <= self.walk_climb(a, b)
        };
        let mut i = 0;
        while i + 1 < cells.len() {
            let mut best = i + 1;
            if walkable(cells[i], cells[i + 1]) {
                for k in i + 2..cells.len().min(i + MAX_LOOKAHEAD) {
                    if !walkable(cells[k - 1], cells[k]) || !self.straight_walk(cells[i], cells[k])
                    {
                        break;
                    }
                    best = k;
                }
            }
            let (from, to) = (self.cell(cells[best - 1]), self.cell(cells[best]));
            waypoints.push(Waypoint {
                position: self.position(cells[best]),
                jump: self.needs_jump(from, to),
            });
            i = best;
        }
        waypoints
    }

    /// Whether a soldier walking the straight line between the centers of `a` and `b`
    /// stays on linked cells, needs no jump or drop, and keeps clear of walls: cells at the
    /// edge of the walkable area are only crossed near their middle.
    fn straight_walk(&self, a: CellRef, b: CellRef) -> bool {
        let (dx, dz) = (b.x as f32 - a.x as f32, b.z as f32 - a.z as f32);
        let length = (dx * dx + dz * dz).sqrt().max(1e-6);
        let off_line = |c: CellRef| {
            ((c.x as f32 - a.x as f32) * dz - (c.z as f32 - a.z as f32) * dx).abs() / length
        };
        let (step_x, step_z) = (if dx > 0.0 { 0 } else { 2 }, if dz > 0.0 { 1 } else { 3 });
        let delta_x = if dx != 0.0 {
            1.0 / dx.abs()
        } else {
            f32::INFINITY
        };
        let delta_z = if dz != 0.0 {
            1.0 / dz.abs()
        } else {
            f32::INFINITY
        };
        // Starting in the middle of a cell, the first boundary is half a cell away.
        let (mut t_x, mut t_z) = (0.5 * delta_x, 0.5 * delta_z);
        let step = |c: CellRef, dir: usize| {
            let n = self.neighbour(c, dir)?;
            let (from, to) = (self.cell(c), self.cell(n));
            let clear = to.dist >= CLEARANCE || off_line(n) < CENTER_TOLERANCE;
            ((to.y - from.y).abs() <= self.walk_climb(from, to) && clear).then_some(n)
        };
        let mut c = a;
        for _ in 0..(dx.abs() + dz.abs()) as usize + 2 {
            if c.x == b.x && c.z == b.z {
                return c.index == b.index;
            }
            if (t_x - t_z).abs() < 1e-5 {
                // Through a corner: both ways around must be clear.
                let via_x = step(c, step_x).and_then(|n| step(n, step_z));
                let via_z = step(c, step_z).and_then(|n| step(n, step_x));
                match (via_x, via_z) {
                    (Some(p), Some(q)) if p.index == q.index => c = p,
                    _ => return false,
                }
                t_x += delta_x;
                t_z += delta_z;
            } else if t_x < t_z {
                let Some(n) = step(c, step_x) else {
                    return false;
                };
                c = n;
                t_x += delta_x;
            } else {
                let Some(n) = step(c, step_z) else {
                    return false;
                };
                c = n;
                t_z += delta_z;
            }
        }
        false
    }
}

/// Paths prefer to keep a cell or two away from walls and ledges.
fn wall_penalty(cell: &NavCell) -> f32 {
    match cell.dist {
        0 => 3.0,
        1..=2 => 1.5,
        _ => 1.0,
    }
}
