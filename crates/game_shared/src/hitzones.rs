//! Where a soldier can be hit: capsules around its bones (`game_data::HitZone`, BF2's
//! per-bone hit capsules) posed by stance, and the ray test bullets use. Each body part has
//! its own damage table column (head, body, body armour, limbs).

use std::sync::LazyLock;

use bevy::prelude::*;
use game_data::HitZone;
use serde::{Deserialize, Serialize};

use crate::{
    skeleton::{AnimState, BONES, Pose},
    soldier::{SoldierMotion, Stance},
};

/// Head, body, body armour and limbs: the damage table columns of BF2's soldier hit
/// capsules.
pub const HEAD: u32 = 25;
pub const BODY: u32 = 24;
pub const LIMBS: u32 = 77;

/// No hit zone reaches further than this from a soldier's feet (prone soldiers lie about
/// 2 m long).
pub const REACH: f32 = 2.4;

/// The server's tick, on an entity of its own, replicated every tick: clients tell from it
/// which tick the world they see is from (`InputFrame::view_tick`), for lag compensation.
#[derive(Component, Serialize, Deserialize, Clone, Copy, Debug, Default, PartialEq)]
pub struct ServerClock(pub u32);

/// `BF2_HITREG_LOG=1`: the server logs every bullet's verdict on soldiers and clients their
/// predicted impacts, prefixed `hitreg`, for checking hit registration.
pub fn hitreg_log() -> bool {
    static ON: LazyLock<bool> = LazyLock::new(|| std::env::var_os("BF2_HITREG_LOG").is_some());
    *ON
}

/// `BF2_HITREG_BEFORE=1`: hit registration as it was before hit zones followed the
/// animations (stance capsules, client tracers stopped by the movement capsule, the host's
/// shots judged against the present), for comparison.
pub fn legacy() -> bool {
    static ON: LazyLock<bool> = LazyLock::new(|| std::env::var_os("BF2_HITREG_BEFORE").is_some());
    *ON
}

/// Where a ray met a soldier.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct ZoneHit {
    /// Along the ray.
    pub distance: f32,
    pub point: Vec3,
    /// Out of the capsule.
    pub normal: Vec3,
    /// The body part's damage table column.
    pub material: u32,
    /// Which of the zones it met (index into the soldier's hit zones).
    pub zone: usize,
}

/// How a soldier's body is posed for its hit zones: where it stands, which way it faces,
/// its stance and, on foot, what its animations depend on ([`crate::skeleton`]). Soldiers
/// riding in vehicles sit, which the crouching pose is closest to.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct BodyPose {
    pub position: Vec3,
    pub yaw: f32,
    pub stance: Stance,
    /// Without (seated, climbing, swimming, ...), the zones keep their stance's pose.
    pub anim: Option<AnimState>,
}

impl BodyPose {
    pub fn of(motion: &SoldierMotion, seated: bool) -> Self {
        Self {
            position: motion.position,
            yaw: motion.yaw,
            stance: if seated { Stance::Crouching } else { motion.stance },
            anim: None,
        }
    }

    /// A zone's capsule ends in the world, in its stance's pose.
    pub fn capsule(&self, zone: &HitZone) -> (Vec3, Vec3) {
        let ends = match self.stance {
            Stance::Standing => &zone.standing,
            Stance::Crouching => &zone.crouching,
            Stance::Prone => &zone.prone,
        };
        let rotation = Quat::from_rotation_y(self.yaw);
        let at = |p: [f32; 3]| self.position + rotation * Vec3::from_array(p);
        (at(ends[0]), at(ends[1]))
    }

    /// A zone's capsule ends in the world, on its bone as `bones` pose it (see
    /// [`crate::skeleton::HitRigs::pose`]), else in its stance's pose.
    pub fn posed_capsule(&self, zone: &HitZone, bones: Option<&Pose>) -> (Vec3, Vec3) {
        let bone = bones
            .filter(|_| zone.length != 0.0)
            .and_then(|bones| Some(bones[BONES.iter().position(|b| *b == zone.bone)?]));
        let Some((q, t)) = bone else {
            return self.capsule(zone);
        };
        let start = t + q * Vec3::from(zone.offset);
        let end = start + q * Vec3::new(0.0, -zone.length, 0.0);
        let rotation = Quat::from_rotation_y(self.yaw);
        (self.position + rotation * start, self.position + rotation * end)
    }

    /// Whether a ray from `origin` along `direction` (normalized) passes within reach of the
    /// soldier within `max` meters: most rays pass far from him.
    pub fn near(&self, origin: Vec3, direction: Vec3, max: f32) -> bool {
        let center = self.position + Vec3::Y * 0.9;
        let along = (center - origin).dot(direction).clamp(0.0, max);
        (origin + direction * along).distance(center) <= REACH
    }

    /// The nearest zone, in its stance's pose, the ray from `origin` along `direction`
    /// (normalized) meets within `max` meters.
    pub fn ray(&self, zones: &[HitZone], origin: Vec3, direction: Vec3, max: f32) -> Option<ZoneHit> {
        self.ray_posed(zones, || None, origin, direction, max)
    }

    /// The same, with the zones on the bones `bones` poses (only asked for if the ray passes
    /// near).
    pub fn ray_posed(
        &self,
        zones: &[HitZone],
        bones: impl FnOnce() -> Option<Pose>,
        origin: Vec3,
        direction: Vec3,
        max: f32,
    ) -> Option<ZoneHit> {
        if !self.near(origin, direction, max) {
            return None;
        }
        let bones = bones();
        zones
            .iter()
            .enumerate()
            .filter_map(|(index, zone)| {
                let (a, b) = self.posed_capsule(zone, bones.as_ref());
                let distance = ray_capsule(origin, direction, a, b, zone.radius)?;
                (distance <= max).then(|| {
                    let point = origin + direction * distance;
                    let axis = b - a;
                    let t = ((point - a).dot(axis) / axis.length_squared().max(1e-8)).clamp(0.0, 1.0);
                    ZoneHit {
                        distance,
                        point,
                        normal: (point - (a + axis * t)).normalize_or(-direction),
                        material: zone.material,
                        zone: index,
                    }
                })
            })
            .min_by(|a, b| a.distance.total_cmp(&b.distance))
    }
}

/// Server -> everyone: a bullet hit a soldier here (the blood, which clients leave to the
/// server: their own tracers only predict where it stops).
#[derive(Message, Serialize, Deserialize, Clone, Copy, Debug)]
pub struct SoldierImpact {
    pub point: Vec3,
    /// Out of the body.
    pub normal: Vec3,
    /// The projectile's material and the body part's (the impact effect's pair).
    pub projectile: u32,
    pub body_part: u32,
}

/// Distance along a ray (`direction` normalized) to where it enters the capsule around `a`-`b`
/// (0 if it starts inside).
pub fn ray_capsule(origin: Vec3, direction: Vec3, a: Vec3, b: Vec3, radius: f32) -> Option<f32> {
    let ba = b - a;
    let oa = origin - a;
    let baba = ba.dot(ba);
    let bard = ba.dot(direction);
    let baoa = ba.dot(oa);
    let rdoa = direction.dot(oa);
    let oaoa = oa.dot(oa);
    // Starting inside: the part of the axis closest to the origin is within the radius.
    let t = if baba > 0.0 { (baoa / baba).clamp(0.0, 1.0) } else { 0.0 };
    if (a + ba * t).distance_squared(origin) <= radius * radius {
        return Some(0.0);
    }
    // The cylinder's side.
    let k2 = baba - bard * bard;
    if k2.abs() > 1e-9 {
        let k1 = baba * rdoa - baoa * bard;
        let k0 = baba * oaoa - baoa * baoa - radius * radius * baba;
        let h = k1 * k1 - k2 * k0;
        if h < 0.0 {
            return None;
        }
        let distance = (-k1 - h.sqrt()) / k2;
        let y = baoa + distance * bard;
        if y > 0.0 && y < baba {
            return (distance >= 0.0).then_some(distance);
        }
    }
    // The caps.
    let sphere = |center: Vec3| {
        let oc = origin - center;
        let b = direction.dot(oc);
        let h = b * b - (oc.dot(oc) - radius * radius);
        (h >= 0.0).then(|| -b - h.sqrt()).filter(|d| *d >= 0.0)
    };
    match (sphere(a), sphere(b)) {
        (Some(x), Some(y)) => Some(x.min(y)),
        (x, y) => x.or(y),
    }
}

/// Rough hit zones for soldiers without imported ones (the test range).
pub fn fallback() -> &'static [HitZone] {
    static ZONES: LazyLock<Vec<HitZone>> = LazyLock::new(|| {
        let zone = |bone: &str, material, radius, standing, crouching, prone| HitZone {
            bone: bone.into(),
            material,
            radius,
            standing,
            crouching,
            prone,
            offset: [0.0; 3],
            length: 0.0,
        };
        vec![
            zone(
                "head",
                HEAD,
                0.11,
                [[0.0, 1.6, 0.0], [0.0, 1.7, 0.0]],
                [[0.0, 1.1, -0.2], [0.0, 1.2, -0.2]],
                [[0.0, 0.3, -0.9], [0.0, 0.3, -1.0]],
            ),
            zone(
                "torso",
                BODY,
                0.18,
                [[0.0, 1.0, 0.0], [0.0, 1.4, 0.0]],
                [[0.0, 0.6, 0.0], [0.0, 0.95, -0.15]],
                [[0.0, 0.2, 0.0], [0.0, 0.25, -0.7]],
            ),
            zone(
                "left_leg",
                BODY,
                0.09,
                [[-0.1, 0.1, 0.0], [-0.1, 0.9, 0.0]],
                [[-0.15, 0.1, 0.1], [-0.12, 0.55, -0.1]],
                [[-0.15, 0.12, 0.9], [-0.1, 0.15, 0.05]],
            ),
            zone(
                "right_leg",
                BODY,
                0.09,
                [[0.1, 0.1, 0.0], [0.1, 0.9, 0.0]],
                [[0.15, 0.1, 0.1], [0.12, 0.55, -0.1]],
                [[0.15, 0.12, 0.9], [0.1, 0.15, 0.05]],
            ),
        ]
    });
    &ZONES
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rays_meet_capsules_on_the_side_the_caps_and_from_inside() {
        let (a, b) = (Vec3::ZERO, Vec3::Y);
        let side = ray_capsule(Vec3::new(-5.0, 0.5, 0.0), Vec3::X, a, b, 0.2).unwrap();
        assert!((side - 4.8).abs() < 1e-4);
        let cap = ray_capsule(Vec3::new(0.0, 5.0, 0.0), Vec3::NEG_Y, a, b, 0.2).unwrap();
        assert!((cap - 3.8).abs() < 1e-4);
        assert_eq!(ray_capsule(Vec3::new(0.05, 0.5, 0.0), Vec3::X, a, b, 0.2), Some(0.0));
        assert!(ray_capsule(Vec3::new(-5.0, 0.5, 0.3), Vec3::X, a, b, 0.2).is_none());
        assert!(
            ray_capsule(Vec3::new(5.0, 0.5, 0.0), Vec3::X, a, b, 0.2).is_none(),
            "behind"
        );
    }

    #[test]
    fn zones_follow_the_soldier_and_tell_the_head_from_the_legs() {
        let pose = BodyPose {
            position: Vec3::new(10.0, 2.0, 0.0),
            yaw: std::f32::consts::FRAC_PI_2,
            stance: Stance::Standing,
            anim: None,
        };
        let zones = fallback();
        let head = pose.ray(zones, Vec3::new(0.0, 3.65, 0.0), Vec3::X, 100.0).unwrap();
        assert_eq!(head.material, HEAD);
        let leg = pose.ray(zones, Vec3::new(0.0, 2.4, 0.1), Vec3::X, 100.0).unwrap();
        assert_eq!(leg.material, BODY);
        assert!(
            pose.ray(zones, Vec3::new(0.0, 4.5, 0.0), Vec3::X, 100.0).is_none(),
            "over the head"
        );
        let prone = BodyPose {
            stance: Stance::Prone,
            ..pose
        };
        assert!(prone.ray(zones, Vec3::new(0.0, 3.65, 0.0), Vec3::X, 100.0).is_none());
    }
}
