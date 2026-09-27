//! BF2's radio on the client: the commo rose ([`rose`]), what soldiers say and who hears it
//! ([`voices`]: radio messages, man down, automatic call-outs) and spotted enemies on the
//! HUD and the maps ([`spots`]). The rules are in
//! `game_shared::radio` and `game_server::radio`.

use std::{collections::HashMap, sync::Arc};

use bevy::prelude::*;
use game_data::{RadioRank, RadioVoice};
use game_shared::{
    config::GamePaths, conquest::team_index, level::LoadedLevel, protocol::Team, squad::SquadMember,
};

mod rose;
mod spots;
mod voices;

pub struct RadioPlugin;

impl Plugin for RadioPlugin {
    fn build(&self, app: &mut App) {
        app.init_resource::<RadioVoices>()
            .add_systems(Update, reset_voices.run_if(resource_exists_and_changed::<LoadedLevel>))
            .add_plugins((rose::RosePlugin, voices::VoicesPlugin, spots::SpotsPlugin));
    }
}

/// The teams' radio voices (`radio/<language>.ron`), loaded when first needed.
#[derive(Resource, Default)]
pub struct RadioVoices(HashMap<String, Option<Arc<RadioVoice>>>);

/// Voices of levels without their own language.
const FALLBACK_LANGUAGE: &str = "english";

impl RadioVoices {
    fn load(&mut self, paths: &GamePaths, language: &str) -> Option<Arc<RadioVoice>> {
        self.0
            .entry(language.to_string())
            .or_insert_with(|| {
                let path = paths.imported.join("radio").join(format!("{language}.ron"));
                path.exists().then(|| game_data::read_ron(&path).map_err(|e| warn!("{e}")).ok().map(Arc::new)).flatten()
            })
            .clone()
    }

    /// The voice of `team` on this level, falling back to English.
    pub fn team(&mut self, paths: &GamePaths, level: &LoadedLevel, team: Team) -> Option<Arc<RadioVoice>> {
        let language = team_index(team)
            .and_then(|i| level.desc.teams.get(i))
            .map(|t| t.language.to_ascii_lowercase())
            .unwrap_or_default();
        self.load(paths, &language).or_else(|| self.load(paths, FALLBACK_LANGUAGE))
    }
}

fn reset_voices(mut voices: ResMut<RadioVoices>) {
    voices.0.clear();
}

/// Which recordings a speaker gets: squad leaders have their own.
fn rank(squad: Option<&SquadMember>) -> RadioRank {
    if squad.is_some_and(|s| s.leader) { RadioRank::SquadLeader } else { RadioRank::Grunt }
}
