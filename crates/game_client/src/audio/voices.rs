//! Voices: what is playing, how loud, from where.
//!
//! BF2's distance model is OpenAL's clamped inverse distance: full volume within a sound's
//! `min_distance`, half at `half_distance`, inversely proportional beyond
//! ([`game_data::Falloff`]). Bevy's spatial audio (rodio's `Spatial` source) instead divides
//! by the squared distance (a rifle 20 m away would be at 1/400) and pans a source towards
//! the ear *farther* from it. So a positional voice is a spatial player placed on a small
//! sphere around the listener, mirrored left-right, where rodio only pans it; its volume is
//! set from the BF2 falloff every frame.
//!
//! At most [`MAX_VOICES`] play at once: a new sound quieter than every playing one is
//! dropped, otherwise the quietest one fades out for it. Rapid repeats from one emitter
//! (automatic fire) keep [`PER_EMITTER`] instances and fade out the oldest. Announcements
//! (the commander) are never dropped, and everything else ducks under them.

use std::{
    collections::HashMap,
    hash::{DefaultHasher, Hash, Hasher},
    sync::Arc,
};

use bevy::{
    audio::{AudioSinkPlayback, PlaybackMode, SpatialAudioSink, Volume},
    prelude::*,
};
use game_data::{EffectSound, Falloff, SoundDesc};
use game_shared::level::LevelEntity;

use super::{AudioSystems, Sounds};

pub struct VoicePlugin;

impl Plugin for VoicePlugin {
    fn build(&self, app: &mut App) {
        app.add_message::<PlaySound>()
            .init_resource::<AudioMix>()
            .init_resource::<SoundCache>()
            .init_resource::<VoiceStats>()
            .init_resource::<Ducking>()
            .init_resource::<Muffle>()
            .add_systems(
                PostUpdate,
                (start_sounds, update_voices, log_stats).chain().in_set(AudioSystems::Play),
            );
    }
}

pub const MAX_VOICES: usize = 40;
pub const PER_EMITTER: usize = 3;
/// Quieter sounds aren't started (-54 dB).
const AUDIBLE: f32 = 0.002;
/// Seconds to fade out a voice that makes room for another.
const FADE: f32 = 0.05;
/// Distance of a positional voice from the listener: rodio's own attenuation (`1 / d²`,
/// capped at 1) stays at 1 for both ears.
const PAN_RADIUS: f32 = 0.5;
/// Rodio pans between 0.5 and 1 per ear, 0.75 straight ahead; this brings that back to 1.
const PAN_GAIN: f32 = 4.0 / 3.0;
/// Everything but announcements drops to this while one plays, easing at `DUCK_RATE` per
/// second.
const DUCKED: f32 = 0.45;
const DUCK_RATE: f32 = 6.0;
/// For positional sounds that don't say how they fade.
pub const DEFAULT_FALLOFF: Falloff = Falloff {
    min_distance: 5.0,
    half_distance: 12.0,
};

/// Volume multipliers on top of the master volume (`Settings::master_volume`, applied as
/// Bevy's `GlobalVolume`), from the audio settings. Applied to what's playing, too.
#[derive(Resource, Clone, Copy, Debug)]
pub struct AudioMix {
    /// Weapons, footsteps, impacts, voices, explosions, vehicles.
    pub effects: f32,
    /// Level ambience, flags flapping.
    pub ambience: f32,
}

impl AudioMix {
    pub fn of(&self, channel: Channel) -> f32 {
        match channel {
            Channel::Effects => self.effects,
            Channel::Ambience => self.ambience,
            Channel::Announcement => 1.0,
        }
    }
}

/// Which volume setting a sound follows.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum Channel {
    #[default]
    Effects,
    Ambience,
    /// Voice-overs: only the master volume; never dropped; the rest ducks under them.
    Announcement,
}

/// How far everything but announcements is turned down right now (1 = not at all).
#[derive(Resource)]
pub struct Ducking(pub f32);

/// Turns everything but announcements down on top of the ducking (1 = not at all): ringing
/// ears after a blast or flashbang. Set by `gadgets` every frame; eased like the ducking.
#[derive(Resource)]
pub struct Muffle(pub f32);

impl Default for Muffle {
    fn default() -> Self {
        Self(1.0)
    }
}

impl Default for Ducking {
    fn default() -> Self {
        Self(1.0)
    }
}

impl Default for AudioMix {
    fn default() -> Self {
        Self {
            effects: 1.0,
            ambience: 1.0,
        }
    }
}

/// What to play.
#[derive(Clone, Debug)]
pub enum Sound {
    Desc(Arc<SoundDesc>),
    /// A sound of the library (`sounds.ron`), e.g. `s_impact_concrete`.
    Named(String),
    /// A generic explosion from the library: grenade sized below a 10 m blast radius,
    /// large above.
    #[allow(dead_code)]
    Explosion { radius: f32 },
}

impl From<&SoundDesc> for Sound {
    fn from(desc: &SoundDesc) -> Self {
        Sound::Desc(Arc::new(desc.clone()))
    }
}

impl From<SoundDesc> for Sound {
    fn from(desc: SoundDesc) -> Self {
        Sound::Desc(Arc::new(desc))
    }
}

impl From<Arc<SoundDesc>> for Sound {
    fn from(desc: Arc<SoundDesc>) -> Self {
        Sound::Desc(desc)
    }
}

/// Effect sounds don't say how they fade: played somewhere, they get [`DEFAULT_FALLOFF`].
impl From<&EffectSound> for Sound {
    fn from(sound: &EffectSound) -> Self {
        Sound::Desc(Arc::new(SoundDesc {
            files: sound.files.clone(),
            volume: sound.volume,
            ..SoundDesc::file("")
        }))
    }
}

/// Plays a sound once (or looping, if it loops), if it is audible.
///
/// ```ignore
/// sounds.write(PlaySound::at(Sound::Explosion { radius: 8.0 }, position));
/// if let Some(reload) = &weapon.sounds.reload_3p {
///     sounds.write(PlaySound::at(reload, position).emitter(soldier));
/// }
/// ```
#[derive(Message, Clone, Debug)]
pub struct PlaySound {
    pub sound: Sound,
    /// Where it plays. `None`: not positional (our own soldier, the interface).
    pub at: Option<Vec3>,
    /// Multiplies the sound's own volume.
    pub volume: f32,
    /// What makes it (a soldier): repeats of one sound from one emitter cut the oldest.
    pub emitter: Option<Entity>,
    pub channel: Channel,
    /// For the debug log.
    pub reason: &'static str,
}

impl PlaySound {
    /// A sound where it happens, fading with the distance to the listener.
    pub fn at(sound: impl Into<Sound>, position: Vec3) -> Self {
        Self {
            sound: sound.into(),
            at: Some(position),
            volume: 1.0,
            emitter: None,
            channel: Channel::Effects,
            reason: "",
        }
    }

    /// A sound of our own soldier or the interface, the same everywhere.
    pub fn local(sound: impl Into<Sound>) -> Self {
        Self {
            at: None,
            ..Self::at(sound, Vec3::ZERO)
        }
    }

    pub fn volume(mut self, volume: f32) -> Self {
        self.volume = volume;
        self
    }

    pub fn emitter(mut self, emitter: Entity) -> Self {
        self.emitter = Some(emitter);
        self
    }

    pub fn reason(mut self, reason: &'static str) -> Self {
        self.reason = reason;
        self
    }

    /// A voice-over (see [`Channel::Announcement`]).
    pub fn announcement(mut self) -> Self {
        self.channel = Channel::Announcement;
        self
    }
}

/// Loaded sound files, kept loaded.
#[derive(Resource, Default)]
pub struct SoundCache(HashMap<String, Handle<AudioSource>>);

impl SoundCache {
    pub fn get(&mut self, assets: &AssetServer, file: &str) -> Handle<AudioSource> {
        self.0
            .entry(file.to_string())
            .or_insert_with(|| assets.load(format!("imported://{file}")))
            .clone()
    }

    /// Starts loading every file of `desc`, so its first play isn't late.
    pub fn preload(&mut self, assets: &AssetServer, desc: &SoundDesc) {
        for file in &desc.files {
            self.get(assets, file);
        }
    }
}

#[derive(Component)]
struct Voice {
    file: String,
    /// Identifies the sound (not the file picked) for the per-emitter limit.
    sound: u64,
    /// Volume before distance, mix and ducking.
    level: f32,
    /// Positional voices.
    at: Option<(Vec3, Falloff)>,
    emitter: Option<Entity>,
    channel: Channel,
    started: f32,
    /// Level after distance, last frame.
    gain: f32,
    /// Seconds of fading out left.
    fade: Option<f32>,
}

/// A looping voice another audio module steers (vehicle engines, sound emitters): it sets
/// where the sound is, how loud and how fast every frame. Held voices aren't counted against
/// [`MAX_VOICES`] and never make room for others.
#[derive(Component)]
pub struct HeldVoice {
    /// `None`: not positional.
    pub at: Option<Vec3>,
    /// Volume before distance and mix.
    pub level: f32,
    /// Playback speed (pitch).
    pub speed: f32,
    pub channel: Channel,
}

/// Starts a held voice playing `desc` (looping) from `listener`'s point of view.
pub fn spawn_held(
    commands: &mut Commands,
    cache: &mut SoundCache,
    assets: &AssetServer,
    listener: Option<(Entity, &Transform)>,
    desc: &SoundDesc,
    held: HeldVoice,
) -> Entity {
    let file = &desc.files[fastrand::usize(..desc.files.len().max(1))];
    let at = held.at.map(|at| (at, desc.falloff.unwrap_or(DEFAULT_FALLOFF)));
    let positional = at.is_some() && listener.is_some();
    let settings = PlaybackSettings {
        mode: PlaybackMode::Loop,
        volume: Volume::Linear(0.0),
        speed: held.speed.max(0.05),
        spatial: positional,
        ..PlaybackSettings::LOOP
    };
    let voice = Voice {
        file: file.clone(),
        sound: sound_key(desc),
        level: held.level,
        at,
        emitter: None,
        channel: held.channel,
        started: 0.0,
        gain: 0.0,
        fade: None,
    };
    let mut entity = commands.spawn((AudioPlayer::new(cache.get(assets, file)), settings, voice, held, LevelEntity));
    if let (Some((at, _)), Some((listener, transform))) = (at, listener) {
        entity.insert((ChildOf(listener), Transform::from_translation(pan_offset(transform, at))));
    }
    debug!(target: "audio", "loop {file} starts");
    entity.id()
}

#[derive(Resource, Default)]
struct VoiceStats {
    started: u32,
    culled: u32,
    stolen: u32,
    dropped: u32,
    last_log: f32,
}

type Listener<'a> = (Entity, &'a Transform);

fn uniform([low, high]: [f32; 2]) -> f32 {
    low + (high - low) * fastrand::f32()
}

fn sound_key(desc: &SoundDesc) -> u64 {
    let mut hasher = DefaultHasher::new();
    desc.files.hash(&mut hasher);
    hasher.finish()
}

/// Where a positional voice goes, relative to the listener: towards `at` on a sphere of
/// [`PAN_RADIUS`], mirrored left-right for rodio's panning.
fn pan_offset(listener: &Transform, at: Vec3) -> Vec3 {
    let local = listener.rotation.inverse() * (at - listener.translation);
    let direction = local.normalize_or(Vec3::NEG_Z);
    Vec3::new(-direction.x, direction.y, direction.z) * PAN_RADIUS
}

#[allow(clippy::too_many_arguments)]
fn start_sounds(
    mut commands: Commands,
    time: Res<Time<Real>>,
    mut requests: MessageReader<PlaySound>,
    library: Option<Res<Sounds>>,
    mut cache: ResMut<SoundCache>,
    asset_server: Res<AssetServer>,
    mix: Res<AudioMix>,
    ducking: Res<Ducking>,
    mut stats: ResMut<VoiceStats>,
    listener: Query<(Entity, &Transform), With<SpatialListener>>,
    mut voices: Query<(Entity, &mut Voice), Without<HeldVoice>>,
) {
    let listener: Option<Listener> = listener.iter().next();
    let now = time.elapsed_secs();
    // Announcements neither count nor make room.
    let stealable = |v: &Voice| v.fade.is_none() && v.channel != Channel::Announcement;
    let mut playing: usize = voices.iter().filter(|(_, v)| stealable(v)).count();
    for request in requests.read() {
        let desc = match &request.sound {
            Sound::Desc(desc) => Some(desc.as_ref()),
            Sound::Named(name) => library.as_ref().and_then(|l| l.0.get(name)),
            Sound::Explosion { radius } => library.as_ref().and_then(|l| {
                let explosions = &l.0.explosions;
                let name = if *radius < 10.0 { &explosions.small } else { &explosions.large };
                name.as_ref().or(explosions.small.as_ref()).and_then(|n| l.0.get(n))
            }),
        };
        let Some(desc) = desc.filter(|d| !d.files.is_empty()) else {
            continue;
        };
        let at = match (request.at, listener) {
            (Some(at), Some(_)) => Some((at, desc.falloff.unwrap_or(DEFAULT_FALLOFF))),
            // Nobody to hear it.
            (Some(_), None) => continue,
            (None, _) => None,
        };
        let announcement = request.channel == Channel::Announcement;
        let level = desc.volume * uniform(desc.volume_range) * request.volume;
        let distance = at.zip(listener).map(|((at, _), (_, l))| at.distance(l.translation));
        let duck = if announcement { 1.0 } else { ducking.0 };
        let gain = level
            * mix.of(request.channel)
            * duck
            * at.zip(distance).map_or(1.0, |((_, falloff), d)| falloff.gain(d));
        let file = &desc.files[fastrand::usize(..desc.files.len())];
        let distance_text = distance.map_or(String::new(), |d| format!(" at {d:.1} m"));
        if gain < AUDIBLE && !announcement {
            stats.culled += 1;
            debug!(target: "audio", "culled {file}{distance_text}: gain {gain:.4} ({})", request.reason);
            continue;
        }

        // One emitter repeating a sound keeps its last few.
        let key = sound_key(desc);
        if let Some(emitter) = request.emitter {
            let mut same: Vec<(f32, Entity)> = voices
                .iter()
                .filter(|(_, v)| v.fade.is_none() && v.emitter == Some(emitter) && v.sound == key)
                .map(|(e, v)| (v.started, e))
                .collect();
            if same.len() >= PER_EMITTER {
                same.sort_by(|a, b| a.0.total_cmp(&b.0));
                if let Ok((_, mut oldest)) = voices.get_mut(same[0].1) {
                    oldest.fade = Some(FADE);
                    playing -= 1;
                }
            }
        }
        if playing >= MAX_VOICES && !announcement {
            let quietest = voices
                .iter()
                .filter(|(_, v)| stealable(v))
                .min_by(|a, b| a.1.gain.total_cmp(&b.1.gain))
                .map(|(e, v)| (e, v.gain));
            match quietest {
                Some((entity, quiet)) if quiet < gain => {
                    if let Ok((_, mut voice)) = voices.get_mut(entity) {
                        voice.fade = Some(FADE);
                    }
                    stats.stolen += 1;
                    playing -= 1;
                }
                _ => {
                    stats.dropped += 1;
                    debug!(target: "audio", "dropped {file}{distance_text}: gain {gain:.3}, all voices louder ({})", request.reason);
                    continue;
                }
            }
        }

        let positional = at.is_some();
        let settings = PlaybackSettings {
            mode: if desc.looping { PlaybackMode::Loop } else { PlaybackMode::Despawn },
            volume: Volume::Linear(gain * if positional { PAN_GAIN } else { 1.0 }),
            speed: uniform(desc.pitch).max(0.05),
            spatial: positional,
            ..PlaybackSettings::DESPAWN
        };
        let mut voice = commands.spawn((
            AudioPlayer::new(cache.get(&asset_server, file)),
            settings,
            Voice {
                file: file.clone(),
                sound: key,
                level,
                at,
                emitter: request.emitter,
                channel: request.channel,
                started: now,
                gain,
                fade: None,
            },
            LevelEntity,
        ));
        if let (Some((at, _)), Some((listener, transform))) = (at, listener) {
            voice.insert((ChildOf(listener), Transform::from_translation(pan_offset(transform, at))));
        }
        playing += usize::from(!announcement);
        stats.started += 1;
        debug!(target: "audio", "play {file}{distance_text}: gain {gain:.3} ({})", request.reason);
    }
}

/// Distance, direction, mix, ducking and fading of the playing voices.
#[allow(clippy::too_many_arguments)]
fn update_voices(
    mut commands: Commands,
    time: Res<Time<Real>>,
    global: Res<GlobalVolume>,
    mix: Res<AudioMix>,
    mut ducking: ResMut<Ducking>,
    muffle: Res<Muffle>,
    mut was_announcing: Local<bool>,
    listener: Query<&Transform, (With<SpatialListener>, Without<Voice>)>,
    mut voices: Query<(
        Entity,
        &mut Voice,
        Option<&HeldVoice>,
        Option<&mut Transform>,
        Option<&mut AudioSink>,
        Option<&mut SpatialAudioSink>,
    )>,
) {
    let dt = time.delta_secs();
    let listener = listener.iter().next();
    let master = global.volume.to_linear();
    let announcing = voices.iter().any(|(_, v, ..)| v.channel == Channel::Announcement && v.fade.is_none());
    let target = if announcing { DUCKED } else { 1.0 } * muffle.0;
    if announcing != *was_announcing {
        *was_announcing = announcing;
        debug!(target: "audio", "{}", if announcing { "ducking for an announcement" } else { "announcement over" });
    }
    ducking.0 += (target - ducking.0) * (1.0 - (-DUCK_RATE * dt).exp());
    for (entity, mut voice, held, transform, sink, spatial_sink) in &mut voices {
        let mut fade = 1.0;
        if let Some(left) = &mut voice.fade {
            *left -= dt;
            if *left <= 0.0 {
                commands.entity(entity).despawn();
                continue;
            }
            fade = *left / FADE;
        }
        if let Some(held) = held {
            voice.level = held.level;
            if let (Some(at), Some((_, falloff))) = (held.at, voice.at) {
                voice.at = Some((at, falloff));
            }
        }
        let mut gain = voice.level;
        if let (Some((at, falloff)), Some(listener)) = (voice.at, listener) {
            gain *= falloff.gain(at.distance(listener.translation));
            if let Some(mut transform) = transform {
                transform.translation = pan_offset(listener, at);
            }
        }
        gain *= mix.of(voice.channel);
        if voice.channel != Channel::Announcement {
            gain *= ducking.0;
        }
        voice.gain = gain;
        let volume = Volume::Linear(gain * fade * master);
        let speed = held.map(|h| h.speed.max(0.05));
        if let Some(mut sink) = sink {
            sink.set_volume(volume);
            if let Some(speed) = speed {
                sink.set_speed(speed);
            }
        } else if let Some(mut sink) = spatial_sink {
            sink.set_volume(volume * Volume::Linear(PAN_GAIN));
            if let Some(speed) = speed {
                sink.set_speed(speed);
            }
        }
    }
}

fn log_stats(time: Res<Time<Real>>, mut stats: ResMut<VoiceStats>, voices: Query<&Voice>) {
    let now = time.elapsed_secs();
    if now - stats.last_log < 5.0 {
        return;
    }
    if stats.started + stats.culled + stats.dropped > 0 {
        let loudest = voices
            .iter()
            .max_by(|a, b| a.gain.total_cmp(&b.gain))
            .map_or(String::new(), |v| format!(" (loudest {} at {:.2})", v.file, v.gain));
        debug!(
            target: "audio",
            "{} voices{loudest}; last 5 s: {} started, {} culled, {} stolen, {} dropped",
            voices.iter().count(),
            stats.started,
            stats.culled,
            stats.stolen,
            stats.dropped
        );
    }
    *stats = VoiceStats {
        last_log: now,
        ..default()
    };
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pans_mirrored_and_close() {
        let listener = Transform::from_xyz(10.0, 2.0, 5.0).looking_to(Vec3::NEG_Z, Vec3::Y);
        // A sound to the listener's right goes to the left of the sphere (rodio pans it right).
        let offset = pan_offset(&listener, Vec3::new(40.0, 2.0, 5.0));
        assert!((offset - Vec3::new(-PAN_RADIUS, 0.0, 0.0)).length() < 1e-4);
        let ahead = pan_offset(&listener, Vec3::new(10.0, 2.0, -100.0));
        assert!((ahead - Vec3::new(0.0, 0.0, -PAN_RADIUS)).length() < 1e-4);
        // Both ears (0.25 m apart) stay within 1 m of the voice: no rodio attenuation.
        assert!(offset.distance(Vec3::X * 0.125) <= 1.0 && offset.distance(Vec3::X * -0.125) <= 1.0);
    }
}
