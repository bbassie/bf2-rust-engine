//! Joining a server that shares its content (see `game_shared::content`). Before connecting,
//! the client fetches the server's manifest and its identity (its key, proven by signing a
//! random nonce and the manifest id), works out which files of the levels the server plays it
//! lacks (a file it has in its own import or mods with the same hash doesn't count), asks
//! (setting `content_downloads`, default ask; a server whose key isn't trusted yet always
//! asks, see below), downloads them into its cache, and mounts the server's content for the
//! session:
//!
//! - [`GamePaths`] becomes the server's layers (its mods, then its imported assets if it
//!   shares them, else ours). Our own mods are off: the server's content must win wherever it
//!   differs, or prediction and collision disagree with the server.
//! - The `imported://` asset source follows (see [`crate::mod_assets::set_layers`]).
//! - Libraries loaded at startup (sounds, effects, gadgets) are loaded again.
//!
//! After connecting, the server has the last word (see `crate::join`): it compares the files
//! we would load with its manifest, and names the ones to fetch again; [`begin_repair`]
//! downloads those. On a map change a repair ends with joining again, since the level is
//! already loading.
//!
//! **Trust.** Downloading from a server whose key we haven't trusted before needs a yes, with
//! the server's name, address and key fingerprint shown, even with downloads set to Always
//! (unless `confirm_new_servers` is off). Trusted keys are kept in the settings
//! (`trusted_servers`); Settings > Game lists them and forgets them.
//!
//! Leaving the match unmounts it. A server that shares nothing is joined with our own
//! content. The cache is `cache/` next to the settings file (`--content-cache` or
//! `$GAME_CONTENT_CACHE` override it), content-addressed and shared by every server, limited
//! to `content_cache_gb`.

use std::{
    collections::HashSet,
    io::Read,
    net::{IpAddr, Ipv4Addr, SocketAddr, UdpSocket},
    path::{Path, PathBuf},
    str::FromStr,
    sync::{Arc, Mutex},
    time::{Duration, Instant},
};

use bevy::prelude::*;
use game_auth::{hex, random_bytes};
use game_shared::{
    config::GamePaths,
    content::{
        self, ContentMode, FileEntry, HashIndex, IDENTITY_URL_PATH, IdentityAnswer, MANIFEST_URL_PATH, MAX_MANIFEST_BYTES,
        Manifest,
        download::{self, Fetch, FetchResponse, Progress},
        store::{ContentStore, MountedContent, server_key},
    },
    discovery::{DISCOVERY_PORTS, ServerInfo, encode_query, parse_reply},
    join::CONTENT_PURPOSE,
    level::TEST_RANGE,
};
use serde::{Deserialize, Serialize};

use crate::{
    Cli,
    net::{self, ActiveMatch, MatchNotice},
    settings::{Settings, SettingsFile},
};

/// Refuse to download more than this for one server, whatever it claims.
const MAX_DOWNLOAD_BYTES: u64 = 24_000_000_000;
/// Downloads at a time (per file the file system costs a few milliseconds on Windows).
const WORKERS: usize = 8;
/// How long to wait for a server that is still hashing its content.
const PREPARE_TIMEOUT: Duration = Duration::from_secs(600);

/// Whether to download a server's content when joining.
#[derive(Serialize, Deserialize, Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum ContentDownloads {
    /// Show what it takes and wait for a yes.
    #[default]
    Ask,
    Always,
    /// Join with our own content only.
    Never,
}

impl ContentDownloads {
    pub const ALL: [ContentDownloads; 3] = [ContentDownloads::Ask, ContentDownloads::Always, ContentDownloads::Never];

    pub fn label(self) -> &'static str {
        match self {
            ContentDownloads::Ask => "Ask",
            ContentDownloads::Always => "Always",
            ContentDownloads::Never => "Never",
        }
    }
}

impl FromStr for ContentDownloads {
    type Err = String;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s.trim().to_ascii_lowercase().as_str() {
            "ask" => Ok(ContentDownloads::Ask),
            "always" | "yes" => Ok(ContentDownloads::Always),
            "never" | "no" | "off" => Ok(ContentDownloads::Never),
            other => Err(format!("`{other}`: expected ask, always or never")),
        }
    }
}

/// A server whose content we agreed to download, by its identity key (settings).
#[derive(Serialize, Deserialize, Clone, Debug, Default, PartialEq)]
#[serde(default)]
pub struct TrustedServer {
    /// 64 hex digits.
    pub public_key: String,
    pub fingerprint: String,
    /// Its name and address when we trusted it.
    pub name: String,
    pub address: String,
    /// Seconds since 1970.
    pub since: u64,
}

/// Who a server is: its key, proven (see `game_auth::identity`).
#[derive(Clone, Debug, PartialEq)]
pub struct ServerIdentity {
    pub name: String,
    pub address: String,
    pub public_key: [u8; 32],
    pub fingerprint: String,
}

impl ServerIdentity {
    pub fn trusted_entry(&self) -> TrustedServer {
        TrustedServer {
            public_key: hex(&self.public_key),
            fingerprint: self.fingerprint.clone(),
            name: self.name.clone(),
            address: self.address.clone(),
            since: game_auth::unix_now(),
        }
    }
}

/// Whether downloading from `identity` needs a first-time yes: `(new key, the address was
/// trusted with another key)`.
pub fn trust_status(identity: &ServerIdentity, trusted: &[TrustedServer], confirm_new: bool) -> (bool, bool) {
    let key = hex(&identity.public_key);
    let known = trusted.iter().any(|t| t.public_key.eq_ignore_ascii_case(&key));
    let changed = !known && trusted.iter().any(|t| t.address == identity.address);
    (!known && confirm_new, changed)
}

pub struct ContentPlugin;

impl Plugin for ContentPlugin {
    fn build(&self, app: &mut App) {
        app.init_resource::<ContentJob>()
            .init_resource::<ContentMount>()
            // First thing in the frame, like leaving from the menu: connecting, mounting and
            // leaving swap resources other systems' run conditions look at.
            .add_systems(First, poll_job);
    }
}

/// The content check or download of a join, if one is running.
#[derive(Resource, Default)]
pub struct ContentJob(pub Option<Job>);

pub struct Job {
    pub server: SocketAddr,
    pub shared: Arc<JobShared>,
    pub kind: JobKind,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum JobKind {
    /// Before connecting: then connect.
    Join,
    /// Connected, files the server named: then report again, or join again (`rejoin`).
    Repair { rejoin: bool },
    /// Connected, the server checking our files (only shows the panel; `crate::join`).
    Verify,
}

/// Shared between the job's thread and the game.
#[derive(Default)]
pub struct JobShared {
    phase: Mutex<Phase>,
    pub progress: Progress,
    decision: Mutex<Option<bool>>,
}

impl JobShared {
    pub fn phase(&self) -> Phase {
        self.phase.lock().unwrap().clone()
    }

    pub fn set(&self, phase: Phase) {
        *self.phase.lock().unwrap() = phase;
    }

    /// The player's answer to [`Phase::Ask`].
    pub fn decide(&self, download: bool) {
        *self.decision.lock().unwrap() = Some(download);
    }

    /// Waits for the player's answer to `offer`.
    fn ask(&self, offer: &Offer) -> Result<(), String> {
        info!(
            "content: asking the player: {} files, {}{}",
            offer.files,
            content::format_bytes(offer.bytes),
            match (&offer.identity, offer.new_server) {
                (Some(identity), true) => format!(", new server {} ({}, key {})", identity.name, identity.address, identity.fingerprint),
                _ => String::new(),
            }
        );
        self.set(Phase::Ask(offer.clone()));
        loop {
            if self.progress.cancelled() {
                return Err("cancelled".into());
            }
            match self.decision.lock().unwrap().take() {
                Some(true) => return Ok(()),
                Some(false) => return Err("cancelled".into()),
                None => {}
            }
            std::thread::sleep(Duration::from_millis(50));
        }
    }
}

#[derive(Clone, Debug, Default)]
pub enum Phase {
    #[default]
    Contacting,
    /// The server is still hashing its files (its answer, e.g. "preparing 40%").
    Preparing(String),
    /// Comparing the manifest with the cache and our own files.
    Checking,
    /// Waiting for the player.
    Ask(Offer),
    Downloading(Offer),
    Mounting,
    /// Connected: the server compares our files with its own.
    Verifying,
    Done(Box<Joined>),
    Failed(String),
}

/// What joining takes.
#[derive(Clone, Debug, Default)]
pub struct Offer {
    pub server_name: String,
    pub mode: ContentMode,
    /// The shared mods' names.
    pub mods: Vec<String>,
    /// Includes the server's imported BF2 assets.
    pub imported: bool,
    pub files: usize,
    pub bytes: u64,
    /// Bytes needed that we already have (cache or our own files).
    pub have_bytes: u64,
    /// Who the server is.
    pub identity: Option<ServerIdentity>,
    /// Its key isn't trusted yet: accepting trusts it.
    pub new_server: bool,
    /// We trusted this address with another key before.
    pub key_changed: bool,
    /// A repair: the server found these of our files different from its own.
    pub repair: bool,
}

/// What a finished job leaves behind for the session.
#[derive(Clone, Debug, Default)]
pub struct Joined {
    /// A new view of the server's content to mount.
    pub mounted: Option<MountedContent>,
    pub manifest: Option<Arc<Manifest>>,
    pub identity: Option<ServerIdentity>,
    /// The content endpoint (`http://ip:port`).
    pub base: Option<String>,
    /// Where files come from: the download URL (if any), then `<base>/content`.
    pub bases: Vec<String>,
    /// The player agreed to download from this server (or downloads are set to Always).
    pub accepted: bool,
}

/// The server content mounted for the session, and our own paths to go back to.
#[derive(Resource, Default)]
pub struct ContentMount {
    original: Option<GamePaths>,
    pub mounted: Option<MountedContent>,
    /// The server's manifest and identity, from before connecting.
    pub manifest: Option<Arc<Manifest>>,
    pub identity: Option<ServerIdentity>,
    pub base: Option<String>,
    pub bases: Vec<String>,
    pub accepted: bool,
    /// Join again next frame (after a repair during a map change).
    pub rejoin: bool,
    /// Left for rejoining: join this next frame, once the connection's end went through
    /// (joining in the same frame would count as a failed connection).
    rejoin_next: Option<net::MatchSetup>,
}

impl ContentMount {
    /// Our own content, whatever is mounted.
    pub fn own_paths(&self, current: &GamePaths) -> GamePaths {
        self.original.clone().unwrap_or_else(|| current.clone())
    }

    fn remember(&mut self, joined: &Joined) {
        self.manifest = joined.manifest.clone();
        self.identity = joined.identity.clone();
        self.base = joined.base.clone();
        self.bases = joined.bases.clone();
        self.accepted |= joined.accepted;
    }
}

/// The cache folder: `--content-cache`, `$GAME_CONTENT_CACHE`, else `cache` next to the
/// settings file (or where it would be).
pub fn cache_dir(world: &World) -> Option<PathBuf> {
    let cli = world.resource::<Cli>();
    if let Some(dir) = &cli.content_cache {
        return Some(dir.clone());
    }
    if let Some(dir) = std::env::var_os("GAME_CONTENT_CACHE").filter(|v| !v.is_empty()) {
        return Some(PathBuf::from(dir));
    }
    let settings_dir = world
        .resource::<SettingsFile>()
        .0
        .as_ref()
        .and_then(|f| f.parent().map(Path::to_path_buf))
        .or_else(|| crate::settings::config_dir().map(|d| d.join("bf2-rust-engine")))?;
    let dir = if settings_dir.as_os_str().is_empty() { PathBuf::from(".") } else { settings_dir };
    Some(std::path::absolute(dir.join("cache")).unwrap_or_else(|_| dir.join("cache")))
}

/// `--content`, else the setting; scripted runs download without asking.
pub fn downloads_mode(world: &World) -> ContentDownloads {
    let cli = world.resource::<Cli>();
    if let Some(mode) = cli.content {
        return mode;
    }
    if cli.scenario.is_some() || cli.screenshot.is_some() {
        return ContentDownloads::Always;
    }
    world.resource::<Settings>().content_downloads
}

/// Whether a new server key needs a yes. Scripted runs trust new servers unless `--content`
/// is given (so scenarios can test the question).
pub fn confirm_new(world: &World) -> bool {
    let cli = world.resource::<Cli>();
    if cli.content.is_none() && (cli.scenario.is_some() || cli.screenshot.is_some()) {
        return false;
    }
    world.resource::<Settings>().confirm_new_servers
}

/// Joins `server`: first its content (unless downloads are off), then the connection, which
/// [`poll_job`] makes once the content is there. Called by [`net::start_match`].
pub fn begin_join(world: &mut World, server: SocketAddr) -> Result<()> {
    let mode = downloads_mode(world);
    let cache = cache_dir(world);
    let (Some(cache), false) = (cache, mode == ContentDownloads::Never) else {
        return net::connect(world, server);
    };
    let local = world.resource::<GamePaths>().clone();
    let limit = (world.resource::<Settings>().content_cache_gb.max(1) as u64) * 1_000_000_000;
    let shared = Arc::new(JobShared::default());
    let job = JobInput {
        server,
        cache,
        local,
        ask: mode == ContentDownloads::Ask,
        limit,
        trusted: world.resource::<Settings>().trusted_servers.clone(),
        confirm_new: confirm_new(world),
    };
    let thread_shared = shared.clone();
    std::thread::Builder::new()
        .name("content download".into())
        .spawn(move || {
            let result = run_job(&job, &thread_shared);
            let cancelled = thread_shared.progress.cancelled();
            thread_shared.set(match result {
                _ if cancelled => Phase::Failed("cancelled".into()),
                Ok(joined) => Phase::Done(Box::new(joined)),
                Err(err) => Phase::Failed(err),
            });
        })?;
    world.insert_resource(ContentJob(Some(Job { server, shared, kind: JobKind::Join })));
    Ok(())
}

/// Stops a content job and unmounts the server's content. Called by [`net::leave_match`].
pub fn leave(world: &mut World) {
    if let Some(job) = world.resource_mut::<ContentJob>().0.take() {
        job.shared.progress.cancel.store(true, std::sync::atomic::Ordering::Relaxed);
    }
    unmount(world);
    let mut mount = world.resource_mut::<ContentMount>();
    mount.manifest = None;
    mount.identity = None;
    mount.base = None;
    mount.bases.clear();
    mount.accepted = false;
}

fn poll_job(world: &mut World) {
    if let Some(setup) = world.resource_mut::<ContentMount>().rejoin_next.take() {
        net::start_match(world, setup);
    }
    if std::mem::take(&mut world.resource_mut::<ContentMount>().rejoin)
        && let Some(setup) = world.resource::<ActiveMatch>().setup.clone()
    {
        info!("content: joining again with the fetched files");
        net::leave_match(world);
        world.resource_mut::<ContentMount>().rejoin_next = Some(setup);
    }
    let Some(job) = world.resource::<ContentJob>().0.as_ref() else {
        return;
    };
    let (server, kind) = (job.server, job.kind);
    let phase = job.shared.phase();
    match phase {
        Phase::Done(joined) => {
            world.resource_mut::<ContentJob>().0 = None;
            world.resource_mut::<ContentMount>().remember(&joined);
            if let Some(mounted) = joined.mounted.clone() {
                mount(world, mounted);
            }
            match kind {
                JobKind::Join => {
                    if let Err(err) = net::connect(world, server) {
                        let notice = format!("Can't connect to {server}: {err}");
                        error!("{notice}");
                        net::leave_match(world);
                        world.insert_resource(MatchNotice(Some(notice)));
                    }
                }
                JobKind::Repair { rejoin: true } => world.resource_mut::<ContentMount>().rejoin = true,
                JobKind::Repair { rejoin: false } => crate::join::report_again(world),
                JobKind::Verify => {}
            }
        }
        Phase::Failed(err) => {
            world.resource_mut::<ContentJob>().0 = None;
            net::leave_match(world);
            if err != "cancelled" {
                warn!("content: {err}");
                world.insert_resource(MatchNotice(Some(err)));
            }
        }
        _ => {}
    }
}

fn mount(world: &mut World, mounted: MountedContent) {
    let local = world.resource::<GamePaths>().clone();
    let original = world.resource_mut::<ContentMount>().original.get_or_insert(local).clone();
    let paths = mounted.apply(&original);
    info!(
        "content: playing with {}'s content ({} files, {}): {}",
        mounted.server_name,
        mounted.files,
        content::format_bytes(mounted.bytes),
        paths.roots().iter().map(|r| r.display().to_string()).collect::<Vec<_>>().join(", ")
    );
    crate::mod_assets::set_layers(&paths);
    world.insert_resource(paths);
    world.resource_mut::<ContentMount>().mounted = Some(mounted);
    reload_libraries(world);
}

fn unmount(world: &mut World) {
    let Some(original) = world.resource_mut::<ContentMount>().original.take() else {
        return;
    };
    world.resource_mut::<ContentMount>().mounted = None;
    crate::mod_assets::set_layers(&original);
    world.insert_resource(original);
    reload_libraries(world);
}

/// What the game loaded from disk at startup comes from the other layers now.
fn reload_libraries(world: &mut World) {
    let _ = world.run_system_cached(crate::audio::load_library);
    let _ = world.run_system_cached(crate::effects::load_library);
    let _ = world.run_system_cached(crate::gadgets::load_assets);
}

struct JobInput {
    server: SocketAddr,
    cache: PathBuf,
    /// Our own content.
    local: GamePaths,
    ask: bool,
    limit: u64,
    trusted: Vec<TrustedServer>,
    confirm_new: bool,
}

pub fn agent() -> ureq::Agent {
    ureq::Agent::config_builder()
        .timeout_connect(Some(Duration::from_secs(4)))
        .timeout_recv_response(Some(Duration::from_secs(20)))
        .http_status_as_error(false)
        .user_agent(format!("bf2-rust-engine/{}", env!("CARGO_PKG_VERSION")))
        .build()
        .into()
}

/// Asks the server's discovery port about it, like the server browser: whether it shares
/// content and on which port. `None` without an answer (then we try the game port).
fn probe(server: SocketAddr) -> Option<ServerInfo> {
    let IpAddr::V4(ip) = server.ip() else {
        return None;
    };
    let bind = if ip.is_loopback() { Ipv4Addr::LOCALHOST } else { Ipv4Addr::UNSPECIFIED };
    let socket = UdpSocket::bind((bind, 0)).ok()?;
    socket.set_read_timeout(Some(Duration::from_millis(100))).ok()?;
    let token = fastrand::u64(..);
    let query = encode_query(token);
    for port in DISCOVERY_PORTS {
        let _ = socket.send_to(&query, (ip, port));
    }
    let deadline = Instant::now() + Duration::from_millis(700);
    let mut buffer = [0u8; 4096];
    while Instant::now() < deadline {
        let Ok((len, _)) = socket.recv_from(&mut buffer) else {
            continue;
        };
        if let Some((answer, info)) = parse_reply(&buffer[..len])
            && answer == token
            && info.port == server.port()
        {
            return Some(info);
        }
    }
    None
}

enum ManifestAnswer {
    Manifest(Box<Manifest>),
    /// The server shares nothing.
    None,
    /// Still hashing: its message.
    Preparing(String),
}

fn fetch_manifest(agent: &ureq::Agent, base: &str) -> Result<ManifestAnswer, ureq::Error> {
    let mut response = agent.get(format!("{base}{MANIFEST_URL_PATH}")).call()?;
    let status = response.status().as_u16();
    let body = response.body_mut().with_config().limit(MAX_MANIFEST_BYTES).read_to_vec()?;
    Ok(match status {
        200 => match Manifest::parse(&body) {
            Ok(manifest) => ManifestAnswer::Manifest(Box::new(manifest)),
            Err(err) => return Err(ureq::Error::Io(std::io::Error::other(err))),
        },
        503 => ManifestAnswer::Preparing(String::from_utf8_lossy(&body).chars().take(80).collect()),
        _ => ManifestAnswer::None,
    })
}

/// The server's identity from its content endpoint, checked: it signed our nonce with its
/// key. Also the manifest id it signed.
fn fetch_identity(agent: &ureq::Agent, base: &str, server: SocketAddr) -> Result<(ServerIdentity, String), String> {
    let nonce: [u8; 32] = random_bytes();
    let mut response = agent
        .get(format!("{base}{IDENTITY_URL_PATH}?nonce={}", hex(&nonce)))
        .call()
        .map_err(|err| format!("Can't get {server}'s identity: {err}"))?;
    let body = response.body_mut().with_config().limit(64 * 1024).read_to_string().map_err(|err| err.to_string())?;
    if response.status().as_u16() != 200 {
        return Err(format!("{server} didn't prove its identity (HTTP {})", response.status().as_u16()));
    }
    let answer: IdentityAnswer = ron::from_str(&body).map_err(|err| format!("{server} sent a broken identity: {err}"))?;
    let key = answer
        .proof
        .check(CONTENT_PURPOSE, &nonce, &answer.manifest_id, &answer.name)
        .map_err(|err| format!("{server}: {err}"))?;
    let identity = ServerIdentity {
        name: answer.name,
        address: server.to_string(),
        public_key: key,
        fingerprint: game_auth::fingerprint(&key),
    };
    Ok((identity, answer.manifest_id))
}

/// Downloads over HTTP (`ureq`).
pub struct HttpFetch(pub ureq::Agent);

impl Fetch for HttpFetch {
    fn get(&self, url: &str, offset: u64, size: u64) -> Result<FetchResponse, String> {
        // No reads without progress for long, but big files may take a while: assume at least
        // 32 KB/s. A timeout resumes where it stopped.
        let timeout = Duration::from_secs(30) + Duration::from_secs_f64(size as f64 / 32_768.0);
        let mut request = self.0.get(url);
        if offset > 0 {
            request = request.header("Range", format!("bytes={offset}-"));
        }
        let response = request
            .config()
            .timeout_recv_body(Some(timeout))
            .build()
            .call()
            .map_err(|err| err.to_string())?;
        let status = response.status().as_u16();
        // `bytes 100-199/200`
        let range_start = response
            .headers()
            .get("content-range")
            .and_then(|v| v.to_str().ok())
            .and_then(|v| v.trim().strip_prefix("bytes "))
            .and_then(|v| v.split('-').next())
            .and_then(|v| v.trim().parse().ok());
        Ok(FetchResponse { status, range_start, body: Box::new(response.into_body().into_reader()) })
    }
}

/// Downloaded sounds must decode: Bevy's audio panics on sounds it can't read.
pub fn validate(entry: &FileEntry, file: &Path) -> Result<(), String> {
    let extension = entry.path.rsplit('.').next().unwrap_or_default().to_ascii_lowercase();
    if extension != "wav" && extension != "ogg" {
        return Ok(());
    }
    let mut head = [0u8; 12];
    let read = std::fs::File::open(file).and_then(|mut f| f.read(&mut head)).unwrap_or(0);
    let magic_ok = match extension.as_str() {
        "wav" => read == 12 && &head[0..4] == b"RIFF" && &head[8..12] == b"WAVE",
        _ => read >= 4 && &head[0..4] == b"OggS",
    };
    if !magic_ok {
        return Err(format!("{}: not a sound file", entry.path));
    }
    let bytes: Arc<[u8]> = std::fs::read(file).map_err(|err| err.to_string())?.into();
    let decodes = std::panic::catch_unwind(move || {
        use bevy::audio::Decodable;
        let source = bevy::audio::AudioSource { bytes };
        drop(source.decoder());
    });
    decodes.map_err(|_| format!("{}: a sound the game can't play", entry.path))
}

/// The server's manifest: `None` if it shares nothing (or has no endpoint and didn't say it
/// shares anything). Waits while the server is preparing it.
fn get_manifest(
    agent: &ureq::Agent,
    base: &str,
    server: SocketAddr,
    advertised: bool,
    shared: &JobShared,
) -> Result<Option<Box<Manifest>>, String> {
    let started = Instant::now();
    loop {
        if shared.progress.cancelled() {
            return Err("cancelled".into());
        }
        match fetch_manifest(agent, base) {
            Ok(ManifestAnswer::Manifest(manifest)) => return Ok(Some(manifest)),
            Ok(ManifestAnswer::None) => return Ok(None),
            Ok(ManifestAnswer::Preparing(message)) => {
                if started.elapsed() > PREPARE_TIMEOUT {
                    return Err(format!("{server} has been preparing its content for too long."));
                }
                shared.set(Phase::Preparing(message));
                std::thread::sleep(Duration::from_millis(700));
            }
            // Nothing there: a server that doesn't share (or an older one). Join as before.
            Err(err) if !advertised => {
                info!("content: no content endpoint at {base} ({err}); joining with our own content");
                return Ok(None);
            }
            Err(err) => return Err(format!("Can't get {server}'s content: {err}")),
        }
    }
}

/// The manifest for the in-match check: fetched from `base` (blocking), for clients that
/// didn't get it before connecting.
pub fn manifest_from(base: &str) -> Result<Manifest, String> {
    let agent = agent();
    for _ in 0..30 {
        match fetch_manifest(&agent, base) {
            Ok(ManifestAnswer::Manifest(manifest)) => return Ok(*manifest),
            Ok(ManifestAnswer::Preparing(_)) => std::thread::sleep(Duration::from_millis(700)),
            Ok(ManifestAnswer::None) => return Err(format!("{base} shares nothing")),
            Err(err) => return Err(format!("can't reach the server's content at {base} ({err})")),
        }
    }
    Err(format!("{base} has been preparing its content for too long"))
}

/// Attempts at fetching the manifest and downloading, for a server whose files change meanwhile.
const JOB_ATTEMPTS: u32 = 3;

fn offer_for(manifest: &Manifest, plan: &download::Plan, identity: &ServerIdentity, trust: (bool, bool)) -> Offer {
    Offer {
        server_name: manifest.server_name.clone(),
        mode: manifest.mode,
        mods: manifest
            .layers
            .iter()
            .filter(|l| !l.imported)
            .map(|l| if l.title.is_empty() { l.name.clone() } else { l.title.clone() })
            .collect(),
        imported: manifest.has_imported(),
        files: plan.download.len(),
        bytes: plan.download_bytes,
        have_bytes: plan.needed_bytes.saturating_sub(plan.download_bytes),
        identity: Some(identity.clone()),
        new_server: trust.0,
        key_changed: trust.1,
        repair: false,
    }
}

fn run_job(job: &JobInput, shared: &JobShared) -> Result<Joined, String> {
    let server = job.server;
    // Where the endpoint is, if the server says.
    let info = probe(server);
    if let Some(info) = &info
        && info.content.is_none()
    {
        info!("content: {server} shares no content");
        return Ok(Joined::default());
    }
    let port = info.as_ref().and_then(|i| i.content.as_ref()).map_or(server.port(), |c| c.port);
    let base = format!("http://{}", SocketAddr::new(server.ip(), port));
    let agent = agent();
    let store = ContentStore::open(&job.cache).map_err(|err| format!("content cache {}: {err}", job.cache.display()))?;
    let mut index = HashIndex::load(Some(store.index_file()));
    let local_roots: Vec<PathBuf> = job.local.roots().into_iter().map(Path::to_path_buf).collect();
    let mut accepted = !job.ask;
    let mut attempt = 0;
    let (manifest, plan, identity, bases) = loop {
        attempt += 1;
        let Some(manifest) = get_manifest(&agent, &base, server, info.is_some(), shared)? else {
            return Ok(Joined::default());
        };
        manifest.check_compatible()?;
        // Who serves it: the key, and that it signed this manifest.
        let (identity, signed_manifest) = fetch_identity(&agent, &base, server)?;
        if signed_manifest != manifest.content_id() {
            if attempt < JOB_ATTEMPTS {
                warn!("content: {server}'s manifest changed while joining; fetching it again");
                continue;
            }
            return Err(format!("{server}'s content doesn't match what its key signed."));
        }
        let trust = trust_status(&identity, &job.trusted, job.confirm_new);
        info!(
            "content: {} ({}) shares {} ({} files, {} in {} layers), playing {:?}",
            manifest.server_name,
            identity.fingerprint,
            manifest.mode,
            manifest.files().count(),
            content::format_bytes(manifest.total_bytes()),
            manifest.layers.len(),
            manifest.playing
        );

        shared.set(Phase::Checking);
        let checked = Instant::now();
        let plan = download::plan(&manifest, &manifest.playing, &store, &local_roots, &mut index, &shared.progress)?;
        info!(
            "content: need {} files ({}); {} to download ({}), {} from our own files; checked in {:.1} s",
            plan.needed.len(),
            content::format_bytes(plan.needed_bytes),
            plan.download.len(),
            content::format_bytes(plan.download_bytes),
            plan.adopt.len(),
            checked.elapsed().as_secs_f32()
        );
        // The current level must be there after mounting.
        if let Some(current) = manifest.playing.first().filter(|l| *l != TEST_RANGE) {
            let level_file = format!("levels/{current}/level.ron");
            let shared_level = manifest.files().any(|(_, f)| f.path.eq_ignore_ascii_case(&level_file));
            let ours = !manifest.has_imported() && job.local.imported.join(&level_file).is_file();
            if !shared_level && !ours {
                return Err(format!(
                    "{} plays {current}, which you don't have: it isn't in your imported BF2 assets and the server doesn't share it.",
                    manifest.server_name
                ));
            }
        }
        if plan.download_bytes > MAX_DOWNLOAD_BYTES {
            return Err(format!(
                "{} wants {} downloaded, which is implausibly much.",
                manifest.server_name,
                content::format_bytes(plan.download_bytes)
            ));
        }
        let offer = offer_for(&manifest, &plan, &identity, trust);
        if !plan.download.is_empty() && (!accepted || trust.0) {
            shared.ask(&offer)?;
            accepted = true;
        }
        shared.set(Phase::Downloading(offer));
        let copied = download::adopt_all(&plan, &store, &shared.progress)?;
        if copied > 0 {
            info!("content: copied {} of our own files into the cache", content::format_bytes(copied));
        }
        let bases: Vec<String> = manifest.download_url.iter().cloned().chain([format!("{base}/content")]).collect();
        if !plan.download.is_empty() {
            let downloading = Instant::now();
            let fetch = HttpFetch(agent.clone());
            match download::download_all(&fetch, &bases, &plan.download, &store, &shared.progress, WORKERS, &validate) {
                Ok(()) => {}
                Err(err) if err == "cancelled" => return Err(err),
                // The server's files changed (it builds its manifest again): start over with
                // the new manifest; what arrived stays in the cache.
                Err(err) if attempt < JOB_ATTEMPTS => {
                    warn!("content: {err}; asking {server} for its manifest again");
                    continue;
                }
                Err(err) => return Err(format!("Downloading {}'s content failed: {err}", manifest.server_name)),
            }
            let seconds = downloading.elapsed().as_secs_f64();
            info!(
                "content: downloaded {} files ({}) in {seconds:.1} s ({}/s)",
                plan.download.len(),
                content::format_bytes(plan.download_bytes),
                content::format_bytes((plan.download_bytes as f64 / seconds.max(0.001)) as u64)
            );
        }
        // Verified on the way in: remembered, so the next join doesn't hash them again.
        for entry in plan.download.iter().chain(plan.adopt.iter().map(|(entry, _)| entry)) {
            let file = store.file(&entry.hash);
            if let Ok(meta) = std::fs::metadata(&file) {
                index.insert(&file, &meta, entry.hash.clone());
            }
        }
        index.save();
        break (manifest, plan, identity, bases);
    };

    shared.set(Phase::Mounting);
    let mounting = Instant::now();
    let key = server_key(&server.to_string());
    let mounted = store
        .build_view(&key, &manifest, &plan.needed, &manifest.playing)
        .map_err(|err| format!("preparing {}'s content: {err}", manifest.server_name))?;
    let keep: HashSet<String> = plan.needed.iter().map(|(_, f)| f.hash.clone()).collect();
    let (removed, freed) = store.cleanup(job.limit, &keep, mounted.layers.first().map(|l| l.dir.as_path()));
    if removed > 0 {
        info!("content: cache over its limit, removed {removed} old files ({})", content::format_bytes(freed));
    }
    info!("content: laid out {} files in {:.1} s", plan.needed.len(), mounting.elapsed().as_secs_f32());
    Ok(Joined {
        mounted: Some(mounted),
        manifest: Some(Arc::new(*manifest)),
        identity: Some(identity),
        base: Some(base),
        bases,
        accepted,
    })
}

/// Downloads the files the server named after comparing our report (see `crate::join`) and
/// puts them where the game loads them from. `more`: the server found more than it listed,
/// so the whole plan for the level is redone. Ends with reporting again, or with joining
/// again when the level was already loading (`rejoin`).
pub fn begin_repair(world: &mut World, files: Vec<FileEntry>, more: u32, level: String, identity: ServerIdentity, rejoin: bool) {
    let mode = downloads_mode(world);
    let Some(server) = world.resource::<ActiveMatch>().server() else {
        return;
    };
    let mount = world.resource::<ContentMount>();
    let input = RepairInput {
        files,
        more,
        level,
        manifest: mount.manifest.clone(),
        mounted: mount.mounted.clone(),
        base: mount.base.clone(),
        bases: mount.bases.clone(),
        cache: cache_dir(world),
        local: mount.own_paths(world.resource::<GamePaths>()),
        server,
        ask: mode == ContentDownloads::Ask && !mount.accepted,
        trust: trust_status(&identity, &world.resource::<Settings>().trusted_servers, confirm_new(world)),
        identity,
        limit: (world.resource::<Settings>().content_cache_gb.max(1) as u64) * 1_000_000_000,
    };
    let shared = Arc::new(JobShared::default());
    shared.set(Phase::Checking);
    let thread_shared = shared.clone();
    let spawned = std::thread::Builder::new().name("content repair".into()).spawn(move || {
        let result = run_repair(input, &thread_shared);
        let cancelled = thread_shared.progress.cancelled();
        thread_shared.set(match result {
            _ if cancelled => Phase::Failed("cancelled".into()),
            Ok(joined) => Phase::Done(Box::new(joined)),
            Err(err) => Phase::Failed(err),
        });
    });
    if let Err(err) = spawned {
        warn!("content: {err}");
        return;
    }
    world.insert_resource(ContentJob(Some(Job { server, shared, kind: JobKind::Repair { rejoin } })));
}

struct RepairInput {
    files: Vec<FileEntry>,
    more: u32,
    level: String,
    manifest: Option<Arc<Manifest>>,
    mounted: Option<MountedContent>,
    base: Option<String>,
    bases: Vec<String>,
    cache: Option<PathBuf>,
    local: GamePaths,
    server: SocketAddr,
    ask: bool,
    trust: (bool, bool),
    identity: ServerIdentity,
    limit: u64,
}

fn run_repair(input: RepairInput, shared: &JobShared) -> Result<Joined, String> {
    let server = input.server;
    let (Some(manifest), Some(base), Some(cache)) = (input.manifest.clone(), input.base.clone(), input.cache.clone()) else {
        return Err(format!(
            "{server} needs {} files of its content that differ from yours, but its content endpoint wasn't reachable before joining.",
            input.files.len() as u32 + input.more
        ));
    };
    let store = ContentStore::open(&cache).map_err(|err| format!("content cache {}: {err}", cache.display()))?;
    let mut index = HashIndex::load(Some(store.index_file()));
    let bytes: u64 = input.files.iter().map(|f| f.size).sum();
    let offer = Offer {
        server_name: manifest.server_name.clone(),
        mode: manifest.mode,
        mods: Vec::new(),
        imported: manifest.has_imported(),
        files: input.files.len() + input.more as usize,
        bytes,
        have_bytes: 0,
        identity: Some(input.identity.clone()),
        new_server: input.trust.0,
        key_changed: input.trust.1,
        repair: true,
    };
    let accepted = if input.ask || input.trust.0 {
        shared.ask(&offer)?;
        true
    } else {
        false
    };
    info!(
        "content: the server wants {} files ({}) fetched again, e.g. {}",
        offer.files,
        content::format_bytes(bytes),
        input.files.first().map_or("", |f| f.path.as_str())
    );
    shared.set(Phase::Downloading(offer));
    // The copies we have are wrong: out of the cache (links elsewhere keep their files).
    for file in &input.files {
        let _ = std::fs::remove_file(store.file(&file.hash));
    }
    let fetch = HttpFetch(agent());
    let mut joined = Joined { accepted, ..default() };
    // The view's layers must still be the manifest's for fixing single files.
    let same_layers = input.mounted.as_ref().is_some_and(|m| {
        m.layers.len() == manifest.layers.len() && m.layers.iter().zip(&manifest.layers).all(|(a, b)| a.name == b.name)
    });
    match (&input.mounted, input.more) {
        (Some(mounted), 0) if same_layers => {
            download::download_all(&fetch, &input.bases, &input.files, &store, &shared.progress, WORKERS, &validate)
                .map_err(|err| format!("Downloading {}'s content failed: {err}", manifest.server_name))?;
            for file in &input.files {
                let layer = manifest.layer_of(&file.path).ok_or_else(|| format!("{}: not in the manifest", file.path))?;
                let dir = &mounted.layers.get(layer).ok_or("the server's layers changed")?.dir;
                store.link_into(&file.hash, dir, &file.path).map_err(|err| format!("{}: {err}", file.path))?;
                let stored = store.file(&file.hash);
                if let Ok(meta) = std::fs::metadata(&stored) {
                    index.insert(&stored, &meta, file.hash.clone());
                }
            }
            index.save();
        }
        _ => {
            // Lay the level's files out again.
            let mut levels = manifest.playing.clone();
            if !levels.iter().any(|l| l.eq_ignore_ascii_case(&input.level)) {
                levels.insert(0, input.level.clone());
            }
            let local_roots: Vec<PathBuf> = input.local.roots().into_iter().map(Path::to_path_buf).collect();
            let plan = download::plan(&manifest, &levels, &store, &local_roots, &mut index, &shared.progress)?;
            download::adopt_all(&plan, &store, &shared.progress)?;
            download::download_all(&fetch, &input.bases, &plan.download, &store, &shared.progress, WORKERS, &validate)
                .map_err(|err| format!("Downloading {}'s content failed: {err}", manifest.server_name))?;
            shared.set(Phase::Mounting);
            let mounted = store
                .build_view(&server_key(&server.to_string()), &manifest, &plan.needed, &levels)
                .map_err(|err| format!("preparing {}'s content: {err}", manifest.server_name))?;
            let keep: HashSet<String> = plan.needed.iter().map(|(_, f)| f.hash.clone()).collect();
            store.cleanup(input.limit, &keep, mounted.layers.first().map(|l| l.dir.as_path()));
            joined.mounted = Some(mounted);
        }
    }
    joined.manifest = Some(manifest);
    joined.identity = Some(input.identity);
    joined.base = Some(base);
    joined.bases = input.bases;
    Ok(joined)
}

/// A short note on a listed server for the server browser: ranked (needs an account), and
/// what it shares.
pub fn browser_tag(info: &ServerInfo) -> Option<String> {
    let shares = info.content.as_ref().and_then(|content| {
        let what = match content.mode {
            ContentMode::Off => return None,
            ContentMode::Mods => "mods",
            ContentMode::All => "all content",
        };
        Some(if content.ready && content.bytes > 0 {
            format!("shares {what}, {}", content::format_bytes(content.bytes))
        } else {
            format!("shares {what}")
        })
    });
    match (info.ranked, shares) {
        (true, Some(shares)) => Some(format!("ranked, {shares}")),
        (true, None) => Some("ranked".into()),
        (false, shares) => shares,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn trust_needs_a_known_key() {
        let key = game_auth::Identity::generate();
        let identity = ServerIdentity {
            name: "Server".into(),
            address: "10.0.0.2:16567".into(),
            public_key: key.public_key(),
            fingerprint: key.fingerprint(),
        };
        assert_eq!(trust_status(&identity, &[], true), (true, false));
        assert_eq!(trust_status(&identity, &[], false), (false, false), "confirmation switched off");
        let trusted = vec![identity.trusted_entry()];
        assert_eq!(trust_status(&identity, &trusted, true), (false, false));
        // Same address, new key: asked again, and told the key changed.
        let other = game_auth::Identity::generate();
        let new_key = ServerIdentity { public_key: other.public_key(), fingerprint: other.fingerprint(), ..identity.clone() };
        assert_eq!(trust_status(&new_key, &trusted, true), (true, true));
    }
}
