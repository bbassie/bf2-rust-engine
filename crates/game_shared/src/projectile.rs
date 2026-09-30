//! Grenades, rockets and placed charges: their replicated state, the flight rules server and
//! clients share (the server decides what happens; clients run the same steps to show
//! projectiles smoothly between updates and to predict their own throws), and smoke clouds.

use avian3d::prelude::*;
use bevy::{ecs::system::SystemParam, prelude::*};
use game_data::{FireKind, Impact, ProjectileDesc, TriggerDesc, WeaponDesc};
use serde::{Deserialize, Serialize};

use crate::physics::GameLayer;

/// A grenade, rocket or charge in the world. Replicated; bullets are not (clients draw
/// tracers for those).
#[derive(Component, Serialize, Deserialize, Clone, Debug)]
pub struct Projectile {
    /// The player who threw or fired it.
    #[entities]
    pub player: Entity,
    /// Weapon name.
    pub weapon: String,
}

/// Where a projectile is and how it moves. Replicated while it changes.
#[derive(Component, Serialize, Deserialize, Clone, Copy, Debug, PartialEq)]
pub struct ProjectileMotion {
    pub position: Vec3,
    pub velocity: Vec3,
    /// Its forward axis (-Z) is the thrower's facing, until it sticks to a surface, which
    /// tilts it to match.
    pub rotation: Quat,
    /// Lying still, or stuck to something.
    pub resting: bool,
}

impl ProjectileMotion {
    pub fn new(position: Vec3, velocity: Vec3, yaw: f32) -> Self {
        Self {
            position,
            velocity,
            rotation: Quat::from_rotation_y(yaw),
            resting: false,
        }
    }

    /// The facing along the ground (claymores point their blast this way).
    pub fn facing(&self) -> Vec3 {
        let forward = self.rotation * Vec3::NEG_Z;
        Vec3::new(forward.x, 0.0, forward.z).normalize_or(Vec3::NEG_Z)
    }
}

/// Something solid a projectile ran into.
#[derive(Clone, Copy, Debug)]
pub struct Contact {
    pub entity: Entity,
    pub point: Vec3,
    pub normal: Vec3,
    /// A soldier's body part: its damage table column (see [`crate::hitzones`]).
    pub body_part: Option<u32>,
}

/// What happened during one [`step`].
#[derive(Clone, Copy, Debug, Default)]
pub struct Step {
    /// It hit something and stops there (bullets, rockets, armed rifle grenades).
    pub hit: Option<Contact>,
    /// It stuck to something (charges).
    pub stuck: Option<Contact>,
    /// Bounced off something.
    pub bounced: bool,
    /// Came to rest.
    pub came_to_rest: bool,
    /// Meters flown.
    pub distance: f32,
}

/// Share of the speed into a surface kept when bouncing off it.
const RESTITUTION: f32 = 0.3;
/// Coulomb friction of a bouncing projectile: every bounce takes this times the impulse off
/// the speed along the surface. Also makes grenades roll to a stop.
const FRICTION: f32 = 0.7;
/// Slower than this on level enough ground, a bouncing projectile lies still.
const REST_SPEED: f32 = 0.6;
/// Distance kept from surfaces so the next ray starts outside.
const SKIN: f32 = 0.02;

/// Layers a projectile runs into: the world and vehicles. Soldiers are found by their hit
/// zones instead (the `soldiers` of [`step`]).
pub fn collision_layers(_desc: &ProjectileDesc) -> LayerMask {
    [GameLayer::World, GameLayer::Vehicle].into()
}

/// Advances a projectile by `dt`: its rocket motor, gravity, and what it runs into.
/// `age` is the seconds since launch (arming and motor delays count from it).
/// `soldiers(origin, direction, length)` finds the nearest soldier along a stretch of the
/// flight; only what can hurt soldiers directly (bullets, rockets, armed shells) asks.
pub fn step(
    spatial: &SpatialQuery,
    filter: &SpatialQueryFilter,
    desc: &ProjectileDesc,
    motion: &mut ProjectileMotion,
    age: f32,
    dt: f32,
    mut soldiers: impl FnMut(Vec3, Dir3, f32) -> Option<(f32, Contact)>,
) -> Step {
    let mut result = Step::default();
    if motion.resting {
        // Charges stay stuck; something lying around falls again when its support goes.
        if matches!(desc.impact, Impact::Stick { .. }) {
            return result;
        }
        let support = SpatialQueryFilter::from_mask([GameLayer::World, GameLayer::Vehicle]);
        if spatial
            .cast_ray(motion.position + Vec3::Y * 0.05, Dir3::NEG_Y, 0.2, true, &support)
            .is_some()
        {
            return result;
        }
        motion.resting = false;
    }
    let armed = age >= desc.arming_delay;

    if desc.acceleration > 0.0 && age >= desc.motor_delay {
        let speed = motion.velocity.length();
        if speed < desc.max_speed {
            let forward = motion.rotation * Vec3::NEG_Z;
            motion.velocity =
                motion.velocity.normalize_or(forward) * (speed + desc.acceleration * dt).min(desc.max_speed);
        }
    }
    let start_velocity = motion.velocity;
    motion.velocity += Vec3::NEG_Y * crate::physics::gravity(desc.gravity) * dt;
    let mut travel = (start_velocity + motion.velocity) * 0.5 * dt;

    for _ in 0..4 {
        let length = travel.length();
        let Ok(direction) = Dir3::new(travel) else {
            break;
        };
        let world = spatial.cast_ray(motion.position, direction, length, true, filter);
        let reach = world.map_or(length, |hit| hit.distance);
        let soldier = match desc.impact {
            Impact::Stop if armed => soldiers(motion.position, direction, reach),
            _ => None,
        };
        let (distance, contact) = match (soldier, world) {
            (Some(soldier), _) => soldier,
            (None, Some(hit)) => (
                hit.distance,
                Contact {
                    entity: hit.entity,
                    point: motion.position + direction * hit.distance,
                    normal: hit.normal,
                    body_part: None,
                },
            ),
            (None, None) => {
                motion.position += travel;
                result.distance += length;
                break;
            }
        };
        let point = contact.point;
        result.distance += distance;
        let hit = RayHitData {
            entity: contact.entity,
            distance,
            normal: contact.normal,
        };
        match desc.impact {
            Impact::Stop if armed => {
                motion.position = point;
                result.hit = Some(contact);
                return result;
            }
            Impact::Stick { max_angle } if hit.normal.angle_between(Vec3::Y).to_degrees() <= max_angle + 0.5 => {
                motion.position = point + hit.normal * SKIN;
                motion.velocity = Vec3::ZERO;
                motion.rotation = Quat::from_rotation_arc(Vec3::Y, hit.normal) * motion.rotation;
                motion.resting = true;
                result.stuck = Some(contact);
                return result;
            }
            _ => {}
        }
        // Bounce: lose some of the speed into the surface, and some along it by friction.
        let normal = hit.normal;
        let into = motion.velocity.dot(normal).min(0.0);
        let along = motion.velocity - normal * motion.velocity.dot(normal);
        let impulse = -into * (1.0 + RESTITUTION);
        let along_speed = along.length();
        let along = along * ((along_speed - FRICTION * impulse).max(0.0) / along_speed.max(1e-6));
        motion.velocity = along - normal * into * RESTITUTION;
        motion.position = point + normal * SKIN;
        result.bounced = true;
        if motion.velocity.length() < REST_SPEED && normal.y > 0.7 {
            motion.velocity = Vec3::ZERO;
            motion.resting = true;
            result.came_to_rest = true;
            break;
        }
        travel = motion.velocity * dt * (1.0 - hit.distance / length).max(0.0);
    }
    result
}

/// Turns `velocity` towards `direction` by at most `max_angle` radians.
pub fn steer(velocity: Vec3, direction: Vec3, max_angle: f32) -> Vec3 {
    let speed = velocity.length();
    let (Some(current), Some(wanted)) = (velocity.try_normalize(), direction.try_normalize()) else {
        return velocity;
    };
    let angle = current.angle_between(wanted);
    if angle <= max_angle {
        return wanted * speed;
    }
    let rotation = Quat::from_rotation_arc(current, wanted);
    let (axis, _) = rotation.to_axis_angle();
    Quat::from_axis_angle(axis, max_angle) * current * speed
}

/// The underhand throw (BF2's alternative fire for grenades) flies this much slower.
pub const SOFT_THROW: f32 = 0.5;

/// A projectile's velocity leaving the weapon along `direction`. What is thrown or placed
/// keeps the thrower's momentum.
pub fn launch_velocity(weapon: &WeaponDesc, direction: Vec3, soft: bool, thrower: Vec3) -> Vec3 {
    let speed = weapon.projectile.velocity * if soft { SOFT_THROW } else { 1.0 };
    let carried = if weapon.fire.kind == FireKind::Gun {
        Vec3::ZERO
    } else {
        thrower
    };
    direction * speed + carried
}

/// Where a projectile leaves the weapon: `offset` from the eye in view space, pulled back
/// in front of any wall between the eye and there.
pub fn launch_origin(spatial: &SpatialQuery, eye: Vec3, view: Quat, offset: Vec3) -> Vec3 {
    let origin = eye + view * offset;
    let to = origin - eye;
    let Ok(direction) = Dir3::new(to) else {
        return origin;
    };
    let filter = SpatialQueryFilter::from_mask([GameLayer::World, GameLayer::Vehicle]);
    match spatial.cast_ray(eye, direction, to.length(), true, &filter) {
        Some(hit) => eye + direction * (hit.distance - 0.05).max(0.0),
        None => origin,
    }
}

/// Whether a mine's trigger catches something at `target` moving at `speed` (m/s): close
/// enough, fast enough, and in front if the trigger only looks ahead.
pub fn in_trigger(trigger: &TriggerDesc, mine: &ProjectileMotion, target: Vec3, speed: f32) -> bool {
    let offset = target - mine.position;
    if offset.length() > trigger.radius || speed < trigger.min_speed {
        return false;
    }
    let along_ground = Vec3::new(offset.x, 0.0, offset.z);
    trigger.angle <= 0.0
        || (along_ground.length() > 0.01 && along_ground.angle_between(mine.facing()).to_degrees() <= trigger.angle)
}

/// A smoke cloud (replicated). Hides soldiers from bots and blocks the view.
#[derive(Component, Serialize, Deserialize, Clone, Copy, Debug, PartialEq)]
pub struct SmokeCloud {
    /// Center of the cloud.
    pub position: Vec3,
    /// Meters, once it has spread.
    pub radius: f32,
    pub duration: f32,
    /// Seconds since the grenade went off.
    pub age: f32,
    /// Tear gas: hit points per second taken from those inside without a gas mask. 0 =
    /// plain smoke.
    #[serde(default)]
    pub gas_damage: f32,
}

/// Seconds a smoke cloud takes to spread, and to thin out at the end.
const SMOKE_SPREAD: f32 = 2.0;
const SMOKE_FADE: f32 = 3.0;

impl SmokeCloud {
    /// Radius now: the cloud billows out over its first seconds.
    pub fn current_radius(&self) -> f32 {
        let t = (self.age / SMOKE_SPREAD).clamp(0.0, 1.0);
        self.radius * (1.0 - (1.0 - t) * (1.0 - t))
    }

    /// How thick it is, 0..1: thin at first, thinning out again over its last seconds.
    pub fn density(&self) -> f32 {
        let rise = (self.age / (SMOKE_SPREAD * 0.5)).clamp(0.0, 1.0);
        let fade = ((self.duration - self.age) / SMOKE_FADE).clamp(0.0, 1.0);
        rise.min(fade)
    }

    /// Thickness at `point`, 0..1: thickest in the middle.
    pub fn density_at(&self, point: Vec3) -> f32 {
        let radius = self.current_radius();
        if radius <= 0.0 {
            return 0.0;
        }
        let depth = 1.0 - point.distance(self.position) / radius;
        (depth * 2.0).clamp(0.0, 1.0) * self.density()
    }

    /// Whether the line from `from` to `to` passes through the thick of it. Soldiers within
    /// a few meters of each other still see each other.
    pub fn blocks(&self, from: Vec3, to: Vec3) -> bool {
        if self.density() < 0.5 || from.distance(to) < 3.0 {
            return false;
        }
        let segment = to - from;
        let t = ((self.position - from).dot(segment) / segment.length_squared()).clamp(0.0, 1.0);
        (from + segment * t).distance(self.position) < self.current_radius() * 0.8
    }
}

/// Smoke clouds in the world.
#[derive(SystemParam)]
pub struct Smoke<'w, 's> {
    clouds: Query<'w, 's, &'static SmokeCloud>,
}

impl Smoke<'_, '_> {
    /// Whether smoke hides `to` from someone looking from `from` (for bots' line of sight).
    pub fn blocks(&self, from: Vec3, to: Vec3) -> bool {
        self.clouds.iter().any(|cloud| cloud.blocks(from, to))
    }

    /// How thick the smoke is at `point`, 0..1.
    pub fn density_at(&self, point: Vec3) -> f32 {
        self.clouds
            .iter()
            .map(|cloud| cloud.density_at(point))
            .fold(0.0, f32::max)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn steering_turns_at_most_the_given_angle() {
        let v = steer(Vec3::NEG_Z * 10.0, Vec3::X, 0.1);
        assert!((v.length() - 10.0).abs() < 1e-4);
        assert!((v.normalize().angle_between(Vec3::NEG_Z) - 0.1).abs() < 1e-4);
        let v = steer(Vec3::NEG_Z * 10.0, Vec3::new(0.01, 0.0, -1.0), 0.1);
        assert!(v.normalize().angle_between(Vec3::new(0.01, 0.0, -1.0).normalize()) < 1e-4);
    }

    #[test]
    fn claymores_catch_what_moves_in_front_of_them() {
        let trigger = TriggerDesc {
            by: game_data::TriggerBy::Soldiers,
            radius: 7.0,
            angle: 30.0,
            min_speed: 1.0,
        };
        // Facing -Z.
        let mine = ProjectileMotion::new(Vec3::ZERO, Vec3::ZERO, 0.0);
        let ahead = Vec3::new(1.0, 0.9, -4.0);
        assert!(in_trigger(&trigger, &mine, ahead, 3.0));
        assert!(!in_trigger(&trigger, &mine, ahead, 0.5), "sneaking past");
        assert!(!in_trigger(&trigger, &mine, Vec3::new(0.0, 0.9, -8.0), 3.0), "too far");
        assert!(
            !in_trigger(&trigger, &mine, Vec3::new(4.0, 0.9, -2.0), 3.0),
            "off to the side"
        );
        assert!(!in_trigger(&trigger, &mine, Vec3::new(0.0, 0.9, 3.0), 3.0), "behind");
        // Turned to face +X.
        let mine = ProjectileMotion::new(Vec3::ZERO, Vec3::ZERO, -std::f32::consts::FRAC_PI_2);
        assert!(in_trigger(&trigger, &mine, Vec3::new(4.0, 0.9, 0.5), 3.0));
    }

    #[test]
    fn smoke_blocks_lines_through_its_middle_only_while_thick() {
        let mut cloud = SmokeCloud {
            position: Vec3::ZERO,
            radius: 6.0,
            duration: 12.0,
            age: 0.1,
            gas_damage: 0.0,
        };
        let (from, to) = (Vec3::new(-20.0, 0.0, 0.0), Vec3::new(20.0, 0.0, 0.0));
        assert!(!cloud.blocks(from, to), "not spread yet");
        cloud.age = 5.0;
        assert!(cloud.blocks(from, to));
        assert!(
            !cloud.blocks(from + Vec3::Z * 10.0, to + Vec3::Z * 10.0),
            "passes beside it"
        );
        assert!(
            !cloud.blocks(Vec3::new(-1.0, 0.0, 0.0), Vec3::new(1.0, 0.0, 0.0)),
            "too close"
        );
        cloud.age = 11.5;
        assert!(!cloud.blocks(from, to), "thinned out");
    }
}
