//! Game audio: BF2's sounds, played with BF2's distance model.
//!
//! Anything that makes a noise writes a [`PlaySound`] (explosions, grenades and effects can
//! too); [`voices`] decides whether it is audible and worth one of the limited voices, and
//! keeps its volume and direction up to date. What plays when:
//!
//! - [`weapons`]: fire (first person for our shots; third person, crossfading into a muffled
//!   far version, for everyone else's), reload, deploy, zoom, fire mode, dry fire; bullet
//!   impacts per surface material and near misses (cracks), found by tracing each shot's
//!   path when it is fired; deaths, being hurt.
//! - [`footsteps`]: every soldier's steps by stance and speed on the surface under them,
//!   landings and ladder rungs.
//! - [`ambience`]: the level's looping background sounds, fading in around their areas.
//! - [`emitters`]: loops that belong to something in the world (flags), while audible.
//! - [`vehicles`]: engines following the revs while someone drives.
//!
//! Sounds come from `sounds.ron` (shared: footsteps, impacts, voices), the weapon and vehicle
//! descriptions and `levels/<name>/sounds.ron` (ambience), all written by the importer.
//! `RUST_LOG=info,audio=debug` logs every sound started or dropped, with its gain.

use std::sync::Arc;

use bevy::{prelude::*, transform::TransformSystems};
use game_data::{SoundDesc, SoundLibrary};
use game_shared::{config::GamePaths, level::LoadedLevel};

use crate::{camera::CameraSystems, prediction::RenderStateSystems};

mod ambience;
mod emitters;
mod footsteps;
mod vehicles;
mod voices;
mod weapons;

// For whatever else makes noise (explosions, effects) and the audio settings.
#[allow(unused_imports)]
pub use emitters::SoundEmitter;
#[allow(unused_imports)]
pub use voices::{AudioMix, Channel, Muffle, PlaySound, Sound};

pub struct AudioPlugin;

impl Plugin for AudioPlugin {
    fn build(&self, app: &mut App) {
        app.configure_sets(
            PostUpdate,
            (AudioSystems::Trigger, AudioSystems::Play)
                .chain()
                .after(RenderStateSystems)
                .after(CameraSystems)
                .before(TransformSystems::Propagate),
        )
        .add_systems(Startup, load_library)
        .add_systems(Update, preload_voices.run_if(resource_exists_and_changed::<LoadedLevel>))
        .add_plugins((
            voices::VoicePlugin,
            weapons::WeaponAudioPlugin,
            footsteps::FootstepPlugin,
            ambience::AmbiencePlugin,
            emitters::EmitterPlugin,
            vehicles::VehicleAudioPlugin,
        ));
    }
}

/// Sounds are triggered, then started and mixed, every frame after the soldiers and the
/// camera moved.
#[derive(SystemSet, Debug, Clone, PartialEq, Eq, Hash)]
enum AudioSystems {
    Trigger,
    Play,
}

/// `sounds.ron`: sounds shared by every level.
#[derive(Resource, Default, Clone)]
pub struct Sounds(pub Arc<SoundLibrary>);

fn load_library(mut commands: Commands, paths: Res<GamePaths>) {
    let path = paths.imported.join("sounds.ron");
    let library = if path.exists() {
        game_data::read_ron(&path).unwrap_or_else(|err| {
            warn!("{err}");
            SoundLibrary::default()
        })
    } else {
        info!("no {}: footsteps, impacts and voices are silent (re-import a level)", path.display());
        SoundLibrary::default()
    };
    commands.insert_resource(Sounds(Arc::new(library)));
}

/// The commander's voice-overs of the level's teams, so announcements aren't late.
fn preload_voices(level: Res<LoadedLevel>, mut cache: ResMut<voices::SoundCache>, assets: Res<AssetServer>) {
    for team in &level.desc.teams {
        let voice = &team.voice;
        let lines = [
            &voice.we_captured,
            &voice.we_lost,
            &voice.enemy_captured,
            &voice.bleed_start,
            &voice.bleed_end,
            &voice.low_tickets,
        ];
        for line in lines.into_iter().flatten() {
            cache.preload(&assets, &SoundDesc::file(line.clone()));
        }
    }
}
