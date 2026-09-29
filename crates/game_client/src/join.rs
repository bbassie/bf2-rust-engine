//! The client side of the join handshake (`game_shared::join`). After connecting we send a
//! [`JoinRequest`]; the server answers with a [`JoinChallenge`], whose identity proof we check
//! (and compare with the key the content endpoint showed before connecting). Then, on a
//! thread: a ticket from the master server if the server takes accounts and we are logged in
//! (`crate::account`), and a report of the files our game would load for the level
//! (`game_shared::content::verify`). The server decides:
//!
//! - `Accepted`: in the match (we say hello with our name), or free to spawn after a map
//!   change;
//! - `SendHashes`: the per-file hashes of the last report;
//! - `ManifestChanged`: its content changed since we fetched the manifest: fetch it, report;
//! - `Fetch`: download the files it names ([`crate::content::begin_repair`]), then report
//!   again; during a map change, join again instead, since the level is already loading.
//!   With downloads off we decline, and the server kicks us saying why.

use std::{
    net::SocketAddr,
    path::PathBuf,
    sync::{Arc, Mutex},
};

use bevy::prelude::*;
use bevy_replicon::prelude::*;
use game_auth::random_bytes;
use game_shared::{
    config::GamePaths,
    content::{
        HashIndex, Manifest,
        store::ContentStore,
        verify::{self},
    },
    join::{AccountTicket, ContentReport, JOIN_PURPOSE, JoinChallenge, JoinRequest, JoinVerdict},
    protocol::ClientHello,
};

use crate::{
    account::Account,
    content::{ContentDownloads, ContentJob, ContentMount, Job, JobKind, JobShared, Phase, ServerIdentity},
    net::{self, ActiveMatch, MatchNotice, MatchSetup},
};

pub struct JoinPlugin;

impl Plugin for JoinPlugin {
    fn build(&self, app: &mut App) {
        app.init_resource::<JoinState>()
            .add_systems(OnEnter(ClientState::Connected), send_request)
            .add_systems(PreUpdate, (receive_challenges, receive_verdicts).chain().after(ClientSystems::Receive))
            .add_systems(Update, send_prepared);
    }
}

/// Where the handshake is.
#[derive(Resource, Default)]
pub struct JoinState {
    nonce: Option<[u8; 32]>,
    /// The server's identity, proven in the challenge.
    pub identity: Option<ServerIdentity>,
    challenge: Option<JoinChallenge>,
    /// The server let us in (at least once this connection).
    pub authorized: bool,
    /// The last report and its per-file hashes, for `SendHashes`.
    last_report: Option<(ContentReport, Vec<[u8; 32]>)>,
    /// A manifest fetched for reports (when none came before connecting, or it changed).
    manifest: Option<Arc<Manifest>>,
    /// Filled by the report and ticket threads.
    outbox: Arc<Mutex<Vec<Outgoing>>>,
}

enum Outgoing {
    Report { report: ContentReport, hashes: Vec<[u8; 32]>, manifest: Option<Arc<Manifest>> },
    Ticket(Option<String>),
    /// Leave, saying why.
    Fail(String),
}

fn server_of(active: &ActiveMatch) -> Option<SocketAddr> {
    match &active.setup {
        Some(MatchSetup::Join { server, .. }) => Some(*server),
        _ => None,
    }
}

fn send_request(
    mut state: ResMut<JoinState>,
    active: Res<ActiveMatch>,
    protocol: Res<ProtocolHash>,
    mut requests: MessageWriter<JoinRequest>,
) {
    if server_of(&active).is_none() {
        return;
    }
    let nonce = random_bytes();
    *state = JoinState { nonce: Some(nonce), ..default() };
    requests.write(JoinRequest {
        protocol: *protocol,
        game_version: env!("CARGO_PKG_VERSION").into(),
        nonce,
    });
    info!("join: asked to join");
}

/// Shows the join panel with `phase` while the server checks our content.
fn show_phase(world: &mut World, phase: Phase) {
    let Some(server) = server_of(world.resource::<ActiveMatch>()) else {
        return;
    };
    let mut job = world.resource_mut::<ContentJob>();
    match job.0.as_ref() {
        // A download of ours is running: it shows itself.
        Some(running) if running.kind != JobKind::Verify => {}
        Some(running) => running.shared.set(phase),
        None => {
            let shared = Arc::new(JobShared::default());
            shared.set(phase);
            job.0 = Some(Job { server, shared, kind: JobKind::Verify });
        }
    }
}

fn hide_phase(world: &mut World) {
    let mut job = world.resource_mut::<ContentJob>();
    if job.0.as_ref().is_some_and(|j| j.kind == JobKind::Verify) {
        job.0 = None;
    }
}

/// Leaves the match with a notice.
fn fail(world: &mut World, notice: String) {
    warn!("join: {notice}");
    net::leave_match(world);
    world.insert_resource(MatchNotice(Some(notice)));
}

fn receive_challenges(mut commands: Commands, mut challenges: MessageReader<JoinChallenge>) {
    for challenge in challenges.read() {
        let challenge = challenge.clone();
        commands.queue(move |world: &mut World| on_challenge(world, challenge));
    }
}

fn on_challenge(world: &mut World, challenge: JoinChallenge) {
    let Some(server) = server_of(world.resource::<ActiveMatch>()) else {
        return;
    };
    let Some(nonce) = world.resource::<JoinState>().nonce else {
        return;
    };
    let key = match challenge.identity.check(JOIN_PURPOSE, &nonce, challenge.manifest_id(), &challenge.server_name) {
        Ok(key) => key,
        Err(err) => return fail(world, format!("{server}: {err}. Not joining.")),
    };
    let identity = ServerIdentity {
        name: challenge.server_name.clone(),
        address: server.to_string(),
        public_key: key,
        fingerprint: game_auth::fingerprint(&key),
    };
    // The key the content endpoint proved before connecting must be the same.
    if let Some(before) = &world.resource::<ContentMount>().identity
        && before.public_key != key
    {
        return fail(
            world,
            format!(
                "{server}'s key changed between checking its content ({}) and joining ({}). Try again.",
                before.fingerprint, identity.fingerprint
            ),
        );
    }
    info!(
        "join: {} ({}) proved its identity; {} {}",
        challenge.server_name,
        identity.fingerprint,
        if challenge.map_change { "map change to" } else { "level" },
        challenge.level
    );
    {
        let mut state = world.resource_mut::<JoinState>();
        state.identity = Some(identity.clone());
        state.challenge = Some(challenge.clone());
        state.last_report = None;
    }
    let outbox = world.resource::<JoinState>().outbox.clone();
    // An account ticket, if the server takes them (only when joining).
    if let Some(accounts) = challenge.accounts.clone().filter(|_| !challenge.map_change) {
        let account = world.get_resource::<Account>().cloned();
        let fingerprint = identity.fingerprint.clone();
        let _ = std::thread::Builder::new().name("join ticket".into()).spawn(move || {
            let ticket = account.map_or(Ok(None), |a| a.ticket(&fingerprint, &accounts.master_fingerprint));
            let outgoing = match ticket {
                Ok(ticket) => Outgoing::Ticket(ticket),
                Err(err) if accounts.required => Outgoing::Fail(format!(
                    "This is a ranked server and your account couldn't get a ticket for it: {err}"
                )),
                Err(err) => {
                    warn!("join: no account ticket ({err}); joining without");
                    Outgoing::Ticket(None)
                }
            };
            outbox.lock().unwrap().push(outgoing);
        });
    }
    if challenge.preparing {
        show_phase(world, Phase::Preparing("The server is still preparing its content".into()));
        return;
    }
    if challenge.content.is_some() {
        start_report(world);
    }
}

/// Hashes the files our game would load for the challenge's level, on a thread, and queues
/// the report.
fn start_report(world: &mut World) {
    let state = world.resource::<JoinState>();
    let (Some(challenge), Some(server)) = (state.challenge.clone(), server_of(world.resource::<ActiveMatch>())) else {
        return;
    };
    let Some(check) = challenge.content.clone() else {
        return;
    };
    let mount = world.resource::<ContentMount>();
    // The manifest from before connecting, if it is still the server's.
    let manifest = [mount.manifest.clone(), state.manifest.clone()]
        .into_iter()
        .flatten()
        .find(|m| m.content_id() == check.manifest_id);
    let base = format!("http://{}", SocketAddr::new(server.ip(), check.port));
    let paths = world.resource::<GamePaths>().clone();
    let index_file: Option<PathBuf> = crate::content::cache_dir(world)
        .and_then(|dir| ContentStore::open(&dir).ok())
        .map(|store| store.index_file());
    let outbox = state.outbox.clone();
    let level = challenge.level.clone();
    show_phase(world, Phase::Verifying);
    let _ = std::thread::Builder::new().name("content report".into()).spawn(move || {
        let fetched = manifest.is_none();
        let manifest = match manifest {
            Some(manifest) => manifest,
            None => match crate::content::manifest_from(&base) {
                Ok(manifest) if manifest.content_id() == check.manifest_id => Arc::new(manifest),
                Ok(_) => {
                    // Changed again meanwhile: the server says so, and we try once more.
                    Arc::new(Manifest::default())
                }
                Err(err) => {
                    let report = ContentReport { level, manifest_id: check.manifest_id, declined: Some(err), ..default() };
                    outbox.lock().unwrap().push(Outgoing::Report { report, hashes: Vec::new(), manifest: None });
                    return;
                }
            },
        };
        let required = manifest.required(&level);
        let mut index = HashIndex::load(index_file);
        let (hashes, digest) = verify::local_hashes(&required, &paths, &mut index);
        let report = ContentReport {
            level,
            manifest_id: manifest.content_id(),
            digest,
            hashes: Vec::new(),
            declined: None,
        };
        outbox.lock().unwrap().push(Outgoing::Report { report, hashes, manifest: fetched.then_some(manifest) });
    });
}

/// Reports again after a repair (called by `content`).
pub fn report_again(world: &mut World) {
    start_report(world);
}

fn send_prepared(mut commands: Commands, state: Res<JoinState>, mut reports: MessageWriter<ContentReport>, mut tickets: MessageWriter<AccountTicket>) {
    let outgoing: Vec<Outgoing> = std::mem::take(&mut *state.outbox.lock().unwrap());
    for out in outgoing {
        match out {
            Outgoing::Report { report, hashes, manifest } => {
                info!("join: reporting our content for {} ({} files)", report.level, hashes.len());
                reports.write(report.clone());
                commands.queue(move |world: &mut World| {
                    if let Some(manifest) = manifest {
                        // Repairs work from the server's current manifest.
                        let mut mount = world.resource_mut::<ContentMount>();
                        if mount.mounted.is_some() {
                            mount.manifest = Some(manifest.clone());
                        }
                        world.resource_mut::<JoinState>().manifest = Some(manifest);
                    }
                    world.resource_mut::<JoinState>().last_report = Some((report, hashes));
                });
            }
            Outgoing::Ticket(ticket) => {
                info!("join: {}", if ticket.is_some() { "offering our account ticket" } else { "no account ticket to offer" });
                tickets.write(AccountTicket { ticket });
            }
            Outgoing::Fail(notice) => commands.queue(move |world: &mut World| fail(world, notice)),
        }
    }
}

fn receive_verdicts(mut commands: Commands, mut verdicts: MessageReader<JoinVerdict>) {
    for verdict in verdicts.read() {
        let verdict = verdict.clone();
        commands.queue(move |world: &mut World| on_verdict(world, verdict));
    }
}

fn on_verdict(world: &mut World, verdict: JoinVerdict) {
    if server_of(world.resource::<ActiveMatch>()).is_none() {
        return;
    }
    let level = world.resource::<JoinState>().challenge.as_ref().map(|c| c.level.clone()).unwrap_or_default();
    match verdict {
        JoinVerdict::Accepted => {
            hide_phase(world);
            let first = !std::mem::replace(&mut world.resource_mut::<JoinState>().authorized, true);
            info!("join: content verified by the server for {level}{}", if first { "; joined" } else { "" });
            if first && let Some(MatchSetup::Join { name, .. }) = world.resource::<ActiveMatch>().setup.clone() {
                world.write_message(ClientHello { name });
            }
        }
        JoinVerdict::SendHashes => {
            let Some((mut report, hashes)) = world.resource::<JoinState>().last_report.clone() else {
                return;
            };
            info!("join: the server asks for our per-file hashes");
            report.hashes = hashes;
            world.write_message(report);
        }
        JoinVerdict::ManifestChanged { manifest_id } => {
            info!("join: the server's content changed ({manifest_id}); fetching its manifest again");
            world.resource_mut::<JoinState>().manifest = None;
            if let Some(challenge) = world.resource_mut::<JoinState>().challenge.as_mut()
                && let Some(content) = challenge.content.as_mut()
            {
                content.manifest_id = manifest_id;
            }
            start_report(world);
        }
        JoinVerdict::Fetch { files, more } => {
            let files: Vec<game_shared::content::FileEntry> = files.into_iter().map(Into::into).collect();
            let total = files.len() as u32 + more;
            info!(
                "join: the server says {total} of our files for {level} differ from its own, e.g. {}",
                files.first().map_or("", |f| f.path.as_str())
            );
            let Some(identity) = world.resource::<JoinState>().identity.clone() else {
                return;
            };
            if crate::content::downloads_mode(world) == ContentDownloads::Never {
                let bytes: u64 = files.iter().map(|f| f.size).sum();
                let report = ContentReport {
                    level,
                    declined: Some(format!(
                        "{total} files differ from yours ({}{}) and downloading server content is off (Settings > Game > Server content).",
                        game_shared::content::format_bytes(bytes),
                        if more > 0 { " and more" } else { "" }
                    )),
                    ..default()
                };
                world.write_message(report);
                return;
            }
            hide_phase(world);
            let rejoin = world.resource::<JoinState>().authorized;
            crate::content::begin_repair(world, files, more, level, identity, rejoin);
        }
    }
}
