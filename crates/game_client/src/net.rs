//! Starting, joining and leaving matches, and working out which entities are ours.
//!
//! A match comes from the command line or the menu as a [`MatchSetup`]: either we run the
//! server (singleplayer, listen server; on its own thread, see `local_server`) or we connect
//! to one. [`leave_match`]
//! undoes everything a match brought, so another one can start.

use std::{
    net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr, UdpSocket},
    time::SystemTime,
};

use bevy::{
    prelude::*,
    window::{CursorGrabMode, CursorOptions},
};
use bevy_replicon::prelude::*;
use bevy_replicon_renet::{
    RenetChannelsExt, RenetClient,
    netcode::{ClientAuthentication, NetcodeClientTransport},
    renet::ConnectionConfig,
};
use game_server::ServerSettings;
use game_shared::{
    PROTOCOL_ID,
    chat::Kicked,
    level::{LevelEntity, LoadedLevel},
    protocol::{ControlledBy, Player, PlayerNetId},
    soldier::{Soldier, SoldierMotion},
    weapons::Armory,
};

use crate::{
    combat::{CombatFeedback, WeaponSelection},
    deploy::DeployScreen,
    local_input::{InputHistory, LookState},
    menu::Screen,
    settings::{MAX_RECENT_SERVERS, SavedServer, Settings, SettingsFile},
};

pub struct NetPlugin;

impl Plugin for NetPlugin {
    fn build(&self, app: &mut App) {
        app.init_resource::<ActiveMatch>()
            .init_resource::<MatchNotice>()
            .init_resource::<KickReason>()
            .add_systems(
                OnEnter(ClientState::Connected),
                (send_hello, || info!("connected")),
            )
            .add_systems(OnExit(ClientState::Connected), || warn!("disconnected"))
            .add_systems(OnEnter(ClientState::Disconnected), connection_lost)
            .add_observer(level_load_failed)
            .add_systems(
                PreUpdate,
                (tag_local_entities, receive_kick).after(ClientSystems::Receive),
            );
    }
}

/// A match to play.
#[derive(Clone, Debug)]
pub enum MatchSetup {
    /// Singleplayer, or a listen server when `network` is set: we run the server (on a thread
    /// of its own, see `local_server`).
    Local(ServerSettings),
    /// Play on a server.
    Join {
        server: SocketAddr,
        name: String,
        /// Only changes what scenarios wait for; the server gives everyone a soldier.
        spectate: bool,
    },
}

impl MatchSetup {
    /// Watching without a soldier.
    pub fn spectating(&self) -> bool {
        match self {
            MatchSetup::Local(settings) => settings.local_player.is_none(),
            MatchSetup::Join { spectate, .. } => *spectate,
        }
    }
}

/// The match being played, if any.
#[derive(Resource, Default)]
pub struct ActiveMatch {
    pub setup: Option<MatchSetup>,
    /// A joined match got as far as connecting.
    connected: bool,
}

impl ActiveMatch {
    pub fn spectating(&self) -> bool {
        self.setup.as_ref().is_some_and(MatchSetup::spectating)
    }

    /// The server we joined, if we joined one.
    pub fn server(&self) -> Option<SocketAddr> {
        match &self.setup {
            Some(MatchSetup::Join { server, .. }) => Some(*server),
            _ => None,
        }
    }
}

/// Why the last match ended when it wasn't left on purpose; the menu shows it.
#[derive(Resource, Default)]
pub struct MatchNotice(pub Option<String>);

/// Why the server is about to disconnect us, if it said.
#[derive(Resource, Default)]
struct KickReason(Option<String>);

/// Our network id; matches [`PlayerNetId`] of our player.
#[derive(Resource, Clone, Copy, Debug)]
pub struct LocalClientId(pub u64);

/// The player we are.
#[derive(Component)]
pub struct LocalPlayer;

/// The soldier we control.
#[derive(Component)]
pub struct LocalSoldier;

/// Starts playing `setup`: starts the server in this app or connects to one. On failure we
/// end up back in the menu, which says why.
pub fn start_match(world: &mut World, setup: MatchSetup) {
    if world.resource::<ActiveMatch>().setup.is_some() {
        leave_match(world);
    }
    let result = match &setup {
        MatchSetup::Local(settings) => {
            world.insert_resource(LocalClientId(game_server::embedded::LINK_CLIENT_ID));
            let mut settings = settings.clone();
            // Stats and bans of our own games next to the settings (none in scripted runs).
            if let Some(dir) = world.resource::<SettingsFile>().0.as_ref().and_then(|f| f.parent()) {
                let admin = &mut settings.admin;
                admin.stats_file.get_or_insert_with(|| dir.join("stats.ron"));
                admin.ban_file.get_or_insert_with(|| dir.join("bans.ron"));
            }
            // On a thread of its own, linked to us (`local_server`).
            crate::local_server::start(world, settings)
        }
        // The server's content first (if it shares any), then `connect`.
        MatchSetup::Join { server, .. } => crate::content::begin_join(world, *server),
    };
    let failure = match &setup {
        MatchSetup::Local(_) => "Can't start the server".to_string(),
        MatchSetup::Join { server, .. } => format!("Can't connect to {server}"),
    };
    world.insert_resource(ActiveMatch {
        setup: Some(setup),
        connected: false,
    });
    set_screen(world, Screen::Loading);
    if let Err(err) = result {
        error!("{failure}: {err}");
        leave_match(world);
        world.insert_resource(MatchNotice(Some(format!("{failure}: {err}"))));
    }
}

/// Leaves the match: disconnects from the server or stops ours, and despawns everything the
/// match brought (players, soldiers, flags, the level). Back to the menu.
pub fn leave_match(world: &mut World) {
    crate::content::leave(world);
    if let Some(mut transport) = world.remove_resource::<NetcodeClientTransport>() {
        transport.disconnect();
    }
    world.remove_resource::<RenetClient>();
    // Stops our server, if we run one (its thread ends).
    world.remove_resource::<crate::local_server::LocalServer>();
    // What a remote server replicated, and what the level spawned on our side.
    let entities: Vec<Entity> = world
        .query_filtered::<Entity, Or<(With<Remote>, With<LevelEntity>)>>()
        .iter(world)
        .collect();
    for entity in entities {
        let _ = world.try_despawn(entity);
    }
    world.remove_resource::<LoadedLevel>();
    world.remove_resource::<LocalClientId>();
    world.insert_resource(Armory::default());
    world.insert_resource(InputHistory::default());
    world.insert_resource(CombatFeedback::default());
    world.insert_resource(WeaponSelection::default());
    world.insert_resource(DeployScreen::default());
    world.insert_resource(ActiveMatch::default());
    world.insert_resource(MatchNotice::default());
    world.insert_resource(KickReason::default());
    world.resource_mut::<Time<Virtual>>().unpause();
    let mut look = world.resource_mut::<LookState>();
    look.pitch = 0.0;
    let mut cursors = world.query::<&mut CursorOptions>();
    for mut cursor in cursors.iter_mut(world) {
        cursor.visible = true;
        cursor.grab_mode = CursorGrabMode::None;
    }
    set_screen(world, Screen::Menu);
}

fn set_screen(world: &mut World, screen: Screen) {
    world
        .resource_mut::<NextState<Screen>>()
        .into_inner()
        .set_if_neq(screen);
}

pub(crate) fn connect(world: &mut World, server: SocketAddr) -> Result<()> {
    let channels = world.resource::<RepliconChannels>();
    let client = RenetClient::new(ConnectionConfig {
        server_channels_config: channels.server_configs(),
        client_channels_config: channels.client_configs(),
        ..default()
    });
    let current_time = SystemTime::now().duration_since(SystemTime::UNIX_EPOCH)?;
    // Never 0 or 1, the local host player's and our own server's link client's ids.
    let client_id = (current_time.as_nanos() as u64).max(2);
    // Loopback servers get a loopback socket: no firewall prompt for local tests.
    let bind_ip: IpAddr = match server.ip() {
        IpAddr::V4(ip) if ip.is_loopback() => Ipv4Addr::LOCALHOST.into(),
        IpAddr::V6(ip) if ip.is_loopback() => Ipv6Addr::LOCALHOST.into(),
        IpAddr::V4(_) => Ipv4Addr::UNSPECIFIED.into(),
        IpAddr::V6(_) => Ipv6Addr::UNSPECIFIED.into(),
    };
    let socket = UdpSocket::bind((bind_ip, 0))?;
    let authentication = ClientAuthentication::Unsecure {
        client_id,
        protocol_id: PROTOCOL_ID,
        server_addr: server,
        user_data: None,
    };
    let transport = NetcodeClientTransport::new(current_time, authentication, socket)?;

    world.insert_resource(client);
    world.insert_resource(transport);
    world.insert_resource(LocalClientId(client_id));
    info!("connecting to {server}");
    Ok(())
}

/// Connected: remembers the server. The hello with our name follows once the server let us
/// in (`crate::join`).
fn send_hello(mut active: ResMut<ActiveMatch>, mut settings: ResMut<Settings>) {
    active.connected = true;
    if let Some(MatchSetup::Join { server, .. }) = &active.setup {
        // Newest first in the browser's recent servers.
        let address = server.ip().to_string();
        let recent = &mut settings.recent_servers;
        let old = recent.iter().position(|s| s.is(&address, server.port()));
        let name = old.map(|i| recent.remove(i).name).unwrap_or_default();
        recent.insert(0, SavedServer { address, port: server.port(), name });
        recent.truncate(MAX_RECENT_SERVERS);
    }
}

fn receive_kick(mut kicks: MessageReader<Kicked>, mut reason: ResMut<KickReason>) {
    for kick in kicks.read() {
        warn!("kicked: {}", kick.reason);
        reason.0 = Some(kick.reason.clone());
    }
}

/// The server went away or never answered: back to the menu, saying so. Leaving on
/// purpose clears the active match first, so that ends here quietly.
fn connection_lost(world: &mut World) {
    let kicked = world.resource_mut::<KickReason>().0.take();
    let active = world.resource::<ActiveMatch>();
    let Some(MatchSetup::Join { server, .. }) = &active.setup else {
        return;
    };
    let notice = if let Some(reason) = kicked {
        format!("Kicked from {server}: {reason}")
    } else if active.connected {
        let reason = world
            .get_resource::<RenetClient>()
            .and_then(|client| client.disconnect_reason())
            .map(|reason| format!(" ({reason})"))
            .unwrap_or_default();
        format!("Lost connection to {server}{reason}")
    } else {
        format!("No answer from {server}. Is the server running, and the port open?")
    };
    warn!("{notice}");
    leave_match(world);
    world.insert_resource(MatchNotice(Some(notice)));
}

/// The server's level can't be loaded here (missing, or a name that isn't a folder name):
/// leave, rather than play another level than the one the server simulates.
fn level_load_failed(failed: On<game_shared::level::LevelLoadFailed>, mut commands: Commands) {
    let level: String = failed.level.escape_debug().take(64).collect();
    let notice = format!("Can't load the server's level `{level}`: {}", failed.error);
    commands.queue(move |world: &mut World| {
        warn!("{notice}");
        leave_match(world);
        world.insert_resource(MatchNotice(Some(notice)));
    });
}

fn tag_local_entities(
    mut commands: Commands,
    local_id: Option<Res<LocalClientId>>,
    players: Query<(Entity, &PlayerNetId, Has<LocalPlayer>), With<Player>>,
    soldiers: Query<
        (Entity, &ControlledBy, &SoldierMotion),
        (With<Soldier>, Without<LocalSoldier>),
    >,
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
