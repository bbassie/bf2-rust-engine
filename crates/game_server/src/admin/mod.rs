//! Server administration: admin commands ([`commands`]) typed into the remote console
//! ([`rcon`]) or into the chat by the host and logged-in admins, and the ban list
//! ([`bans`]).

use std::path::PathBuf;

use bevy::prelude::*;
use bevy_replicon::prelude::*;
use game_shared::protocol::Player;

use crate::ServerSettings;

pub mod bans;
pub mod commands;
pub mod rcon;

/// How the server is administered.
#[derive(Clone, Debug)]
pub struct AdminSettings {
    /// Password for the remote console and `/login` in the chat. Empty turns both off.
    pub password: String,
    /// TCP port of the remote console (BF2's is 4711); 0 turns it off.
    pub rcon_port: u16,
    /// Accept remote console connections from other machines, not just this one.
    pub rcon_public: bool,
    /// Sent to every player who joins.
    pub motd: String,
    /// Where bans are kept; in memory only if unset.
    pub ban_file: Option<PathBuf>,
    /// Where player stats are kept; in memory only if unset.
    pub stats_file: Option<PathBuf>,
}

impl Default for AdminSettings {
    fn default() -> Self {
        Self {
            password: String::new(),
            rcon_port: rcon::DEFAULT_PORT,
            rcon_public: false,
            motd: String::new(),
            ban_file: None,
            stats_file: None,
        }
    }
}

pub struct AdminPlugin;

impl Plugin for AdminPlugin {
    fn build(&self, app: &mut App) {
        app.init_resource::<bans::BanList>()
            .init_resource::<NextPlayerId>()
            .add_systems(
                Update,
                (
                    number_players,
                    bans::enforce_bans,
                    rcon::process_requests.run_if(resource_exists::<rcon::RconServer>),
                )
                    .chain()
                    .run_if(in_state(ClientState::Disconnected)),
            );
    }
}

/// Server-side: the number admin commands know a player by.
#[derive(Component, Clone, Copy, Debug, PartialEq, Eq)]
pub struct PlayerId(pub u32);

/// Server-side: this player logged in with the admin password and may use admin commands.
#[derive(Component)]
pub struct Admin;

#[derive(Resource, Default)]
struct NextPlayerId(u32);

fn number_players(
    mut commands: Commands,
    mut next: ResMut<NextPlayerId>,
    players: Query<Entity, (With<Player>, Without<PlayerId>)>,
) {
    for player in &players {
        next.0 += 1;
        commands.entity(player).insert(PlayerId(next.0));
    }
}

/// Loads the ban list and opens the remote console, if configured.
pub fn start(world: &mut World) {
    let settings = world.resource::<ServerSettings>().admin.clone();
    world.insert_resource(bans::BanList::load(settings.ban_file.clone()));
    world.insert_resource(NextPlayerId::default());
    if settings.password.is_empty() || settings.rcon_port == 0 {
        return;
    }
    match rcon::RconServer::start(&settings) {
        Ok(server) => {
            info!("remote console on TCP port {}", settings.rcon_port);
            world.insert_resource(server);
        }
        Err(err) => error!("can't open the remote console on port {}: {err}", settings.rcon_port),
    }
}

/// Closes the remote console.
pub fn stop(world: &mut World) {
    world.remove_resource::<rcon::RconServer>();
}
