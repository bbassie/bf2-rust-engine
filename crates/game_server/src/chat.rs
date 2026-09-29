//! The server side of the chat: relays what players type to everyone, their team or their
//! squad, announces leaves (joins and the message of the day go out with the client's hello,
//! see `receive_hello`), and runs admin commands typed by the host or by players who logged
//! in with `/login`. Lines and commands are flood-limited, failed logins locked out
//! ([`crate::limits`]).

use std::net::IpAddr;

use bevy::prelude::*;
use bevy_replicon::{prelude::*, shared::backend::connected_client::NetworkId};
use bevy_replicon_renet::netcode::NetcodeServerTransport;
use game_shared::{
    chat::{ChatChannel, ChatLine, ChatRequest, MAX_CHAT_LENGTH, clean_text},
    protocol::{Player, Team},
    squad::SquadMember,
};

use crate::{
    ClientPlayer, HostPlayer, PlayerClient, ServerSettings,
    admin::{self, Admin},
    limits::{Failed, LoginBackoff, Rate, RateLimiter, client_ip, constant_time_eq},
    sender_player,
};

pub struct ChatPlugin;

impl Plugin for ChatPlugin {
    fn build(&self, app: &mut App) {
        app.add_observer(announce_leave).add_systems(
            PreUpdate,
            receive_chat
                .after(ServerSystems::Receive)
                .run_if(in_state(ClientState::Disconnected)),
        );
    }
}

/// Longest admin answer shown in the chat, in lines.
const MAX_REPLY_LINES: usize = 24;

/// Where to send messages for `player`: its connection, or this app for the host.
pub fn client_of(world: &World, player: Entity) -> Option<ClientId> {
    if let Some(client) = world.get::<PlayerClient>(player) {
        return Some(ClientId::Client(client.0));
    }
    (world.get_resource::<HostPlayer>().map(|h| h.0) == Some(player)).then_some(ClientId::Server)
}

/// A server message to everyone.
pub fn announce(world: &mut World, text: impl Into<String>) {
    let text = text.into();
    info!("server: {text}");
    world.write_message(ToClients {
        targets: SendTargets::All,
        message: ChatLine::server(text),
    });
}

/// A server message to one player, line by line.
pub fn tell(world: &mut World, player: Entity, text: &str) {
    let Some(target) = client_of(world, player) else {
        return;
    };
    for line in text.lines().take(MAX_REPLY_LINES) {
        world.write_message(ToClients {
            targets: SendTargets::Single(target),
            message: ChatLine::private(line),
        });
    }
}

fn announce_leave(
    remove: On<Remove, ConnectedClient>,
    clients: Query<&ClientPlayer>,
    players: Query<&Player>,
    mut lines: MessageWriter<ToClients<ChatLine>>,
) {
    let Ok(player) = clients.get(remove.entity).and_then(|c| players.get(c.0)) else {
        return;
    };
    info!("server: {} left the game", player.name);
    lines.write(ToClients {
        targets: SendTargets::All,
        message: ChatLine::server(format!("{} left the game", player.name)),
    });
}

/// Who failed to log in: the address of a remote player, else the player.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
enum LoginKey {
    Address(IpAddr),
    Player(Entity),
}

/// The chat's flood limits and failed logins.
#[derive(Default)]
struct ChatLimits {
    lines: RateLimiter<Entity>,
    commands: RateLimiter<Entity>,
    logins: LoginBackoff<LoginKey>,
}

#[allow(clippy::too_many_arguments, clippy::type_complexity)]
fn receive_chat(
    mut commands: Commands,
    time: Res<Time<Real>>,
    settings: Res<ServerSettings>,
    mut requests: MessageReader<FromClient<ChatRequest>>,
    clients: Query<&ClientPlayer>,
    network_ids: Query<&NetworkId>,
    transport: Option<Res<NetcodeServerTransport>>,
    host: Option<Res<HostPlayer>>,
    players: Query<(Entity, &Player, &Team, Option<&SquadMember>, Option<&PlayerClient>, Has<Admin>)>,
    mut lines: MessageWriter<ToClients<ChatLine>>,
    mut limits: Local<ChatLimits>,
) {
    let now = time.elapsed_secs_f64();
    let host_player = host.as_ref().map(|h| h.0);
    for request in requests.read() {
        let Some(sender) = sender_player(request.client_id, &clients, host.as_deref()) else {
            continue;
        };
        let Ok((_, player, team, squad, _, logged_in)) = players.get(sender) else {
            continue;
        };
        let text = clean_text(&request.message.text, MAX_CHAT_LENGTH);
        if text.is_empty() {
            continue;
        }
        let private = |text: &str| ToClients {
            targets: SendTargets::Single(request.client_id),
            message: ChatLine::private(text),
        };

        if let Some(command) = text.strip_prefix(['/', '!']) {
            // Commands, `/login` above all, have their own flood limit.
            if !limits.commands.allow(sender, Rate::CHAT_COMMANDS, now) {
                lines.write(private("You are sending commands too fast."));
                continue;
            }
            let command = command.trim().to_string();
            let (word, rest) = command.split_once(' ').unwrap_or((&command, ""));
            match word.to_ascii_lowercase().as_str() {
                "login" => {
                    // By address, so reconnecting doesn't start over.
                    let key = match request.client_id {
                        ClientId::Client(client) => network_ids
                            .get(client)
                            .ok()
                            .and_then(|id| client_ip(id, transport.as_deref()))
                            .map_or(LoginKey::Player(sender), LoginKey::Address),
                        ClientId::Server => LoginKey::Player(sender),
                    };
                    if let Some(left) = limits.logins.locked(key, now) {
                        lines.write(private(&format!("Too many failed logins. Try again in {:.0} s.", left.ceil())));
                        continue;
                    }
                    let password = &settings.admin.password;
                    if !password.is_empty() && constant_time_eq(rest.trim().as_bytes(), password.as_bytes()) {
                        limits.logins.succeed(key);
                        info!("{} logged in as admin", player.name);
                        commands.entity(sender).insert(Admin);
                        lines.write(private("Logged in as admin. Type /help for the commands."));
                        continue;
                    }
                    match limits.logins.fail(key, now) {
                        Failed::TriesLeft(_) => {
                            info!("{} failed to log in as admin", player.name);
                            lines.write(private("Wrong admin password."));
                        }
                        Failed::LockedOut(seconds) => {
                            warn!("{} failed to log in as admin too often: kicked, locked out for {seconds:.0} s", player.name);
                            let reason = format!("Too many failed admin logins. Try again in {seconds:.0} s.");
                            commands.queue(move |world: &mut World| {
                                let _ = admin::commands::kick(world, sender, &reason, false);
                            });
                        }
                    }
                }
                "logout" => {
                    commands.entity(sender).remove::<Admin>();
                    lines.write(private("Logged out."));
                }
                _ if logged_in || Some(sender) == host_player => {
                    let admin = player.name.clone();
                    commands.queue(move |world: &mut World| {
                        let reply = admin::commands::execute(world, &command, &admin);
                        tell(world, sender, &reply);
                    });
                }
                _ => {
                    lines.write(private("Admin commands need /login <password> first."));
                }
            }
            continue;
        }

        if !limits.lines.allow(sender, Rate::CHAT, now) {
            lines.write(private("You are sending messages too fast."));
            continue;
        }

        let channel = request.message.channel;
        let line = ChatLine {
            channel,
            sender: Some(player.name.clone()),
            team: *team,
            text: text.clone(),
        };
        info!("chat [{channel:?}] {}: {text}", player.name);
        let same_group = |other_team: &Team, other_squad: Option<&SquadMember>| match channel {
            ChatChannel::Team => other_team == team,
            ChatChannel::Squad => {
                other_team == team && other_squad.map(|s| s.squad) == squad.map(|s| s.squad)
            }
            _ => false,
        };
        match channel {
            ChatChannel::All => {
                lines.write(ToClients {
                    targets: SendTargets::All,
                    message: line,
                });
            }
            ChatChannel::Squad if squad.is_none() => {
                lines.write(private("You are not in a squad."));
            }
            ChatChannel::Team | ChatChannel::Squad => {
                for (entity, _, other_team, other_squad, client, _) in &players {
                    if !same_group(other_team, other_squad) {
                        continue;
                    }
                    let target = match client {
                        Some(client) => ClientId::Client(client.0),
                        None if Some(entity) == host_player => ClientId::Server,
                        None => continue,
                    };
                    lines.write(ToClients {
                        targets: SendTargets::Single(target),
                        message: line.clone(),
                    });
                }
            }
            // Only the server speaks on these.
            ChatChannel::Server | ChatChannel::Private => {}
        }
    }
}
