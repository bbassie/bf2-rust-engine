//! Server authority.
//!
//! [`GameServerPlugin`] runs the match: it accepts connections, creates players, spawns
//! soldiers, applies inputs and drives bots. The same plugin powers the headless dedicated
//! server and the listen server / singleplayer modes of the client.

use std::{
    collections::VecDeque,
    net::{Ipv4Addr, UdpSocket},
    time::SystemTime,
};

use avian3d::prelude::*;
use bevy::prelude::*;
use bevy_replicon::{prelude::*, shared::backend::connected_client::NetworkId};
use bevy_replicon_renet::{
    RenetChannelsExt, RenetServer,
    netcode::{NetcodeServerTransport, ServerAuthentication, ServerConfig},
    renet::ConnectionConfig,
};
use game_shared::{
    PROTOCOL_ID,
    input::{InputFrame, InputPacket},
    level::LoadedLevel,
    protocol::{ClientHello, ControlledBy, MatchInfo, Player, PlayerNetId, Team},
    soldier::{InputAck, Soldier, SoldierMotion, SoldierShapes, SoldierTuning, step_soldier},
};

pub mod bots;

/// How the server was configured to run.
#[derive(Resource, Clone, Debug)]
pub struct ServerSettings {
    /// Level folder name, or `test_range`.
    pub level: String,
    pub mode: String,
    pub size: u32,
    /// Number of AI soldiers to add.
    pub bots: u32,
    pub max_clients: usize,
    pub port: u16,
    /// Accept connections over the network. Off for singleplayer.
    pub network: bool,
    /// Create a player for the local user (listen server / singleplayer).
    pub local_player: Option<String>,
    pub respawn_seconds: f32,
}

impl Default for ServerSettings {
    fn default() -> Self {
        Self {
            level: game_shared::level::TEST_RANGE.into(),
            mode: "gpm_cq".into(),
            size: 16,
            bots: 0,
            max_clients: 64,
            port: game_shared::DEFAULT_PORT,
            network: true,
            local_player: None,
            respawn_seconds: 5.0,
        }
    }
}

pub struct GameServerPlugin {
    pub settings: ServerSettings,
}

impl Plugin for GameServerPlugin {
    fn build(&self, app: &mut App) {
        app.insert_resource(self.settings.clone())
            .add_plugins(bots::BotPlugin)
            .add_systems(Startup, (start_networking, start_match))
            .add_observer(create_client_player)
            .add_observer(remove_client_player)
            .add_systems(
                PreUpdate,
                (receive_hello, receive_inputs)
                    .after(ServerSystems::Receive)
                    .run_if(in_state(ClientState::Disconnected)),
            )
            .add_systems(
                FixedUpdate,
                apply_inputs
                    .in_set(ServerSimSystems::ApplyInputs)
                    .run_if(in_state(ClientState::Disconnected)),
            )
            .add_systems(
                Update,
                respawn_players
                    .run_if(resource_exists::<LoadedLevel>)
                    .run_if(in_state(ClientState::Disconnected)),
            );
    }
}

/// Ordering for server simulation systems in `FixedUpdate`.
#[derive(SystemSet, Debug, Clone, PartialEq, Eq, Hash)]
pub enum ServerSimSystems {
    /// Bots decide what to press.
    Think,
    /// Inputs are applied to soldiers.
    ApplyInputs,
}

/// Server-side: the connected client entity behind a player.
#[derive(Component)]
pub struct PlayerClient(pub Entity);

/// Server-side: the player created for a connected client.
#[derive(Component)]
pub struct ClientPlayer(pub Entity);

/// Server-side: the soldier a player currently controls.
#[derive(Component)]
pub struct Controls(pub Entity);

/// Server-side: counts down until a player without a soldier respawns.
#[derive(Component)]
pub struct RespawnTimer(pub Timer);

/// The local user's player on a listen server or in singleplayer.
#[derive(Resource)]
pub struct HostPlayer(pub Entity);

/// Inputs received for a player, applied one per tick.
#[derive(Component, Default)]
pub struct InputBuffer {
    queue: VecDeque<InputFrame>,
    last_received: Option<u32>,
    current: InputFrame,
}

impl InputBuffer {
    /// Frames beyond this are dropped to keep input latency bounded.
    const MAX_QUEUED: usize = 6;

    pub fn push(&mut self, frame: InputFrame) {
        if self.last_received.is_none_or(|last| frame.seq > last) {
            self.queue.push_back(frame);
            self.last_received = Some(frame.seq);
        }
    }

    /// The input to apply this tick. Repeats the previous input if nothing new arrived.
    pub fn next(&mut self) -> InputFrame {
        while self.queue.len() > Self::MAX_QUEUED {
            self.queue.pop_front();
        }
        if let Some(frame) = self.queue.pop_front() {
            self.current = frame;
        }
        self.current
    }
}

fn start_networking(
    mut commands: Commands,
    channels: Res<RepliconChannels>,
    settings: Res<ServerSettings>,
) -> Result<()> {
    if !settings.network {
        return Ok(());
    }
    let server = RenetServer::new(ConnectionConfig {
        server_channels_config: channels.server_configs(),
        client_channels_config: channels.client_configs(),
        ..default()
    });
    let current_time = SystemTime::now().duration_since(SystemTime::UNIX_EPOCH)?;
    let socket = UdpSocket::bind((Ipv4Addr::UNSPECIFIED, settings.port))?;
    let transport = NetcodeServerTransport::new(
        ServerConfig {
            current_time,
            max_clients: settings.max_clients,
            protocol_id: PROTOCOL_ID,
            // TODO: secure connect tokens once there is a master server / login.
            authentication: ServerAuthentication::Unsecure,
            public_addresses: Default::default(),
        },
        socket,
    )?;
    commands.insert_resource(server);
    commands.insert_resource(transport);
    info!("listening on UDP port {}", settings.port);
    Ok(())
}

fn start_match(mut commands: Commands, settings: Res<ServerSettings>) {
    commands.spawn((
        MatchInfo {
            level: settings.level.clone(),
            mode: settings.mode.clone(),
            size: settings.size,
        },
        Replicated,
    ));
    if let Some(name) = &settings.local_player {
        let player = commands
            .spawn((
                Player {
                    name: name.clone(),
                    is_bot: false,
                },
                PlayerNetId::LOCAL_HOST,
                Team::One,
                InputBuffer::default(),
                Replicated,
            ))
            .id();
        commands.insert_resource(HostPlayer(player));
    }
}

fn create_client_player(
    add: On<Add, AuthorizedClient>,
    mut commands: Commands,
    clients: Query<&NetworkId>,
    teams: Query<&Team, With<Player>>,
) {
    let Ok(network_id) = clients.get(add.entity) else {
        return;
    };
    let team = balanced_team(teams.iter());
    let player = commands
        .spawn((
            Player {
                name: format!("Player {}", network_id.get() % 10_000),
                is_bot: false,
            },
            PlayerNetId(network_id.get()),
            team,
            InputBuffer::default(),
            PlayerClient(add.entity),
            Replicated,
        ))
        .id();
    commands.entity(add.entity).insert(ClientPlayer(player));
    info!("client {} joined team {team:?}", network_id.get());
}

fn remove_client_player(
    remove: On<Remove, ConnectedClient>,
    mut commands: Commands,
    clients: Query<&ClientPlayer>,
    controls: Query<&Controls>,
) {
    let Ok(ClientPlayer(player)) = clients.get(remove.entity) else {
        return;
    };
    if let Ok(Controls(soldier)) = controls.get(*player) {
        commands.entity(*soldier).despawn();
    }
    commands.entity(*player).despawn();
    info!("client left");
}

/// Picks the team with fewer players.
pub fn balanced_team<'a>(teams: impl Iterator<Item = &'a Team>) -> Team {
    let (mut one, mut two) = (0, 0);
    for team in teams {
        match team {
            Team::One => one += 1,
            Team::Two => two += 1,
            Team::Spectator => {}
        }
    }
    if one <= two { Team::One } else { Team::Two }
}

/// Maps the sender of a client message to its player entity.
fn sender_player(
    client_id: ClientId,
    clients: &Query<&ClientPlayer>,
    host: Option<&HostPlayer>,
) -> Option<Entity> {
    match client_id {
        ClientId::Server => host.map(|h| h.0),
        ClientId::Client(entity) => clients.get(entity).ok().map(|c| c.0),
    }
}

fn receive_hello(
    mut hellos: MessageReader<FromClient<ClientHello>>,
    clients: Query<&ClientPlayer>,
    host: Option<Res<HostPlayer>>,
    mut players: Query<&mut Player>,
) {
    for hello in hellos.read() {
        let Some(entity) = sender_player(hello.client_id, &clients, host.as_deref()) else {
            continue;
        };
        if let Ok(mut player) = players.get_mut(entity) {
            let name: String = hello.message.name.trim().chars().take(24).collect();
            if !name.is_empty() {
                info!("{} is now known as {name}", player.name);
                player.name = name;
            }
        }
    }
}

fn receive_inputs(
    mut packets: MessageReader<FromClient<InputPacket>>,
    clients: Query<&ClientPlayer>,
    host: Option<Res<HostPlayer>>,
    mut buffers: Query<&mut InputBuffer>,
) {
    for packet in packets.read() {
        let Some(entity) = sender_player(packet.client_id, &clients, host.as_deref()) else {
            continue;
        };
        if let Ok(mut buffer) = buffers.get_mut(entity) {
            for frame in &packet.message.frames {
                buffer.push(*frame);
            }
        }
    }
}

fn apply_inputs(
    time: Res<Time>,
    tuning: Res<SoldierTuning>,
    shapes: Res<SoldierShapes>,
    mover: MoveAndSlide,
    mut soldiers: Query<
        (
            &ControlledBy,
            &mut SoldierMotion,
            &mut InputAck,
            &mut Transform,
        ),
        With<Soldier>,
    >,
    mut buffers: Query<&mut InputBuffer>,
) {
    let dt = time.delta_secs();
    for (controlled_by, mut motion, mut ack, mut transform) in &mut soldiers {
        let Ok(mut buffer) = buffers.get_mut(controlled_by.0) else {
            continue;
        };
        let input = buffer.next();
        let mut next = *motion;
        step_soldier(&mut next, &input, dt, &tuning, &shapes, &mover);
        motion.set_if_neq(next);
        ack.set_if_neq(InputAck(input.seq));
        let body = next.body_transform();
        if transform.translation != body.translation || transform.rotation != body.rotation {
            *transform = body;
        }
    }
}

fn respawn_players(
    mut commands: Commands,
    time: Res<Time>,
    level: Res<LoadedLevel>,
    match_info: Single<&MatchInfo>,
    mut players: Query<(Entity, &Team, Option<&mut RespawnTimer>), (With<Player>, Without<Controls>)>,
) {
    for (player, team, timer) in &mut players {
        if *team == Team::Spectator {
            continue;
        }
        match timer {
            None => {
                // First spawn is immediate.
                spawn_soldier(&mut commands, player, *team, &level, &match_info);
            }
            Some(mut timer) => {
                if timer.0.tick(time.delta()).is_finished() {
                    commands.entity(player).remove::<RespawnTimer>();
                    spawn_soldier(&mut commands, player, *team, &level, &match_info);
                }
            }
        }
    }
}

/// Spawns a soldier for `player` at one of its team's spawn points.
pub fn spawn_soldier(
    commands: &mut Commands,
    player: Entity,
    team: Team,
    level: &LoadedLevel,
    match_info: &MatchInfo,
) -> Entity {
    let (position, yaw) = pick_spawn(level, match_info, team);
    let motion = SoldierMotion::at(position, yaw);
    let soldier = commands
        .spawn((
            Soldier,
            ControlledBy(player),
            motion,
            motion.body_transform(),
            Replicated,
        ))
        .id();
    commands.entity(player).insert(Controls(soldier));
    soldier
}

fn pick_spawn(level: &LoadedLevel, match_info: &MatchInfo, team: Team) -> (Vec3, f32) {
    let team_id = match team {
        Team::One => 1,
        Team::Two => 2,
        Team::Spectator => 0,
    };
    let candidates: Vec<_> = level
        .game_mode(&match_info.mode, match_info.size)
        .map(|mode| {
            mode.spawn_points
                .iter()
                .filter(|sp| {
                    mode.control_points
                        .iter()
                        .any(|cp| cp.id == sp.control_point && cp.initial_team == team_id)
                })
                .collect()
        })
        .unwrap_or_default();

    let (mut position, yaw) = match fastrand::choice(&candidates) {
        Some(sp) => {
            let rotation = Quat::from_array(sp.placement.rotation);
            let (yaw, _, _) = rotation.to_euler(EulerRot::YXZ);
            (Vec3::from_array(sp.placement.position), yaw)
        }
        None => {
            let center = level
                .heightmap
                .as_ref()
                .map(|h| h.center())
                .unwrap_or_default();
            (center, 0.0)
        }
    };
    if let Some(heightmap) = &level.heightmap {
        let ground = heightmap.height_at(position.x, position.z);
        position.y = position.y.max(ground + 0.1);
    }
    (position, yaw)
}
