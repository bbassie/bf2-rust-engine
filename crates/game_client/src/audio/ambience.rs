//! Level ambience: looping background sounds (`levels/<name>/sounds.ron`), not positional
//! but heard around their area: at full volume within half their radius, fading out to the
//! edge. Areas playing the same file share one voice at the loudest area's volume.

use bevy::{
    audio::{AudioSinkPlayback, PlaybackMode, Volume},
    prelude::*,
};
use game_data::LevelSounds;
use game_shared::level::{LevelEntity, LoadedLevel};

use super::{
    AudioSystems,
    voices::{AudioMix, SoundCache},
};

pub struct AmbiencePlugin;

impl Plugin for AmbiencePlugin {
    fn build(&self, app: &mut App) {
        app.init_resource::<Ambience>().add_systems(
            PostUpdate,
            (load_ambience.run_if(resource_exists_and_changed::<LoadedLevel>), update_ambience)
                .chain()
                .in_set(AudioSystems::Play),
        );
    }
}

/// Voices below this are stopped.
const SILENT: f32 = 0.001;

#[derive(Resource, Default)]
struct Ambience {
    loops: Vec<AmbientLoop>,
}

struct AmbientLoop {
    file: String,
    volume: f32,
    /// Center (`None`: everywhere) and radius.
    areas: Vec<(Option<Vec3>, f32)>,
    voice: Option<Entity>,
}

#[derive(Component)]
struct AmbientVoice;

fn load_ambience(
    mut commands: Commands,
    level: Res<LoadedLevel>,
    mut ambience: ResMut<Ambience>,
    voices: Query<Entity, With<AmbientVoice>>,
) {
    for voice in &voices {
        commands.entity(voice).despawn();
    }
    ambience.loops.clear();
    let Some(path) = level.dir.as_ref().map(|dir| dir.join("sounds.ron")).filter(|p| p.exists()) else {
        return;
    };
    let sounds: LevelSounds = match game_data::read_ron(&path) {
        Ok(sounds) => sounds,
        Err(err) => {
            warn!("{err}");
            return;
        }
    };
    for ambient in sounds.ambience {
        let Some(file) = ambient.sound.files.first() else { continue };
        let area = (ambient.position.map(Vec3::from_array), ambient.radius);
        match ambience.loops.iter_mut().find(|l| l.file == *file && l.volume == ambient.sound.volume) {
            Some(existing) => existing.areas.push(area),
            None => ambience.loops.push(AmbientLoop {
                file: file.clone(),
                volume: ambient.sound.volume,
                areas: vec![area],
                voice: None,
            }),
        }
    }
    info!("{} ambient loops", ambience.loops.len());
}

fn area_gain((center, radius): (Option<Vec3>, f32), at: Vec3) -> f32 {
    let Some(center) = center else {
        return 1.0;
    };
    let t = ((center.distance(at) - radius * 0.5) / (radius * 0.5).max(0.01)).clamp(0.0, 1.0);
    1.0 - t * t * (3.0 - 2.0 * t)
}

#[allow(clippy::too_many_arguments)]
fn update_ambience(
    mut commands: Commands,
    mut ambience: ResMut<Ambience>,
    mix: Res<AudioMix>,
    global: Res<GlobalVolume>,
    mut cache: ResMut<SoundCache>,
    asset_server: Res<AssetServer>,
    listener: Query<&Transform, With<SpatialListener>>,
    mut sinks: Query<Option<&mut AudioSink>, With<AmbientVoice>>,
) {
    let Some(ear) = listener.iter().next().map(|t| t.translation) else {
        return;
    };
    let master = global.volume.to_linear();
    for ambient in &mut ambience.loops {
        let gain = ambient.areas.iter().map(|&a| area_gain(a, ear)).fold(0.0, f32::max) * ambient.volume * mix.ambience;
        match ambient.voice {
            None if gain > SILENT => {
                let voice = commands
                    .spawn((
                        AudioPlayer::new(cache.get(&asset_server, &ambient.file)),
                        PlaybackSettings {
                            mode: PlaybackMode::Loop,
                            volume: Volume::Linear(gain),
                            ..PlaybackSettings::LOOP
                        },
                        AmbientVoice,
                        LevelEntity,
                    ))
                    .id();
                ambient.voice = Some(voice);
                debug!(target: "audio", "ambience {} starts: gain {gain:.2}", ambient.file);
            }
            Some(voice) if gain <= SILENT => {
                commands.entity(voice).despawn();
                ambient.voice = None;
                debug!(target: "audio", "ambience {} stops", ambient.file);
            }
            Some(voice) => match sinks.get_mut(voice) {
                Ok(Some(mut sink)) => sink.set_volume(Volume::Linear(gain * master)),
                // Still loading.
                Ok(None) => {}
                // Gone with the level.
                Err(_) => ambient.voice = None,
            },
            None => {}
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn areas_fade_out_to_their_edge() {
        let area = (Some(Vec3::ZERO), 100.0);
        assert_eq!(area_gain(area, Vec3::new(40.0, 0.0, 0.0)), 1.0);
        assert!((area_gain(area, Vec3::new(75.0, 0.0, 0.0)) - 0.5).abs() < 1e-4);
        assert_eq!(area_gain(area, Vec3::new(120.0, 0.0, 0.0)), 0.0);
        assert_eq!(area_gain((None, 0.0), Vec3::splat(1e4)), 1.0);
    }
}
