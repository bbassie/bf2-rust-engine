//! The server's messaging backend for Replicon: renet (netcode over UDP) for clients on
//! the network, and in-memory links for the client of the same process (singleplayer and
//! hosting run this server on a thread of its own, see [`crate::embedded`]).
//!
//! It takes the place of `bevy_replicon_renet`'s server plugin, whose systems drain all of
//! [`ServerMessages`] into renet: here each client's messages go the way it came, a link
//! client's straight into its channel.

use bevy::prelude::*;
use bevy_replicon::{
    bytes::Bytes,
    prelude::*,
    shared::backend::connected_client::{NetworkId, NetworkIdMap},
};
use bevy_replicon_renet::{
    RenetReceive, RenetSend, RenetServer, RenetServerEvent, RenetServerPlugin, netcode::NetcodeServerPlugin,
    renet::ServerEvent,
};
use crossbeam_channel::{Receiver, Sender, TryRecvError};

/// Replicon over renet and in-memory links (see the module docs).
pub struct ServerTransportPlugin;

impl Plugin for ServerTransportPlugin {
    fn build(&self, app: &mut App) {
        app.add_plugins((RenetServerPlugin, NetcodeServerPlugin))
            .configure_sets(PreUpdate, ServerSystems::ReceivePackets.after(RenetReceive))
            .configure_sets(PostUpdate, ServerSystems::SendPackets.before(RenetSend))
            .add_observer(process_server_events)
            .add_observer(disconnect_client)
            .add_systems(
                PreUpdate,
                (update_state, receive_packets).chain().in_set(ServerSystems::ReceivePackets),
            )
            .add_systems(
                PostUpdate,
                (
                    send_packets.in_set(ServerSystems::SendPackets),
                    // After sending, so clients get their last messages before going.
                    disconnect_by_request.after(RenetSend),
                ),
            );
    }
}

/// Serving without a network (singleplayer): the server runs for its link clients.
#[derive(Resource, Default)]
pub struct LinksOnly;

/// On a client entity: a client in this process, connected through channels. Its messages
/// are `(channel id, message)`.
#[derive(Component)]
pub struct LinkClient {
    pub to_client: Sender<(usize, Bytes)>,
    pub from_client: Receiver<(usize, Bytes)>,
}

/// Running while renet listens or link clients are served.
fn update_state(
    renet: Option<Res<RenetServer>>,
    links: Option<Res<LinksOnly>>,
    state: Res<State<ServerState>>,
    mut next: ResMut<NextState<ServerState>>,
) {
    let wanted = if renet.is_some() || links.is_some() {
        ServerState::Running
    } else {
        ServerState::Stopped
    };
    if *state.get() != wanted {
        next.set(wanted);
    }
}

fn process_server_events(event: On<RenetServerEvent>, mut commands: Commands, network_map: Res<NetworkIdMap>) {
    match **event {
        ServerEvent::ClientConnected { client_id } => {
            let network_id = NetworkId::new(client_id);
            // renet's packet size (renet/src/packet.rs).
            let client = commands.spawn((ConnectedClient { max_size: 1200 }, network_id)).id();
            debug!("spawning client `{client}` with `{network_id:?}`");
        }
        ServerEvent::ClientDisconnected { client_id, reason } => {
            let network_id = NetworkId::new(client_id);
            if let Some(&client) = network_map.get(&network_id) {
                commands.entity(client).try_despawn();
                debug!("despawning client `{client}` with `{network_id:?}`: {reason}");
            }
        }
    }
}

fn receive_packets(
    mut commands: Commands,
    channels: Res<RepliconChannels>,
    server: Option<ResMut<RenetServer>>,
    mut messages: ResMut<ServerMessages>,
    mut clients: Query<(Entity, &NetworkId, &mut ConnectedClientStats), Without<LinkClient>>,
    links: Query<(Entity, &LinkClient)>,
) {
    if let Some(mut server) = server {
        for (client, network_id, mut stats) in &mut clients {
            for channel_id in 0..channels.client_channels().len() as u8 {
                while let Some(message) = server.receive_message(network_id.get(), channel_id) {
                    messages.insert_received(client, channel_id, message);
                }
            }
            // Renet's events are read in parallel: the client may be gone already.
            if let Ok(info) = server.network_info(network_id.get()) {
                stats.rtt = info.rtt;
                stats.packet_loss = info.packet_loss;
                stats.sent_bps = info.bytes_sent_per_second;
                stats.received_bps = info.bytes_received_per_second;
            }
        }
    }
    for (client, link) in &links {
        loop {
            match link.from_client.try_recv() {
                Ok((channel_id, message)) => messages.insert_received(client, channel_id, message),
                Err(TryRecvError::Empty) => break,
                Err(TryRecvError::Disconnected) => {
                    info!("link client `{client}` left");
                    commands.entity(client).try_despawn();
                    break;
                }
            }
        }
    }
}

fn send_packets(
    server: Option<ResMut<RenetServer>>,
    mut messages: ResMut<ServerMessages>,
    clients: Query<(&NetworkId, Option<&LinkClient>)>,
) {
    let mut server = server;
    for (client, channel_id, message) in messages.drain_sent() {
        let Ok((network_id, link)) = clients.get(client) else {
            continue;
        };
        match (link, server.as_mut()) {
            // A client that is gone shows up as gone on the next receive.
            (Some(link), _) => {
                let _ = link.to_client.send((channel_id, message));
            }
            (None, Some(server)) => server.send_message(network_id.get(), channel_id as u8, message),
            (None, None) => {}
        }
    }
}

fn disconnect_by_request(mut commands: Commands, mut disconnects: MessageReader<DisconnectRequest>) {
    for disconnect in disconnects.read() {
        debug!("despawning client `{}` by disconnect request", disconnect.client);
        commands.entity(disconnect.client).try_despawn();
    }
}

fn disconnect_client(
    remove: On<Remove, ConnectedClient>,
    server: Option<ResMut<RenetServer>>,
    clients: Query<&NetworkId, Without<LinkClient>>,
) {
    if let (Some(mut server), Ok(network_id)) = (server, clients.get(remove.entity)) {
        debug!("disconnecting despawned client `{}`", remove.entity);
        server.disconnect(network_id.get());
    }
}
