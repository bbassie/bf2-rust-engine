//! Geometry for individual behaviours: finding cover, flanking positions, grenade arcs and
//! safe strafing.

use std::f32::consts::TAU;

use avian3d::prelude::*;
use bevy::prelude::*;
use game_shared::physics::GameLayer;

use crate::nav::NavGrid;

/// Eye height of a crouching soldier above his feet.
const CROUCHED_EYE: f32 = 1.05;

/// Whether `a` sees `b` past the level's collision.
pub fn line_of_sight(spatial: &SpatialQuery, a: Vec3, b: Vec3) -> bool {
    let to = b - a;
    Dir3::new(to).is_ok_and(|dir| {
        spatial
            .cast_ray(a, dir, to.length(), true, &SpatialQueryFilter::from_mask(GameLayer::World))
            .is_none()
    })
}

/// A spot within about 15 m of `from` where someone looking from `threat` (an eye position)
/// can't see a crouching soldier, reachable on the grid and not closer to the threat.
/// Casts at most a couple of dozen rays.
pub fn find_cover(nav: &NavGrid, spatial: &SpatialQuery, from: Vec3, threat: Vec3) -> Option<Vec3> {
    let start = nav.locate(from, 2.0, None)?;
    let region = nav.cell(start).region;
    let threat_distance = from.xz().distance(threat.xz());
    let offset = fastrand::f32() * TAU;
    let mut rays = 0;
    for radius in [3.0, 6.0, 10.0, 15.0] {
        let mut found: Option<(f32, Vec3)> = None;
        for i in 0..8 {
            let angle = offset + i as f32 * TAU / 8.0;
            let candidate = from + Vec3::new(angle.cos(), 0.0, angle.sin()) * radius;
            let Some(cell) = nav.locate(candidate, 1.5, Some(region)) else {
                continue;
            };
            let spot = nav.position(cell);
            if spot.xz().distance(threat.xz()) < threat_distance - 2.0 {
                continue;
            }
            rays += 1;
            if !line_of_sight(spatial, threat, spot + Vec3::Y * CROUCHED_EYE) {
                // Close to a wall is better cover than the open.
                let score = spot.distance(from) + nav.cell(cell).dist as f32 * 0.5;
                if found.is_none_or(|(best, _)| score < best) {
                    found = Some((score, spot));
                }
            }
            if rays >= 24 {
                break;
            }
        }
        if let Some((_, spot)) = found {
            return Some(spot);
        }
        if rays >= 24 {
            break;
        }
    }
    None
}

/// A spot off to one side (`side` +1 or -1) of an enemy at `enemy`, as seen from `from`,
/// to attack him from there. On the grid, in the region `from` is in.
pub fn flank_spot(nav: &NavGrid, from: Vec3, enemy: Vec3, side: f32) -> Option<Vec3> {
    let start = nav.locate(from, 2.0, None)?;
    let region = nav.cell(start).region;
    let to_us = (from - enemy).with_y(0.0);
    let distance = to_us.length().clamp(20.0, 60.0);
    for angle in [70f32, 50.0, 90.0] {
        let dir = Quat::from_rotation_y(side * angle.to_radians()) * to_us.normalize_or_zero();
        let candidate = enemy + dir * distance * 0.8;
        if let Some(cell) = nav.locate(candidate, 6.0, Some(region)) {
            return Some(nav.position(cell));
        }
    }
    None
}

/// Pitch (radians, up positive) to throw something at `speed` from `from` so it lands at
/// `to`: the low arc. `None` if out of reach.
pub fn throw_pitch(from: Vec3, to: Vec3, speed: f32, gravity: f32) -> Option<f32> {
    let x = from.xz().distance(to.xz());
    let y = to.y - from.y;
    if x < 0.5 {
        return Some(-1.2);
    }
    let v2 = speed * speed;
    let root = v2 * v2 - gravity * (gravity * x * x + 2.0 * y * v2);
    (root >= 0.0).then(|| ((v2 - root.sqrt()) / (gravity * x)).atan())
}

/// Whether a soldier at `feet` can step `distance` meters in `dir` without walking into a
/// wall or off a ledge. Off the grid (or without one) everything goes.
pub fn can_step(nav: Option<&NavGrid>, feet: Vec3, dir: Vec3, distance: f32) -> bool {
    let Some(nav) = nav else {
        return true;
    };
    if nav.locate(feet, 1.0, None).is_none() {
        return true;
    }
    nav.walkable_line(feet, feet + dir * distance)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn throws_reach_their_target() {
        let (from, speed, g) = (Vec3::new(0.0, 1.6, 0.0), 25.0, game_shared::physics::WORLD_GRAVITY);
        for target in [Vec3::new(20.0, 0.0, 0.0), Vec3::new(0.0, 3.0, -40.0), Vec3::new(10.0, -5.0, 10.0)] {
            let pitch = throw_pitch(from, target, speed, g).unwrap();
            // Fly the arc and see where it comes down at the target's height.
            let horizontal = (target - from).with_y(0.0).normalize();
            let mut p = from;
            let mut v = (horizontal * pitch.cos() + Vec3::Y * pitch.sin()) * speed;
            let dt = 0.001;
            while !(v.y < 0.0 && p.y <= target.y) {
                v.y -= g * dt;
                p += v * dt;
            }
            assert!(p.distance(target) < 0.5, "{target}: landed at {p}");
        }
        assert!(throw_pitch(from, Vec3::new(100.0, 0.0, 0.0), speed, g).is_none());
    }
}
