//! Where bots go: the level's combat areas (BF2 `CombatArea`, see
//! [`game_data::CombatAreaDesc`]) for each kind of traveller, grown by a margin.
//!
//! The grids leave out everything outside the area that applies to them (the infantry grid
//! the soldiers' area, the land vehicle grid the land vehicles', the water grid the boats'),
//! which also crops them to the area's bounds: less memory, a faster build and smaller
//! region and distance computations, and no cover spot, flank or wander goal out where
//! players would be warned back. Gameplay points outside the area (a control point, a
//! spawn, a vehicle spawner, a strategic area, a ladder just outside) keep a circle around
//! them and a corridor back to it. Air vehicles keep to the air areas (see
//! [`super::vehicle::VehicleNavGrid::air_area`]).

use bevy::prelude::*;
use game_data::{CombatAreaDesc, GameModeDesc};

/// How far outside its combat area a traveller's grid reaches, meters. BF2 gives players
/// a countdown out there, so a strip beyond the line is fair game (a path cutting a corner,
/// cover just outside), but nothing further out is worth going to.
pub const COMBAT_AREA_MARGIN: f32 = 20.0;
/// Radius kept around gameplay points outside the combat area, and the width of the
/// corridor from them back to it, meters.
pub const KEEP_RADIUS: f32 = 25.0;
/// Ladders this far outside the combat area (beyond the margin) are kept, meters.
pub const LADDER_REACH: f32 = 40.0;

/// Combat area kinds (BF2's `CombatArea.vehicles`, inferred; see
/// [`CombatAreaDesc::vehicles`]).
const BOATS: u8 = 1;
const JETS: u8 = 2;
const HELICOPTERS: u8 = 3;
const EVERYTHING: u8 = 5;

/// Who a play area is for.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Traveller {
    Soldier,
    Land,
    Boat,
    Helicopter,
    Jet,
}

impl Traveller {
    /// The combat area kinds that bound it, best first: its own, else the main area (BF2's
    /// soldiers' area also bounds whatever has no area of its own), else AIX's areas for
    /// everything.
    fn kinds(self) -> &'static [u8] {
        match self {
            Self::Soldier => &[CombatAreaDesc::SOLDIERS, CombatAreaDesc::LAND, EVERYTHING],
            Self::Land => &[CombatAreaDesc::LAND, CombatAreaDesc::SOLDIERS, EVERYTHING],
            Self::Boat => &[BOATS, CombatAreaDesc::SOLDIERS, EVERYTHING],
            Self::Helicopter => &[HELICOPTERS, CombatAreaDesc::SOLDIERS, EVERYTHING],
            Self::Jet => &[JETS, CombatAreaDesc::SOLDIERS, EVERYTHING],
        }
    }
}

/// A circle swept along a segment: kept walkable around a gameplay point outside the area.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Capsule {
    pub a: Vec2,
    pub b: Vec2,
    pub radius: f32,
}

impl Capsule {
    fn contains(&self, p: Vec2) -> bool {
        segment_distance(p, self.a, self.b) <= self.radius
    }
}

/// Where one kind of traveller may go: inside any of `polygons` or within `margin` of one,
/// or inside one of the `keep` capsules. World XZ.
#[derive(Clone, Debug, PartialEq, Default)]
pub struct PlayArea {
    pub polygons: Vec<Vec<Vec2>>,
    pub margin: f32,
    pub keep: Vec<Capsule>,
}

impl PlayArea {
    /// The area of a layout for `traveller` (for both teams: the union of what bounds each),
    /// `None` without combat areas. For soldiers, the areas BF2 marks for its bots'
    /// pathfinding (`usedByPathfinding`, the co-op layouts' main area) come first.
    pub fn for_layout(layout: &GameModeDesc, traveller: Traveller, margin: f32) -> Option<Self> {
        let mut polygons: Vec<Vec<Vec2>> = Vec::new();
        for team in [1u8, 2] {
            let candidates: Vec<&CombatAreaDesc> = layout
                .combat_areas
                .iter()
                .filter(|a| a.points.len() >= 3 && a.area() > 1.0 && a.team.is_none_or(|t| t == team))
                .collect();
            let pathfinding: Vec<&CombatAreaDesc> = candidates
                .iter()
                .copied()
                .filter(|a| traveller == Traveller::Soldier && a.used_by_pathfinding && a.vehicles != BOATS)
                .collect();
            let chosen = if pathfinding.is_empty() {
                traveller
                    .kinds()
                    .iter()
                    .map(|&kind| candidates.iter().copied().filter(|a| a.vehicles == kind).collect::<Vec<_>>())
                    .find(|areas| !areas.is_empty())
                    .unwrap_or_default()
            } else {
                pathfinding
            };
            for area in chosen {
                let polygon: Vec<Vec2> = area.points.iter().map(|&[x, z]| Vec2::new(x, z)).collect();
                if !polygons.contains(&polygon) {
                    polygons.push(polygon);
                }
            }
        }
        (!polygons.is_empty()).then_some(Self {
            polygons,
            margin,
            keep: Vec::new(),
        })
    }

    /// Whether a point is inside one of the polygons (even-odd rule, like
    /// [`CombatAreaDesc::contains`]).
    pub fn inside_polygons(&self, p: Vec2) -> bool {
        self.polygons.iter().any(|poly| {
            let mut inside = false;
            let mut j = poly.len() - 1;
            for i in 0..poly.len() {
                let (a, b) = (poly[i], poly[j]);
                if (a.y > p.y) != (b.y > p.y) && p.x < (b.x - a.x) * (p.y - a.y) / (b.y - a.y) + a.x {
                    inside = !inside;
                }
                j = i;
            }
            inside
        })
    }

    fn edges(&self) -> impl Iterator<Item = (Vec2, Vec2)> + '_ {
        self.polygons
            .iter()
            .flat_map(|poly| (0..poly.len()).map(move |i| (poly[i], poly[(i + 1) % poly.len()])))
    }

    /// The nearest point on the polygons' outlines, and how far it is.
    pub fn nearest_edge_point(&self, p: Vec2) -> (Vec2, f32) {
        self.edges()
            .map(|(a, b)| {
                let q = closest_on_segment(p, a, b);
                (q, q.distance(p))
            })
            .min_by(|x, y| x.1.total_cmp(&y.1))
            .unwrap_or((p, 0.0))
    }

    /// Whether a traveller may go to a point: inside, within the margin or kept.
    pub fn contains(&self, p: Vec2) -> bool {
        self.inside_polygons(p) || self.nearest_edge_point(p).1 <= self.margin || self.keep.iter().any(|k| k.contains(p))
    }

    /// Keeps a point outside the polygons: a circle of `radius` around it and a corridor
    /// as wide to the nearest point of the area. Returns whether it was outside.
    pub fn keep_point(&mut self, p: Vec2, radius: f32) -> bool {
        if self.inside_polygons(p) {
            return false;
        }
        let (edge, _) = self.nearest_edge_point(p);
        // A little way in, so the corridor surely joins the inside.
        let inward = (edge - p).normalize_or_zero() * radius.min(5.0);
        self.keep.push(Capsule {
            a: p,
            b: edge + inward,
            radius,
        });
        true
    }

    /// The XZ bounds of everything it contains.
    pub fn aabb(&self) -> (Vec2, Vec2) {
        let (mut lo, mut hi) = self
            .polygons
            .iter()
            .flatten()
            .fold((Vec2::MAX, Vec2::MIN), |(lo, hi), &p| (lo.min(p), hi.max(p)));
        lo -= self.margin;
        hi += self.margin;
        for k in &self.keep {
            lo = lo.min(k.a.min(k.b) - k.radius);
            hi = hi.max(k.a.max(k.b) + k.radius);
        }
        (lo, hi)
    }

    /// `bounds` cropped to [`Self::aabb`] (the area's bounds if there are none). Unchanged
    /// if they don't overlap.
    pub fn crop(&self, bounds: Option<(Vec2, Vec2)>) -> (Vec2, Vec2) {
        let (a_lo, a_hi) = self.aabb();
        match bounds {
            Some((lo, hi)) => {
                let (c_lo, c_hi) = (lo.max(a_lo), hi.min(a_hi));
                if c_lo.x < c_hi.x && c_lo.y < c_hi.y { (c_lo, c_hi) } else { (lo, hi) }
            }
            None => (a_lo, a_hi),
        }
    }

    /// The point itself if the area contains it, else the nearest point on its outline moved
    /// `inset` meters further in.
    pub fn clamp(&self, p: Vec2, inset: f32) -> Vec2 {
        if self.contains(p) {
            return p;
        }
        let (edge, _) = self.nearest_edge_point(p);
        edge + (edge - p).normalize_or_zero() * inset
    }

    /// Square meters inside the polygons (overlaps counted twice).
    pub fn polygon_area(&self) -> f32 {
        self.polygons
            .iter()
            .map(|poly| {
                let twice: f32 = (0..poly.len()).map(|i| poly[i].perp_dot(poly[(i + 1) % poly.len()])).sum();
                twice.abs() * 0.5
            })
            .sum()
    }

    /// Words identifying it, for the grid cache's key.
    pub fn key_words(&self) -> Vec<f32> {
        let mut words = vec![self.margin, self.polygons.len() as f32, self.keep.len() as f32];
        for poly in &self.polygons {
            words.push(poly.len() as f32);
            words.extend(poly.iter().flat_map(|p| [p.x, p.y]));
        }
        for k in &self.keep {
            words.extend([k.a.x, k.a.y, k.b.x, k.b.y, k.radius]);
        }
        words
    }

    /// Per column of a grid (`width` by `depth` columns of `cell` meters from `origin`, row
    /// by row): whether the area contains the column's middle. Scanlines for the insides and
    /// bands along the edges for the margin, so it costs about a pass over the columns.
    pub fn mask(&self, origin: Vec2, cell: f32, width: u32, depth: u32) -> Vec<bool> {
        let mut mask = vec![false; width as usize * depth as usize];
        if width == 0 || depth == 0 {
            return mask;
        }
        // Column index range [first, end) whose middles lie in world [x0, x1].
        let columns = |x0: f32, x1: f32| {
            let first = ((x0 - origin.x) / cell - 0.5).ceil().max(0.0);
            let end = ((x1 - origin.x) / cell - 0.5).floor() + 1.0;
            (first as usize, end.clamp(0.0, width as f32) as usize)
        };
        let mut crossings = Vec::new();
        for z in 0..depth {
            let wz = origin.y + (z as f32 + 0.5) * cell;
            let row = &mut mask[(z * width) as usize..((z + 1) * width) as usize];
            for poly in &self.polygons {
                crossings.clear();
                for i in 0..poly.len() {
                    let (a, b) = (poly[i], poly[(i + 1) % poly.len()]);
                    if (a.y > wz) != (b.y > wz) {
                        crossings.push(a.x + (wz - a.y) * (b.x - a.x) / (b.y - a.y));
                    }
                }
                crossings.sort_by(f32::total_cmp);
                for pair in crossings.chunks_exact(2) {
                    let (first, end) = columns(pair[0], pair[1]);
                    if first < end {
                        row[first..end].fill(true);
                    }
                }
            }
        }
        let mut fill = |a: Vec2, b: Vec2, r: f32| {
            let (lo, hi) = (a.min(b) - r, a.max(b) + r);
            let z0 = ((lo.y - origin.y) / cell - 0.5).ceil().max(0.0) as u32;
            let z1 = (((hi.y - origin.y) / cell - 0.5).floor() + 1.0).clamp(0.0, depth as f32) as u32;
            let d = b - a;
            for z in z0..z1 {
                let wz = origin.y + (z as f32 + 0.5) * cell;
                // The part of the segment within `r` of this row, grown by `r`.
                let (t0, t1) = if d.y.abs() < 1e-6 {
                    if (a.y - wz).abs() > r {
                        continue;
                    }
                    (0.0, 1.0)
                } else {
                    let (u, v) = ((wz - r - a.y) / d.y, (wz + r - a.y) / d.y);
                    (u.min(v).max(0.0), u.max(v).min(1.0))
                };
                if t0 > t1 {
                    continue;
                }
                let (xa, xb) = (a.x + t0 * d.x, a.x + t1 * d.x);
                let (first, end) = columns(xa.min(xb) - r, xa.max(xb) + r);
                for x in first..end {
                    let i = (z * width) as usize + x;
                    if !mask[i] {
                        let p = Vec2::new(origin.x + (x as f32 + 0.5) * cell, wz);
                        mask[i] = segment_distance(p, a, b) <= r;
                    }
                }
            }
        };
        for (a, b) in self.edges() {
            fill(a, b, self.margin);
        }
        for k in &self.keep {
            fill(k.a, k.b, k.radius);
        }
        mask
    }
}

fn closest_on_segment(p: Vec2, a: Vec2, b: Vec2) -> Vec2 {
    let d = b - a;
    let len2 = d.length_squared();
    let t = if len2 > 0.0 { ((p - a).dot(d) / len2).clamp(0.0, 1.0) } else { 0.0 };
    a + d * t
}

fn segment_distance(p: Vec2, a: Vec2, b: Vec2) -> f32 {
    closest_on_segment(p, a, b).distance(p)
}

#[cfg(test)]
mod tests {
    use game_data::{CombatAreaDesc, GameModeDesc};

    use super::*;

    fn square(vehicles: u8, half: f32, pathfinding: bool) -> CombatAreaDesc {
        CombatAreaDesc {
            team: None,
            vehicles,
            used_by_pathfinding: pathfinding,
            points: vec![[-half, -half], [half, -half], [half, half], [-half, half]],
        }
    }

    fn layout(areas: Vec<CombatAreaDesc>) -> GameModeDesc {
        GameModeDesc {
            combat_areas: areas,
            ..Default::default()
        }
    }

    #[test]
    fn picks_the_area_for_each_traveller() {
        let l = layout(vec![square(CombatAreaDesc::SOLDIERS, 100.0, false), square(JETS, 500.0, false), square(CombatAreaDesc::LAND, 200.0, false)]);
        let half = |t| PlayArea::for_layout(&l, t, 0.0).unwrap().aabb().1.x;
        assert_eq!(half(Traveller::Soldier), 100.0);
        assert_eq!(half(Traveller::Land), 200.0);
        assert_eq!(half(Traveller::Jet), 500.0);
        // No boat or helicopter area: the main one.
        assert_eq!(half(Traveller::Boat), 100.0);
        assert_eq!(half(Traveller::Helicopter), 100.0);
        // No soldiers' area: the land vehicles'.
        let l = layout(vec![square(CombatAreaDesc::LAND, 200.0, false)]);
        assert_eq!(PlayArea::for_layout(&l, Traveller::Soldier, 0.0).unwrap().aabb().1.x, 200.0);
        // BF2's pathfinding area comes first for soldiers.
        let l = layout(vec![square(CombatAreaDesc::SOLDIERS, 100.0, false), square(HELICOPTERS, 300.0, true)]);
        assert_eq!(PlayArea::for_layout(&l, Traveller::Soldier, 0.0).unwrap().aabb().1.x, 300.0);
        // Without combat areas, nothing.
        assert!(PlayArea::for_layout(&layout(Vec::new()), Traveller::Soldier, 20.0).is_none());
        // Degenerate areas (BF2's empty ones) don't count.
        let mut empty = square(CombatAreaDesc::SOLDIERS, 0.0, false);
        empty.points.truncate(3);
        assert!(PlayArea::for_layout(&layout(vec![empty]), Traveller::Soldier, 20.0).is_none());
    }

    #[test]
    fn team_areas_make_a_union() {
        let mut a = square(CombatAreaDesc::SOLDIERS, 50.0, false);
        a.team = Some(1);
        a.points.iter_mut().for_each(|p| p[0] -= 100.0);
        let mut b = square(CombatAreaDesc::SOLDIERS, 50.0, false);
        b.team = Some(2);
        b.points.iter_mut().for_each(|p| p[0] += 100.0);
        let area = PlayArea::for_layout(&layout(vec![a, b]), Traveller::Soldier, 0.0).unwrap();
        assert_eq!(area.polygons.len(), 2);
        assert!(area.contains(Vec2::new(-100.0, 0.0)) && area.contains(Vec2::new(100.0, 0.0)));
        assert!(!area.contains(Vec2::ZERO));
    }

    #[test]
    fn margin_and_kept_points() {
        let mut area = PlayArea::for_layout(&layout(vec![square(CombatAreaDesc::SOLDIERS, 100.0, false)]), Traveller::Soldier, 20.0).unwrap();
        assert!(area.contains(Vec2::new(0.0, 0.0)));
        assert!(area.contains(Vec2::new(119.0, 0.0)));
        assert!(!area.contains(Vec2::new(121.0, 0.0)));
        assert!(!area.contains(Vec2::new(115.0, 115.0)), "the margin rounds the corners");
        // A flag 200 m out: a circle around it and a corridor back.
        assert!(!area.keep_point(Vec2::new(50.0, 50.0), KEEP_RADIUS));
        assert!(area.keep_point(Vec2::new(300.0, 0.0), KEEP_RADIUS));
        assert!(area.contains(Vec2::new(320.0, 0.0)));
        assert!(area.contains(Vec2::new(200.0, 20.0)));
        assert!(!area.contains(Vec2::new(200.0, 40.0)));
        assert_eq!(area.aabb(), (Vec2::splat(-120.0), Vec2::new(325.0, 120.0)));
        // Cropping bounds.
        let bounds = Some((Vec2::splat(-500.0), Vec2::new(250.0, 50.0)));
        assert_eq!(area.crop(bounds), (Vec2::splat(-120.0), Vec2::new(250.0, 50.0)));
        assert_eq!(area.crop(Some((Vec2::splat(1000.0), Vec2::splat(1100.0)))), (Vec2::splat(1000.0), Vec2::splat(1100.0)));
        // Clamping a point back in.
        let p = area.clamp(Vec2::new(0.0, 400.0), 10.0);
        assert!((p - Vec2::new(0.0, 90.0)).length() < 1e-3, "{p}");
        assert_eq!(area.clamp(Vec2::new(0.0, 110.0), 10.0), Vec2::new(0.0, 110.0));
    }

    #[test]
    fn mask_matches_contains() {
        // A concave polygon (an L) and a kept point, on a grid not aligned with it.
        let l = layout(vec![CombatAreaDesc {
            team: None,
            vehicles: CombatAreaDesc::SOLDIERS,
            used_by_pathfinding: false,
            points: vec![[0.0, 0.0], [60.0, 0.0], [60.0, 20.0], [25.0, 25.0], [20.0, 70.0], [0.0, 70.0]],
        }]);
        let mut area = PlayArea::for_layout(&l, Traveller::Soldier, 7.0).unwrap();
        area.keep_point(Vec2::new(90.0, 60.0), 6.0);
        let (origin, cell, width, depth) = (Vec2::new(-23.3, -17.1), 0.7, 200, 160);
        let mask = area.mask(origin, cell, width, depth);
        let mut inside = 0;
        for z in 0..depth {
            for x in 0..width {
                let p = origin + (Vec2::new(x as f32, z as f32) + 0.5) * cell;
                let expected = area.contains(p);
                let distance = area.nearest_edge_point(p).1;
                let borderline = (distance - area.margin).abs() < 1e-3 || area.keep.iter().any(|k| (segment_distance(p, k.a, k.b) - k.radius).abs() < 1e-3);
                if !borderline {
                    assert_eq!(mask[(z * width + x) as usize], expected, "column {x}, {z} at {p}");
                }
                inside += expected as usize;
            }
        }
        assert!(inside > 1000 && inside < (width * depth) as usize / 2, "{inside}");
    }
}
