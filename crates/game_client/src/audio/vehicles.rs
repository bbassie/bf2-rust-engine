//! Vehicle engines: while a vehicle has a driver its engine loops play at the hull, their
//! pitch and volume following the revs through BF2's curves ([`game_data::VehicleSounds`]).
//! BF2's engine model isn't known; here the revs climb through the gears as the vehicle
//! speeds up (with a gear change sound at each shift), and the pulling and coasting loops
//! crossfade with the acceleration. The engine starts when a driver gets in and stops when
//! the driver leaves or the vehicle is wrecked; occupants also hear the interior loop.

use std::collections::HashMap;

use bevy::prelude::*;
use game_data::{VehicleSounds, curve_at};
use game_shared::vehicle::{Seated, VehicleData, VehicleHealth};

use super::{
    AudioSystems,
    voices::{AudioMix, HeldVoice, PlaySound, SoundCache, spawn_held},
};
use crate::{net::LocalSoldier, vehicles::{VehicleView, VehicleViewSystems}};

pub struct VehicleAudioPlugin;

impl Plugin for VehicleAudioPlugin {
    fn build(&self, app: &mut App) {
        app.add_systems(
            PostUpdate,
            engines.after(VehicleViewSystems).in_set(AudioSystems::Trigger),
        );
    }
}

/// Revs while idling and at the bottom of a gear.
const IDLE_REVS: f32 = 0.0;
const LOW_REVS: f32 = 0.3;
/// Below this speed (m/s) the engine idles.
const CRAWL: f32 = 0.5;
/// How quickly the revs and the load follow their targets, per second.
const REVS_RATE: f32 = 5.0;
const LOAD_RATE: f32 = 3.0;
/// Accelerating harder than this (m/s²) is full load.
const FULL_LOAD: f32 = 1.5;

#[derive(Default)]
struct Engine {
    /// Engine loop voices, by index in [`VehicleSounds::engine`].
    loops: Vec<Entity>,
    interior: Option<Entity>,
    revs: f32,
    load: f32,
    gear: u32,
    speed: f32,
}

impl Engine {
    fn stop(&mut self, commands: &mut Commands) {
        for voice in self.loops.drain(..).chain(self.interior.take()) {
            commands.entity(voice).try_despawn();
        }
    }
}

#[allow(clippy::too_many_arguments)]
fn engines(
    mut commands: Commands,
    time: Res<Time>,
    mix: Res<AudioMix>,
    mut cache: ResMut<SoundCache>,
    assets: Res<AssetServer>,
    listener: Query<(Entity, &Transform), With<SpatialListener>>,
    vehicles: Query<(Entity, &VehicleView, &VehicleData, Option<&VehicleHealth>)>,
    seated: Query<(&Seated, Has<LocalSoldier>)>,
    mut held: Query<&mut HeldVoice>,
    mut engines: Local<HashMap<Entity, Engine>>,
    mut last_log: Local<f32>,
    mut sounds: MessageWriter<PlaySound>,
) {
    let dt = time.delta_secs().max(1e-4);
    let log = time.elapsed_secs() - *last_log > 1.0;
    if log {
        *last_log = time.elapsed_secs();
    }
    let listener = listener.iter().next();
    for (vehicle, view, data, health) in &vehicles {
        let desc = &data.0.desc;
        let audio: &VehicleSounds = &desc.sounds;
        let driven = seated.iter().any(|(s, _)| s.vehicle == vehicle && s.seat == 0);
        let running = driven && !health.is_some_and(VehicleHealth::wrecked) && !audio.engine.is_empty();
        let hull = view.transform.translation;
        let engine = engines.entry(vehicle).or_default();
        let started = !engine.loops.is_empty();
        if running != started {
            let (edge, reason) = if running { (&audio.start, "engine start") } else { (&audio.stop, "engine stop") };
            if let Some(sound) = edge {
                sounds.write(PlaySound::at(sound, hull).emitter(vehicle).reason(reason));
            }
            engine.stop(&mut commands);
            if running {
                engine.revs = IDLE_REVS;
                engine.loops = audio
                    .engine
                    .iter()
                    .map(|e| {
                        let held = HeldVoice { at: Some(hull), level: 0.0, speed: 1.0 };
                        spawn_held(&mut commands, &mut cache, &assets, listener, &e.sound, held)
                    })
                    .collect();
            }
        }
        let speed = view.speed.abs();
        let acceleration = (speed - engine.speed) / dt;
        engine.speed = speed;
        if !running {
            continue;
        }

        // Up through the gears to the top speed.
        let gears = audio.gears.max(1);
        let progress = (speed / desc.engine.top_speed.max(1.0)).clamp(0.0, 1.0) * gears as f32;
        let gear = (progress as u32).min(gears - 1);
        if gear > engine.gear
            && let Some(shift) = &audio.gear_shift
        {
            sounds.write(PlaySound::at(shift, hull).emitter(vehicle).reason("gear shift"));
        }
        engine.gear = gear;
        let target = if speed < CRAWL { IDLE_REVS } else { LOW_REVS + (1.0 - LOW_REVS) * (progress - gear as f32).min(1.0) };
        engine.revs += (target - engine.revs) * (1.0 - (-REVS_RATE * dt).exp());
        let pulling = if speed < CRAWL { 0.0 } else { (acceleration / FULL_LOAD).clamp(0.0, 1.0).max(0.3) };
        engine.load += (pulling - engine.load) * (1.0 - (-LOAD_RATE * dt).exp());

        let mut levels = Vec::new();
        for (sound, voice) in audio.engine.iter().zip(&engine.loops) {
            let Ok(mut voice) = held.get_mut(*voice) else { continue };
            let load = match sound.load {
                Some(true) => engine.load,
                Some(false) => 1.0 - engine.load,
                None => 1.0,
            };
            voice.at = Some(hull);
            voice.level = sound.sound.volume * curve_at(&sound.volume, engine.revs, 1.0) * load * mix.effects;
            let pitch = (sound.sound.pitch[0] + sound.sound.pitch[1]) * 0.5;
            voice.speed = pitch * curve_at(&sound.pitch, engine.revs, 1.0);
            if log {
                levels.push(format!("{:.2}@{:.2}", voice.level, voice.speed));
            }
        }
        if log {
            debug!(
                target: "audio",
                "engine {}: {speed:.1} m/s, gear {}, revs {:.2}, load {:.2}, loops {}",
                desc.name,
                engine.gear + 1,
                engine.revs,
                engine.load,
                levels.join(" ")
            );
        }

        // Inside, for whoever rides along.
        let inside = seated.iter().any(|(s, local)| local && s.vehicle == vehicle);
        match (&audio.interior, engine.interior, inside) {
            (Some(interior), None, true) => {
                let level = interior.volume * mix.effects;
                let held = HeldVoice { at: None, level, speed: interior.pitch[0] };
                engine.interior = Some(spawn_held(&mut commands, &mut cache, &assets, listener, interior, held));
            }
            (_, Some(voice), false) => {
                commands.entity(voice).try_despawn();
                engine.interior = None;
            }
            _ => {}
        }
    }
    engines.retain(|vehicle, engine| {
        let exists = vehicles.contains(*vehicle);
        if !exists {
            engine.stop(&mut commands);
        }
        exists
    });
}
