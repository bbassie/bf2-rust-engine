//! The commander's announcements, BF2's voice-overs in our team's language: flags captured
//! and lost, ticket bleed starting and stopping, and running low on tickets.

use bevy::prelude::*;
use game_data::TeamVoice;
use game_shared::{
    conquest::{FlagEvent, FlagEventKind, Tickets, team_index},
    level::LoadedLevel,
    protocol::Team,
};

use crate::net::LocalPlayer;

pub struct AnnouncerPlugin;

impl Plugin for AnnouncerPlugin {
    fn build(&self, app: &mut App) {
        app.init_resource::<Announcer>()
            .add_systems(Update, (announce_flags, announce_tickets));
    }
}

/// Announcements closer together than this are dropped rather than talked over.
const MIN_GAP: f64 = 2.5;
/// Below this share of the starting tickets we get the warning.
const LOW_TICKETS: f32 = 0.1;

#[derive(Resource, Default)]
struct Announcer {
    last: f64,
    bleeding: bool,
    warned_low: bool,
}

impl Announcer {
    fn say(&mut self, commands: &mut Commands, asset_server: &AssetServer, now: f64, lines: &[String]) {
        let Some(line) = fastrand::choice(lines) else {
            return;
        };
        if now - self.last < MIN_GAP {
            return;
        }
        self.last = now;
        commands.spawn((
            AudioPlayer::new(asset_server.load(format!("imported://{line}"))),
            PlaybackSettings::DESPAWN,
        ));
    }
}

fn voice<'a>(level: &'a LoadedLevel, team: Team) -> Option<&'a TeamVoice> {
    level.desc.teams.get(team_index(team)?).map(|t| &t.voice)
}

fn announce_flags(
    mut commands: Commands,
    time: Res<Time>,
    asset_server: Res<AssetServer>,
    level: Option<Res<LoadedLevel>>,
    players: Query<&Team, With<LocalPlayer>>,
    mut events: MessageReader<FlagEvent>,
    mut announcer: ResMut<Announcer>,
) {
    let (Some(level), Ok(local)) = (level, players.single()) else {
        events.clear();
        return;
    };
    let Some(voice) = voice(&level, *local) else {
        events.clear();
        return;
    };
    for event in events.read() {
        let ours = event.team == *local;
        let lines = match (event.kind, ours) {
            (FlagEventKind::Captured, true) => &voice.we_captured,
            (FlagEventKind::Captured, false) => &voice.enemy_captured,
            // The enemy took one of our flags down.
            (FlagEventKind::Neutralized, false) => &voice.we_lost,
            (FlagEventKind::Neutralized, true) => continue,
        };
        announcer.say(&mut commands, &asset_server, time.elapsed_secs_f64(), lines);
    }
}

fn announce_tickets(
    mut commands: Commands,
    time: Res<Time>,
    asset_server: Res<AssetServer>,
    level: Option<Res<LoadedLevel>>,
    players: Query<&Team, With<LocalPlayer>>,
    tickets: Query<&Tickets, Changed<Tickets>>,
    mut announcer: ResMut<Announcer>,
) {
    let (Some(level), Ok(local), Ok(tickets)) = (level, players.single(), tickets.single()) else {
        return;
    };
    let (Some(index), Some(voice)) = (team_index(*local), voice(&level, *local)) else {
        return;
    };
    let now = time.elapsed_secs_f64();
    let bleeding = tickets.bleed[index] > 0.0;
    if bleeding != announcer.bleeding {
        announcer.bleeding = bleeding;
        let lines = if bleeding { &voice.bleed_start } else { &voice.bleed_end };
        announcer.say(&mut commands, &asset_server, now, lines);
    }
    let low = tickets.remaining[index] < tickets.start[index] * LOW_TICKETS;
    if low && !announcer.warned_low {
        announcer.warned_low = true;
        announcer.last = 0.0;
        announcer.say(&mut commands, &asset_server, now, &voice.low_tickets);
    } else if !low {
        // A new round.
        announcer.warned_low = false;
    }
}
