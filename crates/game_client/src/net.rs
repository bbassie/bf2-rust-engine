//! Connecting to a server and working out which entities are ours.

use std::{
    net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr, UdpSocket},
    time::SystemTime,
};

use bevy::prelude::*;
use bevy_replicon::prelude::*;
use bevy_replicon_renet::{
    RenetChannelsExt, RenetClient,
    netcode::{ClientAuthentication, NetcodeClientTransport},
    renet::ConnectionConfig,
};
use game_shared::{
    PROTOCOL_ID,
    protocol::{ClientHello, ControlledBy, Player, PlayerNetId},
    soldier::{Soldier, SoldierMotion},
};

use crate::{Cli, local_input::LookState};

pub struct NetPlugin;

impl Plugin for NetPlugin {
    fn build(&self, app: &mut App) {
        app.add_systems(Startup, connect)
            .add_systems(OnEnter(ClientState::Connected), send_hello)
            .add_systems(OnEnter(ClientState::Connected), || info!("connected"))
            .add_systems(OnExit(ClientState::Connected), || warn!("disconnected"))
            .add_systems(PreUpdate, tag_local_entities.after(ClientSystems::Receive));
    }
}

/// Our network id; matches [`PlayerNetId`] of our player.
#[derive(Resource, Clone, Copy, Debug)]
pub struct LocalClientId(pub u64);

/// The player we are.
#[derive(Component)]
pub struct LocalPlayer;

/// The soldier we control.
#[derive(Component)]
pub struct LocalSoldier;

fn connect(mut commands: Commands, cli: Res<Cli>, channels: Res<RepliconChannels>) -> Result<()> {
    let Some(ip) = cli.connect else {
        commands.insert_resource(LocalClientId(PlayerNetId::LOCAL_HOST.0));
        return Ok(());
    };

    let client = RenetClient::new(ConnectionConfig {
        server_channels_config: channels.server_configs(),
        client_channels_config: channels.client_configs(),
        ..default()
    });
    let current_time = SystemTime::now().duration_since(SystemTime::UNIX_EPOCH)?;
    // Never 0, which is reserved for the local host player.
    let client_id = (current_time.as_nanos() as u64).max(1);
    let bind_ip: IpAddr = match ip {
        IpAddr::V4(_) => Ipv4Addr::UNSPECIFIED.into(),
        IpAddr::V6(_) => Ipv6Addr::UNSPECIFIED.into(),
    };
    let socket = UdpSocket::bind((bind_ip, 0))?;
    let authentication = ClientAuthentication::Unsecure {
        client_id,
        protocol_id: PROTOCOL_ID,
        server_addr: SocketAddr::new(ip, cli.port),
        user_data: None,
    };
    let transport = NetcodeClientTransport::new(current_time, authentication, socket)?;

    commands.insert_resource(client);
    commands.insert_resource(transport);
    commands.insert_resource(LocalClientId(client_id));
    info!("connecting to {ip}:{}", cli.port);
    Ok(())
}

fn send_hello(mut hello: MessageWriter<ClientHello>, cli: Res<Cli>) {
    hello.write(ClientHello {
        name: cli.name.clone(),
    });
}

fn tag_local_entities(
    mut commands: Commands,
    local_id: Option<Res<LocalClientId>>,
    players: Query<(Entity, &PlayerNetId, Has<LocalPlayer>), With<Player>>,
    soldiers: Query<(Entity, &ControlledBy, &SoldierMotion), (With<Soldier>, Without<LocalSoldier>)>,
    mut look: ResMut<LookState>,
) {
    let Some(local_id) = local_id else {
        return;
    };
    let Some((local_player, _, tagged)) = players.iter().find(|(_, id, _)| id.0 == local_id.0)
    else {
        return;
    };
    if !tagged {
        commands.entity(local_player).insert(LocalPlayer);
    }
    for (soldier, controlled_by, motion) in &soldiers {
        if controlled_by.0 == local_player {
            commands.entity(soldier).insert(LocalSoldier);
            // Look where the soldier spawned facing.
            look.yaw = motion.yaw;
            look.pitch = 0.0;
        }
    }
}
