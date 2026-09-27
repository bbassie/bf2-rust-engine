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
    conquest::{ControlPoint, Deployment, FlagState, RoundState},
    input::{InputFrame, InputPacket},
    level::LoadedLevel,
    protocol::{ClientHello, ControlledBy, MatchInfo, Player, PlayerNetId, Team},
    soldier::{InputAck, Soldier, SoldierMotion, SoldierShapes, SoldierTuning, step_soldier},
    weapons::{Armory, Inventory, Loadout, WeaponState},
};

pub mod bots;
pub mod combat;
pub mod conquest;

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
    /// Team of the local player (1 or 2).
    pub local_team: u8,
    pub respawn_seconds: f32,
    /// Whether bullets hurt teammates.
    pub friendly_fire: bool,
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
            local_team: 1,
            respawn_seconds: 10.0,
            friendly_fire: false,
        }
    }
}

pub struct GameServerPlugin {
    pub settings: ServerSettings,
}

impl Plugin for GameServerPlugin {
    fn build(&self, app: &mut App) {
        app.insert_resource(self.settings.clone())
            .add_plugins((bots::BotPlugin, combat::CombatPlugin, conquest::ConquestPlugin))
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

/// Server-side: the input applied to a soldier this tick (weapons read it after movement).
#[derive(Component, Clone, Copy, Debug, Default)]
pub struct AppliedInput(pub InputFrame);

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
                if settings.local_team == 2 { Team::Two } else { Team::One },
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
            &mut AppliedInput,
        ),
        With<Soldier>,
    >,
    mut buffers: Query<&mut InputBuffer>,
) {
    let dt = time.delta_secs();
    for (controlled_by, mut motion, mut ack, mut transform, mut applied) in &mut soldiers {
        let Ok(mut buffer) = buffers.get_mut(controlled_by.0) else {
            continue;
        };
        let input = buffer.next();
        applied.0 = input;
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

#[allow(clippy::type_complexity)]
fn respawn_players(
    mut commands: Commands,
    time: Res<Time>,
    level: Res<LoadedLevel>,
    armory: Res<Armory>,
    match_state: Single<(&MatchInfo, Option<&RoundState>)>,
    control_points: Query<(&ControlPoint, &FlagState, &conquest::ControlPointRules)>,
    mut players: Query<
        (Entity, &Team, &mut Deployment, Option<&mut RespawnTimer>),
        (With<Player>, Without<Controls>),
    >,
) {
    let (match_info, round) = *match_state;
    // Kits load right after the level; spawning earlier would leave soldiers unarmed.
    // Nobody spawns between rounds.
    if armory.kits.is_empty() || round != Some(&RoundState::Playing) {
        return;
    }
    for (player, team, mut deployment, timer) in &mut players {
        if *team == Team::Spectator {
            continue;
        }
        let ready = match timer {
            // First spawn is immediate.
            None => true,
            Some(mut timer) => {
                timer.0.tick(time.delta());
                let left = timer.0.remaining_secs();
                // Tenths are enough for the countdown and replicate less often.
                let shown = (left * 10.0).ceil() / 10.0;
                if deployment.respawn_in != shown {
                    deployment.respawn_in = shown;
                }
                timer.0.is_finished()
            }
        };
        if !ready {
            continue;
        }
        let Some((position, yaw)) =
            pick_spawn(&level, match_info, *team, &deployment, &control_points)
        else {
            // No spawn point held: wait.
            continue;
        };
        commands.entity(player).remove::<RespawnTimer>();
        deployment.respawn_in = 0.0;
        spawn_soldier(&mut commands, player, *team, deployment.kit, position, yaw, &armory);
    }
}

/// Spawns a soldier for `player` with kit slot `kit`.
pub fn spawn_soldier(
    commands: &mut Commands,
    player: Entity,
    team: Team,
    kit: u8,
    position: Vec3,
    yaw: f32,
    armory: &Armory,
) -> Entity {
    let motion = SoldierMotion::at(position, yaw);
    let team_index = if team == Team::Two { 1 } else { 0 };
    let loadout = armory
        .kit_for(team_index, kit as usize)
        .map(|k| Loadout {
            kit: k.name.clone(),
            weapons: k.weapons.clone(),
        })
        .unwrap_or_default();
    let inventory = Inventory::full(&loadout, armory);
    let soldier = commands
        .spawn((
            Soldier,
            ControlledBy(player),
            motion,
            motion.body_transform(),
            loadout,
            inventory,
            WeaponState::default(),
            AppliedInput::default(),
            Replicated,
        ))
        .id();
    commands.entity(player).insert(Controls(soldier));
    soldier
}

/// A spawn point at a control point `team` holds: the chosen one if possible. `None` if the
/// team holds no point with spawn points.
fn pick_spawn(
    level: &LoadedLevel,
    match_info: &MatchInfo,
    team: Team,
    deployment: &Deployment,
    control_points: &Query<(&ControlPoint, &FlagState, &conquest::ControlPointRules)>,
) -> Option<(Vec3, f32)> {
    let layout = level.game_mode(&match_info.mode, match_info.size);
    let spawn_points = layout.map(|l| l.spawn_points.as_slice()).unwrap_or_default();
    let held: Vec<(u8, &str)> = control_points
        .iter()
        .filter(|(_, state, _)| state.owner == team)
        .map(|(cp, _, rules)| (cp.index, rules.id.as_str()))
        .collect();
    let at = |ids: &[&str]| -> Vec<_> {
        spawn_points
            .iter()
            .filter(|sp| ids.contains(&sp.control_point.as_str()))
            .collect()
    };
    let chosen: Vec<&str> = held
        .iter()
        .filter(|(index, _)| Some(*index) == deployment.control_point)
        .map(|(_, id)| *id)
        .collect();
    let mut candidates = at(&chosen);
    if candidates.is_empty() {
        let all: Vec<&str> = held.iter().map(|(_, id)| *id).collect();
        candidates = at(&all);
    }

    let (mut position, yaw) = match fastrand::choice(&candidates) {
        Some(sp) => {
            let rotation = Quat::from_array(sp.placement.rotation);
            let (yaw, _, _) = rotation.to_euler(EulerRot::YXZ);
            (Vec3::from_array(sp.placement.position), yaw)
        }
        // Levels without control points (or spawn points): the middle of the map.
        None if control_points.is_empty() || spawn_points.is_empty() => {
            let center = level.heightmap.as_ref().map(|h| h.center()).unwrap_or_default();
            (center, 0.0)
        }
        None => return None,
    };
    if let Some(heightmap) = &level.heightmap {
        let ground = heightmap.height_at(position.x, position.z);
        position.y = position.y.max(ground + 0.1);
    }
    Some((position, yaw))
}
