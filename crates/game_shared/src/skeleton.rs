//! Soldier skeletons posed the way clients draw them, so hit zones follow the body.
//!
//! Clients animate a soldier with two layers of BF2's clips (`game_client::render::soldiers`):
//! the body's movement clips (root and legs) and the held weapon's upper-body clips (root,
//! spine, arms and head). Bevy averages the bones both layers animate (the root) by weight.
//! [`Rig`] reads the same `.glb` files, and [`AnimState`] picks the clips, weights and times
//! from a soldier's state exactly as the client's animator does ([`AnimState::clips`]), so the
//! server can pose the bones the hit zones hang on for any tick it keeps, and clients can
//! predict hits against the same capsules.
//!
//! What they share: the movement clips' step phase is [`SoldierMotion::stride`] (replicated),
//! idle loops run on the server clock, a reload's clip on the tick it started
//! ([`crate::weapons::Inventory::reload_started`]).

use std::{path::Path, sync::Arc};


use bevy::{platform::collections::HashMap, prelude::*};

use crate::soldier::{SoldierMotion, Stance};

/// The bones hit zones hang on and their ancestors in BF2's `3p_setup` skeleton, parents
/// first.
pub const BONES: [&str; 21] = [
    "root",
    "spine2",
    "spine3",
    "torso",
    "joint20",
    "neck",
    "head",
    "left_collar",
    "left_shoulder",
    "left_elbow",
    "left_low_arm",
    "right_collar",
    "right_shoulder",
    "right_elbow",
    "right_low_arm",
    "left_upperleg",
    "left_knee",
    "left_lowerleg",
    "right_upperleg",
    "right_knee",
    "right_lowerleg",
];

/// Clip names: the body's movement clips and the weapon sets' upper-body clips.
pub mod clips {
    pub const STAND: &str = "3p_stand";
    pub const CROUCH: &str = "3p_crouchstill";
    pub const PRONE: &str = "3p_pronestill";
    pub const SPRINT: &str = "3p_sprint";
    /// Directional sets: forward, backward, left, right.
    pub const WALK: [&str; 4] = ["3p_walkforward", "3p_walkbackward", "3p_walkleft", "3p_walkright"];
    pub const RUN: [&str; 4] = ["3p_runforward", "3p_runbackward", "3p_strafeleft", "3p_straferight"];
    pub const CROUCH_MOVE: [&str; 4] =
        ["3p_crouchforward", "3p_crouchbackward", "3p_crouchstrafeleft", "3p_crouchstraferight"];
    pub const PRONE_MOVE: [&str; 4] =
        ["3p_proneforward", "3p_pronebackward", "3p_pronestrafeleft", "3p_pronestraferight"];
    /// Upper-body one-shots, standing (and crouched) and prone.
    pub const FIRE: [&str; 2] = ["standfire", "pronefire"];
    pub const RELOAD: [&str; 2] = ["reload", "pronereload"];

    /// The upper-body clip named like a movement clip (`3p_crouchstill` → `crouchstill`).
    pub fn upper_for(legs: &str) -> &str {
        legs.trim_start_matches("3p_")
    }

    /// Every movement clip a [`super::Rig`] needs.
    pub fn legs() -> impl Iterator<Item = &'static str> {
        [STAND, CROUCH, PRONE, SPRINT].into_iter().chain(WALK).chain(RUN).chain(CROUCH_MOVE).chain(PRONE_MOVE)
    }
}

/// How a soldier moves his legs.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Gait {
    Still,
    /// Standing, slower than a run.
    Walk,
    /// Running, crouch-walking or crawling, by stance.
    Move,
    Sprint,
}

/// Ground speeds (m/s) at which the movement clips play at normal speed, from BF2's
/// animation value holders (they match the foot speed in the clips).
pub const WALK_SPEED: f32 = 1.5;
pub const RUN_SPEED: f32 = 3.9;
pub const SPRINT_SPEED: f32 = 6.3;
pub const CROUCH_SPEED: f32 = 1.7;
pub const PRONE_SPEED: f32 = 0.7;

/// Faster than this (m/s) the legs move.
const MOVING: f32 = 0.4;
/// Standing, slower than this walks, faster than [`SPRINTING`] (forward) sprints.
const WALKING: f32 = 2.2;
const SPRINTING: f32 = 5.0;

/// The gait for a stance and a horizontal velocity in the soldier's frame (right, forward).
pub fn gait(stance: Stance, velocity: Vec2) -> Gait {
    let speed = velocity.length();
    match stance {
        _ if speed <= MOVING => Gait::Still,
        Stance::Standing if speed > SPRINTING && velocity.y > 0.0 => Gait::Sprint,
        Stance::Standing if speed < WALKING => Gait::Walk,
        _ => Gait::Move,
    }
}

/// The movement clips of a gait (all four directions, or one) and the speed they're made for.
pub fn gait_clips(stance: Stance, gait: Gait) -> (&'static [&'static str], f32) {
    match (gait, stance) {
        (Gait::Still, Stance::Standing) => (&[clips::STAND], 1.0),
        (Gait::Still, Stance::Crouching) => (&[clips::CROUCH], 1.0),
        (Gait::Still, Stance::Prone) => (&[clips::PRONE], 1.0),
        (Gait::Walk, _) => (&clips::WALK, WALK_SPEED),
        (Gait::Sprint, _) => (&[clips::SPRINT], SPRINT_SPEED),
        (Gait::Move, Stance::Standing) => (&clips::RUN, RUN_SPEED),
        (Gait::Move, Stance::Crouching) => (&clips::CROUCH_MOVE, CROUCH_SPEED),
        (Gait::Move, Stance::Prone) => (&clips::PRONE_MOVE, PRONE_SPEED),
    }
}

/// Seconds one step cycle of a gait's clips lasts (BF2's clips: all directions of a gait are
/// equally long).
fn cycle_seconds(stance: Stance, gait: Gait) -> f32 {
    match (gait, stance) {
        (Gait::Walk, _) => 1.0417,
        (Gait::Sprint, _) => 0.5,
        (_, Stance::Standing) => 0.625,
        (_, Stance::Crouching) => 0.7917,
        (_, Stance::Prone) => 1.125,
    }
}

/// How far through its step cycle a soldier's legs get walking `speed` m/s for `dt` seconds:
/// what [`SoldierMotion::stride`] advances by.
pub fn stride_step(stance: Stance, velocity: Vec2, dt: f32) -> f32 {
    let gait = gait(stance, velocity);
    if gait == Gait::Still {
        return 0.0;
    }
    let (_, normal) = gait_clips(stance, gait);
    velocity.length() * dt / (normal * cycle_seconds(stance, gait))
}

/// Weights of the forward, backward, left and right clips for a movement direction.
pub fn direction_weights(velocity: Vec2) -> [f32; 4] {
    let sum = velocity.x.abs() + velocity.y.abs();
    if sum < 1e-4 {
        return [1.0, 0.0, 0.0, 0.0];
    }
    [velocity.y, -velocity.y, -velocity.x, velocity.x].map(|w| w.max(0.0) / sum)
}

/// Horizontal velocity in a soldier's frame: (right, forward).
pub fn local_velocity(yaw: f32, velocity: Vec3) -> Vec2 {
    let local = Quat::from_rotation_y(-yaw) * velocity;
    Vec2::new(local.x, -local.z)
}

/// A looping clip's time for a step phase (0..1).
pub fn cycle_time(stride: f32, duration: f32) -> f32 {
    stride.rem_euclid(1.0) * duration
}

/// A looping clip's time on a clock (seconds).
pub fn loop_time(clock: f32, duration: f32) -> f32 {
    if duration > 0.0 { clock.rem_euclid(duration) } else { 0.0 }
}

/// Everything a soldier's pose depends on at one moment. Clients fill it from the drawn
/// (interpolated) state, the server from each tick's.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct AnimState {
    pub stance: Stance,
    /// Horizontal velocity in the soldier's frame: (right, forward).
    pub velocity: Vec2,
    /// [`SoldierMotion::stride`].
    pub stride: f32,
    /// Seconds on the server clock, for idle loops.
    pub clock: f32,
    /// Seconds into a reload.
    pub reload: Option<f32>,
    /// The held weapon (index into the loadout), for its upper-body clips.
    pub weapon: u8,
}

impl AnimState {
    /// The reload clip and its time, while it plays (the client plays it once, then the
    /// loops again for the rest of a long reload).
    fn reload_clip(&self, upper: &impl Fn(&str) -> Option<f32>) -> Option<(&'static str, f32)> {
        let name = clips::RELOAD[usize::from(self.stance == Stance::Prone)];
        let reload = self.reload?;
        let duration = upper(name)?;
        (reload < duration).then_some((name, reload))
    }

    /// What the client's animator crossfades between: the legs' state.
    fn legs_state(&self) -> (Stance, Gait) {
        (self.stance, gait(self.stance, self.velocity))
    }
    /// The state of a soldier on his feet with a weapon in hand; `None` when the client draws
    /// him some other way (climbing, swimming, under a parachute, hanging from a zipline,
    /// airborne), where hit zones keep their stance's pose.
    pub fn of(motion: &SoldierMotion, clock: f32, reload: Option<f32>, weapon: u8) -> Option<Self> {
        let on_foot = motion.grounded && !motion.climbing && !motion.riding && !motion.parachute && !motion.swimming;
        on_foot.then(|| Self {
            stance: motion.stance,
            velocity: local_velocity(motion.yaw, motion.velocity),
            stride: motion.stride,
            clock,
            reload,
            weapon,
        })
    }

    /// The clips to blend: (upper body, name, weight, time), given the lengths of the
    /// movement clips and of the weapon's upper-body clips (`None`: the set has no such clip).
    /// The client's animator picks the same (`game_client::render::soldiers`).
    pub fn clips(
        &self,
        legs: impl Fn(&str) -> Option<f32>,
        upper: impl Fn(&str) -> Option<f32>,
    ) -> Vec<(bool, &'static str, f32, f32)> {
        let gait = gait(self.stance, self.velocity);
        let (names, _) = gait_clips(self.stance, gait);
        let weights = if names.len() == 4 { direction_weights(self.velocity) } else { [1.0, 0.0, 0.0, 0.0] };
        let time = |duration: f32| match gait {
            Gait::Still => loop_time(self.clock, duration),
            _ => cycle_time(self.stride, duration),
        };
        let mut out = Vec::with_capacity(8);
        for (&name, weight) in names.iter().zip(weights).filter(|(_, w)| *w > 0.02) {
            if let Some(duration) = legs(name) {
                out.push((false, name, weight, time(duration)));
            }
        }
        if let Some((name, time)) = self.reload_clip(&upper) {
            out.push((true, name, 1.0, time));
            return out;
        }
        for (&name, weight) in names.iter().zip(weights).filter(|(_, w)| *w > 0.02) {
            let paired = clips::upper_for(name);
            let (name, duration) = match upper(paired) {
                Some(duration) => (paired, duration),
                None => ("stand", upper("stand").unwrap_or(0.0)),
            };
            out.push((true, name, weight, time(duration)));
        }
        out
    }
}

/// The client animator's crossfade times (seconds), roughly BF2's bundle fade times.
pub mod fade {
    pub const FADE: f32 = 0.2;
    pub const START_MOVING: f32 = 0.15;
    pub const STANCE: f32 = 0.3;
    pub const PRONE: f32 = 0.4;
    pub const RELOAD_IN: f32 = 0.15;
    pub const ACTION_OUT: f32 = 0.2;
    /// The longest of them.
    pub const LONGEST: f32 = PRONE;
}

/// How long the client crossfades from one legs state to another (the on-foot part of its
/// animator's rules).
pub fn fade_time(from: (Stance, Gait), to: (Stance, Gait)) -> f32 {
    match (from, to) {
        ((a, _), (b, _)) if a != b => {
            if a == Stance::Prone || b == Stance::Prone {
                fade::PRONE
            } else {
                fade::STANCE
            }
        }
        ((_, Gait::Still), _) => fade::START_MOVING,
        _ => fade::FADE,
    }
}

/// A clip crossfading in a [`Blend`].
#[derive(Clone, Copy, Debug)]
struct Fading {
    upper: bool,
    name: &'static str,
    weight: f32,
    target: f32,
    /// Its time when it was last played (clips fading out stand still).
    time: f32,
}

/// The client's crossfades (`game_client::render::blend`), replayed from a soldier's recent
/// states, so a pose in the middle of a crossfade (strafing from one side to the other,
/// crouching) blends the same clips by the same weights.
#[derive(Clone, Debug, Default)]
pub struct Blend {
    tracks: Vec<Fading>,
    last: Option<((Stance, Gait), bool)>,
    legs_fade: f32,
    upper_fade: f32,
}

impl Blend {
    /// The next state, `dt` seconds after the last.
    pub fn step(&mut self, state: &AnimState, dt: f32, legs: impl Fn(&str) -> Option<f32>, upper: impl Fn(&str) -> Option<f32>) {
        let key = state.legs_state();
        let action = state.reload_clip(&upper).is_some();
        let settled = self.last.is_none();
        if let Some((last, last_action)) = self.last {
            if last != key {
                self.legs_fade = fade_time(last, key);
                if !action && !last_action {
                    self.upper_fade = self.legs_fade;
                }
            }
            if action && !last_action {
                self.upper_fade = fade::RELOAD_IN;
            } else if !action && last_action {
                self.upper_fade = fade::ACTION_OUT;
            }
        }
        self.last = Some((key, action));
        for track in &mut self.tracks {
            track.target = 0.0;
        }
        for (is_upper, name, weight, time) in state.clips(&legs, &upper) {
            match self.tracks.iter_mut().find(|t| t.upper == is_upper && t.name == name) {
                Some(track) => {
                    track.target += weight;
                    track.time = time;
                }
                None => self.tracks.push(Fading {
                    upper: is_upper,
                    name,
                    weight: 0.0,
                    target: weight,
                    time,
                }),
            }
        }
        // Each layer moves its weights toward their targets over its fade time; with nothing
        // showing yet (or at the first state) there is nothing to fade from.
        for upper_layer in [false, true] {
            let fade = if upper_layer { self.upper_fade } else { self.legs_fade };
            let silent = self.tracks.iter().filter(|t| t.upper == upper_layer).all(|t| t.weight <= 0.0);
            let step = if fade > 0.0 && !silent && !settled { dt / fade } else { f32::INFINITY };
            for track in self.tracks.iter_mut().filter(|t| t.upper == upper_layer) {
                track.weight += (track.target - track.weight).clamp(-step, step);
            }
        }
        self.tracks.retain(|t| t.weight > 0.0 || t.target > 0.0);
    }

    /// The clips to blend now: (upper body, name, weight, time).
    pub fn clips(&self) -> Vec<(bool, &'static str, f32, f32)> {
        self.tracks
            .iter()
            .filter(|t| t.weight > 0.0)
            .map(|t| (t.upper, t.name, t.weight, t.time))
            .collect()
    }
}

/// One bone's keyframes in a clip (glTF linear interpolation).
#[derive(Clone, Debug, Default)]
struct Track {
    rotation: Option<(Vec<f32>, Vec<Quat>)>,
    translation: Option<(Vec<f32>, Vec<Vec3>)>,
}

fn sample<T: Copy>(keys: &(Vec<f32>, Vec<T>), time: f32, mix: impl Fn(T, T, f32) -> T) -> Option<T> {
    let (times, values) = keys;
    let last = times.len().min(values.len()).checked_sub(1)?;
    if time <= times[0] {
        return Some(values[0]);
    }
    if time >= times[last] {
        return Some(values[last]);
    }
    let next = times.partition_point(|t| *t <= time).min(last);
    let prev = next - 1;
    let span = times[next] - times[prev];
    let t = if span > 0.0 { (time - times[prev]) / span } else { 0.0 };
    Some(mix(values[prev], values[next], t))
}

/// A clip's tracks for [`BONES`].
#[derive(Clone, Debug, Default)]
pub struct Clip {
    pub duration: f32,
    tracks: Vec<Option<Track>>,
}

/// Clips by name.
#[derive(Clone, Debug, Default)]
pub struct ClipSet {
    clips: HashMap<String, Clip>,
}

impl ClipSet {
    /// A clip's length, if the set has it.
    pub fn duration(&self, name: &str) -> Option<f32> {
        self.clips.get(name).map(|c| c.duration)
    }

    /// The clips named in `wanted` from a `.glb` (bones in [`BONES`] only).
    pub fn load(path: &Path, wanted: impl Fn(&str) -> bool) -> anyhow::Result<(Self, Skeleton)> {
        let bytes = std::fs::read(path)?;
        let gltf = gltf::Gltf::from_slice(&bytes)?;
        let blob = gltf.blob.as_deref().unwrap_or_default();
        let node_bone: Vec<Option<usize>> =
            gltf.nodes().map(|n| n.name().and_then(|name| BONES.iter().position(|b| *b == name))).collect();
        // The rest pose and hierarchy, for the body.
        let mut skeleton = Skeleton::default();
        let mut parent_of = vec![None; gltf.nodes().len()];
        for node in gltf.nodes() {
            for child in node.children() {
                parent_of[child.index()] = Some(node.index());
            }
        }
        for node in gltf.nodes() {
            let Some(bone) = node_bone[node.index()] else {
                continue;
            };
            let (translation, rotation, _) = node.transform().decomposed();
            skeleton.rest[bone] = (Quat::from_array(rotation), Vec3::from_array(translation));
            // The nearest ancestor that is one of the bones.
            let mut up = parent_of[node.index()];
            while let Some(p) = up {
                if let Some(parent) = node_bone[p] {
                    skeleton.parents[bone] = Some(parent);
                    break;
                }
                up = parent_of[p];
            }
            skeleton.found[bone] = true;
        }
        let mut set = ClipSet::default();
        for animation in gltf.animations() {
            let Some(name) = animation.name().filter(|n| wanted(n)) else {
                continue;
            };
            let mut clip = Clip {
                duration: 0.0,
                tracks: vec![None; BONES.len()],
            };
            for channel in animation.channels() {
                let reader = channel.reader(|_| Some(blob));
                let Some(times) = reader.read_inputs().map(|t| t.collect::<Vec<f32>>()) else {
                    continue;
                };
                clip.duration = clip.duration.max(times.last().copied().unwrap_or(0.0));
                let Some(bone) = node_bone[channel.target().node().index()] else {
                    continue;
                };
                let track = clip.tracks[bone].get_or_insert_with(Track::default);
                match reader.read_outputs() {
                    Some(gltf::animation::util::ReadOutputs::Rotations(r)) => {
                        track.rotation = Some((times, r.into_f32().map(Quat::from_array).collect()));
                    }
                    Some(gltf::animation::util::ReadOutputs::Translations(t)) => {
                        track.translation = Some((times, t.map(Vec3::from_array).collect()));
                    }
                    _ => {}
                }
            }
            set.clips.insert(name.to_string(), clip);
        }
        Ok((set, skeleton))
    }
}

/// The rest pose and hierarchy of [`BONES`].
#[derive(Clone, Debug)]
pub struct Skeleton {
    rest: [(Quat, Vec3); BONES.len()],
    parents: [Option<usize>; BONES.len()],
    found: [bool; BONES.len()],
}

impl Default for Skeleton {
    fn default() -> Self {
        Self {
            rest: [(Quat::IDENTITY, Vec3::ZERO); BONES.len()],
            parents: [None; BONES.len()],
            found: [false; BONES.len()],
        }
    }
}

/// A soldier body's skeleton with its movement clips.
#[derive(Clone, Debug)]
pub struct Rig {
    skeleton: Skeleton,
    legs: ClipSet,
}

/// Bone transforms in the body's frame (feet at the origin, facing -Z).
pub type Pose = [(Quat, Vec3); BONES.len()];

impl Rig {
    /// The body `.glb` (`SoldierDesc::mesh`).
    pub fn load(path: &Path) -> anyhow::Result<Self> {
        let (legs, skeleton) = ClipSet::load(path, |name| clips::legs().any(|c| c == name))?;
        if let Some(missing) = BONES.iter().zip(skeleton.found).find(|(_, found)| !found) {
            anyhow::bail!("{}: no bone {}", path.display(), missing.0);
        }
        Ok(Self { skeleton, legs })
    }

    pub fn legs_duration(&self, name: &str) -> Option<f32> {
        self.legs.duration(name)
    }

    /// The bones posed by `state` with the weapon's upper-body clips.
    pub fn pose(&self, state: &AnimState, upper: &ClipSet) -> Pose {
        let clips = state.clips(|n| self.legs.duration(n), |n| upper.duration(n));
        self.pose_clips(&clips, upper)
    }

    /// The bones at the last of a soldier's recent states (oldest first, each with the
    /// seconds since the one before), crossfaded like the client does.
    pub fn pose_recent(&self, states: &[(AnimState, f32)], upper: &ClipSet) -> Pose {
        let mut blend = Blend::default();
        for (state, dt) in states {
            blend.step(state, *dt, |n| self.legs.duration(n), |n| upper.duration(n));
        }
        self.pose_clips(&blend.clips(), upper)
    }

    /// The bones posed by clips (upper body or not, name, weight, time), blended like Bevy:
    /// every clip animating a bone counts by its weight, normalized.
    pub fn pose_clips(&self, clips: &[(bool, &str, f32, f32)], upper: &ClipSet) -> Pose {
        let found: Vec<(&Clip, f32, f32)> = clips
            .iter()
            .filter_map(|&(is_upper, name, weight, time)| {
                let set = if is_upper { upper } else { &self.legs };
                set.clips.get(name).map(|clip| (clip, weight, time))
            })
            .collect();
        let mut pose: Pose = self.skeleton.rest;
        for (bone, local) in pose.iter_mut().enumerate() {
            let (mut rotation, mut rotation_weight) = (None::<Quat>, 0.0);
            let (mut translation, mut translation_weight) = (None::<Vec3>, 0.0);
            for &(clip, weight, time) in &found {
                let Some(track) = clip.tracks.get(bone).and_then(Option::as_ref) else {
                    continue;
                };
                if let Some(value) = track.rotation.as_ref().and_then(|keys| sample(keys, time, Quat::slerp)) {
                    rotation_weight += weight;
                    rotation = Some(match rotation {
                        Some(current) => current.slerp(value, weight / rotation_weight),
                        None => value,
                    });
                }
                if let Some(value) = track.translation.as_ref().and_then(|keys| sample(keys, time, Vec3::lerp)) {
                    translation_weight += weight;
                    translation = Some(match translation {
                        Some(current) => current.lerp(value, weight / translation_weight),
                        None => value,
                    });
                }
            }
            if let Some(rotation) = rotation {
                local.0 = rotation;
            }
            if let Some(translation) = translation {
                local.1 = translation;
            }
        }
        // To the body's frame, parents first.
        for bone in 0..BONES.len() {
            if let Some(parent) = self.skeleton.parents[bone] {
                let (pq, pt) = pose[parent];
                let (q, t) = pose[bone];
                pose[bone] = (pq * q, pt + pq * t);
            }
        }
        pose
    }
}

/// The upper-body set of weapons without one of their own (the client's too).
pub const DEFAULT_WEAPON_SET: &str = "objects/weapons/handheld/rurif_ak47/animations/3p.glb";

/// The idle loops' clock (seconds) at a moment on the server clock (seconds): the same on the
/// server and every client.
pub fn clock(server_seconds: f64) -> f32 {
    // Wrapped, to keep an f32's precision; the loops jump once an hour.
    server_seconds.rem_euclid(3600.0) as f32
}

/// Seconds into a reload that started on server tick `started`, at a moment on the server
/// clock (seconds); `None` if not reloading (yet: clients learn of a reload before they draw
/// its start).
pub fn reload_elapsed(reloading: bool, started: u32, server_seconds: f64) -> Option<f32> {
    let elapsed = server_seconds - started as f64 / crate::TICK_HZ;
    (reloading && elapsed >= 0.0).then_some(elapsed as f32)
}

/// The rigs of the current level's soldier bodies (by kit) and the upper-body sets of the
/// weapons they carry (by `.glb` path), for hit zones that follow the body. Bodies whose hit
/// zones were imported without bone offsets have none: theirs keep the stance's pose.
#[derive(Resource, Default, Clone)]
pub struct HitRigs {
    kits: HashMap<String, Arc<Rig>>,
    sets: HashMap<String, Option<Arc<ClipSet>>>,
}

impl HitRigs {
    pub fn rig(&self, kit: &str) -> Option<&Arc<Rig>> {
        self.kits.get(kit)
    }

    /// The upper-body set of a weapon.
    pub fn set(&self, weapon: Option<&game_data::WeaponDesc>) -> Option<&Arc<ClipSet>> {
        let path = weapon.and_then(|w| w.animations_3p.as_deref()).unwrap_or(DEFAULT_WEAPON_SET);
        self.sets.get(path)?.as_ref()
    }

    /// The bones of a soldier wearing `kit`, posed by `state` with the weapon in hand.
    pub fn pose(
        &self,
        kit: &str,
        loadout: &crate::weapons::Loadout,
        armory: &crate::weapons::Armory,
        state: &AnimState,
    ) -> Option<Pose> {
        let rig = self.rig(kit)?;
        let weapon = loadout.weapons.get(state.weapon as usize).and_then(|w| armory.weapon(w));
        let set = self.set(weapon.map(|w| &**w))?;
        Some(rig.pose(state, set))
    }

    /// The bones of a soldier wearing `kit` at the last of his recent states (oldest first,
    /// with the seconds since the one before; see [`Rig::pose_recent`]).
    pub fn pose_recent(
        &self,
        kit: &str,
        loadout: &crate::weapons::Loadout,
        armory: &crate::weapons::Armory,
        states: &[(AnimState, f32)],
    ) -> Option<Pose> {
        let rig = self.rig(kit)?;
        let (last, _) = states.last()?;
        let weapon = loadout.weapons.get(last.weapon as usize).and_then(|w| armory.weapon(w));
        let set = self.set(weapon.map(|w| &**w))?;
        Some(rig.pose_recent(states, set))
    }

    /// Loads the upper-body set of a weapon, if it isn't yet.
    fn ensure_set(&mut self, weapon: Option<&game_data::WeaponDesc>, paths: &crate::config::GamePaths) {
        let path = weapon.and_then(|w| w.animations_3p.as_deref()).unwrap_or(DEFAULT_WEAPON_SET);
        if !self.sets.contains_key(path) {
            let set = ClipSet::load(&paths.find(path), |_| true)
                .map(|(set, _)| Arc::new(set))
                .map_err(|err| warn!("hit zones: {err:#}"))
                .ok();
            self.sets.insert(path.to_string(), set);
        }
    }
}

pub struct SkeletonPlugin;

impl Plugin for SkeletonPlugin {
    fn build(&self, app: &mut App) {
        app.init_resource::<HitRigs>().add_systems(
            Update,
            (
                load_rigs.run_if(resource_exists_and_changed::<crate::level::LoadedLevel>),
                load_weapon_sets,
            )
                .chain()
                .after(crate::weapons::load_armory),
        );
    }
}

/// The rig of every kit's body, when a level loads.
fn load_rigs(
    level: Res<crate::level::LoadedLevel>,
    paths: Res<crate::config::GamePaths>,
    armory: Res<crate::weapons::Armory>,
    mut rigs: ResMut<HitRigs>,
) {
    let mut bodies: HashMap<String, Option<Arc<Rig>>> = HashMap::default();
    let mut kits = HashMap::default();
    for slot in level.desc.teams.iter().flat_map(|team| &team.kits) {
        let rig = bodies
            .entry(slot.soldier.clone())
            .or_insert_with(|| {
                let desc = paths
                    .read_ron::<game_data::SoldierDesc>(format!("soldiers/{}.ron", slot.soldier))
                    .ok()?;
                // Only bodies whose hit zones know where they sit on their bones.
                if desc.hit_zones.is_empty() || desc.hit_zones.iter().any(|z| z.length == 0.0) {
                    return None;
                }
                Rig::load(&paths.find(&desc.mesh))
                    .map(Arc::new)
                    .map_err(|err| warn!("hit zones: {err:#}"))
                    .ok()
            })
            .clone();
        if let Some(rig) = rig {
            kits.insert(slot.kit.clone(), rig);
        }
    }
    info!("hit zones: {} of {} bodies posed from their animations", bodies.values().flatten().count(), bodies.len());
    rigs.kits = kits;
    // Every weapon of every kit, so a soldier's first shots find its set loaded.
    let mut sets = std::mem::take(&mut rigs.sets);
    sets.retain(|_, set| set.is_some());
    rigs.sets = sets;
    for weapon in armory.weapons.values() {
        rigs.ensure_set(Some(weapon), &paths);
    }
    rigs.ensure_set(None, &paths);
}

/// The upper-body sets of weapons picked from the arsenal.
fn load_weapon_sets(
    loadouts: Query<&crate::weapons::Loadout, Changed<crate::weapons::Loadout>>,
    armory: Res<crate::weapons::Armory>,
    paths: Option<Res<crate::config::GamePaths>>,
    mut rigs: ResMut<HitRigs>,
) {
    let Some(paths) = paths else {
        return;
    };
    for loadout in &loadouts {
        for weapon in &loadout.weapons {
            if let Some(weapon) = armory.weapon(weapon) {
                let path = weapon.animations_3p.as_deref().unwrap_or(DEFAULT_WEAPON_SET);
                if !rigs.sets.contains_key(path) {
                    rigs.ensure_set(Some(weapon), &paths);
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn stride_advances_one_cycle_per_cycle_length() {
        // Running at the clip's speed for one clip length: one step cycle.
        let step = stride_step(Stance::Standing, Vec2::new(0.0, RUN_SPEED), 0.625);
        assert!((step - 1.0).abs() < 1e-3);
        assert_eq!(stride_step(Stance::Standing, Vec2::new(0.0, 0.1), 1.0), 0.0);
        assert_eq!(gait(Stance::Standing, Vec2::new(0.0, 6.0)), Gait::Sprint);
        assert_eq!(gait(Stance::Standing, Vec2::new(6.0, 0.0)), Gait::Move, "no sideways sprint");
        assert_eq!(gait(Stance::Crouching, Vec2::new(0.0, 1.0)), Gait::Move);
    }

    #[test]
    fn samples_between_keys_and_clamps_outside() {
        let keys = (vec![0.0, 1.0], vec![Vec3::ZERO, Vec3::X]);
        assert_eq!(sample(&keys, 0.25, Vec3::lerp), Some(Vec3::new(0.25, 0.0, 0.0)));
        assert_eq!(sample(&keys, -1.0, Vec3::lerp), Some(Vec3::ZERO));
        assert_eq!(sample(&keys, 5.0, Vec3::lerp), Some(Vec3::X));
        assert_eq!(loop_time(2.5, 1.0), 0.5);
        assert_eq!(cycle_time(1.25, 2.0), 0.5);
    }
}
