//! The server side of the chat: relays what players type to everyone, their team or their
//! squad, greets joining players with the message of the day, announces joins and leaves,
//! and runs admin commands typed by the host or by players who logged in with `/login`.

use std::collections::VecDeque;

use bevy::{platform::collections::HashMap, prelude::*};
use bevy_replicon::prelude::*;
use game_shared::{
    chat::{ChatChannel, ChatLine, ChatRequest, MAX_CHAT_LENGTH, clean_text},
    protocol::{ClientHello, Player, Team},
    squad::SquadMember,
};

use crate::{
    ClientPlayer, HostPlayer, PlayerClient, ServerSettings,
    admin::{self, Admin},
    sender_player,
};

pub struct ChatPlugin;

impl Plugin for ChatPlugin {
    fn build(&self, app: &mut App) {
        app.add_observer(announce_leave).add_systems(
            PreUpdate,
            (greet_players, receive_chat)
                .after(ServerSystems::Receive)
                .run_if(in_state(ClientState::Disconnected)),
        );
    }
}

/// More lines than this within [`FLOOD_SECONDS`] are dropped.
const FLOOD_LINES: usize = 5;
const FLOOD_SECONDS: f32 = 5.0;
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

/// Says hello to players when they tell us their name, and tells everyone else.
fn greet_players(
    mut hellos: MessageReader<FromClient<ClientHello>>,
    clients: Query<&ClientPlayer>,
    players: Query<&Player>,
    settings: Res<ServerSettings>,
    mut lines: MessageWriter<ToClients<ChatLine>>,
) {
    for hello in hellos.read() {
        let name = clean_text(&hello.message.name, 24);
        let name = match hello.client_id {
            _ if !name.is_empty() => name,
            ClientId::Client(client) => clients
                .get(client)
                .and_then(|c| players.get(c.0))
                .map_or_else(|_| "Someone".into(), |p| p.name.clone()),
            ClientId::Server => continue,
        };
        info!("server: {name} joined the game");
        lines.write(ToClients {
            targets: SendTargets::AllExcept(hello.client_id),
            message: ChatLine::server(format!("{name} joined the game")),
        });
        let mut greeting = vec![format!("Welcome to {}, {name}!", settings.name)];
        greeting.extend(settings.admin.motd.lines().map(str::to_string));
        for line in greeting {
            lines.write(ToClients {
                targets: SendTargets::Single(hello.client_id),
                message: ChatLine::private(line),
            });
        }
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

#[allow(clippy::too_many_arguments, clippy::type_complexity)]
fn receive_chat(
    mut commands: Commands,
    time: Res<Time<Real>>,
    settings: Res<ServerSettings>,
    mut requests: MessageReader<FromClient<ChatRequest>>,
    clients: Query<&ClientPlayer>,
    host: Option<Res<HostPlayer>>,
    players: Query<(Entity, &Player, &Team, Option<&SquadMember>, Option<&PlayerClient>, Has<Admin>)>,
    mut lines: MessageWriter<ToClients<ChatLine>>,
    mut recent: Local<HashMap<Entity, VecDeque<f32>>>,
) {
    let now = time.elapsed_secs();
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
            let command = command.trim().to_string();
            let (word, rest) = command.split_once(' ').unwrap_or((&command, ""));
            match word.to_ascii_lowercase().as_str() {
                "login" => {
                    let password = &settings.admin.password;
                    if !password.is_empty() && rest.trim() == password {
                        info!("{} logged in as admin", player.name);
                        commands.entity(sender).insert(Admin);
                        lines.write(private("Logged in as admin. Type /help for the commands."));
                    } else {
                        warn!("{} failed to log in as admin", player.name);
                        lines.write(private("Wrong admin password."));
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

        let history = recent.entry(sender).or_default();
        history.retain(|at| now - at < FLOOD_SECONDS);
        if history.len() >= FLOOD_LINES {
            lines.write(private("You are sending messages too fast."));
            continue;
        }
        history.push_back(now);

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
    recent.retain(|player, _| players.contains(*player));
}
