//! Answers LAN browser queries (see `game_shared::discovery`) with the server's name, map
//! and player count, and announces the server to a master server if one is configured.

use std::net::{IpAddr, Ipv4Addr, SocketAddr, ToSocketAddrs, UdpSocket};

use bevy::prelude::*;
use bevy_replicon::prelude::*;
use game_shared::{
    PROTOCOL_ID,
    discovery::{
        DISCOVERY_PORTS, HEARTBEAT_SECONDS, MASTER_PORT, ServerInfo, encode_bye, encode_heartbeat_with, encode_reply,
        parse_query,
    },
    level::LoadedLevel,
    protocol::Player,
};

use crate::ServerSettings;

pub struct DiscoveryPlugin;

impl Plugin for DiscoveryPlugin {
    fn build(&self, app: &mut App) {
        app.add_systems(
            Update,
            (answer_queries, heartbeat)
                .run_if(resource_exists::<DiscoveryResponder>)
                .run_if(in_state(ClientState::Disconnected)),
        );
    }
}

#[derive(Resource)]
pub struct DiscoveryResponder {
    socket: UdpSocket,
    port: u16,
    master: Option<Master>,
}

impl DiscoveryResponder {
    /// The UDP port browser queries go to.
    pub fn query_port(&self) -> u16 {
        self.port
    }
}

/// The master server this server announces itself to.
struct Master {
    socket: UdpSocket,
    address: SocketAddr,
    /// Seconds until the next heartbeat.
    next: f32,
}

impl Master {
    fn connect(address: &str) -> std::io::Result<Self> {
        let address = (address, MASTER_PORT)
            .to_socket_addrs()
            .ok()
            .and_then(|mut a| a.next())
            .or_else(|| address.to_socket_addrs().ok()?.next())
            .ok_or_else(|| std::io::Error::other(format!("can't find `{address}`")))?;
        // Loopback masters get a loopback socket (tests without firewall prompts).
        let ip: IpAddr = if address.ip().is_loopback() { Ipv4Addr::LOCALHOST.into() } else { Ipv4Addr::UNSPECIFIED.into() };
        let socket = UdpSocket::bind((ip, 0))?;
        socket.set_nonblocking(true)?;
        Ok(Self { socket, address, next: 0.0 })
    }
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
        let master = settings.master_server.as_deref().and_then(|address| match Master::connect(address) {
            Ok(master) => {
                info!("announcing to master server {}", master.address);
                Some(master)
            }
            Err(err) => {
                warn!("master server: {err}");
                None
            }
        });
        world.insert_resource(DiscoveryResponder { socket, port, master });
        return;
    }
    warn!("no free discovery port: LAN browsers won't list this server");
}

pub fn stop(world: &mut World) {
    let game_port = world.resource::<ServerSettings>().port;
    if let Some(responder) = world.remove_resource::<DiscoveryResponder>()
        && let Some(master) = responder.master
    {
        let _ = master.socket.send_to(&encode_bye(game_port), master.address);
    }
}

/// Tells the master server we're still here, every half minute.
fn heartbeat(
    time: Res<Time<Real>>,
    settings: Res<ServerSettings>,
    mut responder: ResMut<DiscoveryResponder>,
    level: Option<Res<LoadedLevel>>,
    players: Query<&Player>,
) {
    let port = responder.port;
    let Some(master) = responder.master.as_mut() else {
        return;
    };
    master.next -= time.delta_secs();
    if master.next > 0.0 {
        return;
    }
    master.next = HEARTBEAT_SECONDS;
    let bots = players.iter().filter(|p| p.is_bot).count();
    let humans = players.iter().count() - bots;
    let level_name = level.as_ref().map_or_else(|| settings.level.clone(), |l| l.desc.display_name.clone());
    let text = format!("{}\n{level_name}\n{}", settings.name, settings.mode);
    let packet = encode_heartbeat_with(settings.port, port, humans as u16, settings.max_clients as u16, bots as u16, &text);
    if let Err(err) = master.socket.send_to(&packet, master.address) {
        warn!("master server {}: {err}", master.address);
    }
}

fn answer_queries(
    responder: Res<DiscoveryResponder>,
    settings: Res<ServerSettings>,
    level: Option<Res<LoadedLevel>>,
    players: Query<&Player>,
    content: Option<Res<crate::content::ContentServer>>,
    accounts: Option<Res<crate::accounts::Accounts>>,
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
                content: content.as_ref().map(|c| c.advert()),
                ranked: accounts.as_ref().is_some_and(|a| a.required()),
            }
        });
        let _ = responder.socket.send_to(&encode_reply(token, info), from);
    }
}
