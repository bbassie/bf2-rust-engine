//! Looping sounds that belong to something in the world (a flag flapping on its pole):
//! insert a [`SoundEmitter`] and the sound loops at the entity's position while it is
//! audible, stopping when the listener walks away or the entity goes.

use std::collections::HashMap;

use bevy::prelude::*;
use game_data::SoundDesc;

use super::{
    AudioSystems,
    voices::{Channel, DEFAULT_FALLOFF, HeldVoice, SoundCache, spawn_held},
};

pub struct EmitterPlugin;

impl Plugin for EmitterPlugin {
    fn build(&self, app: &mut App) {
        app.add_systems(PostUpdate, emitters.in_set(AudioSystems::Trigger));
    }
}

/// Loops start above this gain (before mix and ducking) and stop below `STOP` (-40 dB):
/// BF2's inverse falloff would keep a flag's flapping audible, barely, half a map away.
const START: f32 = 0.012;
const STOP: f32 = 0.01;

/// Plays `sound` looping where this entity is (its `GlobalTransform`), while audible.
#[derive(Component, Clone, Debug)]
pub struct SoundEmitter {
    pub sound: SoundDesc,
    /// Multiplies the sound's own volume.
    pub volume: f32,
    pub channel: Channel,
}

impl SoundEmitter {
    pub fn new(sound: SoundDesc) -> Self {
        Self {
            sound,
            volume: 1.0,
            channel: Channel::Effects,
        }
    }

    pub fn channel(mut self, channel: Channel) -> Self {
        self.channel = channel;
        self
    }
}

/// Starts loops that became audible, stops those that went quiet or lost their emitter.
fn emitters(
    mut commands: Commands,
    mut cache: ResMut<SoundCache>,
    assets: Res<AssetServer>,
    listener: Query<(Entity, &Transform), With<SpatialListener>>,
    emitters: Query<(Entity, &SoundEmitter, &GlobalTransform)>,
    mut held: Query<&mut HeldVoice>,
    mut voices: Local<HashMap<Entity, Entity>>,
) {
    let listener = listener.iter().next();
    let Some(ear) = listener.map(|(_, t)| t.translation) else {
        return;
    };
    for (entity, emitter, transform) in &emitters {
        let at = transform.translation();
        let level = emitter.sound.volume * emitter.volume;
        let falloff = emitter.sound.falloff.unwrap_or(DEFAULT_FALLOFF);
        // Before mix and ducking: a loop turned down by the settings keeps playing.
        let gain = level * falloff.gain(at.distance(ear));
        let playing = voices.get(&entity).copied();
        let audible = gain > if playing.is_some() { STOP } else { START };
        match (playing, audible) {
            (None, true) if !emitter.sound.files.is_empty() => {
                let voice = HeldVoice {
                    at: Some(at),
                    level,
                    speed: (emitter.sound.pitch[0] + emitter.sound.pitch[1]) * 0.5,
                    channel: emitter.channel,
                };
                let voice = spawn_held(&mut commands, &mut cache, &assets, listener, &emitter.sound, voice);
                voices.insert(entity, voice);
            }
            (Some(voice), false) => {
                commands.entity(voice).try_despawn();
                voices.remove(&entity);
            }
            (Some(voice), true) => {
                if let Ok(mut voice) = held.get_mut(voice) {
                    voice.at = Some(at);
                    voice.level = level;
                } else {
                    // Gone with the level: start again next frame.
                    voices.remove(&entity);
                }
            }
            _ => {}
        }
    }
    voices.retain(|entity, voice| {
        let exists = emitters.contains(*entity);
        if !exists {
            commands.entity(*voice).try_despawn();
        }
        exists
    });
}
