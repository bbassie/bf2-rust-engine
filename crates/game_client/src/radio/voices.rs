//! What soldiers say and who hears it, like BF2 (`gamelogic.messages.addRadioVoice` flags):
//! anyone within [`IN_PERSON`] meters hears the speaker himself, whichever team (the
//! unfiltered recording, positional); farther away his squad (the whole team for spotting)
//! hears the radio (the filtered recording) and sees the line in the chat. Squad leaders
//! have their own recordings. Besides the commo rose's messages soldiers call out on their
//! own: "reloading", "grenade out" (frag or smoke), "incoming grenade" near a live
//! grenade, "out of ammo", "check your fire" when a teammate hits them, and a scream when
//! critically wounded that goes out over the radio too. BF2 has no recordings for taking
//! fire or an enemy going down.

use std::collections::{HashMap, HashSet};

use bevy::{ecs::system::SystemParam, prelude::*};
use game_data::{Falloff, SoundDesc};
use game_shared::{
    chat::{ChatChannel, ChatLine},
    config::GamePaths,
    level::LoadedLevel,
    projectile::{Projectile, ProjectileMotion},
    protocol::{ControlledBy, HitConfirmed, Player, Team},
    radio::RadioMessage,
    revive::Downed,
    soldier::Soldier,
    squad::SquadMember,
    weapons::{Armory, Inventory, Loadout},
};

use super::{RadioVoices, rank};
use crate::{
    audio::PlaySound,
    net::{LocalPlayer, LocalSoldier},
    prediction::SoldierRender,
};

pub struct VoicesPlugin;

impl Plugin for VoicesPlugin {
    fn build(&self, app: &mut App) {
        app.init_resource::<CallOuts>()
            .add_systems(Update, (receive_radio, man_down, reloading, grenades, friendly_fire));
    }
}

/// Speakers closer than this are heard in person [our choice].
const IN_PERSON: f32 = 25.0;
/// How a voice fades in person [our choice; BF2's voice-overs don't say].
const SHOUT: Falloff = Falloff {
    min_distance: 3.0,
    half_distance: 8.0,
};
/// A soldier says nothing on his own sooner than this after his last line, and repeats a
/// call-out no sooner than [`REPEAT`] (seconds).
const QUIET: f32 = 2.0;
const REPEAT: f32 = 8.0;
/// Soldiers this close to a live grenade shout a warning (meters).
const GRENADE_WARNING: f32 = 7.0;

/// Who hears a line over the radio.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Hearers {
    /// Only in person.
    Nobody,
    /// The speaker's squad, or his team when he has none.
    Squad,
    Team,
}

/// When each soldier last spoke (key `""`) and last said each call-out.
#[derive(Resource, Default)]
struct CallOuts(HashMap<(Entity, &'static str), f32>);

struct Speaker {
    player: Entity,
    name: String,
    team: Team,
    squad: Option<SquadMember>,
    /// Feet.
    position: Vec3,
}

#[derive(SystemParam)]
struct Voices<'w, 's> {
    paths: Res<'w, GamePaths>,
    level: Option<Res<'w, LoadedLevel>>,
    radio: ResMut<'w, RadioVoices>,
    time: Res<'w, Time<Real>>,
    players: Query<'w, 's, (&'static Player, &'static Team, Option<&'static SquadMember>)>,
    local: Query<'w, 's, Entity, With<LocalPlayer>>,
    listener: Query<'w, 's, &'static Transform, With<SpatialListener>>,
    last: ResMut<'w, CallOuts>,
    sounds: MessageWriter<'w, PlaySound>,
    chat: MessageWriter<'w, ChatLine>,
}

impl Voices<'_, '_> {
    fn speaker(&self, player: Entity, position: Vec3) -> Option<Speaker> {
        let (info, team, squad) = self.players.get(player).ok()?;
        Some(Speaker {
            player,
            name: info.name.clone(),
            team: *team,
            squad: squad.copied(),
            position,
        })
    }

    /// Says `id` in the speaker's voice. `text` stands in for a line without one.
    fn say(&mut self, speaker: &Speaker, id: &str, hearers: Hearers, text: &str) {
        let Some(level) = self.level.as_deref() else {
            return;
        };
        let Some(line) = self
            .radio
            .team(&self.paths, level, speaker.team)
            .and_then(|voice| voice.messages.get(id).cloned())
        else {
            debug!(target: "audio", "no radio line {id} for {:?}", speaker.team);
            return;
        };
        let local = self.local.single().ok();
        let (local_team, local_squad) = local
            .and_then(|l| self.players.get(l).ok())
            .map_or((Team::Spectator, None), |(_, t, s)| (*t, s.copied()));
        let same_team = local_team == speaker.team && local_team != Team::Spectator;
        let same_squad = same_team && speaker.squad.zip(local_squad).is_some_and(|(a, b)| a.squad == b.squad);
        let over_radio = Some(speaker.player) == local
            || match hearers {
                Hearers::Nobody => false,
                Hearers::Team => same_team,
                Hearers::Squad => same_squad || (same_team && speaker.squad.is_none()),
            };
        let near = self
            .listener
            .iter()
            .next()
            .is_some_and(|ear| ear.translation.distance(speaker.position) <= IN_PERSON);
        let rank = rank(speaker.squad.as_ref());
        let recording = |files: &[String]| SoundDesc {
            files: files.to_vec(),
            ..SoundDesc::file("")
        };
        let in_person = line.files(rank, true);
        let radio = line.files(rank, false);
        if near && !in_person.is_empty() {
            let sound = SoundDesc {
                falloff: Some(SHOUT),
                ..recording(in_person)
            };
            self.sounds.write(PlaySound::at(sound, speaker.position + Vec3::Y * 1.5).reason("voice"));
        } else if over_radio && !radio.is_empty() {
            self.sounds.write(PlaySound::local(recording(radio)).announcement().reason("radio"));
        }
        let text = if line.text.is_empty() { text } else { &line.text };
        if over_radio && hearers != Hearers::Nobody && !text.is_empty() {
            self.chat.write(ChatLine {
                channel: if same_squad { ChatChannel::Squad } else { ChatChannel::Team },
                sender: Some(speaker.name.clone()),
                team: speaker.team,
                text: text.to_string(),
            });
        }
    }

    /// A call-out of the soldier `key`, unless he spoke just now or said it recently.
    fn call_out(&mut self, key: Entity, speaker: &Speaker, id: &'static str) {
        let now = self.time.elapsed_secs();
        let recent = |at: Option<&f32>, gap: f32| at.is_some_and(|at| now - at < gap);
        let last = &mut self.last.0;
        if recent(last.get(&(key, "")), QUIET) || recent(last.get(&(key, id)), REPEAT) {
            return;
        }
        last.insert((key, ""), now);
        last.insert((key, id), now);
        last.retain(|_, at| now - *at < REPEAT);
        self.say(speaker, id, Hearers::Nobody, "");
    }
}

fn receive_radio(mut messages: MessageReader<RadioMessage>, mut voices: Voices) {
    for message in messages.read() {
        let Some(speaker) = voices.speaker(message.player, message.position) else {
            continue;
        };
        debug!(target: "audio", "radio: {} says {:?}", speaker.name, message.command);
        let hearers = if message.command.is_spot() { Hearers::Team } else { Hearers::Squad };
        voices.say(&speaker, message.command.message_id(), hearers, message.command.label());
    }
}

/// Critically wounded soldiers scream for a medic, over the radio too.
fn man_down(downed: Query<(&ControlledBy, &SoldierRender), Added<Downed>>, mut voices: Voices) {
    for (controlled_by, render) in &downed {
        if let Some(speaker) = voices.speaker(controlled_by.0, render.position) {
            voices.say(&speaker, "revive", Hearers::Squad, "");
        }
    }
}

/// "Reloading!" when a reload starts, "out of ammo" when the last magazine is empty.
#[allow(clippy::type_complexity)]
fn reloading(
    armory: Res<Armory>,
    soldiers: Query<(Entity, &Inventory, &Loadout, &ControlledBy, &SoldierRender), (With<Soldier>, Without<Downed>)>,
    mut known: Local<HashMap<Entity, (bool, bool)>>,
    mut voices: Voices,
) {
    for (soldier, inventory, loadout, controlled_by, render) in &soldiers {
        let magazine = loadout
            .weapons
            .get(inventory.active as usize)
            .and_then(|w| armory.weapon(w))
            .is_some_and(|w| w.magazine_size > 0);
        let empty = magazine && inventory.ammo.get(inventory.active as usize).is_some_and(|a| a[0] == 0 && a[1] == 0);
        let Some((was_reloading, was_empty)) = known.insert(soldier, (inventory.reloading, empty)) else {
            continue;
        };
        let id = if inventory.reloading && !was_reloading {
            "AUTO_MOODGP_reloading"
        } else if empty && !was_empty {
            "out_of_ammo"
        } else {
            continue;
        };
        if let Some(speaker) = voices.speaker(controlled_by.0, render.position) {
            voices.call_out(soldier, &speaker, id);
        }
    }
    known.retain(|soldier, _| soldiers.contains(*soldier));
}

/// "Grenade out!" from the thrower, "incoming!" from whoever is closest to a live grenade.
#[allow(clippy::type_complexity)]
fn grenades(
    armory: Res<Armory>,
    thrown: Query<(Entity, &Projectile), Added<Projectile>>,
    live: Query<(Entity, &Projectile, &ProjectileMotion)>,
    soldiers: Query<(Entity, &ControlledBy, &SoldierRender), (With<Soldier>, Without<Downed>)>,
    mut warned: Local<HashSet<Entity>>,
    mut voices: Voices,
) {
    let kind = |projectile: &Projectile| {
        let weapon = armory.weapon(&projectile.weapon)?;
        if weapon.fire.kind != game_data::FireKind::Thrown || weapon.projectile.trigger.is_some() {
            return None;
        }
        if weapon.projectile.smoke.is_some() {
            Some("AUTO_MOODGP_throwingsmokegrenade")
        } else {
            weapon.projectile.explodes().then_some("AUTO_MOODGP_throwingfraggrenade")
        }
    };
    for (_, projectile) in &thrown {
        let Some(id) = kind(projectile) else { continue };
        if let Some((soldier, _, render)) = soldiers.iter().find(|(_, c, _)| c.0 == projectile.player)
            && let Some(speaker) = voices.speaker(projectile.player, render.position)
        {
            voices.call_out(soldier, &speaker, id);
        }
    }
    for (grenade, projectile, motion) in &live {
        if warned.contains(&grenade) || kind(projectile) != Some("AUTO_MOODGP_throwingfraggrenade") {
            continue;
        }
        let closest = soldiers
            .iter()
            .filter(|(_, c, _)| c.0 != projectile.player)
            .map(|(soldier, c, render)| (soldier, c.0, render.position, render.position.distance(motion.position)))
            .filter(|(.., distance)| *distance < GRENADE_WARNING)
            .min_by(|a, b| a.3.total_cmp(&b.3));
        if let Some((soldier, player, position, _)) = closest
            && let Some(speaker) = voices.speaker(player, position)
        {
            warned.insert(grenade);
            voices.call_out(soldier, &speaker, "AUTO_MOODGP_incominggrenade");
        }
    }
    warned.retain(|grenade| live.contains(*grenade));
}

/// A teammate we hit tells us to check our fire.
fn friendly_fire(
    mut hits: MessageReader<HitConfirmed>,
    soldiers: Query<(&ControlledBy, &SoldierRender), Without<LocalSoldier>>,
    local: Query<&Team, With<LocalPlayer>>,
    mut voices: Voices,
) {
    let local = local.single().copied().unwrap_or_default();
    for hit in hits.read() {
        let Ok((controlled_by, render)) = soldiers.get(hit.victim) else {
            continue;
        };
        if let Some(speaker) = voices.speaker(controlled_by.0, render.position)
            && speaker.team == local
            && local != Team::Spectator
        {
            voices.call_out(hit.victim, &speaker, "AUTO_MOODGP_friendlyfire");
        }
    }
}
