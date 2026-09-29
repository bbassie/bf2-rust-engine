//! Cover for firefights: spots next to something that hides a soldier from a threat, found on
//! the navigation grid and checked with rays, and where to shoot from there (standing up
//! over low cover, or stepping out beside a corner).
//!
//! The grid does the cheap part: candidate cells are those near an edge of the walkable area
//! (a wall, a crate, a ledge) with no walkable ground right beside them towards the threat,
//! scored by distance, by whether they lead on towards where the bot is going and by
//! teammates already there. Only the best few are checked with rays (a crouching and a
//! standing soldier's eye against the threat's), so a search casts a bounded number of rays
//! and the callers ration searches per tick.

use avian3d::prelude::*;
use bevy::prelude::*;

use super::tactics::line_of_sight;
use crate::nav::NavGrid;

/// Eye heights above the feet of a crouching and a standing soldier (a little under the
/// real ones: the head shows a bit above what hides the eyes).
const CROUCHED_EYE: f32 = 1.1;
const STANDING_EYE: f32 = 1.6;
/// Candidates checked with rays per search, at most.
const CHECKED: usize = 6;
/// Candidates checked on the grid per search, at most.
const MAX_PROBES: usize = 120;

/// A place to fight from.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct CoverSpot {
    /// Where to stand hidden (feet).
    pub spot: Vec3,
    /// Where to shoot from: the spot itself over low cover (standing up), or a step beside
    /// it past a corner. `None`: cover to hide in only.
    pub peek: Option<Vec3>,
    /// Hidden crouching but not standing: peek by standing up.
    pub low: bool,
    /// The threat (an eye position) it hides from.
    pub threat: Vec3,
}

impl CoverSpot {
    /// Whether this cover still hides from a threat at `threat` (roughly the same direction
    /// and distance as the one it was found for).
    pub fn still_good(&self, threat: Vec3) -> bool {
        let was = (self.threat - self.spot).with_y(0.0);
        let now = (threat - self.spot).with_y(0.0);
        was.length() > 1.0 && now.length() > 1.0 && was.normalize().dot(now.normalize()) > 0.85
    }
}

/// What to look for.
pub struct CoverQuery<'a> {
    /// The bot's feet.
    pub from: Vec3,
    /// The threat's eye.
    pub threat: Vec3,
    /// How far from `from` to look, meters.
    pub radius: f32,
    /// Where the bot is heading: cover on the way there is worth more (moving cover to
    /// cover). `None`: the closer the better, and not closer to the threat.
    pub toward: Option<Vec3>,
    /// Cover teammates hold or are going to: kept a couple of meters from.
    pub taken: &'a [Vec3],
    /// Wants somewhere to shoot from (not just to hide).
    pub fire: bool,
}

/// Finds cover (see the module docs). `rays` is decremented per ray cast; the search stops
/// when it runs out.
pub fn find(nav: &NavGrid, spatial: &SpatialQuery, q: &CoverQuery, rays: &mut u32) -> Option<CoverSpot> {
    let start = nav.locate(q.from, 2.0, None)?;
    let region = nav.cell(start).region;
    let from_threat = q.from.xz().distance(q.threat.xz());
    let toward = q.toward.map(|t| (t - q.from).with_y(0.0).normalize_or_zero());
    let mut candidates: Vec<(f32, Vec3, Vec2)> = Vec::new();
    for cell in nav.cells_near(q.from.xz(), q.radius) {
        let c = nav.cell(cell);
        // Near an edge but not right on it (a soldier can't stand on a cell touching a wall).
        if c.region != region || !(1..=3).contains(&c.dist) {
            continue;
        }
        let spot = nav.position(cell);
        // On the same level: not up on a planter or down a step.
        if (spot.y - q.from.y).abs() > 0.7 {
            continue;
        }
        let offset = (spot - q.from).with_y(0.0);
        let distance = offset.length();
        if distance > q.radius {
            continue;
        }
        let to_threat = (q.threat - spot).with_y(0.0);
        let threat_distance = to_threat.length();
        if threat_distance < 6.0 {
            continue;
        }
        let dir = to_threat / threat_distance;
        let mut score = distance;
        match toward {
            // On the way: progress counts, going back costs; not right in the enemy's face.
            Some(toward) => {
                score -= 0.8 * offset.dot(toward);
                if threat_distance < 12.0 {
                    score += 8.0;
                }
            }
            None => {
                if threat_distance < from_threat - 2.0 {
                    score += 2.0 * (from_threat - threat_distance);
                }
            }
        }
        if q.taken.iter().any(|t| t.distance_squared(spot) < 2.5 * 2.5) {
            score += 12.0;
        }
        candidates.push((score, spot, dir.xz()));
    }
    candidates.sort_by(|a, b| a.0.total_cmp(&b.0));
    // Neighbouring cells along the same wall are nearly the same spot: check spots apart.
    let mut checked: Vec<Vec3> = Vec::new();
    let mut hide_only: Option<CoverSpot> = None;
    let mut probes = 0;
    for (_, spot, dir) in candidates {
        if checked.len() >= CHECKED || *rays < 2 || probes >= MAX_PROBES {
            break;
        }
        if checked.iter().any(|c| c.distance_squared(spot) < 1.5 * 1.5) {
            continue;
        }
        // Something solid (or a drop) right beside it towards the threat: the grid's cheap
        // line of sight.
        probes += 1;
        if !blocked_towards(nav, spot, Vec3::new(dir.x, 0.0, dir.y), region) {
            continue;
        }
        checked.push(spot);
        *rays -= 1;
        if line_of_sight(spatial, q.threat, spot + Vec3::Y * CROUCHED_EYE) {
            continue;
        }
        *rays -= 1;
        let standing_seen = line_of_sight(spatial, q.threat, spot + Vec3::Y * STANDING_EYE);
        if standing_seen {
            // Low cover: crouch to hide, stand up to shoot.
            return Some(CoverSpot {
                spot,
                peek: Some(spot),
                low: true,
                threat: q.threat,
            });
        }
        // Full cover: a step to the side past the corner to shoot from.
        let side = Vec3::new(-dir.y, 0.0, dir.x);
        for offset in [0.9f32, -0.9, 1.5, -1.5] {
            if *rays == 0 {
                break;
            }
            let Some(cell) = nav.locate(spot + side * offset, 0.4, Some(region)) else {
                continue;
            };
            let peek = nav.position(cell);
            if (peek.y - spot.y).abs() > 0.6 || !nav.walkable_line(spot, peek) {
                continue;
            }
            *rays -= 1;
            if line_of_sight(spatial, q.threat, peek + Vec3::Y * STANDING_EYE) {
                return Some(CoverSpot {
                    spot,
                    peek: Some(peek),
                    low: false,
                    threat: q.threat,
                });
            }
        }
        let cover = CoverSpot {
            spot,
            peek: None,
            low: false,
            threat: q.threat,
        };
        if !q.fire {
            return Some(cover);
        }
        hide_only.get_or_insert(cover);
    }
    hide_only
}

/// Whether there is no walkable ground at about the same height half a meter to a meter
/// from `spot` in direction `dir` (XZ): a wall, an obstacle or a drop on that side.
fn blocked_towards(nav: &NavGrid, spot: Vec3, dir: Vec3, region: u16) -> bool {
    [0.6f32, 1.1].iter().any(|step| {
        let probe = spot + dir * *step;
        nav.locate(probe, 0.3, Some(region))
            .is_none_or(|cell| {
                let p = nav.position(cell);
                (p.y - spot.y).abs() > 0.8 || p.xz().distance(probe.xz()) > 0.45
            })
    })
}

/// Whether someone with his eye at `from` could shoot towards `to` without the shot hitting
/// something near himself: the first thing in the way (if any) is closer to `to` than to
/// him. For suppressive fire at where an enemy was.
pub fn fire_lane(spatial: &SpatialQuery, from: Vec3, to: Vec3) -> bool {
    let offset = to - from;
    let Ok(dir) = Dir3::new(offset) else {
        return false;
    };
    let distance = offset.length();
    let filter = SpatialQueryFilter::from_mask(game_shared::physics::GameLayer::World);
    spatial
        .cast_ray(from, dir, distance, true, &filter)
        .is_none_or(|hit| hit.distance > distance * 0.7 || distance - hit.distance < 4.0)
}
