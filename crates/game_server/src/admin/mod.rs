//! Server administration: admin commands ([`commands`]) typed into the remote console
//! ([`rcon`]) or into the chat by the host and logged-in admins, and the ban list
//! ([`bans`]).

use std::path::PathBuf;

use bevy::prelude::*;
use bevy_replicon::prelude::*;
use game_auth::token::Claims;
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
    /// Accounts that get [`Admin`] on join, on servers that require accounts (see
    /// [`account_is_admin`]): an entry `id:<account id>` matches by id, anything else matches
    /// the verified account name case-insensitively. Removing an entry takes effect on the
    /// account's next join.
    pub admins: Vec<String>,
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
            admins: Vec::new(),
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

/// Whether a verified account matches the server's `admins` list (`admin_password`'s
/// `/login` is a separate, unrelated path; see the module docs and `chat::receive_chat`).
/// `id:<account id>` matches by id; any other entry matches the account name
/// case-insensitively.
pub fn account_is_admin(admins: &[String], claims: &Claims) -> bool {
    admins.iter().any(|entry| match entry.strip_prefix("id:") {
        Some(id) => id.parse::<u64>().is_ok_and(|id| id == claims.sub),
        None => entry.eq_ignore_ascii_case(&claims.name),
    })
}

/// Grants `player` admin rights and tells them, if their verified account matches `admins`.
/// No-op otherwise. Safe to call whether the player or its account showed up first (join
/// order: see `join::receive_tickets` and `create_client_player`).
pub fn grant_if_admin(commands: &mut Commands, admins: &[String], player: Entity, claims: &Claims) {
    if !account_is_admin(admins, claims) {
        return;
    }
    info!("account {} ({}) is an admin on this server", claims.name, claims.sub);
    commands.entity(player).insert(Admin);
    let name = claims.name.clone();
    commands.queue(move |world: &mut World| {
        crate::chat::tell(world, player, &format!("Signed in as admin (account {name})."));
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    fn claims(sub: u64, name: &str) -> Claims {
        Claims {
            v: game_auth::token::TOKEN_VERSION,
            kind: game_auth::token::TokenKind::Ticket,
            sub,
            name: name.into(),
            rank: 0,
            rank_name: "Private".into(),
            rank_short: "Pvt".into(),
            iat: 0,
            exp: 0,
            aud: None,
            jti: "t".into(),
        }
    }

    #[test]
    fn matches_by_name_case_insensitively() {
        let admins = vec!["Alice".to_string()];
        assert!(account_is_admin(&admins, &claims(1, "alice")));
        assert!(account_is_admin(&admins, &claims(1, "ALICE")));
        assert!(account_is_admin(&admins, &claims(1, "Alice")));
    }

    #[test]
    fn matches_by_id() {
        let admins = vec!["id:42".to_string()];
        assert!(account_is_admin(&admins, &claims(42, "bob")));
        // A different account with the same name as the digits isn't an id match.
        assert!(!account_is_admin(&admins, &claims(1, "42")));
    }

    #[test]
    fn unlisted_account_is_not_admin() {
        let admins = vec!["alice".to_string(), "id:42".to_string()];
        assert!(!account_is_admin(&admins, &claims(7, "carol")));
    }

    #[test]
    fn empty_list_matches_nobody() {
        assert!(!account_is_admin(&[], &claims(1, "alice")));
    }

    #[test]
    fn malformed_id_entry_matches_nobody() {
        let admins = vec!["id:not-a-number".to_string()];
        assert!(!account_is_admin(&admins, &claims(1, "alice")));
    }
}
