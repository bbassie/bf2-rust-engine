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
use bevy::{platform::collections::HashMap, prelude::*};
use bevy_replicon::{prelude::*, shared::backend::connected_client::NetworkId};
use bevy_replicon_renet::{
    RenetChannelsExt, RenetServer,
    netcode::{NetcodeServerTransport, ServerAuthentication, ServerConfig},
    renet::ConnectionConfig,
};
use game_shared::{
    PROTOCOL_ID,
    chat::ChatLine,
    config::GamePaths,
    conquest::{ControlPoint, Deployment, FlagState, RoundState},
    squad::SquadMember,
    input::{InputFrame, InputPacket},
    level::{LevelEntity, LoadedLevel},
    protocol::{ClientHello, ControlledBy, MatchInfo, Player, PlayerNetId, Team},
    revive::Downed,
    soldier::{InputAck, Soldier, SoldierMotion, SoldierShapes, SoldierTuning, step_soldier},
    vehicle::{Seated, water_height},
    weapons::{Armory, Inventory, Loadout, WeaponState},
};

pub mod abilities;
pub mod accounts;
pub mod admin;
pub mod ai;
pub mod bots;
pub mod chat;
pub mod commander;
pub mod combat;
pub mod conquest;
pub mod content;
pub mod coop;
pub mod discovery;
pub mod dummy;
pub mod embedded;
pub mod gear;
pub mod join;
pub mod limits;
pub mod loadouts;
pub mod modes;
pub mod radio;
pub mod rotation;
pub mod server_config;
pub mod profile;
pub mod soak;
pub mod stats;
pub mod squads;
pub mod transport;
pub mod destruction;
pub mod nav;
pub mod out_of_bounds;
pub mod roadkill;
pub mod vehicles;
pub mod voice;

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
    /// Listen on all network interfaces so other machines can join. Otherwise only this
    /// machine can connect (127.0.0.1), which also avoids firewall prompts while testing.
    pub public: bool,
    /// Create a player for the local user (listen server / singleplayer).
    pub local_player: Option<String>,
    /// Team of the local player (1 or 2).
    pub local_team: u8,
    pub respawn_seconds: f32,
    /// Whether bullets hurt teammates.
    pub friendly_fire: bool,
    /// BF2's out-of-bounds warning and countdown outside the level's combat areas (see
    /// [`out_of_bounds`]). Levels without combat areas are unaffected either way.
    pub out_of_bounds: bool,
    /// Soldiers climb onto ledges by jumping into them (not in BF2; see
    /// `game_shared::soldier::SoldierTuning::mantle`).
    pub mantle: bool,
    /// How well bots aim and how quickly they react, 0..1 (BF2's bot skill).
    pub bot_skill: f32,
    /// How hard the bots are: scales reaction, aim, tactics and awareness (see
    /// [`ai::skill::BotDifficulty`]); it also sets `bot_skill` where that isn't given.
    pub bot_difficulty: ai::skill::BotDifficulty,
    /// Testing: bots of this team (1, 2; 3 for both) play with the tactics from before
    /// cover, suppression, memory and squad coordination (`--bot-legacy-team`), to compare.
    pub bot_legacy_team: u8,
    /// Testing: bots of this team (1 or 2) play at this difficulty (skill included) instead
    /// (`--bot-team-difficulty 2:easy`).
    pub bot_team_difficulty: Option<(u8, ai::skill::BotDifficulty)>,
    /// Shown in server browsers and greetings.
    pub name: String,
    /// Percent of the level's tickets each team starts a round with.
    pub ticket_ratio: f32,
    /// Maps to play in turn (see [`rotation`]); empty keeps playing `level`.
    pub rotation: Vec<rotation::MapEntry>,
    /// Remote console, bans, stats and the message of the day.
    pub admin: admin::AdminSettings,
    /// How co-op maps (`gpm_coop`) are played.
    pub coop: coop::CoopSettings,
    /// Master server to announce this server to (`host[:port]`, see `crates/master_server`).
    pub master_server: Option<String>,
    /// What joining clients may download from this server (see [`content`]).
    pub content: content::ContentSettings,
    /// Optional accounts: a master server whose tickets the server checks, ranked or not
    /// (see [`accounts`]).
    pub accounts: accounts::AccountSettings,
    /// What players may carry: BF2's kits or picks from each class's weapons, faction
    /// locks, unlocks by rank (see [`loadouts`]).
    pub loadouts: game_shared::arsenal::LoadoutRules,
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
            public: false,
            local_player: None,
            local_team: 1,
            respawn_seconds: 10.0,
            friendly_fire: false,
            out_of_bounds: true,
            mantle: true,
            bot_skill: 0.5,
            bot_difficulty: ai::skill::BotDifficulty::Normal,
            bot_legacy_team: 0,
            bot_team_difficulty: None,
            name: "BF2 Rust server".into(),
            ticket_ratio: 100.0,
            rotation: Vec::new(),
            admin: default(),
            coop: default(),
            master_server: None,
            content: default(),
            accounts: default(),
            loadouts: default(),
        }
    }
}

pub struct GameServerPlugin {
    /// Serve a match with these settings from startup (the dedicated server). Without, the
    /// server is idle until [`start_server`] (the client's menu).
    pub settings: Option<ServerSettings>,
}

impl Plugin for GameServerPlugin {
    fn build(&self, app: &mut App) {
        if let Some(settings) = self.settings.clone() {
            app.add_systems(Startup, move |world: &mut World| start_server(world, settings.clone()));
        }
        app.insert_resource(self.settings.clone().unwrap_or_default())
            .add_plugins((bots::BotPlugin, combat::CombatPlugin, modes::ModesPlugin, nav::NavPlugin, squads::SquadPlugin, vehicles::VehiclesPlugin))
            .add_plugins((destruction::DestructionPlugin, roadkill::RoadkillPlugin, abilities::AbilitiesPlugin, gear::GearPlugin))
            .add_plugins(out_of_bounds::OutOfBoundsPlugin)
            .add_plugins((
                admin::AdminPlugin,
                chat::ChatPlugin,
                radio::RadioPlugin,
                commander::CommanderPlugin,
                discovery::DiscoveryPlugin,
                rotation::RotationPlugin,
                stats::StatsPlugin,
                content::ContentPlugin,
                voice::VoicePlugin,
                dummy::DummyPlugin,
            ))
            .add_plugins((join::JoinPlugin, accounts::AccountsPlugin, loadouts::LoadoutsPlugin))
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
                (coop::balance_teams, respawn_players)
                    .chain()
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
    /// Frames in a row that were too far ahead of `last_received` (see [`Self::push`]).
    ahead: u32,
    /// The fewest frames queued beyond the ticks about to run this [`Self::DRAIN_WINDOW`], and
    /// how far into it we are (see [`Self::drain`]).
    lowest: Option<usize>,
    window: f32,
}

impl InputBuffer {
    /// Frames queued beyond this (plus the ticks about to run) are dropped to keep input
    /// latency bounded.
    const MAX_QUEUED: usize = 6;
    /// Most a frame's sequence number may be ahead of the last one received (10 s of ticks,
    /// far more than any packet loss the connection survives). Further ahead is refused, so a
    /// client can't jump to `u32::MAX` and have every later frame count as old.
    const MAX_SEQ_JUMP: u32 = 600;
    /// Unless frames keep coming that far ahead: then the client really moved on (a long
    /// stall), and its numbers are taken as they are.
    const RESYNC_AFTER: u32 = 60;
    /// Frames a remote client's queue keeps beyond the ticks about to run, against jitter.
    const SLACK: usize = 2;
    /// Seconds a queue must have held more than that before it is drained.
    const DRAIN_WINDOW: f32 = 1.0;

    /// Queues a frame received from the network: dropped if it isn't newer than the last one,
    /// or if it isn't valid ([`game_shared::validate::input_frame`]: NaN or infinite angles).
    pub fn push(&mut self, frame: InputFrame) {
        let Some(frame) = game_shared::validate::input_frame(frame) else {
            return;
        };
        if let Some(last) = self.last_received {
            if frame.seq <= last {
                return;
            }
            if frame.seq - last > Self::MAX_SEQ_JUMP {
                self.ahead += 1;
                if self.ahead < Self::RESYNC_AFTER {
                    return;
                }
            }
        }
        self.ahead = 0;
        self.queue.push_back(frame);
        self.last_received = Some(frame.seq);
    }

    /// Drops the oldest frames the coming `ticks` and [`Self::MAX_QUEUED`] won't use. Once per
    /// frame, not per tick: after a stall the catch-up ticks need the whole backlog, which the
    /// client already predicted with.
    pub fn trim(&mut self, ticks: usize) {
        while self.queue.len() > Self::MAX_QUEUED + ticks {
            self.queue.pop_front();
        }
    }

    /// A remote client's queue that holds more than it needs: filled up by one slow frame it
    /// never drains by itself, and every input then waits that many ticks (a standing 100 ms
    /// at [`Self::MAX_QUEUED`], which lag compensation then has to rewind on top of the
    /// round trip, beyond its cap). Once the queue has held more than [`Self::SLACK`] frames
    /// beyond the coming `ticks` for a whole [`Self::DRAIN_WINDOW`], the oldest of those go,
    /// their buttons carried over. Once per frame, with the frame's `dt`.
    pub fn drain(&mut self, ticks: usize, dt: f32) {
        let surplus = self.queue.len().saturating_sub(ticks);
        let lowest = self.lowest.map_or(surplus, |l| l.min(surplus));
        self.lowest = Some(lowest);
        self.window += dt;
        if self.window >= Self::DRAIN_WINDOW {
            if lowest > Self::SLACK {
                let keep = self.queue.len() - (lowest - Self::SLACK);
                self.keep_newest(keep);
            }
            self.window = 0.0;
            self.lowest = None;
        }
    }

    /// Frames waiting to be applied.
    pub fn queued(&self) -> usize {
        self.queue.len()
    }

    /// Drops all but the newest `count` queued frames; their buttons carry over to the next
    /// frame, so a short press isn't lost.
    pub fn keep_newest(&mut self, count: usize) {
        let mut buttons = game_shared::input::Buttons::empty();
        while self.queue.len() > count.max(1) {
            if let Some(frame) = self.queue.pop_front() {
                buttons |= frame.buttons;
            }
        }
        if let Some(next) = self.queue.front_mut() {
            next.buttons |= buttons;
        }
    }

    /// The input to apply this tick. Repeats the previous input if nothing new arrived.
    pub fn next(&mut self) -> InputFrame {
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
    let address = if settings.public { Ipv4Addr::UNSPECIFIED } else { Ipv4Addr::LOCALHOST };
    let socket = UdpSocket::bind((address, settings.port))?;
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

/// Starts serving a match: listens for connections if `settings.network`, then spawns the
/// match (which loads the level) and the local player. [`stop_server`] ends it.
pub fn start_server(world: &mut World, mut settings: ServerSettings) -> Result<()> {
    let rotation = rotation::MapRotation::new(&mut settings, world.resource::<GamePaths>());
    world.insert_resource(rotation);
    world.insert_resource(settings);
    world.run_system_cached::<(), _, _>(start_networking)?;
    discovery::start(world);
    // The identity first: the content endpoint signs with it.
    join::start(world);
    accounts::start(world);
    content::start(world);
    admin::start(world);
    stats::start(world);
    world.run_system_cached(start_match)?;
    Ok(())
}

/// Ends the match started by [`start_server`]: disconnects every client and despawns the
/// match, its players, soldiers and control points, and the level. Another match can start
/// afterwards. Does nothing when no match is running.
pub fn stop_server(world: &mut World) {
    stats::stop(world);
    admin::stop(world);
    discovery::stop(world);
    content::stop(world);
    accounts::stop(world);
    join::stop(world);
    if let Some(mut transport) = world.remove_resource::<NetcodeServerTransport>() {
        if let Some(mut server) = world.get_resource_mut::<RenetServer>() {
            transport.disconnect_all(&mut server);
        }
        info!("stopped listening");
    }
    world.remove_resource::<RenetServer>();
    // Clients first: that despawns their players (see `remove_client_player`).
    let clients: Vec<Entity> = world
        .query_filtered::<Entity, With<ConnectedClient>>()
        .iter(world)
        .collect();
    for client in clients {
        world.despawn(client);
    }
    let entities: Vec<Entity> = world
        .query_filtered::<Entity, Or<(With<Replicated>, With<LevelEntity>)>>()
        .iter(world)
        .collect();
    for entity in entities {
        // Children went with their parents.
        let _ = world.try_despawn(entity);
    }
    world.remove_resource::<HostPlayer>();
    rotation::forget_level(world);
}

fn create_client_player(
    add: On<Add, AuthorizedClient>,
    mut commands: Commands,
    clients: Query<(&NetworkId, Option<&join::ClientAccount>, Option<&embedded::LinkPlayer>)>,
    teams: Query<&Team, With<Player>>,
    settings: Res<ServerSettings>,
) {
    let Ok((network_id, account, link)) = clients.get(add.entity) else {
        return;
    };
    // Co-op: all humans on one team. The player of this process (`embedded`) chose his.
    let team = if let Some(link) = link {
        link.team
    } else if coop::is_coop(&settings.mode) {
        coop::human_team(&settings)
    } else {
        balanced_team(teams.iter())
    };
    let name = match (account, link) {
        // A verified account's name (see `accounts`), else the hello's.
        (Some(account), _) => account.0.name.clone(),
        (None, Some(embedded::LinkPlayer { name: Some(name), .. })) => name.clone(),
        (None, Some(_)) => "Spectator".into(),
        (None, None) => format!("Player {}", network_id.get() % 10_000),
    };
    let player = commands
        .spawn((
            Player { name, is_bot: false },
            PlayerNetId(network_id.get()),
            team,
            InputBuffer::default(),
            PlayerClient(add.entity),
            Replicated,
        ))
        .id();
    if let Some(account) = account {
        commands.entity(player).insert(game_shared::join::AccountBadge {
            rank: account.0.rank,
            rank_name: account.0.rank_name.clone(),
            rank_short: account.0.rank_short.clone(),
        });
        admin::grant_if_admin(&mut commands, &settings.admin.admins, player, &account.0);
    }
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
    // `try_`: `stop_server` may despawn them first.
    if let Ok(Controls(soldier)) = controls.get(*player) {
        commands.entity(*soldier).try_despawn();
    }
    commands.entity(*player).try_despawn();
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

/// Where to send messages meant for a player's human, if it has one (the host's own player
/// plays on the server). The inverse of [`sender_player`].
fn player_client(player: Entity, clients: &Query<&PlayerClient>, host: Option<&HostPlayer>) -> Option<ClientId> {
    if host.is_some_and(|h| h.0 == player) {
        return Some(ClientId::Server);
    }
    clients.get(player).ok().map(|c| ClientId::Client(c.0))
}

/// Server-side: the player's client said hello; further hellos are ignored.
#[derive(Component)]
pub struct Greeted;

/// A client's hello, once per connection: its name (cleaned, and made unique among the
/// players; a verified account keeps its own), a line to everyone that it joined, the
/// greeting and the message of the day to it, and its career stats kept from now on
/// ([`stats::Identified`]).
#[allow(clippy::type_complexity)]
fn receive_hello(
    mut commands: Commands,
    mut hellos: MessageReader<FromClient<ClientHello>>,
    clients: Query<&ClientPlayer>,
    host: Option<Res<HostPlayer>>,
    settings: Res<ServerSettings>,
    mut players: Query<(Entity, &mut Player, Has<game_shared::join::AccountBadge>, Has<Greeted>)>,
    mut lines: MessageWriter<ToClients<ChatLine>>,
    links: Query<(), With<transport::LinkClient>>,
) {
    // Greeted in this pass (the marker is inserted later).
    let mut greeted = Vec::new();
    for hello in hellos.read() {
        let Some(entity) = sender_player(hello.client_id, &clients, host.as_deref()) else {
            continue;
        };
        let Ok((_, player, verified, already)) = players.get(entity) else {
            continue;
        };
        if already || greeted.contains(&entity) {
            debug!("ignoring another hello from {}", player.name);
            continue;
        }
        greeted.push(entity);
        let wanted = game_shared::validate::player_name(&hello.message.name).filter(|_| !verified);
        if let Some(wanted) = wanted {
            let name = game_shared::validate::unique_name(&wanted, |name| {
                players
                    .iter()
                    .any(|(other, player, ..)| other != entity && player.name.eq_ignore_ascii_case(name))
            });
            if let Ok((_, mut player, ..)) = players.get_mut(entity)
                && player.name != name
            {
                info!("{} is now known as {name}", player.name);
                player.name = name;
            }
        }
        let Ok((_, player, ..)) = players.get(entity) else {
            continue;
        };
        let name = player.name.clone();
        commands.entity(entity).insert((Greeted, stats::Identified));
        info!("server: {name} joined the game");
        lines.write(ToClients {
            targets: SendTargets::AllExcept(hello.client_id),
            message: ChatLine::server(format!("{name} joined the game")),
        });
        // Not the player of this process (`embedded`): he runs the server.
        if matches!(hello.client_id, ClientId::Client(client) if links.contains(client)) {
            continue;
        }
        let greeting = std::iter::once(format!("Welcome to {}, {name}!", settings.name))
            .chain(settings.admin.motd.lines().map(str::to_string));
        for line in greeting {
            lines.write(ToClients {
                targets: SendTargets::Single(hello.client_id),
                message: ChatLine::private(line),
            });
        }
    }
}

fn receive_inputs(
    mut packets: MessageReader<FromClient<InputPacket>>,
    clients: Query<&ClientPlayer>,
    host: Option<Res<HostPlayer>>,
    virtual_time: Res<Time<Virtual>>,
    fixed_time: Res<Time<Fixed>>,
    mut buffers: Query<(Entity, &mut InputBuffer)>,
) {
    for packet in packets.read() {
        let Some(entity) = sender_player(packet.client_id, &clients, host.as_deref()) else {
            continue;
        };
        if let Ok((_, mut buffer)) = buffers.get_mut(entity) {
            // The newest frames only: clients send a few; a packet of thousands is no input.
            let frames = &packet.message.frames;
            for frame in &frames[frames.len().saturating_sub(game_shared::validate::MAX_INPUT_FRAMES)..] {
                buffer.push(*frame);
            }
        }
    }
    // The fixed ticks this frame will run to catch up with the virtual clock.
    let ticks = ((fixed_time.overstep() + virtual_time.delta()).as_secs_f64()
        / fixed_time.timestep().as_secs_f64()) as usize;
    let host_player = host.as_deref().map(|h| h.0);
    for (entity, mut buffer) in &mut buffers {
        buffer.trim(ticks);
        if Some(entity) != host_player {
            buffer.drain(ticks, virtual_time.delta_secs());
        }
    }
    // The host's own inputs come every frame, without loss or jitter: queued beyond the
    // ticks about to run they would only wait. (A queue filled up by a slow frame never
    // drained and kept the host's input 6 ticks, 100 ms, behind: flying, driving, walking.)
    if let Some((_, mut buffer)) = host.as_deref().and_then(|h| buffers.get_mut(h.0).ok()) {
        buffer.keep_newest(ticks.max(1));
    }
}

fn apply_inputs(
    time: Res<Time>,
    tuning: Res<SoldierTuning>,
    shapes: Res<SoldierShapes>,
    mover: MoveAndSlide,
    level: Option<Res<LoadedLevel>>,
    mut soldiers: Query<
        (
            &ControlledBy,
            &mut SoldierMotion,
            &mut InputAck,
            &mut Transform,
            &mut AppliedInput,
            Option<&Downed>,
        ),
        // Seated soldiers ride along; `vehicles` takes their input.
        (With<Soldier>, Without<Seated>),
    >,
    mut buffers: Query<&mut InputBuffer>,
    bots: Query<(), With<bots::BotBrain>>,
) {
    let dt = time.delta_secs();
    let water = water_height(level.as_deref());
    // This tick's input of every soldier, from its player's buffer.
    for (controlled_by, _, _, _, mut applied, downed) in &mut soldiers {
        let Ok(mut buffer) = buffers.get_mut(controlled_by.0) else {
            continue;
        };
        let mut input = buffer.next();
        // Critically wounded: lying still (see `abilities`).
        if let Some(downed) = downed {
            input = game_shared::revive::downed_input(input, downed);
        }
        applied.0 = input;
    }
    // Movement, in parallel: soldiers only move against the world and vehicles, never each
    // other (`GameLayer::soldier_movement_mask`), so the order makes no difference.
    let (buffers, bots) = (&buffers, &bots);
    soldiers
        .par_iter_mut()
        .for_each(|(controlled_by, mut motion, mut ack, mut transform, applied, downed)| {
            if !buffers.contains(controlled_by.0) {
                return;
            }
            let input = applied.0;
            let mut next = *motion;
            // Critically wounded under a parachute: it's let go, and he falls (his client
            // predicts the same, see `game_client::prediction::predict`).
            if downed.is_some() {
                next.parachute = false;
            }
            step_soldier(&mut next, &input, dt, &tuning, &shapes, &mover, water);
            motion.set_if_neq(next);
            // Only players predict their soldier from the ack; a bot's input number goes up every
            // tick and would replicate its ack every tick for nothing.
            if !bots.contains(controlled_by.0) {
                ack.set_if_neq(InputAck(input.seq));
            }
            let body = next.body_transform();
            if transform.translation != body.translation || transform.rotation != body.rotation {
                *transform = body;
            }
        });
}

#[allow(clippy::type_complexity, clippy::too_many_arguments)]
fn respawn_players(
    mut commands: Commands,
    time: Res<Time>,
    level: Res<LoadedLevel>,
    armory: Res<Armory>,
    // Loadouts (`loadouts`): the picks players made for their kit's class.
    loadout_rules: (Res<ServerSettings>, Res<game_shared::arsenal::Arsenal>),
    match_state: Single<(&MatchInfo, Option<&RoundState>, Option<&game_shared::modes::ModeState>, Option<&game_shared::conquest::Tickets>)>,
    control_points: Query<(&ControlPoint, &FlagState, &conquest::ControlPointRules, Option<&game_shared::modes::SpawnBlocked>)>,
    mut players: Query<
        (
            Entity,
            &Team,
            &mut Deployment,
            Option<&mut RespawnTimer>,
            Option<&SquadMember>,
            (Option<&game_shared::arsenal::LoadoutPicks>, Option<&game_shared::join::AccountBadge>),
        ),
        // Nobody spawns while their client checks its content for a new map (`join`).
        (With<Player>, Without<Controls>, Without<join::ContentPending>),
    >,
    leaders: Query<(&Team, &SquadMember, &Controls)>,
    // Nobody spawns on a critically wounded leader.
    soldiers: Query<&SoldierMotion, Without<Downed>>,
) {
    let (match_info, round, mode, tickets) = *match_state;
    // Where each squad's leader is, for spawning on them.
    let leader_at: HashMap<(Team, u8), (Vec3, f32)> = leaders
        .iter()
        .filter(|(_, member, _)| member.leader)
        .filter_map(|(team, member, controls)| {
            let motion = soldiers.get(controls.0).ok()?;
            Some(((*team, member.squad), (motion.position, motion.yaw)))
        })
        .collect();
    // Kits load right after the level; spawning earlier would leave soldiers unarmed.
    // Nobody spawns between rounds.
    if armory.kits.is_empty() || round != Some(&RoundState::Playing) {
        return;
    }
    for (player, team, mut deployment, timer, squad, (picks, badge)) in &mut players {
        // Staged modes: attackers out of tickets don't come back.
        if *team == Team::Spectator || mode.is_some_and(|m| !m.can_spawn(*team, tickets)) {
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
        let on_leader = squad
            .filter(|member| deployment.on_squad_leader && !member.leader)
            .and_then(|member| leader_at.get(&(*team, member.squad)).copied());
        let Some((position, yaw)) = on_leader
            .or_else(|| pick_spawn(&level, match_info, *team, &deployment, &control_points))
        else {
            // No spawn point held: wait.
            continue;
        };
        commands.entity(player).remove::<RespawnTimer>();
        deployment.respawn_in = 0.0;
        let (settings, arsenal) = &loadout_rules;
        let picked = loadouts::picked_loadout(settings, &armory, arsenal, *team, deployment.kit, picks, badge);
        spawn_soldier(&mut commands, player, *team, deployment.kit, position, yaw, &armory, picked);
    }
}

/// Spawns a soldier for `player` with kit slot `kit`, carrying `picked` (see
/// [`loadouts::picked_loadout`]) or else the kit's own weapons.
#[allow(clippy::too_many_arguments)]
pub fn spawn_soldier(
    commands: &mut Commands,
    player: Entity,
    team: Team,
    kit: u8,
    position: Vec3,
    yaw: f32,
    armory: &Armory,
    picked: Option<Loadout>,
) -> Entity {
    let mut motion = SoldierMotion::at(position, yaw);
    let team_index = if team == Team::Two { 1 } else { 0 };
    motion.heavy = armory
        .kit_for(team_index, kit as usize)
        .is_some_and(|k| game_shared::soldier::heavy_kit(&k.kind));
    let loadout = picked
        .or_else(|| {
            armory.kit_for(team_index, kit as usize).map(|k| Loadout {
                kit: k.name.clone(),
                weapons: k.weapons.clone(),
            })
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
    control_points: &Query<(&ControlPoint, &FlagState, &conquest::ControlPointRules, Option<&game_shared::modes::SpawnBlocked>)>,
) -> Option<(Vec3, f32)> {
    let layout = level.game_mode(&match_info.mode, match_info.size);
    let spawn_points = layout.map(|l| l.spawn_points.as_slice()).unwrap_or_default();
    let held: Vec<(u8, &str)> = control_points
        .iter()
        // Not where the mode closed it to us (enemies at the flag, see `modes::staged`).
        .filter(|(_, state, _, blocked)| game_shared::modes::can_spawn_at(state.owner, *blocked, team))
        .map(|(cp, _, rules, _)| (cp.index, rules.id.as_str()))
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

#[cfg(test)]
mod tests {
    use super::*;

    fn frame(seq: u32) -> InputFrame {
        InputFrame {
            seq,
            yaw: 1.0,
            ..default()
        }
    }

    fn queued(buffer: &InputBuffer) -> Vec<u32> {
        buffer.queue.iter().map(|f| f.seq).collect()
    }

    #[test]
    fn frames_with_nan_angles_are_dropped() {
        let mut buffer = InputBuffer::default();
        buffer.push(InputFrame { yaw: f32::NAN, ..frame(1) });
        buffer.push(InputFrame { pitch: f32::INFINITY, ..frame(2) });
        assert!(queued(&buffer).is_empty());
        buffer.push(frame(3));
        assert_eq!(queued(&buffer), [3]);
        assert!(buffer.next().yaw.is_finite());
    }

    #[test]
    fn a_huge_sequence_jump_does_not_block_later_input() {
        let mut buffer = InputBuffer::default();
        buffer.push(frame(10));
        buffer.push(frame(u32::MAX));
        buffer.push(frame(11));
        assert_eq!(queued(&buffer), [10, 11]);
        // Old and repeated frames are still ignored.
        buffer.push(frame(11));
        buffer.push(frame(5));
        assert_eq!(queued(&buffer), [10, 11]);
    }

    #[test]
    fn a_hello_names_the_player_once_and_uniquely() {
        let mut app = App::new();
        app.add_message::<FromClient<ClientHello>>()
            .add_message::<ToClients<ChatLine>>()
            .insert_resource(ServerSettings::default())
            .add_systems(Update, receive_hello);
        app.world_mut().spawn(Player {
            name: "Bob".into(),
            is_bot: true,
        });
        let player = app
            .world_mut()
            .spawn(Player {
                name: "Player 1".into(),
                is_bot: false,
            })
            .id();
        let client = app.world_mut().spawn(ClientPlayer(player)).id();
        let hello = |name: &str| FromClient {
            client_id: ClientId::Client(client),
            message: ClientHello { name: name.into() },
        };
        // Two in one packet: the first counts.
        app.world_mut().write_message(hello("  bob  "));
        app.world_mut().write_message(hello("Alice"));
        app.update();
        let name = |app: &App| app.world().get::<Player>(player).unwrap().name.clone();
        assert_eq!(name(&app), "bob (2)", "cleaned, and not the bot's name");
        assert!(app.world().entity(player).contains::<Greeted>());
        assert!(app.world().entity(player).contains::<stats::Identified>());
        // Later hellos change nothing and greet nobody.
        let before = name(&app);
        let mut lines = app.world().resource::<Messages<ToClients<ChatLine>>>().get_cursor_current();
        app.world_mut().write_message(hello("Alice"));
        app.update();
        assert_eq!(name(&app), before);
        let sent = lines.read(app.world().resource::<Messages<ToClients<ChatLine>>>()).count();
        assert_eq!(sent, 0, "no second greeting");
    }

    #[test]
    fn frames_that_keep_coming_far_ahead_are_taken_eventually() {
        let mut buffer = InputBuffer::default();
        buffer.push(frame(1));
        let far = 1 + InputBuffer::MAX_SEQ_JUMP + 1000;
        for seq in far..far + InputBuffer::RESYNC_AFTER {
            buffer.push(frame(seq));
        }
        assert_eq!(buffer.last_received, Some(far + InputBuffer::RESYNC_AFTER - 1));
        buffer.push(frame(far + InputBuffer::RESYNC_AFTER));
        assert_eq!(buffer.last_received, Some(far + InputBuffer::RESYNC_AFTER));
    }

    #[test]
    fn a_remote_queue_that_stays_full_drains_to_the_slack() {
        let mut buffer = InputBuffer::default();
        // One slow frame queued up six frames; since then one comes and one goes per tick.
        let mut seq = 0;
        for _ in 0..6 {
            seq += 1;
            buffer.push(frame(seq));
        }
        for _ in 0..61 {
            seq += 1;
            buffer.push(frame(seq));
            buffer.drain(1, 1.0 / 60.0);
            buffer.next();
        }
        assert_eq!(buffer.queue.len(), InputBuffer::SLACK, "the extra frames went");
        // A jittery connection whose queue runs dry now and then keeps what it has: frames
        // arrive four at a time every fourth tick.
        let mut jittery = InputBuffer::default();
        let mut seq = 0;
        for tick in 0..240 {
            if tick % 4 == 0 {
                for _ in 0..4 {
                    seq += 1;
                    jittery.push(frame(seq));
                }
            }
            let before = jittery.queue.len();
            jittery.drain(1, 1.0 / 60.0);
            assert_eq!(jittery.queue.len(), before, "never drained");
            jittery.next();
        }
    }
}
