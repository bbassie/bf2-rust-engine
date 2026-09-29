//! Soldier hit zones: BF2's hit capsules on the skeleton's bones
//! (`setSkeletonCollisionBone <bone> <material> <offset> <radius> <length>` in the soldier
//! templates), posed the way a soldier with a rifle stands, crouches and lies: the body's
//! stance clips for the legs, a rifle's for the upper body, first frame.
//!
//! The capsule runs from the bone (moved by `offset`) `length` meters along the bone's -Y
//! axis: that way every one of them follows its limb (left and right bones point opposite
//! ways, and their lengths have opposite signs).

use bf2_formats::{
    anim::{Animation, Skeleton},
    con::Interpreter,
    vfs::Vfs,
};
use game_data::HitZone;
use glam::{Quat, Vec3};

/// Rifles whose third-person upper-body clips pose the arms, first found wins.
const RIFLES: [&str; 4] = ["usrif_m4", "usrif_m16a2", "rurif_ak101", "rurif_ak47"];
const BODY_CLIPS: &str = "objects/soldiers/common/animations/3p/";

/// Z-mirror of a BF2 rotation (as for the soldier models).
fn rotation(r: [f32; 4]) -> Quat {
    Quat::from_xyzw(-r[0], -r[1], r[2], r[3]).normalize()
}

fn translation(t: [f32; 3]) -> Vec3 {
    Vec3::new(t[0], t[1], -t[2])
}

/// Stance clips: the body's and the rifle's (by the rifle clip's name ending).
struct StanceClips {
    body: Option<Animation>,
    rifle: Option<Animation>,
}

fn read_clip(vfs: &Vfs, path: &str, skeleton: &Skeleton) -> Option<Animation> {
    Animation::parse(&vfs.read(path).ok()?, Some(skeleton))
        .map_err(|e| log::warn!("{path}: {e}"))
        .ok()
}

/// The first rifle's third-person clip ending in `_{state}.baf`.
fn rifle_clip(vfs: &Vfs, state: &str, skeleton: &Skeleton) -> Option<Animation> {
    let suffix = format!("_{state}.baf");
    RIFLES.iter().find_map(|rifle| {
        let dir = format!("objects/weapons/handheld/{rifle}/animations/3p/");
        let path = vfs.list(&dir).find(|p| p.ends_with(&suffix))?.to_string();
        read_clip(vfs, &path, skeleton)
    })
}

/// World transforms of every bone, posed by the clips' first frames (the rifle's bones over
/// the body's).
fn pose(skeleton: &Skeleton, clips: &StanceClips) -> Vec<(Quat, Vec3)> {
    let mut local: Vec<(Quat, Vec3)> = skeleton
        .bones
        .iter()
        .map(|b| (rotation(b.rotation), translation(b.translation)))
        .collect();
    for clip in [&clips.body, &clips.rifle].into_iter().flatten() {
        for track in &clip.tracks {
            if let (Some(slot), Some(r), Some(t)) = (
                local.get_mut(track.bone),
                track.rotations.first(),
                track.translations.first(),
            ) {
                *slot = (rotation(*r), translation(*t));
            }
        }
    }
    let mut world = vec![(Quat::IDENTITY, Vec3::ZERO); skeleton.bones.len()];
    for (i, bone) in skeleton.bones.iter().enumerate() {
        let (q, t) = local[i];
        world[i] = match bone.parent {
            Some(p) => {
                let (pq, pt) = world[p];
                (pq * q, pt + pq * t)
            }
            None => (q, t),
        };
    }
    world
}

fn round(v: Vec3) -> [f32; 3] {
    (v * 1000.0).round().to_array().map(|c| c / 1000.0)
}

/// The hit zones of the soldier template `name`, or none if it has no hit capsules.
pub fn hit_zones(interp: &mut Interpreter, vfs: &Vfs, skeleton: &Skeleton, name: &str) -> Vec<HitZone> {
    interp.ensure_template(name);
    let Some(template) = interp.world.template(name) else {
        return Vec::new();
    };
    let stances: Vec<Vec<(Quat, Vec3)>> = ["stand", "crouchstill", "pronestill"]
        .iter()
        .map(|state| {
            let clips = StanceClips {
                body: read_clip(vfs, &format!("{BODY_CLIPS}3p_{state}.baf"), skeleton),
                rifle: rifle_clip(vfs, state, skeleton),
            };
            pose(skeleton, &clips)
        })
        .collect();
    template
        .get_all("setskeletoncollisionbone")
        .filter_map(|args| {
            let [bone, material, offset, radius, length] = args else {
                return None;
            };
            let index = skeleton.find(bone)?;
            let offset: Vec<f32> = offset.split('/').filter_map(|v| v.parse().ok()).collect();
            let &[x, y, z] = offset.as_slice() else {
                return None;
            };
            // Mirrored like the capsule's axis (see the module docs).
            let offset = Vec3::new(x, -y, -z);
            let length: f32 = length.parse().ok()?;
            let ends = |pose: &Vec<(Quat, Vec3)>| {
                let (q, t) = pose[index];
                let start = t + q * offset;
                [round(start), round(start + q * Vec3::new(0.0, -length, 0.0))]
            };
            Some(HitZone {
                bone: bone.to_ascii_lowercase(),
                material: material.parse().ok()?,
                radius: radius.parse().ok()?,
                standing: ends(&stances[0]),
                crouching: ends(&stances[1]),
                prone: ends(&stances[2]),
                offset: round(offset),
                length,
            })
        })
        .collect()
}
