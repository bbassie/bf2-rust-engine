//! Sounds: BF2 `Sound` templates as [`SoundDesc`]s, the shared sound library (`sounds.ron`)
//! and each level's ambience (`levels/<name>/sounds.ron`).
//!
//! A `Sound` template names one or more files (`soundFilename "a.wav,b.wav"`, one picked at
//! random), `volume`, `pitch`, random variation in the last two values of `pitchEnvelope` and
//! `volumeEnvelope` (`0/1/0.9/1.1/1/1/0/0.98/1.04/`), `is3dSound`, `loopCount` (0 loops) and
//! for 3D sounds `minDistance` (full volume within) and `halfVolumeDistance`.
//! `common/sound/audiotweak.con` scales them per template: `sound.tweakTemplate <name>
//! <volume> <pitch> <minDistance> <halfVolumeDistance> 1 1` [inferred: engines get 1.2/1.45
//! on the distances, footsteps 1.4 on volume].
//!
//! Which sound plays where comes from the material manager
//! (`common/material/materialmanagersettings.con`): `createCell <a> <b>` then
//! `setSoundTemplate <index> <sound>`, with the sounds in `common/material/impactsounds.con`.
//! Soldiers' feet are material 5500 (`Soldier_FootSteps`): index 0 walk, 1 run, 2 prone,
//! 3 sprint, 4 a body falling (on the default material), and `5500 6600` a ladder. A
//! projectile material against a surface: 0 impact, 1 ricochet; against 5501
//! (`Bullet_Flyby`): the near miss. Deaths are `S_<soldier>_Death` in the soldiers'
//! `.tweak`s, being hurt and sprint breathing `Sound.addSound` entries in
//! `common/sound/gamesounds.con`, generic explosions the sounds of the `e_exp_grenade` and
//! `e_exp_large` effects.
//!
//! Level ambience: `AmbientObjects.con` creates 2D `Sound` templates with a `position` and a
//! `minDistance` radius (300000 for the level-wide bed) and registers them with
//! `Sound.addTrigger`.

use std::{
    collections::{BTreeMap, BTreeSet, HashMap},
    path::Path,
    sync::atomic::{AtomicBool, Ordering},
};

use anyhow::{Context, Result};
use bf2_formats::{
    Vfs,
    con::{Interpreter, World, lex},
    vfs::normalize,
};
use game_data::{
    AmbientSound, EngineSound, Falloff, FootstepSounds, ImpactSounds, LevelSounds, SoundDesc,
    SoundLibrary, VehicleSounds,
};

use crate::{audio, coords};

const FOOTSTEPS: u32 = 5500;
const FLYBY: u32 = 5501;
const LADDER: u32 = 6600;
/// `Material.type` of projectiles.
const PROJECTILE_TYPE: u32 = 2;
/// Ambient sounds with a larger radius are heard everywhere.
const LEVEL_WIDE: f32 = 10_000.0;

/// Converts sound templates and the files they name.
pub struct SoundConverter<'a> {
    vfs: &'a Vfs,
    out: &'a Path,
    /// `sound.tweakTemplate` factors (volume, pitch, min distance, half distance) by
    /// lowercase template name.
    tweaks: HashMap<String, [f32; 4]>,
}

impl<'a> SoundConverter<'a> {
    pub fn new(vfs: &'a Vfs, out: &'a Path) -> Self {
        let mut tweaks = HashMap::new();
        for line in vfs.read_text("common/sound/audiotweak.con").unwrap_or_default().lines() {
            let mut words = line.split_whitespace();
            if !words.next().is_some_and(|w| w.eq_ignore_ascii_case("sound.tweaktemplate")) {
                continue;
            }
            let Some(name) = words.next() else { continue };
            let values: Vec<f32> = words.filter_map(|w| w.parse().ok()).collect();
            if let [volume, pitch, min, half, ..] = values[..] {
                tweaks.insert(name.to_ascii_lowercase(), [volume, pitch, min, half]);
            }
        }
        Self { vfs, out, tweaks }
    }

    /// The `Sound` template `name`.
    pub fn template(&self, world: &World, name: &str) -> Option<SoundDesc> {
        self.desc(name, &world.template(name)?.props)
    }

    /// A sound from its template's settings (lowercase methods, as in `Template::props`).
    pub fn desc(&self, name: &str, props: &[(String, Vec<String>)]) -> Option<SoundDesc> {
        let get = |method: &str| {
            props.iter().rev().find(|(m, _)| m == method).and_then(|(_, args)| args.first()).map(String::as_str)
        };
        let number = |method: &str| get(method).and_then(|v| v.parse::<f32>().ok());
        let files: Vec<String> = get("soundfilename")?
            .trim_matches('"')
            .split(',')
            .map(str::trim)
            .filter(|f| !f.is_empty())
            .filter_map(|file| audio::sound(self.vfs, file, self.out))
            .collect();
        if files.is_empty() {
            log::debug!("sound {name}: no files");
            return None;
        }
        let [volume_tweak, pitch_tweak, min_tweak, half_tweak] =
            self.tweaks.get(&name.to_ascii_lowercase()).copied().unwrap_or([1.0; 4]);
        let (min, half) = (number("mindistance"), number("halfvolumedistance"));
        let positional = get("is3dsound").map_or(min.is_some() || half.is_some(), |v| v != "0");
        let falloff = positional.then(|| {
            // OpenAL's defaults (reference distance 1, rolloff 1) where BF2 sets nothing.
            let (min, half) = match (min, half) {
                (Some(min), Some(half)) => (min, half),
                (Some(min), None) => (min, 2.0 * min),
                (None, Some(half)) => (half * 0.5, half),
                (None, None) => (1.0, 2.0),
            };
            let min = min * min_tweak;
            Falloff {
                min_distance: min,
                half_distance: (half * half_tweak).max(min * 1.1),
            }
        });
        let pitch = number("pitch").unwrap_or(1.0) * pitch_tweak;
        let pitch_range = envelope_range(get("pitchenvelope"));
        Some(SoundDesc {
            files,
            volume: number("volume").unwrap_or(1.0) * volume_tweak,
            volume_range: envelope_range(get("volumeenvelope")),
            pitch: pitch_range.map(|p| p * pitch),
            falloff,
            looping: get("loopcount") == Some("0"),
        })
    }
}

impl SoundConverter<'_> {
    /// A vehicle's engine sounds: the `Sound` children of its `Engine` (`*_Engine_Idle`,
    /// `_Rpm1` coasting, `_Rpm2` pulling, `_Load` gear changes) and the driver's `*_Ambient`.
    /// Their `pitchEnvelope`/`volumeEnvelope` scale pitch and volume by the revs [inferred:
    /// the idle's curve starts at 0.62 of its pitch 1.5]. The idle sample holds the
    /// start, a loop region and the stop, which are split into three files.
    pub fn vehicle(&self, world: &World, name: &str) -> VehicleSounds {
        let mut sounds = VehicleSounds {
            gears: 4,
            ..Default::default()
        };
        let Some(root) = world.template(name) else {
            return sounds;
        };
        let child = |template: &bf2_formats::con::Template, suffix: &str| {
            template
                .children
                .iter()
                .map(|c| c.template.to_ascii_lowercase())
                .find(|c| c.starts_with("s_") && c.ends_with(suffix))
        };
        sounds.interior = child(root, "_ambient").and_then(|s| self.template(world, &s)).map(|mut s| {
            s.falloff = None;
            s
        });
        // The road engine: amphibians have a water engine too, with `_WaterEngine_*` sounds.
        let mut pending = vec![(name.to_ascii_lowercase(), 0)];
        let engine = loop {
            let Some((name, depth)) = pending.pop() else { return sounds };
            let Some(template) = world.template(&name).filter(|_| depth < 16) else { continue };
            let sounding = child(template, "_engine_idle").or_else(|| child(template, "_engine_rpm1"));
            if template.ty.eq_ignore_ascii_case("engine") && sounding.is_some() {
                break template;
            }
            pending.extend(template.children.iter().map(|c| (c.template.to_ascii_lowercase(), depth + 1)));
        };
        sounds.gears = engine.get_f32("setnumberofgears").map_or(4, |g| g.max(1.0) as u32);
        sounds.gear_shift = child(engine, "_engine_load").and_then(|s| self.template(world, &s));
        let has_loaded = child(engine, "_engine_rpm2").is_some();
        for (suffix, load) in [("_engine_idle", None), ("_engine_rpm1", has_loaded.then_some(false)), ("_engine_rpm2", Some(true))] {
            let Some(name) = child(engine, suffix) else { continue };
            let Some(template) = world.template(&name) else { continue };
            let Some(mut sound) = self.desc(&name, &template.props) else { continue };
            let pitch = envelope_points(template.get_str("pitchenvelope"));
            if load.is_none()
                && let Some((start, idle, stop)) = sound.files.first().and_then(|f| self.split_loop(f))
            {
                let part = |file: String| SoundDesc {
                    files: vec![file],
                    looping: false,
                    ..sound.clone()
                };
                sounds.start = start.map(part);
                sounds.stop = stop.map(part);
                sound.files = vec![idle];
            }
            sound.looping = true;
            sounds.engine.push(EngineSound {
                sound,
                pitch,
                volume: envelope_points(template.get_str("volumeenvelope")),
                load,
            });
        }
        sounds
    }

    /// Splits a sample with a loop region (a cue point with a `ltxt` length, as Sound Forge
    /// writes them) into `<file>_start.wav`, `_loop.wav` and `_stop.wav`: the parts before,
    /// in and after the region. The start and stop are `None` when empty.
    fn split_loop(&self, file: &str) -> Option<(Option<String>, String, Option<String>)> {
        let data = std::fs::read(self.out.join(file)).ok()?;
        let (channels, rate, samples) = audio::read_pcm16_wav(&data)?;
        let (start, length) = loop_region(&data)?;
        let frames = samples.len() / channels as usize;
        let (start, end) = (start.min(frames), (start + length).min(frames));
        if end <= start {
            return None;
        }
        let stem = file.strip_suffix(".wav")?;
        let write = |suffix: &str, range: std::ops::Range<usize>| -> Option<String> {
            if range.is_empty() {
                return None;
            }
            let target = format!("{stem}_{suffix}.wav");
            let path = self.out.join(&target);
            if !path.exists() {
                let part = &samples[range.start * channels as usize..range.end * channels as usize];
                std::fs::write(path, audio::pcm16_wav(channels, rate, part)).ok()?;
            }
            Some(target)
        };
        Some((write("start", 0..start), write("loop", start..end)?, write("stop", end..frames)))
    }
}

/// The first cue point with a region length (`LIST`/`adtl`/`ltxt`): start and length in
/// frames.
fn loop_region(wav: &[u8]) -> Option<(usize, usize)> {
    let u32_at = |data: &[u8], at: usize| Some(u32::from_le_bytes(data.get(at..at + 4)?.try_into().ok()?));
    let (mut cues, mut regions) = (HashMap::new(), Vec::new());
    let mut at = 12;
    while at + 8 <= wav.len() {
        let size = u32_at(wav, at + 4)? as usize;
        let body = wav.get(at + 8..(at + 8 + size).min(wav.len()))?;
        match &wav[at..at + 4] {
            b"cue " => {
                for i in 0..u32_at(body, 0)? as usize {
                    let entry = 4 + i * 24;
                    cues.insert(u32_at(body, entry)?, u32_at(body, entry + 20)? as usize);
                }
            }
            b"LIST" if body.get(0..4) == Some(b"adtl") => {
                let mut sub = 4;
                while sub + 8 <= body.len() {
                    let sub_size = u32_at(body, sub + 4)? as usize;
                    if &body[sub..sub + 4] == b"ltxt" {
                        regions.push((u32_at(body, sub + 8)?, u32_at(body, sub + 12)? as usize));
                    }
                    sub += 8 + sub_size + (sub_size & 1);
                }
            }
            _ => {}
        }
        at += 8 + size + (size & 1);
    }
    regions
        .into_iter()
        .find_map(|(cue, length)| Some((*cues.get(&cue)?, length)).filter(|(_, l)| *l > 0))
}

/// The points of an envelope over its control value (the engine's revs):
/// `<control>/<?>/<min>/<max>/<?>/<count>` then `<x>/<y>/<?>` per point, `y` clamped to
/// min..max [inferred from the engine sounds' curves].
fn envelope_points(envelope: Option<&str>) -> Vec<[f32; 2]> {
    let values: Vec<f32> = envelope
        .unwrap_or_default()
        .split('/')
        .filter_map(|v| v.trim().parse().ok())
        .collect();
    let [_, _, min, max, _, count, ..] = values[..] else {
        return Vec::new();
    };
    (0..count as usize)
        .map_while(|i| Some([*values.get(6 + i * 3)?, values.get(7 + i * 3)?.clamp(min, max.max(min))]))
        .collect()
}

/// The random factors in the last two values of an envelope, when they form a range.
fn envelope_range(envelope: Option<&str>) -> [f32; 2] {
    let values: Vec<f32> = envelope
        .unwrap_or_default()
        .split('/')
        .filter_map(|v| v.trim().parse().ok())
        .collect();
    match values[..] {
        [.., low, high] if values.len() >= 9 && low > 0.0 && high >= low => [low, high],
        _ => [1.0; 2],
    }
}

/// Settings of the sounds a script defines with `ObjectTemplate.create Sound <name>`,
/// `ObjectTemplate.active(Safe) [Sound] <name>` or `Sound.addSound <name>`: the
/// `ObjectTemplate.*` lines that follow each. For scripts the interpreter can't take as
/// written (`Sound.addSound`, sounds activated without being created).
fn sound_blocks(text: &str) -> Vec<(String, Vec<(String, Vec<String>)>)> {
    let mut blocks: Vec<(String, Vec<(String, Vec<String>)>)> = Vec::new();
    let mut open = false;
    for stmt in lex(text) {
        let words: Vec<&str> = stmt.tokens.iter().map(|t| t.text.as_str()).collect();
        let head = words[0].to_ascii_lowercase();
        let name = match (head.as_str(), &words[1..]) {
            ("objecttemplate.create" | "objecttemplate.activesafe", [ty, name, ..]) => {
                ty.eq_ignore_ascii_case("sound").then_some(*name)
            }
            ("objecttemplate.active", [.., name]) => Some(*name),
            ("sound.addsound", [name, ..]) => Some(*name),
            ("objecttemplate.create" | "objecttemplate.activesafe" | "objecttemplate.active", _) => None,
            (method, args) => {
                if let (true, Some(method)) = (open, method.strip_prefix("objecttemplate.")) {
                    let args = args.iter().map(|a| a.to_string()).collect();
                    blocks.last_mut().expect("open").1.push((method.to_string(), args));
                }
                continue;
            }
        };
        open = name.is_some();
        if let Some(name) = name {
            blocks.push((name.to_ascii_lowercase(), Vec::new()));
        }
    }
    blocks
}

/// The library is the same for every level: written once per run.
static LIBRARY_WRITTEN: AtomicBool = AtomicBool::new(false);

/// Writes `sounds.ron` (see the module docs). Returns the number of sounds.
pub fn import_library(vfs: &Vfs, out: &Path) -> Result<usize> {
    let converter = SoundConverter::new(vfs, out);
    let mut interp = Interpreter::new(vfs);
    interp.run("common/material/impactsounds.con", &[]);
    interp.run("common/material/materialmanagersettings.con", &[]);

    let mut projectiles = BTreeSet::new();
    let mut cells: BTreeMap<(u32, u32), BTreeMap<u32, String>> = BTreeMap::new();
    let (mut material, mut cell) = (None, None);
    for command in &interp.world.commands {
        let number = |i: usize| command.args.get(i).and_then(|a| a.trim_matches('"').parse::<f32>().ok());
        match command.name.as_str() {
            "material.active" => material = number(0).map(|id| id as u32),
            "material.type" if number(0) == Some(PROJECTILE_TYPE as f32) => projectiles.extend(material),
            "materialmanager.createcell" => cell = number(0).zip(number(1)).map(|(a, b)| (a as u32, b as u32)),
            "materialmanager.setsoundtemplate" => {
                if let (Some(cell), Some(index), Some(sound)) = (cell, number(0), command.args.get(1)) {
                    cells.entry(cell).or_default().insert(index as u32, sound.to_ascii_lowercase());
                }
            }
            _ => {}
        }
    }
    anyhow::ensure!(!cells.is_empty(), "no material sounds in this mod");

    let mut library = SoundLibrary::default();
    for (&(a, b), sounds) in &cells {
        let get = |index: u32| sounds.get(&index).cloned();
        match (a, b) {
            (FOOTSTEPS, LADDER) => library.soldier.ladder = get(0),
            (FOOTSTEPS, surface) => {
                if surface == 0 {
                    library.soldier.body_fall = get(4);
                }
                library.footsteps.insert(
                    surface,
                    FootstepSounds {
                        walk: get(0),
                        run: get(1),
                        prone: get(2),
                        sprint: get(3),
                    },
                );
            }
            (projectile, FLYBY) => {
                library.flybys.extend(get(0).map(|sound| (projectile, sound)));
            }
            (projectile, surface) if projectiles.contains(&projectile) => {
                let sounds = ImpactSounds {
                    impact: get(0),
                    ricochet: get(1),
                };
                if sounds != ImpactSounds::default() {
                    library.impacts.entry(projectile).or_default().insert(surface, sounds);
                }
            }
            _ => {}
        }
    }

    let mut named: BTreeSet<String> = library.footsteps.values().flat_map(|f| [&f.walk, &f.run, &f.prone, &f.sprint]).flatten().cloned().collect();
    named.extend(library.impacts.values().flat_map(|m| m.values()).flat_map(|i| [&i.impact, &i.ricochet]).flatten().cloned());
    named.extend(library.flybys.values().cloned());
    named.extend([&library.soldier.ladder, &library.soldier.body_fall].into_iter().flatten().cloned());
    for name in named {
        if let Some(sound) = converter.template(&interp.world, &name) {
            library.sounds.insert(name, sound);
        }
    }

    // Sounds the interpreter can't read as written.
    let mut blocks: Vec<(String, Vec<(String, Vec<String>)>)> =
        sound_blocks(&vfs.read_text("common/sound/gamesounds.con").unwrap_or_default());
    let mut tweaks: Vec<&str> = vfs.list("objects/soldiers/").filter(|p| p.ends_with(".tweak")).collect();
    tweaks.sort_unstable();
    for path in tweaks {
        blocks.extend(sound_blocks(&vfs.read_text(path).unwrap_or_default()));
    }
    let mut from_blocks = |wanted: &dyn Fn(&str) -> bool| -> Option<String> {
        let (name, props) = blocks.iter().find(|(name, _)| wanted(name))?;
        let sound = converter.desc(name, props)?;
        library.sounds.insert(name.clone(), sound);
        Some(name.clone())
    };
    let injury = from_blocks(&|name| name == "s_injury");
    let sprint_breath = from_blocks(&|name| name == "s_sprintbreath");
    let death = from_blocks(&|name| name.ends_with("_soldier_death"));
    library.soldier.injury = injury;
    library.soldier.sprint_breath = sprint_breath;
    library.soldier.death = death;

    for (effect, large) in [("e_exp_grenade", false), ("e_exp_large", true)] {
        interp.ensure_template(effect);
        let Some(template) = interp.world.template(effect) else { continue };
        let found = template.children.iter().find_map(|child| {
            let sound = converter.template(&interp.world, &child.template)?;
            Some((child.template.to_ascii_lowercase(), sound))
        });
        if let Some((name, sound)) = found {
            library.sounds.insert(name.clone(), sound);
            let slot = if large { &mut library.explosions.large } else { &mut library.explosions.small };
            *slot = Some(name);
        }
    }

    game_data::write_ron(out.join("sounds.ron"), &library)?;
    Ok(library.sounds.len())
}

/// A level's ambient sounds to `levels/<name>/sounds.ron`, and the library if this run
/// hasn't written it yet. Returns the number of ambient sounds.
pub fn import_level(vfs: &Vfs, world: &World, level_dir: &Path, out: &Path) -> Result<usize> {
    if !LIBRARY_WRITTEN.swap(true, Ordering::Relaxed) {
        match import_library(vfs, out) {
            Ok(count) => log::info!("sound library: {count} sounds"),
            Err(err) => log::warn!("sound library: {err:#}"),
        }
    }
    let converter = SoundConverter::new(vfs, out);
    let mut level = LevelSounds::default();
    for command in world.commands_named("sound.addtrigger") {
        let Some(name) = command.args.first() else { continue };
        let Some(template) = world.template(name) else { continue };
        let Some(mut sound) = converter.desc(name, &template.props) else { continue };
        // Heard around a point rather than coming from it.
        sound.falloff = None;
        let radius = template.get_f32("mindistance").unwrap_or(0.0);
        let position = template
            .get_vec3("position")
            .filter(|_| radius < LEVEL_WIDE)
            .map(coords::position);
        level.ambience.push(AmbientSound { sound, position, radius });
    }
    game_data::write_ron(level_dir.join("sounds.ron"), &level).context("writing sounds.ron")?;
    Ok(level.ambience.len())
}

/// A weapon's fire sound as heard from afar, generated from `near`: `<file>_distant.wav`
/// next to each file.
pub fn distant(near: &SoundDesc, out: &Path) -> Option<SoundDesc> {
    let files: Vec<String> = near
        .files
        .iter()
        .filter_map(|file| {
            let stem = file.strip_suffix(".wav")?;
            let target = format!("{stem}_distant.wav");
            if !out.join(&target).exists() {
                let data = std::fs::read(out.join(file)).ok()?;
                let muffled = muffle(&data).or_else(|| {
                    log::debug!("{file}: not 16-bit PCM");
                    None
                })?;
                std::fs::write(out.join(&target), muffled).ok()?;
            }
            Some(normalize(&target))
        })
        .collect();
    (!files.is_empty()).then(|| SoundDesc {
        files,
        // Sound carries: the far version fades out more slowly [our choice].
        falloff: near.falloff.map(|f| Falloff {
            half_distance: f.min_distance + (f.half_distance - f.min_distance) * 2.0,
            ..f
        }),
        ..near.clone()
    })
}

/// Distant gunfire: low-passed (the air and the ground swallow the highs) with a few late
/// echoes, at the original peak level. Mono 16-bit PCM.
fn muffle(wav: &[u8]) -> Option<Vec<u8>> {
    let (channels, rate, samples) = audio::read_pcm16_wav(wav)?;
    let mono: Vec<f32> = samples
        .chunks_exact(channels as usize)
        .map(|frame| frame.iter().map(|&s| s as f32).sum::<f32>() / channels as f32)
        .collect();
    let cutoff = 900.0;
    let a = 1.0 - (-std::f32::consts::TAU * cutoff / rate as f32).exp();
    let mut filtered = mono.clone();
    for _ in 0..2 {
        let mut y = 0.0;
        for sample in &mut filtered {
            y += a * (*sample - y);
            *sample = y;
        }
    }
    let echoes = [(0.13, 0.5), (0.29, 0.32), (0.47, 0.2), (0.74, 0.12)];
    let mut mixed = vec![0.0f32; filtered.len() + (0.8 * rate as f32) as usize];
    for (i, &sample) in filtered.iter().enumerate() {
        mixed[i] += sample;
        for (delay, gain) in echoes {
            mixed[i + (delay * rate as f32) as usize] += sample * gain;
        }
    }
    let peak = |s: &[f32]| s.iter().fold(0.0f32, |m, v| m.max(v.abs()));
    let scale = peak(&mono) / peak(&mixed).max(1.0);
    let out: Vec<i16> = mixed.iter().map(|s| (s * scale).clamp(-32768.0, 32767.0) as i16).collect();
    Some(audio::pcm16_wav(1, rate, &out))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reads_envelope_ranges() {
        assert_eq!(envelope_range(Some("0/1/0.9/1.1/1/1/0/0.98/1.04/")), [0.98, 1.04]);
        assert_eq!(envelope_range(Some("0/1/0/2/0/1/0/1/0/")), [1.0; 2]);
        assert_eq!(envelope_range(Some("0/1/0/1/0/0/")), [1.0; 2]);
        assert_eq!(envelope_range(None), [1.0; 2]);
    }

    #[test]
    fn reads_engine_curves() {
        assert_eq!(
            envelope_points(Some("0/1/0/1.5/0/2/0/0.62/0/0.79/1.6/0/")),
            vec![[0.0, 0.62], [0.79, 1.5]]
        );
        assert_eq!(envelope_points(Some("0/1/0/1/0/0/")), Vec::<[f32; 2]>::new());
        assert!(envelope_points(None).is_empty());
    }

    #[test]
    fn finds_loop_regions() {
        let mut wav = audio::pcm16_wav(1, 22050, &[0; 100]);
        let mut cue = b"cue ".to_vec();
        cue.extend(28u32.to_le_bytes());
        cue.extend(1u32.to_le_bytes());
        for value in [7u32, 40, u32::from_le_bytes(*b"data"), 0, 0, 40] {
            cue.extend(value.to_le_bytes());
        }
        let mut list = b"LIST".to_vec();
        let ltxt: Vec<u8> = [b"ltxt".to_vec(), 12u32.to_le_bytes().to_vec(), 7u32.to_le_bytes().to_vec(), 30u32.to_le_bytes().to_vec(), b"rgn ".to_vec()].concat();
        list.extend(((4 + ltxt.len()) as u32).to_le_bytes());
        list.extend(b"adtl");
        list.extend(ltxt);
        wav.extend(cue);
        wav.extend(list);
        assert_eq!(loop_region(&wav), Some((40, 30)));
    }

    #[test]
    fn splits_sound_blocks() {
        let text = "Sound.addSound S_Injury\nObjectTemplate.soundFilename a.wav,b.wav\n\
                    ObjectTemplate.volume 1\nObjectTemplate.activeSafe Soldier x\n\
                    ObjectTemplate.volume 2\nObjectTemplate.active S_X_Death\n\
                    ObjectTemplate.minDistance 2\n";
        let blocks = sound_blocks(text);
        assert_eq!(blocks.len(), 2);
        assert_eq!(blocks[0].0, "s_injury");
        assert_eq!(blocks[0].1, vec![
            ("soundfilename".to_string(), vec!["a.wav,b.wav".to_string()]),
            ("volume".to_string(), vec!["1".to_string()]),
        ]);
        assert_eq!(blocks[1].1, vec![("mindistance".to_string(), vec!["2".to_string()])]);
    }

    #[test]
    fn muffles_pcm() {
        let samples: Vec<i16> = (0..2205).map(|i| if i % 2 == 0 { 20000 } else { -20000 }).collect();
        let wav = audio::pcm16_wav(1, 22050, &samples);
        let (channels, rate, out) = audio::read_pcm16_wav(&muffle(&wav).unwrap()).unwrap();
        assert_eq!((channels, rate), (1, 22050));
        assert!(out.len() > samples.len());
    }
}
