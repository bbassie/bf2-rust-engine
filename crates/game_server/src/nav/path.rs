//! Path finding on a [`NavGrid`]: A* over the cells (of the level grid and its detail
//! patches, through the portals between them), then string pulling.

use std::{cmp::Ordering, collections::BinaryHeap, f32::consts::SQRT_2};

use bevy::{platform::collections::HashMap, prelude::*};

use super::{CellRef, NavBlocked, NavCell, NavGrid};

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
/// Cost of getting on and off a ladder (and waiting for whoever is on it), in meters of
/// walking; climbing a meter costs [`LADDER_UP_COST`] going up and [`LADDER_DOWN_COST`]
/// going down. Ladders only take one soldier at a time: stairs are better when close.
const LADDER_COST: f32 = 8.0;
const LADDER_UP_COST: f32 = 2.0;
const LADDER_DOWN_COST: f32 = 1.5;
/// Extra cost of stepping onto a cell a parked vehicle stands on, in meters of walking:
/// paths go around unless there's no other way.
const BLOCKED_COST: f32 = 20.0;
/// Water deep enough over a cell that crossing it means swimming, not wading (m). Matches
/// BF2's own soldier depth roughly (see `game_shared::soldier::SoldierTuning::swim_depth`);
/// kept as a separate constant since the nav grid doesn't depend on `game_shared::soldier`.
const SWIM_NAV_DEPTH: f32 = 0.5;
/// Cost multiplier of a cell deep enough to swim across (BF2's own
/// `setVehicleMaterialCost Infantry DeepWater 6`): paths prefer land and boats but can still
/// swim when that's the only or the much shorter way.
const SWIM_COST: f32 = 6.0;
/// (vehicles) Swimming within this of a wall or ledge (m) costs [`SWIM_WALL_COST`] times the
/// distance instead.
const SWIM_WALL_CLEARANCE: f32 = 1.0;
const SWIM_WALL_COST: f32 = 40.0;
/// (vehicles) Extra cost of dropping into water too deep to stand in, in meters of walking.
const SWIM_DROP_COST: f32 = 80.0;

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
    /// Reaching this waypoint takes a ladder, from the previous one: that one is where to
    /// get on (in front of its foot going up, behind its top going down).
    pub ladder: Option<LadderStep>,
}

/// Climbing a ladder on a path.
#[derive(Clone, Copy, Debug)]
pub struct LadderStep {
    /// Horizontal, out of the wall: the side the ladder is climbed from.
    pub front: Vec3,
    pub up: bool,
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
        self.find_path_avoiding(from, to, None)
    }

    /// [`Self::find_path`] around the cells parked vehicles stand on. A goal on such a cell
    /// (a vehicle's door) snaps to the nearest free one.
    pub fn find_path_avoiding(&self, from: Vec3, to: Vec3, blocked: Option<&NavBlocked>) -> Option<NavPath> {
        self.find_path_from(from, None, to, blocked)
    }

    /// [`Self::find_path_avoiding`] starting in walkable region `region` if a cell of it is
    /// near `from` (a soldier beside a thin wall is nearer to cells on its other side, a
    /// closed room, than to those it stands on; see `bots::walk_region`).
    pub fn find_path_from(&self, from: Vec3, region: Option<u16>, to: Vec3, blocked: Option<&NavBlocked>) -> Option<NavPath> {
        let start = region
            .and_then(|r| self.locate(from, 2.5, Some(r)))
            .or_else(|| self.locate(from, 2.5, None))
            .or_else(|| self.locate(from, 6.0, None))?;
        let region = self.cell(start).region;
        let free = |index: u32| blocked.is_none_or(|b| !b.contains(index));
        let goal = self.locate_where(to, GOAL_SNAP, Some(region), free);
        let target = goal.map_or(to, |g| self.position(g));
        let (cells, complete) = self.search(start, goal, target, blocked);
        Some(NavPath {
            waypoints: self.smooth(&cells, blocked),
            complete,
        })
    }

    /// Whether a soldier at `from` (feet) can walk straight to `to` without a jump, a drop
    /// or brushing a wall: for short moves that need no path (strafing, closing in).
    pub fn walkable_line(&self, from: Vec3, to: Vec3) -> bool {
        self.walkable_line_avoiding(from, to, None)
    }

    /// [`Self::walkable_line`], not through parked vehicles either.
    pub fn walkable_line_avoiding(&self, from: Vec3, to: Vec3, blocked: Option<&NavBlocked>) -> bool {
        let Some(start) = self.locate(from, 1.0, None) else {
            return false;
        };
        let region = self.cell(start).region;
        self.locate(to, 0.5, Some(region))
            .is_some_and(|end| (self.cell(end).y - to.y).abs() < 1.0 && self.straight_walk(start, end, blocked))
    }

    /// A* from `start` to `goal`, or towards `target` if there is no goal cell. Returns the
    /// cells of the path and whether it reached the goal.
    fn search(&self, start: CellRef, goal: Option<CellRef>, target: Vec3, blocked: Option<&NavBlocked>) -> (Vec<CellRef>, bool) {
        // Octile distance in world XZ (patches are turned, but only slightly overestimated,
        // and the search is weighted anyway).
        let heuristic = |c: CellRef| {
            let p = self.position(c);
            let (dx, dz) = ((p.x - target.x).abs(), (p.z - target.z).abs());
            (dx.max(dz) + (SQRT_2 - 1.0) * dx.min(dz)) * HEURISTIC_WEIGHT
        };
        let blocked_at = |index: u32| blocked.is_some_and(|b| b.contains(index));
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
            f: heuristic(start),
            index: start.index,
        });
        let mut closest = (heuristic(start), start.index);
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
            let here = CellRef {
                x: node.x,
                z: node.z,
                index,
            };
            let h = heuristic(here);
            if h < closest.0 {
                closest = (h, index);
            }
            expanded += 1;
            if expanded > budget {
                break;
            }
            let cell = self.cell_size(here);
            // Distances to walls in the level grid's half cells (a patch's are finer).
            let dist_scale = cell / self.params.cell;
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
            let ladders = self.ladders_at(index).filter_map(|ladder| {
                let height = self.cell(ladder.top).y - self.cell(ladder.bottom).y;
                match ladder.bottom.index == index {
                    true => Some((ladder.top, LADDER_COST + height * LADDER_UP_COST)),
                    false => ladder.down.then_some((ladder.bottom, LADDER_COST + height * LADDER_DOWN_COST)),
                }
            });
            let from = self.position(here);
            // Cells deep enough underwater cost much more to cross: paths prefer dry land
            // and boats but can still swim across when nothing else gets there.
            // (vehicles) ... and much more along walls: a swimmer floats at the surface, where a
            // hull or a quay above a ledge under the water keeps him from the cells beside it (bots
            // swam out of a carrier's well deck along such a ledge and stayed against the hull).
            // Not at the foot of a ladder, the way out.
            let water_cost = |to: CellRef| {
                let b = self.cell(to);
                match self.params.water_height {
                    Some(w) if w - b.y > SWIM_NAV_DEPTH => {
                        let near_wall = b.dist as f32 * self.cell_size(to) * 0.5 < SWIM_WALL_CLEARANCE;
                        if near_wall && self.ladders_at(to.index).next().is_none() { SWIM_WALL_COST } else { SWIM_COST }
                    }
                    _ => 1.0,
                }
            };
            let portals = self.portals_at(index).iter().map(|&to| {
                let b = self.cell(to);
                let mut cost = self.position(to).xz().distance(from.xz()).max(0.1) * water_cost(to);
                let dy = b.y - a.y;
                if dy.abs() > self.walk_climb(&a, b) {
                    cost += if dy > 0.0 { JUMP_COST } else { DROP_COST };
                }
                (to, cost)
            });
            let walks = moves.into_iter().filter_map(|(to, length)| {
                let to = to?;
                let b = self.cell(to);
                let mut cost = length * cell * wall_penalty(b, dist_scale) * water_cost(to);
                let dy = b.y - a.y;
                if dy.abs() > self.walk_climb(&a, b) {
                    cost += if dy > 0.0 { JUMP_COST } else { DROP_COST };
                    // (vehicles) Down into water too deep to stand in: often no way back out
                    // (off a carrier's walkway into the water behind its stern).
                    if dy < 0.0 && self.params.water_height.is_some_and(|w| w - b.y > SWIM_NAV_DEPTH) {
                        cost += SWIM_DROP_COST;
                    }
                }
                Some((to, cost))
            });
            for (to, mut cost) in walks.chain(portals).chain(ladders) {
                if blocked_at(to.index) && goal.is_none_or(|g| g.index != to.index) {
                    cost += BLOCKED_COST * cell.max(0.5);
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
                        f: g + heuristic(to),
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
    fn smooth(&self, cells: &[CellRef], blocked: Option<&NavBlocked>) -> Vec<Waypoint> {
        let mut waypoints = vec![Waypoint {
            position: self.position(cells[0]),
            jump: false,
            ladder: None,
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
                    if !walkable(cells[k - 1], cells[k]) || !self.straight_walk(cells[i], cells[k], blocked)
                    {
                        break;
                    }
                    best = k;
                }
            }
            let (from, to) = (self.cell(cells[best - 1]), self.cell(cells[best]));
            let ladder = (best == i + 1)
                .then(|| self.ladder_between(cells[i], cells[i + 1]))
                .flatten();
            match ladder {
                Some(ladder) => {
                    // Get on where movement mounts it; getting off lands at the other end.
                    let up = ladder.bottom == cells[i];
                    let (on, off) = if up { (ladder.foot, ladder.head) } else { (ladder.head, ladder.foot) };
                    waypoints.last_mut().unwrap().position = on;
                    waypoints.push(Waypoint {
                        position: off,
                        jump: false,
                        ladder: Some(LadderStep { front: ladder.front, up }),
                    });
                }
                None => waypoints.push(Waypoint {
                    position: self.position(cells[best]),
                    jump: self.needs_jump(from, to),
                    ladder: None,
                }),
            }
            i = best;
        }
        waypoints
    }

    /// The ladder from `a` to `b`, if they are its two ends.
    fn ladder_between(&self, a: CellRef, b: CellRef) -> Option<&super::NavLadder> {
        self.ladders_at(a.index)
            .find(|l| (l.bottom == a && l.top == b) || (l.top == a && l.bottom == b))
    }

    /// Whether a soldier walking the straight line between the centers of `a` and `b`
    /// stays on linked cells, needs no jump or drop, and keeps clear of walls: cells at the
    /// edge of the walkable area are only crossed near their middle. Both must be in the same
    /// grid (the level's or one patch), and the line doesn't step onto parked vehicles.
    fn straight_walk(&self, a: CellRef, b: CellRef, blocked: Option<&NavBlocked>) -> bool {
        if self.space_id(a.index) != self.space_id(b.index) {
            return false;
        }
        let start_blocked = blocked.is_some_and(|bl| bl.contains(a.index));
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
            let free = start_blocked || blocked.is_none_or(|bl| !bl.contains(n.index));
            ((to.y - from.y).abs() <= self.walk_climb(from, to) && clear && free).then_some(n)
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

/// Paths prefer to keep a cell or two (of the level grid) away from walls and ledges.
/// `scale` turns the cell's distance into the level grid's half cells.
fn wall_penalty(cell: &NavCell, scale: f32) -> f32 {
    let dist = cell.dist as f32 * scale;
    if dist < 0.5 {
        3.0
    } else if dist < 2.5 {
        1.5
    } else {
        1.0
    }
}
