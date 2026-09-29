//! Serving the server's content to joining clients (see `game_shared::content`): a small
//! HTTP endpoint on TCP at the game's port number, bound like the game socket (this machine
//! only unless the server is public).
//!
//! ```text
//! GET /content/manifest.ron              the manifest, with the levels the server plays now
//!                                        (503 while the files are still being hashed)
//! GET /content/identity?nonce=<64 hex>   the server's key and its signature of the nonce,
//!                                        the manifest id and the name (see `join`)
//! GET /content/<hash>                    a file of the manifest; `Range` resumes
//! ```
//!
//! What is shared is the admin's choice ([`ContentSettings::mode`]): nothing, the mods
//! (default: content made for this engine) or everything including the imported BF2 assets
//! (EA's copyrighted content: never the default). Only files listed in the manifest are
//! served, looked up by hash, never by a path from the request. A file that changed on disk
//! since it was hashed isn't served: the manifest is built again (only changed files are
//! hashed again), and clients fetch the new one.

use std::{
    collections::HashMap,
    fs::File,
    io::{Read, Seek, SeekFrom},
    net::{Ipv4Addr, TcpListener},
    path::{Path, PathBuf},
    sync::{
        Arc, Mutex, RwLock,
        atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering},
    },
    time::{Duration, SystemTime},
};

use bevy::prelude::*;
use game_shared::{
    config::GamePaths,
    content::{
        self, BuildOptions, BuildProgress, BuiltManifest, ContentAdvert, ContentMode, FILE_URL_PREFIX, FileEntry,
        IDENTITY_URL_PATH, IdentityAnswer, MANIFEST_URL_PATH, is_hash, verify,
    },
    join::CONTENT_PURPOSE,
};
use game_auth::Identity;
use tiny_http::{Header, Method, Request, Response, StatusCode};

use crate::{ServerSettings, rotation::MapRotation};

/// Requests handled at once; more wait in line.
const WORKERS: usize = 8;

/// What a server shares with joining clients, and how.
#[derive(Clone, Debug, Default)]
pub struct ContentSettings {
    /// `Off`, `Mods` (default) or `All` (also the imported BF2 assets).
    pub mode: ContentMode,
    /// TCP port of the endpoint (default: the game port).
    pub port: Option<u16>,
    /// Clients download files from here first (`<url>/<hash>`, a static host or CDN filled by
    /// `server export-content`); the server itself stays the fallback.
    pub download_url: Option<String>,
    /// Where file hashes and the levels' files are remembered between runs (default: the
    /// `server` folder in the user's config directory).
    pub cache_dir: Option<PathBuf>,
    /// The server's identity key (default: `identity.key` in the server's data folder, made on
    /// the first start). Players see its fingerprint before downloading from the server.
    pub identity_file: Option<PathBuf>,
    /// Seconds a joining player has to get the content right before being kicked (see
    /// `join`); 0: the default (10 minutes).
    pub sync_timeout: u32,
}

pub struct ContentPlugin;

impl Plugin for ContentPlugin {
    fn build(&self, app: &mut App) {
        app.add_systems(Update, sync_playing.run_if(resource_exists::<ContentServer>));
    }
}

/// The running endpoint.
#[derive(Resource)]
pub struct ContentServer {
    shared: Arc<Shared>,
    port: u16,
    mode: ContentMode,
}

/// What a level requires of joining clients (see `game_shared::content::verify`).
#[derive(Clone, Debug)]
pub enum Requirement {
    /// Nothing shared, so nothing checked.
    Nothing,
    /// The manifest is still being built.
    Preparing,
    Files(Arc<Required>),
}

/// The required files of a level and what a matching report looks like.
#[derive(Debug)]
pub struct Required {
    pub manifest_id: String,
    pub level: String,
    pub mode: ContentMode,
    /// Sorted by path.
    pub files: Vec<FileEntry>,
    pub digest: String,
    pub bytes: u64,
    /// TCP port of the endpoint.
    pub port: u16,
}

struct Shared {
    /// `None` while the manifest is being built.
    built: RwLock<Option<Arc<Served>>>,
    failed: RwLock<Option<String>>,
    progress: BuildProgress,
    /// What the manifest is built from.
    input: Mutex<(GamePaths, BuildOptions)>,
    building: AtomicBool,
    /// The current level first, then the rotation.
    playing: RwLock<Vec<String>>,
    /// What a client without anything needs for `playing`.
    advert_files: AtomicU32,
    advert_bytes: AtomicU64,
    stop: AtomicBool,
    /// Signs identity answers.
    identity: Option<Arc<Identity>>,
    server_name: String,
    /// The last level's requirement, and the manifest it was worked out from.
    required: Mutex<Option<(Arc<Served>, Arc<Required>)>>,
}

/// A built manifest, and the size and time of each served file when it was hashed.
struct Served {
    built: BuiltManifest,
    stamps: HashMap<String, (u64, Option<SystemTime>)>,
    /// [`content::Manifest::content_id`].
    id: String,
}

impl std::fmt::Debug for Served {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "Served({})", self.id)
    }
}

fn stamp(meta: &std::fs::Metadata) -> (u64, Option<SystemTime>) {
    (meta.len(), meta.modified().ok())
}

impl Shared {
    fn update_advert(&self) {
        let (files, bytes) = self
            .built
            .read()
            .unwrap()
            .as_ref()
            .map_or((0, 0), |s| s.built.manifest.needed_size(&self.playing.read().unwrap()));
        self.advert_files.store(files as u32, Ordering::Relaxed);
        self.advert_bytes.store(bytes, Ordering::Relaxed);
    }
}

/// Builds the manifest on a thread (unless that is already happening). Until it is done the
/// endpoint answers 503.
fn spawn_build(shared: &Arc<Shared>) {
    if shared.building.swap(true, Ordering::AcqRel) {
        return;
    }
    *shared.built.write().unwrap() = None;
    shared.progress.bytes_done.store(0, Ordering::Relaxed);
    let (paths, options) = shared.input.lock().unwrap().clone();
    let builder = shared.clone();
    let spawned = std::thread::Builder::new().name("content manifest".into()).spawn(move || {
        let started = std::time::Instant::now();
        match content::build_manifest(&paths, &options, &builder.progress) {
            Ok(built) => {
                let manifest = &built.manifest;
                info!(
                    "content: sharing {} files ({}) in {} layers, {} levels, ready after {:.1} s",
                    manifest.files().count(),
                    content::format_bytes(manifest.total_bytes()),
                    manifest.layers.len(),
                    manifest.levels.len(),
                    started.elapsed().as_secs_f32()
                );
                let stamps = built
                    .files
                    .iter()
                    .filter_map(|(hash, path)| Some((hash.clone(), stamp(&std::fs::metadata(path).ok()?))))
                    .collect();
                let id = built.manifest.content_id();
                *builder.built.write().unwrap() = Some(Arc::new(Served { built, stamps, id }));
                *builder.failed.write().unwrap() = None;
                builder.update_advert();
            }
            Err(err) => {
                error!("content: {err:#}");
                *builder.failed.write().unwrap() = Some(format!("{err:#}"));
            }
        }
        builder.building.store(false, Ordering::Release);
    });
    if let Err(err) = spawned {
        warn!("content: {err}");
        shared.building.store(false, Ordering::Release);
    }
}

impl ContentServer {
    /// For server browsers.
    pub fn advert(&self) -> ContentAdvert {
        ContentAdvert {
            mode: self.mode,
            port: self.port,
            files: self.shared.advert_files.load(Ordering::Relaxed),
            bytes: self.shared.advert_bytes.load(Ordering::Relaxed),
            ready: self.shared.built.read().unwrap().is_some(),
        }
    }

    /// What `level` requires of joining clients: the files the server shares for it.
    pub fn requirement(&self, level: &str) -> Requirement {
        let Some(served) = self.shared.built.read().unwrap().clone() else {
            // A manifest that can't be built shares nothing.
            return if self.shared.failed.read().unwrap().is_some() { Requirement::Nothing } else { Requirement::Preparing };
        };
        let mut cache = self.shared.required.lock().unwrap();
        if let Some((for_manifest, required)) = cache.as_ref()
            && Arc::ptr_eq(for_manifest, &served)
            && required.level == level
        {
            return Requirement::Files(required.clone());
        }
        let manifest = &served.built.manifest;
        let files: Vec<FileEntry> = manifest.required(level).into_iter().cloned().collect();
        let refs: Vec<&FileEntry> = files.iter().collect();
        let required = Arc::new(Required {
            manifest_id: served.id.clone(),
            level: level.to_string(),
            mode: manifest.mode,
            digest: verify::expected_digest(&refs),
            bytes: files.iter().map(|f| f.size).sum(),
            files,
            port: self.port,
        });
        *cache = Some((served.clone(), required.clone()));
        Requirement::Files(required)
    }
}

impl Drop for ContentServer {
    fn drop(&mut self) {
        self.shared.stop.store(true, Ordering::Relaxed);
    }
}

/// Where the hash index and the levels' files go.
fn cache_dir(settings: &ContentSettings) -> Option<PathBuf> {
    settings.cache_dir.clone().or_else(crate::server_config::data_dir)
}

/// The levels the server plays: the current one, then the rotation.
fn playing_levels(settings: &ServerSettings, rotation: Option<&MapRotation>) -> Vec<String> {
    let mut levels = vec![settings.level.clone()];
    for map in rotation.map(|r| r.maps.as_slice()).unwrap_or(&settings.rotation) {
        if !levels.contains(&map.level) {
            levels.push(map.level.clone());
        }
    }
    levels
}

/// Starts sharing content if the server is on the network and shares something. Called by
/// `start_server`; [`stop`] ends it.
pub fn start(world: &mut World) {
    let settings = world.resource::<ServerSettings>();
    let content = settings.content.clone();
    if !settings.network || content.mode == ContentMode::Off {
        return;
    }
    let port = content.port.unwrap_or(settings.port);
    let ip = if settings.public { Ipv4Addr::UNSPECIFIED } else { Ipv4Addr::LOCALHOST };
    // A listen server started again right away: the old endpoint may take a moment to let go.
    let mut listener = TcpListener::bind((ip, port));
    for _ in 0..5 {
        if listener.is_ok() {
            break;
        }
        std::thread::sleep(Duration::from_millis(200));
        listener = TcpListener::bind((ip, port));
    }
    let server = match listener.map_err(|err| err.to_string()).and_then(|l| {
        tiny_http::Server::from_listener(l, None).map_err(|err| err.to_string())
    }) {
        Ok(server) => Arc::new(server),
        Err(err) => {
            warn!("content: can't listen on TCP port {port} ({err}); clients need this server's content themselves");
            return;
        }
    };
    let download_url = content.download_url.clone().filter(|url| match content::validate_download_url(url) {
        Ok(()) => true,
        Err(err) => {
            warn!("content: {err}; ignoring it");
            false
        }
    });
    let dir = cache_dir(&content);
    let options = BuildOptions {
        mode: content.mode,
        server_name: settings.name.clone(),
        playing: Vec::new(),
        download_url,
        index_file: dir.as_ref().map(|d| d.join("content-index.txt")),
        deps_cache: dir.as_ref().map(|d| d.join("content-levels.txt")),
    };
    let shared = Arc::new(Shared {
        built: RwLock::new(None),
        failed: RwLock::new(None),
        progress: BuildProgress::default(),
        input: Mutex::new((world.resource::<GamePaths>().clone(), options)),
        building: AtomicBool::new(false),
        playing: RwLock::new(playing_levels(settings, world.get_resource::<MapRotation>())),
        advert_files: AtomicU32::new(0),
        advert_bytes: AtomicU64::new(0),
        stop: AtomicBool::new(false),
        identity: world.get_resource::<crate::join::ServerIdentity>().map(|i| i.0.clone()),
        server_name: settings.name.clone(),
        required: Mutex::new(None),
    });
    spawn_build(&shared);
    for i in 0..WORKERS {
        let (server, shared) = (server.clone(), shared.clone());
        let _ = std::thread::Builder::new().name(format!("content http {i}")).spawn(move || {
            while !shared.stop.load(Ordering::Relaxed) {
                match server.recv_timeout(Duration::from_millis(500)) {
                    Ok(Some(request)) => handle(request, &shared),
                    Ok(None) => {}
                    Err(_) => break,
                }
            }
        });
    }
    info!(
        "content: sharing {} on TCP port {port} ({})",
        match content.mode {
            ContentMode::All => "the mods and the imported BF2 assets",
            _ => "the mods",
        },
        if settings.public { "all interfaces" } else { "this machine only" }
    );
    world.insert_resource(ContentServer { shared, port, mode: content.mode });
}

/// Stops sharing (in-flight downloads finish).
pub fn stop(world: &mut World) {
    world.remove_resource::<ContentServer>();
}

fn sync_playing(settings: Res<ServerSettings>, rotation: Option<Res<MapRotation>>, server: Res<ContentServer>) {
    let rotation_changed = rotation.as_ref().is_some_and(|r| r.is_changed());
    if !settings.is_changed() && !rotation_changed {
        return;
    }
    let playing = playing_levels(&settings, rotation.as_deref());
    if *server.shared.playing.read().unwrap() != playing {
        *server.shared.playing.write().unwrap() = playing;
        server.shared.update_advert();
    }
}

fn header(name: &str, value: &str) -> Header {
    Header::from_bytes(name.as_bytes(), value.as_bytes()).expect("valid header")
}

fn text(status: u16, body: impl Into<String>) -> Response<std::io::Cursor<Vec<u8>>> {
    Response::from_string(body.into())
        .with_status_code(StatusCode(status))
        .with_header(header("Content-Type", "text/plain; charset=utf-8"))
}

fn handle(request: Request, shared: &Arc<Shared>) {
    if !matches!(request.method(), Method::Get | Method::Head) {
        let _ = request.respond(text(405, "only GET"));
        return;
    }
    let path = request.url().split(['?', '#']).next().unwrap_or_default().to_string();
    if path == MANIFEST_URL_PATH {
        serve_manifest(request, shared);
    } else if path == IDENTITY_URL_PATH {
        serve_identity(request, shared);
    } else if let Some(hash) = path.strip_prefix(FILE_URL_PREFIX).filter(|h| is_hash(h)) {
        let hash = hash.to_string();
        serve_file(request, shared, &hash);
    } else {
        let _ = request.respond(text(404, "not found"));
    }
}

fn serve_manifest(request: Request, shared: &Shared) {
    let built = shared.built.read().unwrap().clone();
    let Some(built) = built else {
        let response = match &*shared.failed.read().unwrap() {
            Some(err) => text(500, format!("the server can't share its content: {err}")),
            None => {
                let done = shared.progress.bytes_done.load(Ordering::Relaxed);
                let total = shared.progress.bytes_total.load(Ordering::Relaxed).max(1);
                text(503, format!("preparing {}%", (done * 100 / total).min(99))).with_header(header("Retry-After", "2"))
            }
        };
        let _ = request.respond(response);
        return;
    };
    let mut manifest = built.built.manifest.clone();
    manifest.playing = shared.playing.read().unwrap().clone();
    if let Some(from) = request.remote_addr() {
        info!("content: {from} fetched the manifest");
    }
    let _ = request.respond(text(200, manifest.to_ron()));
}

/// Proves the server's identity to a client about to download: signs the client's nonce and
/// the manifest id, which ties the manifest to the server's key.
fn serve_identity(request: Request, shared: &Shared) {
    let nonce = request
        .url()
        .split_once('?')
        .and_then(|(_, query)| query.split('&').find_map(|pair| pair.strip_prefix("nonce=")))
        .and_then(game_auth::unhex_array::<32>);
    let (Some(nonce), Some(identity)) = (nonce, shared.identity.as_ref()) else {
        let _ = request.respond(text(400, "identity?nonce=<64 hex digits>"));
        return;
    };
    let manifest_id = shared.built.read().unwrap().as_ref().map(|s| s.id.clone()).unwrap_or_default();
    let answer = IdentityAnswer {
        name: shared.server_name.clone(),
        proof: identity.prove(CONTENT_PURPOSE, &nonce, &manifest_id, &shared.server_name),
        manifest_id,
    };
    let _ = request.respond(text(200, ron::to_string(&answer).unwrap_or_default()));
}

/// `Range: bytes=a-b`, `bytes=a-` or `bytes=-n` for a file of `len` bytes: `None` without a
/// (single, well-formed) range, `Some(None)` if it can't be satisfied.
fn parse_range(value: &str, len: u64) -> Option<Option<(u64, u64)>> {
    let spec = value.trim().strip_prefix("bytes=")?;
    if spec.contains(',') {
        return None;
    }
    let (start, end) = spec.split_once('-')?;
    let (start, end) = (start.trim(), end.trim());
    let range = if start.is_empty() {
        let suffix: u64 = end.parse().ok()?;
        (suffix > 0 && len > 0).then(|| (len.saturating_sub(suffix), len - 1))
    } else {
        let start: u64 = start.parse().ok()?;
        let end: u64 = if end.is_empty() { len.saturating_sub(1) } else { end.parse::<u64>().ok()?.min(len.saturating_sub(1)) };
        (start < len && start <= end).then_some((start, end))
    };
    Some(range)
}

fn serve_file(request: Request, shared: &Arc<Shared>, hash: &str) {
    let served = shared.built.read().unwrap().clone();
    let Some((path, hashed)) = served.as_ref().and_then(|s| Some((s.built.files.get(hash)?.clone(), *s.stamps.get(hash)?)))
    else {
        let response = match served {
            Some(_) => text(404, "not shared"),
            None => text(503, "preparing").with_header(header("Retry-After", "2")),
        };
        let _ = request.respond(response);
        return;
    };
    let mut file = match File::open(&path) {
        Ok(file) => file,
        Err(err) => {
            warn!("content: {}: {err}", path.display());
            let _ = request.respond(text(410, "gone"));
            return;
        }
    };
    let meta = file.metadata().ok();
    if meta.as_ref().map(stamp) != Some(hashed) {
        warn!("content: {} changed since it was hashed; building the manifest again", path.display());
        spawn_build(shared);
        let _ = request.respond(text(503, "content changed, preparing").with_header(header("Retry-After", "2")));
        return;
    }
    let len = meta.map_or(0, |m| m.len());
    let range = request
        .headers()
        .iter()
        .find(|h| h.field.equiv("Range"))
        .and_then(|h| parse_range(h.value.as_str(), len));
    let mut headers = vec![
        header("Content-Type", "application/octet-stream"),
        header("Accept-Ranges", "bytes"),
        header("Cache-Control", "public, max-age=31536000, immutable"),
        header("ETag", &format!("\"{hash}\"")),
    ];
    let (status, start, count) = match range {
        None => (200, 0, len),
        Some(None) => {
            let response = text(416, "range not satisfiable").with_header(header("Content-Range", &format!("bytes */{len}")));
            let _ = request.respond(response);
            return;
        }
        Some(Some((start, end))) => {
            headers.push(header("Content-Range", &format!("bytes {start}-{end}/{len}")));
            (206, start, end - start + 1)
        }
    };
    if start > 0 && file.seek(SeekFrom::Start(start)).is_err() {
        let _ = request.respond(text(500, "seek failed"));
        return;
    }
    let response = Response::new(StatusCode(status), headers, file.take(count), Some(count as usize), None);
    let _ = request.respond(response);
}

/// `server export-content`: writes the files shared in `mode` to `out/<hash>` (hard links
/// where possible) and `out/manifest.ron`, the layout a `download_url` points at. Returns the
/// files and bytes written.
pub fn export(paths: &GamePaths, mode: ContentMode, out: &Path) -> anyhow::Result<(usize, u64)> {
    let options = BuildOptions {
        mode,
        server_name: "export".into(),
        index_file: crate::server_config::data_dir().map(|d| d.join("content-index.txt")),
        deps_cache: crate::server_config::data_dir().map(|d| d.join("content-levels.txt")),
        ..default()
    };
    let built = content::build_manifest(paths, &options, &BuildProgress::default())?;
    std::fs::create_dir_all(out)?;
    let (mut count, mut bytes) = (0, 0);
    for (hash, file) in &built.files {
        let target = out.join(hash);
        let size = std::fs::metadata(file)?.len();
        if std::fs::metadata(&target).is_ok_and(|m| m.len() == size) {
            continue;
        }
        if std::fs::hard_link(file, &target).is_err() {
            std::fs::copy(file, &target)?;
        }
        count += 1;
        bytes += size;
    }
    std::fs::write(out.join("manifest.ron"), built.manifest.to_ron())?;
    Ok((count, bytes))
}

#[cfg(test)]
mod tests {
    use std::io::{BufRead, BufReader, Write};

    use super::*;

    #[test]
    fn ranges() {
        assert_eq!(parse_range("bytes=0-", 10), Some(Some((0, 9))));
        assert_eq!(parse_range("bytes=4-6", 10), Some(Some((4, 6))));
        assert_eq!(parse_range("bytes=4-60", 10), Some(Some((4, 9))));
        assert_eq!(parse_range("bytes=-3", 10), Some(Some((7, 9))));
        assert_eq!(parse_range("bytes=10-", 10), Some(None));
        assert_eq!(parse_range("bytes=0-1,4-5", 10), None);
        assert_eq!(parse_range("items=0-", 10), None);
    }

    /// A raw HTTP GET: status and body.
    fn get(port: u16, path: &str, range: Option<&str>) -> (u16, Vec<u8>) {
        let mut stream = std::net::TcpStream::connect((Ipv4Addr::LOCALHOST, port)).unwrap();
        let range = range.map(|r| format!("Range: {r}\r\n")).unwrap_or_default();
        write!(stream, "GET {path} HTTP/1.1\r\nHost: localhost\r\n{range}Connection: close\r\n\r\n").unwrap();
        let mut reader = BufReader::new(stream);
        let mut status = String::new();
        reader.read_line(&mut status).unwrap();
        let mut length = 0;
        loop {
            let mut line = String::new();
            reader.read_line(&mut line).unwrap();
            if line.trim().is_empty() {
                break;
            }
            if let Some(value) = line.to_ascii_lowercase().strip_prefix("content-length:") {
                length = value.trim().parse().unwrap();
            }
        }
        let mut body = vec![0; length];
        reader.read_exact(&mut body).unwrap();
        (status.split_whitespace().nth(1).unwrap().parse().unwrap(), body)
    }

    #[test]
    fn serves_manifest_listed_files_only() {
        let dir = std::env::temp_dir().join(format!("bf2_content_server_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let mod_dir = dir.join("mods/sample");
        std::fs::create_dir_all(mod_dir.join("levels/x")).unwrap();
        std::fs::write(mod_dir.join("mod.ron"), "(name: \"Sample\")").unwrap();
        std::fs::write(mod_dir.join("levels/x/level.ron"), "(name: \"x\")").unwrap();
        std::fs::write(mod_dir.join("secret.txt"), "not shared").unwrap();
        let paths = GamePaths::resolve_with_mods(Some(dir.join("imported")), Some(dir.join("mods")));
        let port = 27890;
        let mut app = App::new();
        app.insert_resource(paths).insert_resource(ServerSettings {
            port,
            content: ContentSettings { cache_dir: Some(dir.join("cache")), ..default() },
            ..default()
        });
        start(app.world_mut());
        assert!(app.world().contains_resource::<ContentServer>());
        let mut manifest = None;
        for _ in 0..100 {
            let (status, body) = get(port, MANIFEST_URL_PATH, None);
            if status == 200 {
                manifest = Some(content::Manifest::parse(&body).unwrap());
                break;
            }
            assert_eq!(status, 503);
            std::thread::sleep(Duration::from_millis(50));
        }
        let manifest = manifest.expect("manifest ready");
        assert_eq!(manifest.levels, ["x"]);
        assert_eq!(manifest.playing, [game_shared::level::TEST_RANGE]);
        let level = manifest.files().find(|(_, f)| f.path == "levels/x/level.ron").unwrap().1.clone();
        assert!(manifest.files().all(|(_, f)| f.path != "secret.txt"));
        let (status, body) = get(port, &format!("{FILE_URL_PREFIX}{}", level.hash), None);
        assert_eq!((status, body.as_slice()), (200, b"(name: \"x\")".as_slice()));
        let (status, body) = get(port, &format!("{FILE_URL_PREFIX}{}", level.hash), Some("bytes=7-"));
        assert_eq!((status, body.as_slice()), (206, b"\"x\")".as_slice()));
        let (status, _) = get(port, &format!("{FILE_URL_PREFIX}{}", level.hash), Some("bytes=99-"));
        assert_eq!(status, 416);
        // Not by path, not unlisted hashes.
        assert_eq!(get(port, "/content/levels/x/level.ron", None).0, 404);
        assert_eq!(get(port, "/content/../../secret.txt", None).0, 404);
        assert_eq!(get(port, &format!("{FILE_URL_PREFIX}{}", content::hash_bytes(b"not shared")), None).0, 404);
        // A file changed on disk: not served; the manifest is built again with its new hash.
        std::fs::write(mod_dir.join("levels/x/level.ron"), "(name: \"x2\")").unwrap();
        assert_eq!(get(port, &format!("{FILE_URL_PREFIX}{}", level.hash), None).0, 503);
        let mut rebuilt = None;
        for _ in 0..100 {
            let (status, body) = get(port, MANIFEST_URL_PATH, None);
            if status == 200 {
                rebuilt = Some(content::Manifest::parse(&body).unwrap());
                break;
            }
            std::thread::sleep(Duration::from_millis(50));
        }
        let rebuilt = rebuilt.expect("manifest built again");
        let new = rebuilt.files().find(|(_, f)| f.path == "levels/x/level.ron").unwrap().1.clone();
        assert_eq!(new.hash, content::hash_bytes(b"(name: \"x2\")"));
        assert_eq!(get(port, &format!("{FILE_URL_PREFIX}{}", new.hash), None).1, b"(name: \"x2\")");
        let advert = app.world().resource::<ContentServer>().advert();
        assert!(advert.ready);
        assert_eq!(advert.mode, ContentMode::Mods);
        stop(app.world_mut());
        let _ = std::fs::remove_dir_all(&dir);
    }
}
