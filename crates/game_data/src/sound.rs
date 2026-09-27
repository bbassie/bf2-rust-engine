//! Sounds.
//!
//! - [`SoundDesc`]: one sound (a set of interchangeable `.wav` files) and how it plays.
//! - `sounds.ron` ([`SoundLibrary`]): sounds shared by every level, by name, and the tables
//!   that pick them: footsteps and bullet impacts by surface material, near misses by
//!   projectile material, soldier voices, explosions.
//! - `levels/<name>/sounds.ron` ([`LevelSounds`]): ambience.
//! - [`VehicleSounds`]: engines, part of a vehicle's description.
//!
//! Surface and projectile materials are the ids of the damage table (`materials.ron`).

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

/// A sound: each time it plays, one of `files` picked at random.
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq)]
#[serde(from = "SoundRepr")]
pub struct SoundDesc {
    /// `.wav` paths relative to the imported root.
    pub files: Vec<String>,
    /// Linear gain (1 = as recorded).
    pub volume: f32,
    /// Each time it plays, volume and pitch (playback speed) are multiplied by random
    /// factors from these ranges.
    #[serde(skip_serializing_if = "is_unit")]
    pub volume_range: [f32; 2],
    #[serde(skip_serializing_if = "is_unit")]
    pub pitch: [f32; 2],
    /// Positional sounds fade with the distance to where they play. `None`: heard at the same
    /// volume everywhere (first person, interface, ambience beds).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub falloff: Option<Falloff>,
    /// Loops until stopped.
    #[serde(skip_serializing_if = "is_false")]
    pub looping: bool,
}

impl SoundDesc {
    /// A single file at full volume, not positional.
    pub fn file(path: impl Into<String>) -> Self {
        Self {
            files: vec![path.into()],
            volume: 1.0,
            volume_range: [1.0; 2],
            pitch: [1.0; 2],
            falloff: None,
            looping: false,
        }
    }
}

/// How a sound fades with distance (BF2's `minDistance`/`halfVolumeDistance`): full volume
/// within `min_distance`, half at `half_distance`, and inversely proportional to the distance
/// beyond (OpenAL's clamped inverse distance model, with the rolloff that puts the half-volume
/// point at `half_distance`).
#[derive(Serialize, Deserialize, Debug, Clone, Copy, PartialEq)]
pub struct Falloff {
    pub min_distance: f32,
    pub half_distance: f32,
}

impl Falloff {
    fn rolloff(&self) -> f32 {
        let min = self.min_distance.max(0.01);
        min / (self.half_distance - min).max(0.01)
    }

    /// Gain (0..1) at `distance` meters.
    pub fn gain(&self, distance: f32) -> f32 {
        let min = self.min_distance.max(0.01);
        if distance <= min {
            return 1.0;
        }
        min / (min + self.rolloff() * (distance - min))
    }

    /// Distance at which the gain has dropped to `gain`.
    pub fn distance_for(&self, gain: f32) -> f32 {
        let min = self.min_distance.max(0.01);
        if gain >= 1.0 {
            return min;
        }
        min + (min / gain.max(1e-6) - min) / self.rolloff()
    }
}

/// Everything shared by the levels: `sounds.ron`.
#[derive(Serialize, Deserialize, Debug, Clone, Default)]
pub struct SoundLibrary {
    /// Sounds by name (lowercase, e.g. `s_impact_concrete`).
    #[serde(default)]
    pub sounds: BTreeMap<String, SoundDesc>,
    /// What soldiers sound like moving on a surface material.
    #[serde(default)]
    pub footsteps: BTreeMap<u32, FootstepSounds>,
    /// Bullet impacts: projectile material → surface material → sounds.
    #[serde(default)]
    pub impacts: BTreeMap<u32, BTreeMap<u32, ImpactSounds>>,
    /// Bullets passing close by, by projectile material.
    #[serde(default)]
    pub flybys: BTreeMap<u32, String>,
    #[serde(default)]
    pub soldier: SoldierSounds,
    #[serde(default)]
    pub explosions: ExplosionSounds,
}

impl SoundLibrary {
    pub fn get(&self, name: &str) -> Option<&SoundDesc> {
        self.sounds.get(name)
    }
}

/// Sound names for moving over one surface. Missing entries are silent.
#[derive(Serialize, Deserialize, Debug, Clone, Default, PartialEq)]
pub struct FootstepSounds {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub walk: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub run: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sprint: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub prone: Option<String>,
}

/// Sound names for a projectile hitting a surface.
#[derive(Serialize, Deserialize, Debug, Clone, Default, PartialEq)]
pub struct ImpactSounds {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub impact: Option<String>,
    /// Glancing hits.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ricochet: Option<String>,
}

/// Sound names of a soldier's body and voice.
#[derive(Serialize, Deserialize, Debug, Clone, Default, PartialEq)]
pub struct SoldierSounds {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub death: Option<String>,
    /// Our own soldier getting hurt (not positional).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub injury: Option<String>,
    /// Out of breath after sprinting (not positional).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sprint_breath: Option<String>,
    /// Climbing a ladder, per rung.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ladder: Option<String>,
    /// A body hitting the ground.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub body_fall: Option<String>,
}

/// Generic explosion sound names, for explosions without a sound of their own.
#[derive(Serialize, Deserialize, Debug, Clone, Default, PartialEq)]
pub struct ExplosionSounds {
    /// Hand grenade sized.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub small: Option<String>,
    /// C4, rockets, vehicles.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub large: Option<String>,
}

/// A level's sounds: `levels/<name>/sounds.ron`. What its surfaces are made of is in
/// `surfaces.ron` ([`crate::SurfaceMap`]).
#[derive(Serialize, Deserialize, Debug, Clone, Default)]
pub struct LevelSounds {
    /// Looping background sounds.
    #[serde(default)]
    pub ambience: Vec<AmbientSound>,
}

/// A looping sound heard around a point (or everywhere), not positional itself: it fades in
/// as the listener approaches `position`.
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq)]
pub struct AmbientSound {
    pub sound: SoundDesc,
    /// `None`: everywhere.
    #[serde(default)]
    pub position: Option<[f32; 3]>,
    /// Heard within this many meters of `position`, at full volume within half of it.
    #[serde(default)]
    pub radius: f32,
}

/// A team language's radio voice (`radio/<language>.ron`): BF2's radio messages (the commo
/// rose, spotting, soldiers' automatic call-outs) by message id, e.g. `medic`.
#[derive(Serialize, Deserialize, Debug, Clone, Default)]
pub struct RadioVoice {
    #[serde(default)]
    pub messages: BTreeMap<String, RadioLine>,
}

/// One radio message: what it says and the recordings, per speaker rank.
#[derive(Serialize, Deserialize, Debug, Clone, Default, PartialEq)]
pub struct RadioLine {
    /// What the chat shows (localized). Empty for lines that are only shouted.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub text: String,
    /// Over the radio (a filtered recording), for a grunt, a squad leader and the commander.
    /// One is picked at random. Empty: never over the radio.
    #[serde(default)]
    pub radio: [Vec<String>; 3],
    /// Heard in person near the speaker, by either team (unfiltered, positional).
    #[serde(default)]
    pub local: [Vec<String>; 3],
}

/// Speaker ranks of [`RadioLine`]'s recordings.
#[derive(Serialize, Deserialize, Debug, Clone, Copy, PartialEq, Eq)]
pub enum RadioRank {
    Grunt,
    SquadLeader,
    Commander,
}

impl RadioLine {
    /// The recordings for `rank`, or a lower rank's when it has none.
    pub fn files(&self, rank: RadioRank, local: bool) -> &[String] {
        let sets = if local { &self.local } else { &self.radio };
        let rank = rank as usize;
        (0..=rank).rev().map(|r| &sets[r]).find(|s| !s.is_empty()).map_or(&[], |s| s)
    }
}

/// What a vehicle's engine sounds like (in `vehicles/<name>.ron`).
#[derive(Serialize, Deserialize, Debug, Clone, Default)]
pub struct VehicleSounds {
    /// Loops that play while someone drives, their pitch and volume following the revs.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub engine: Vec<EngineSound>,
    /// The engine starting when a driver gets in, and stopping when the driver leaves.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub start: Option<SoundDesc>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub stop: Option<SoundDesc>,
    /// Changing up a gear. The revs climb through `gears` gears up to the top speed.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub gear_shift: Option<SoundDesc>,
    #[serde(default = "default_gears")]
    pub gears: u32,
    /// Heard inside, by whoever sits in the vehicle (not positional).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub interior: Option<SoundDesc>,
}

/// An engine loop.
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq)]
pub struct EngineSound {
    pub sound: SoundDesc,
    /// Multipliers on the sound's pitch and volume by the revs (0 idling to 1 at the top of
    /// a gear): points `[revs, value]`, linear in between. Empty: 1.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub pitch: Vec<[f32; 2]>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub volume: Vec<[f32; 2]>,
    /// `Some(true)`: heard while the engine pulls, `Some(false)`: while it doesn't (coasting,
    /// braking), `None`: always.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub load: Option<bool>,
}

/// The value of a curve of `[x, value]` points at `x`: linear between points, flat beyond
/// the ends. `default` without points.
pub fn curve_at(points: &[[f32; 2]], x: f32, default: f32) -> f32 {
    let (Some(first), Some(last)) = (points.first(), points.last()) else {
        return default;
    };
    if x <= first[0] {
        return first[1];
    }
    for pair in points.windows(2) {
        let ([x0, y0], [x1, y1]) = (pair[0], pair[1]);
        if x <= x1 {
            let t = if x1 > x0 { (x - x0) / (x1 - x0) } else { 1.0 };
            return y0 + (y1 - y0) * t;
        }
    }
    last[1]
}

fn default_gears() -> u32 {
    4
}

/// Also accepts a bare path, which older imports wrote for weapon sounds.
#[derive(Deserialize)]
#[serde(untagged)]
enum SoundRepr {
    File(String),
    Full(FullSound),
}

#[derive(Deserialize)]
struct FullSound {
    files: Vec<String>,
    #[serde(default = "one")]
    volume: f32,
    #[serde(default = "unit")]
    volume_range: [f32; 2],
    #[serde(default = "unit")]
    pitch: [f32; 2],
    #[serde(default)]
    falloff: Option<Falloff>,
    #[serde(default)]
    looping: bool,
}

impl From<SoundRepr> for SoundDesc {
    fn from(repr: SoundRepr) -> Self {
        match repr {
            SoundRepr::File(path) => SoundDesc::file(path),
            SoundRepr::Full(s) => SoundDesc {
                files: s.files,
                volume: s.volume,
                volume_range: s.volume_range,
                pitch: s.pitch,
                falloff: s.falloff,
                looping: s.looping,
            },
        }
    }
}

fn one() -> f32 {
    1.0
}

fn unit() -> [f32; 2] {
    [1.0; 2]
}

fn is_unit(range: &[f32; 2]) -> bool {
    *range == unit()
}

fn is_false(value: &bool) -> bool {
    !value
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn falloff_halves_at_half_distance() {
        let falloff = Falloff {
            min_distance: 2.0,
            half_distance: 5.6,
        };
        assert_eq!(falloff.gain(1.0), 1.0);
        assert!((falloff.gain(5.6) - 0.5).abs() < 1e-5);
        assert!(falloff.gain(100.0) < 0.05);
        assert!((falloff.distance_for(0.5) - 5.6).abs() < 1e-3);
        assert!((falloff.gain(falloff.distance_for(0.01)) - 0.01).abs() < 1e-5);
    }

    #[test]
    fn interpolates_curves() {
        let points = [[0.0, 0.5], [0.5, 1.0], [1.0, 1.5]];
        assert_eq!(curve_at(&points, -1.0, 9.0), 0.5);
        assert!((curve_at(&points, 0.25, 9.0) - 0.75).abs() < 1e-6);
        assert!((curve_at(&points, 0.75, 9.0) - 1.25).abs() < 1e-6);
        assert_eq!(curve_at(&points, 2.0, 9.0), 1.5);
        assert_eq!(curve_at(&[], 0.3, 9.0), 9.0);
    }

    #[test]
    fn reads_full_and_bare_sounds() {
        let full = SoundDesc {
            files: vec!["a.wav".into(), "b.wav".into()],
            volume: 0.7,
            volume_range: [0.9, 1.0],
            pitch: [0.95, 1.05],
            falloff: Some(Falloff {
                min_distance: 2.0,
                half_distance: 4.0,
            }),
            looping: false,
        };
        let text = ron::to_string(&full).unwrap();
        assert_eq!(ron::from_str::<SoundDesc>(&text).unwrap(), full);
        let bare: SoundDesc = ron::from_str("\"x.wav\"").unwrap();
        assert_eq!(bare, SoundDesc::file("x.wav"));
        let short: SoundDesc = ron::from_str("(files: [\"y.wav\"], volume: 0.5)").unwrap();
        assert_eq!(short.pitch, [1.0; 2]);
        assert_eq!(short.falloff, None);
        let optional: Option<SoundDesc> = ron::from_str("Some(\"z.wav\")").unwrap();
        assert_eq!(optional, Some(SoundDesc::file("z.wav")));
    }
}
