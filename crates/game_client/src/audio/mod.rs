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
//!
//! Sounds come from `sounds.ron` (shared: footsteps, impacts, voices), the weapons'
//! descriptions and `levels/<name>/sounds.ron` (ambience), all written by the importer.
//! `RUST_LOG=info,audio=debug` logs every sound started or dropped, with its gain.

use std::sync::Arc;

use bevy::{prelude::*, transform::TransformSystems};
use game_data::SoundLibrary;
use game_shared::config::GamePaths;

use crate::{camera::CameraSystems, prediction::RenderStateSystems};

mod ambience;
mod footsteps;
mod voices;
mod weapons;

// For whatever else makes noise (explosions, effects) and the audio settings.
#[allow(unused_imports)]
pub use voices::{AudioMix, PlaySound, Sound};

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
        .add_plugins((
            voices::VoicePlugin,
            weapons::WeaponAudioPlugin,
            footsteps::FootstepPlugin,
            ambience::AmbiencePlugin,
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
