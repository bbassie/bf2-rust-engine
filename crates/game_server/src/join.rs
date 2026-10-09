//! The server side of the join handshake (`game_shared::join`): the server decides when a
//! client is let into the match.
//!
//! A client that connects is `Joining`: replicon doesn't replicate to it and it has no
//! player yet. It sends a [`JoinRequest`] (protocol hash, a random nonce); the server answers
//! with a [`JoinChallenge`]: its identity (the nonce signed with its key), the level, what the
//! level requires ([`crate::content::Requirement`]) and whether it takes account tickets. The
//! client reports its files ([`ContentReport`]) and, if asked, its ticket; once both are
//! fine the server authorizes it and creates its player.
//!
//! On a map change every player's client is challenged again for the new level, and the
//! player can't spawn (`ContentPending`) until its report matches. Servers that share
//! nothing skip that check.
//!
//! Kicked, with the reason: another game version, no join request within
//! [`REQUEST_TIMEOUT`], a declined download, no valid ticket on a ranked server, a report
//! that still differs after [`MAX_FETCH_ROUNDS`] downloads, or no match within the sync
//! timeout (`ContentSettings::sync_timeout`, 10 minutes by default).

use std::sync::Arc;

use bevy::{ecs::system::SystemParam, prelude::*};
use bevy_replicon::prelude::*;
use game_auth::{Identity, token::Claims};
use game_shared::{
    chat::Kicked,
    content::{
        format_bytes,
        verify::{self, MAX_FETCH_LISTED},
    },
    join::{
        AccountTicket, ContentCheck, ContentReport, JOIN_PURPOSE, JoinChallenge, JoinRequest, JoinVerdict,
    },
    protocol::MatchInfo,
};

use crate::{
    ClientPlayer, ServerSettings, admin,
    accounts::Accounts,
    content::{ContentServer, Requirement},
    limits::{Rate, RateLimiter},
};

/// A client that hasn't asked to join by then is kicked (an old client, or not a game).
pub const REQUEST_TIMEOUT: f32 = 30.0;
/// Default time a client has to get the content right.
pub const DEFAULT_SYNC_TIMEOUT: u32 = 600;
/// Reports that still differ after this many downloads get the client kicked.
pub const MAX_FETCH_ROUNDS: u32 = 3;

pub struct JoinPlugin;

impl Plugin for JoinPlugin {
    fn build(&self, app: &mut App) {
        app.add_observer(client_connected)
            .add_observer(map_changed)
            .add_systems(
                PreUpdate,
                (receive_requests, receive_tickets, receive_reports)
                    .chain()
                    .after(ServerSystems::Receive)
                    .run_if(resource_exists::<ServerIdentity>)
                    .run_if(in_state(ClientState::Disconnected)),
            )
            .add_systems(
                Update,
                (challenge_waiting, time_out)
                    .run_if(resource_exists::<ServerIdentity>)
                    .run_if(in_state(ClientState::Disconnected)),
            );
    }
}

/// The server's identity key (see `game_auth::identity`).
#[derive(Resource, Clone)]
pub struct ServerIdentity(pub Arc<Identity>);

/// Server-side, on a client entity: joining (or checking its content after a map change).
#[derive(Component, Debug)]
pub struct Joining {
    /// Real seconds when this check started (the connection, or the map change).
    since: f32,
    /// A player already in the match, after a map change.
    map_change: bool,
    /// The level of the last challenge; `None` until the client asked to join.
    challenged: Option<String>,
    /// The last challenge said the server is preparing its content.
    preparing: bool,
    content_ok: bool,
    ticket: TicketState,
    /// Fetch verdicts sent.
    fetches: u32,
}

impl Joining {
    fn new(since: f32, map_change: bool) -> Self {
        Self {
            since,
            map_change,
            challenged: None,
            preparing: false,
            content_ok: false,
            ticket: TicketState::NotAsked,
            fetches: 0,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum TicketState {
    /// The server takes no tickets (or the client isn't asked again on a map change).
    NotAsked,
    Waiting,
    Done,
}

/// Server-side, on a client entity: the nonce of its join request. Identity proofs on map
/// changes sign it again.
#[derive(Component, Clone, Copy)]
pub struct ClientNonce(pub [u8; 32]);

/// Server-side, on a client entity: its verified account.
#[derive(Component, Clone, Debug)]
pub struct ClientAccount(pub Claims);

/// Server-side, on a player: its client is checking its content for the new map; it doesn't
/// spawn meanwhile.
#[derive(Component)]
pub struct ContentPending;

/// Loads (or makes) the server's identity key. Called by `start_server` for networked
/// servers.
pub fn start(world: &mut World) {
    let settings = world.resource::<ServerSettings>();
    if !settings.network {
        return;
    }
    let file = settings
        .content
        .identity_file
        .clone()
        .or_else(|| crate::server_config::data_dir().map(|d| d.join("identity.key")));
    let identity = match file.as_deref().map(Identity::load_or_create) {
        Some(Ok((identity, created))) => {
            let file = file.as_ref().unwrap().display();
            if created {
                info!("server identity: made a new key, fingerprint {} ({file})", identity.fingerprint());
            } else {
                info!("server identity: fingerprint {} ({file})", identity.fingerprint());
            }
            identity
        }
        Some(Err(err)) => {
            let identity = Identity::generate();
            warn!("server identity: {err}; using a temporary key ({}): players will be asked to trust this server again", identity.fingerprint());
            identity
        }
        None => {
            let identity = Identity::generate();
            warn!("server identity: no data folder; using a temporary key ({})", identity.fingerprint());
            identity
        }
    };
    world.insert_resource(ServerIdentity(Arc::new(identity)));
}

pub fn stop(world: &mut World) {
    world.remove_resource::<ServerIdentity>();
}

/// Everything the handshake systems need to answer a client.
#[derive(SystemParam)]
struct Handshake<'w, 's> {
    time: Res<'w, Time<Real>>,
    settings: Res<'w, ServerSettings>,
    identity: Res<'w, ServerIdentity>,
    content: Option<Res<'w, ContentServer>>,
    accounts: Option<ResMut<'w, Accounts>>,
    matches: Query<'w, 's, &'static MatchInfo>,
    challenges: MessageWriter<'w, ToClients<JoinChallenge>>,
    verdicts: MessageWriter<'w, ToClients<JoinVerdict>>,
    kicks: MessageWriter<'w, ToClients<Kicked>>,
    disconnects: MessageWriter<'w, DisconnectRequest>,
}

impl Handshake<'_, '_> {
    fn now(&self) -> f32 {
        self.time.elapsed_secs()
    }

    fn level(&self) -> String {
        self.matches.iter().next().map_or_else(|| self.settings.level.clone(), |m| m.level.clone())
    }

    fn requirement(&self, level: &str) -> Requirement {
        self.content.as_ref().map_or(Requirement::Nothing, |c| c.requirement(level))
    }

    fn sync_timeout(&self) -> f32 {
        match self.settings.content.sync_timeout {
            0 => DEFAULT_SYNC_TIMEOUT as f32,
            secs => secs as f32,
        }
    }

    /// Sends `client` the challenge for `level`. Returns whether the server is still
    /// preparing its content (another challenge follows then).
    fn challenge(&mut self, client: Entity, nonce: &[u8; 32], level: &str, joining: &mut Joining) -> bool {
        let requirement = self.requirement(level);
        let preparing = matches!(requirement, Requirement::Preparing);
        let content = match &requirement {
            Requirement::Files(required) => Some(ContentCheck {
                manifest_id: required.manifest_id.clone(),
                mode: required.mode,
                files: required.files.len() as u32,
                bytes: required.bytes,
                port: required.port,
            }),
            _ => None,
        };
        let manifest_id = content.as_ref().map_or(String::new(), |c| c.manifest_id.clone());
        let name = self.settings.name.clone();
        // Accounts only when joining: a map change keeps the player's.
        let accounts = if joining.map_change { None } else { self.accounts.as_ref().and_then(|a| a.info()) };
        if accounts.is_some() && joining.ticket == TicketState::NotAsked {
            joining.ticket = TicketState::Waiting;
        }
        let remaining = (self.sync_timeout() - (self.now() - joining.since)).max(1.0);
        self.challenges.write(ToClients {
            targets: SendTargets::Single(ClientId::Client(client)),
            message: JoinChallenge {
                identity: self.identity.0.prove(JOIN_PURPOSE, nonce, &manifest_id, &name),
                server_name: name,
                level: level.to_string(),
                content,
                accounts,
                map_change: joining.map_change,
                preparing,
                timeout_secs: remaining as u32,
            },
        });
        joining.challenged = Some(level.to_string());
        joining.preparing = preparing;
        if matches!(requirement, Requirement::Nothing) {
            joining.content_ok = true;
        }
        preparing
    }

    fn kick(&mut self, client: Entity, reason: &str) {
        info!("kicking joining client {client}: {reason}");
        self.kicks.write(ToClients {
            targets: SendTargets::Single(ClientId::Client(client)),
            message: Kicked { reason: reason.to_string() },
        });
        self.disconnects.write(DisconnectRequest { client });
    }

    fn verdict(&mut self, client: Entity, verdict: JoinVerdict) {
        self.verdicts.write(ToClients {
            targets: SendTargets::Single(ClientId::Client(client)),
            message: verdict,
        });
    }
}

fn client_connected(
    add: On<Add, ConnectedClient>,
    mut commands: Commands,
    time: Res<Time<Real>>,
    links: Query<(), With<crate::transport::LinkClient>>,
) {
    // The client of this process (`embedded`) is let in without a handshake.
    if !links.contains(add.entity) {
        commands.entity(add.entity).insert(Joining::new(time.elapsed_secs(), false));
    }
}

/// Lets `client` in if its content and account are both fine.
fn try_accept(
    commands: &mut Commands,
    handshake: &mut Handshake,
    client: Entity,
    joining: &Joining,
    player: Option<&ClientPlayer>,
) {
    if !joining.content_ok || joining.ticket == TicketState::Waiting {
        return;
    }
    let level = joining.challenged.clone().unwrap_or_default();
    commands.entity(client).remove::<Joining>();
    if joining.map_change {
        if let Some(player) = player {
            commands.entity(player.0).remove::<ContentPending>();
        }
        info!("client {client}: content verified for {level}");
    } else {
        // Replication starts; `create_client_player` makes its player.
        commands.entity(client).insert(AuthorizedClient);
        info!("client {client}: content verified for {level}, joined");
    }
    handshake.verdict(client, JoinVerdict::Accepted);
}

fn receive_requests(
    mut commands: Commands,
    mut requests: MessageReader<FromClient<JoinRequest>>,
    protocol: Res<ProtocolHash>,
    mut clients: Query<(&mut Joining, Option<&ClientPlayer>)>,
    mut handshake: Handshake,
) {
    for FromClient { client_id, message } in requests.read() {
        let Some(client) = client_id.entity() else {
            continue;
        };
        let Ok((mut joining, player)) = clients.get_mut(client) else {
            continue;
        };
        if joining.challenged.is_some() {
            continue;
        }
        if message.protocol != *protocol {
            let theirs: String = message.game_version.chars().take(32).collect();
            let reason = if theirs == env!("CARGO_PKG_VERSION") {
                format!("The server runs another build of the game ({theirs} too, but the network protocol differs): both need the same build.")
            } else {
                format!("The server runs another version of the game ({}; yours is {theirs}).", env!("CARGO_PKG_VERSION"))
            };
            handshake.kick(client, &reason);
            continue;
        }
        if handshake.settings.accounts.ranked && handshake.accounts.as_ref().is_some_and(|a| a.info().is_none()) {
            handshake.kick(client, "This ranked server can't check accounts right now (its master server doesn't answer). Try again in a minute.");
            continue;
        }
        commands.entity(client).insert(ClientNonce(message.nonce));
        let level = handshake.level();
        handshake.challenge(client, &message.nonce, &level, &mut joining);
        try_accept(&mut commands, &mut handshake, client, &joining, player);
    }
}

fn receive_tickets(
    mut commands: Commands,
    mut tickets: MessageReader<FromClient<AccountTicket>>,
    mut clients: Query<(&mut Joining, Option<&ClientPlayer>)>,
    mut handshake: Handshake,
) {
    for FromClient { client_id, message } in tickets.read() {
        let Some(client) = client_id.entity() else {
            continue;
        };
        let Ok((mut joining, player)) = clients.get_mut(client) else {
            continue;
        };
        if joining.ticket != TicketState::Waiting {
            continue;
        }
        let fingerprint = handshake.identity.0.fingerprint();
        let Some(accounts) = handshake.accounts.as_mut() else {
            joining.ticket = TicketState::Done;
            continue;
        };
        let required = accounts.required();
        let master_url = accounts.master_url();
        let checked = message.ticket.as_deref().map(|ticket| accounts.check_ticket(ticket, &fingerprint));
        match checked {
            Some(Ok(claims)) => {
                info!("client {client}: account {} ({}) verified", claims.name, claims.rank_name);
                // Usually the player doesn't exist yet (`create_client_player` grants admin
                // rights then, reading `ClientAccount` back); this covers the other order too.
                if let Some(player) = player {
                    admin::grant_if_admin(&mut commands, &handshake.settings.admin.admins, player.0, &claims);
                }
                commands.entity(client).insert(ClientAccount(claims));
            }
            Some(Err(err)) if required => {
                handshake.kick(client, &format!("Your account ticket was refused ({err}). Log in again on the Account page."));
                continue;
            }
            None if required => {
                handshake.kick(
                    client,
                    &format!("This is a ranked server: log in with an account from {master_url} (Account page) to play here."),
                );
                continue;
            }
            Some(Err(err)) => info!("client {client}: account ticket refused ({err}); joining without"),
            None => {}
        }
        joining.ticket = TicketState::Done;
        try_accept(&mut commands, &mut handshake, client, &joining, player);
    }
}

fn receive_reports(
    mut commands: Commands,
    mut reports: MessageReader<FromClient<ContentReport>>,
    mut clients: Query<(&mut Joining, Option<&ClientPlayer>)>,
    mut handshake: Handshake,
    mut limits: Local<RateLimiter<Entity>>,
) {
    let now = handshake.time.elapsed_secs_f64();
    let mut kicked = Vec::new();
    for FromClient { client_id, message: report } in reports.read() {
        let Some(client) = client_id.entity() else {
            continue;
        };
        let Ok((mut joining, player)) = clients.get_mut(client) else {
            continue;
        };
        if kicked.contains(&client) {
            continue;
        }
        // Each report compares the whole required list: a handful per check is all it takes.
        if !limits.allow(client, Rate::CONTENT_REPORTS, now) {
            handshake.kick(client, "Your game sent too many content reports.");
            kicked.push(client);
            continue;
        }
        // A report for an older challenge (the map changed meanwhile).
        if joining.challenged.as_deref() != Some(report.level.as_str()) || joining.content_ok {
            continue;
        }
        if let Some(why) = &report.declined {
            let why: String = why.chars().take(200).collect();
            handshake.kick(client, &format!("You need the server's content for {} to play here: {why}", report.level));
            continue;
        }
        let required = match handshake.requirement(&report.level) {
            Requirement::Nothing => {
                joining.content_ok = true;
                try_accept(&mut commands, &mut handshake, client, &joining, player);
                continue;
            }
            // A new challenge follows once it is ready.
            Requirement::Preparing => continue,
            Requirement::Files(required) => required,
        };
        if report.manifest_id != required.manifest_id {
            handshake.verdict(client, JoinVerdict::ManifestChanged { manifest_id: required.manifest_id.clone() });
            continue;
        }
        if report.digest == required.digest {
            joining.content_ok = true;
            try_accept(&mut commands, &mut handshake, client, &joining, player);
            continue;
        }
        if report.hashes.is_empty() {
            handshake.verdict(client, JoinVerdict::SendHashes);
            continue;
        }
        // One hash per required file, checked before anything is compared.
        if report.hashes.len() != required.files.len() {
            handshake.kick(client, "Your game sent a broken content report.");
            continue;
        }
        let files: Vec<_> = required.files.iter().collect();
        let wrong = verify::mismatches(&files, &report.hashes);
        let Some(wrong) = wrong else {
            handshake.kick(client, "Your game sent a broken content report.");
            continue;
        };
        if wrong.is_empty() {
            // The files match even though the digest didn't (it is only a shortcut).
            joining.content_ok = true;
            try_accept(&mut commands, &mut handshake, client, &joining, player);
            continue;
        }
        joining.fetches += 1;
        if joining.fetches > MAX_FETCH_ROUNDS {
            let reason = format!(
                "Your copy of the server's content still differs after {MAX_FETCH_ROUNDS} downloads ({}, and {} more). Delete the content cache (Settings > Game) and join again.",
                wrong[0].path,
                wrong.len() - 1
            );
            handshake.kick(client, &reason);
            continue;
        }
        let bytes: u64 = wrong.iter().map(|f| f.size).sum();
        info!(
            "client {client}: {} of {} files for {} differ ({}), e.g. {}; asking it to fetch them",
            wrong.len(),
            files.len(),
            report.level,
            format_bytes(bytes),
            wrong[0].path
        );
        let listed = wrong.len().min(MAX_FETCH_LISTED);
        handshake.verdict(
            client,
            JoinVerdict::Fetch {
                files: wrong[..listed].iter().map(|f| (*f).into()).collect(),
                more: (wrong.len() - listed) as u32,
            },
        );
    }
}

/// The server changed the map: every player checks its content for the new level, and
/// nobody spawns until theirs matches. Clients still joining are challenged for the new
/// level instead.
#[allow(clippy::type_complexity)]
fn map_changed(
    add: On<Add, MatchInfo>,
    mut commands: Commands,
    mut clients: Query<(Entity, Option<&ClientPlayer>, Has<AuthorizedClient>, Option<&ClientNonce>, Option<&mut Joining>), With<ConnectedClient>>,
    handshake: Option<Handshake>,
) {
    let Some(mut handshake) = handshake else {
        return;
    };
    let Ok(info) = handshake.matches.get(add.entity) else {
        return;
    };
    let level = info.level.clone();
    // Servers that share nothing don't check anything on a map change.
    if matches!(handshake.requirement(&level), Requirement::Nothing) {
        return;
    }
    let now = handshake.now();
    for (client, player, authorized, nonce, joining) in &mut clients {
        let Some(ClientNonce(nonce)) = nonce.copied() else {
            continue;
        };
        match (authorized, joining) {
            (true, _) => {
                let Some(player) = player else {
                    continue;
                };
                commands.entity(player.0).insert(ContentPending);
                let mut joining = Joining::new(now, true);
                handshake.challenge(client, &nonce, &level, &mut joining);
                commands.entity(client).insert(joining);
            }
            (false, Some(mut joining)) => {
                joining.content_ok = false;
                joining.fetches = 0;
                handshake.challenge(client, &nonce, &level, &mut joining);
            }
            (false, None) => {}
        }
    }
}

/// Clients told the server was preparing its content get a real challenge once it is ready.
fn challenge_waiting(
    mut commands: Commands,
    mut clients: Query<(Entity, &mut Joining, &ClientNonce, Option<&ClientPlayer>)>,
    mut handshake: Handshake,
) {
    for (client, mut joining, nonce, player) in &mut clients {
        if !joining.preparing {
            continue;
        }
        let level = joining.challenged.clone().unwrap_or_else(|| handshake.level());
        if matches!(handshake.requirement(&level), Requirement::Preparing) {
            continue;
        }
        handshake.challenge(client, &nonce.0, &level, &mut joining);
        try_accept(&mut commands, &mut handshake, client, &joining, player);
    }
}

fn time_out(mut commands: Commands, clients: Query<(Entity, &Joining)>, mut handshake: Handshake) {
    let now = handshake.now();
    let limit = handshake.sync_timeout();
    for (client, joining) in &clients {
        let waited = now - joining.since;
        let reason = if joining.challenged.is_none() && waited > REQUEST_TIMEOUT {
            "No join request: does your game run another version?".to_string()
        } else if waited > limit {
            let level = joining.challenged.clone().unwrap_or_default();
            format!("Getting the server's content for {level} took too long ({:.0} minutes).", limit / 60.0)
        } else {
            continue;
        };
        handshake.kick(client, &reason);
        // Once: the disconnect takes a frame.
        commands.entity(client).remove::<Joining>();
    }
}
