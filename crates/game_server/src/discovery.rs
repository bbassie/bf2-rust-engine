//! Answers LAN browser queries (see `game_shared::discovery`) with the server's name, map
//! and player count.

use std::net::{Ipv4Addr, UdpSocket};

use bevy::prelude::*;
use bevy_replicon::prelude::*;
use game_shared::{
    PROTOCOL_ID,
    discovery::{DISCOVERY_PORTS, ServerInfo, encode_reply, parse_query},
    level::LoadedLevel,
    protocol::Player,
};

use crate::ServerSettings;

pub struct DiscoveryPlugin;

impl Plugin for DiscoveryPlugin {
    fn build(&self, app: &mut App) {
        app.add_systems(
            Update,
            answer_queries
                .run_if(resource_exists::<DiscoveryResponder>)
                .run_if(in_state(ClientState::Disconnected)),
        );
    }
}

#[derive(Resource)]
pub struct DiscoveryResponder {
    socket: UdpSocket,
}

/// Listens for browser queries on the first free discovery port, on the same interfaces
/// as the game (only this machine unless the server is public).
pub fn start(world: &mut World) {
    let settings = world.resource::<ServerSettings>();
    if !settings.network {
        return;
    }
    let ip = if settings.public { Ipv4Addr::UNSPECIFIED } else { Ipv4Addr::LOCALHOST };
    for port in DISCOVERY_PORTS {
        let Ok(socket) = UdpSocket::bind((ip, port)) else {
            continue;
        };
        if socket.set_nonblocking(true).is_err() {
            continue;
        }
        info!("answering server browser queries on UDP port {port}");
        world.insert_resource(DiscoveryResponder { socket });
        return;
    }
    warn!("no free discovery port: LAN browsers won't list this server");
}

pub fn stop(world: &mut World) {
    world.remove_resource::<DiscoveryResponder>();
}

fn answer_queries(
    responder: Res<DiscoveryResponder>,
    settings: Res<ServerSettings>,
    level: Option<Res<LoadedLevel>>,
    players: Query<&Player>,
) {
    let mut buffer = [0u8; 64];
    let mut info: Option<ServerInfo> = None;
    // A handful per frame is plenty; the rest waits.
    for _ in 0..16 {
        let Ok((len, from)) = responder.socket.recv_from(&mut buffer) else {
            return;
        };
        let Some(token) = parse_query(&buffer[..len]) else {
            continue;
        };
        let info = info.get_or_insert_with(|| {
            let bots = players.iter().filter(|p| p.is_bot).count() as u32;
            ServerInfo {
                name: settings.name.clone(),
                level: settings.level.clone(),
                level_name: level
                    .as_ref()
                    .map_or_else(|| settings.level.clone(), |l| l.desc.display_name.clone()),
                mode: settings.mode.clone(),
                size: settings.size,
                players: players.iter().count() as u32 - bots,
                max_players: settings.max_clients as u32,
                bots,
                port: settings.port,
                protocol: PROTOCOL_ID,
            }
        });
        let _ = responder.socket.send_to(&encode_reply(token, info), from);
    }
}
