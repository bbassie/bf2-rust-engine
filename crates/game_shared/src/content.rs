//! Server content: a server can hand its data (mods, and optionally the imported BF2
//! assets) to joining clients, so players don't need a server's mods, and a client without
//! its own BF2 import can still join a server that shares everything.
//!
//! The server builds a [`Manifest`] of the files it shares: per layer (each mod, then the
//! imported folder), every file's path relative to the imported root, its size and its
//! BLAKE3 hash. Clients download what they don't have by hash over HTTP (see
//! `game_server::content` and `game_client::content`) and verify every file before use.
//!
//! Only data is shared: RON, glTF, textures, sounds and heightmaps (see
//! [`ALLOWED_EXTENSIONS`]). Paths are checked on both sides ([`validate_path`]): relative,
//! no `..`, no drive letters or alternate streams, no reserved Windows names.
//!
//! ```text
//! GET /content/manifest.ron              the manifest (RON)
//! GET /content/identity?nonce=<64 hex>   the server's key, proving it holds it (RON, [`IdentityAnswer`])
//! GET /content/<hash>                    a file by its hash; `Range: bytes=N-` resumes
//! ```
//!
//! The endpoint listens on TCP at the game's port number (UDP) unless the server says
//! otherwise in its browser answer ([`ContentAdvert`]). Clients cache files by hash
//! ([`store`]), work out what they lack ([`download::plan`]) and only fetch the files of the
//! levels the server plays ([`deps`]).
//!
//! The server stays in charge after that: when a client joins, and on every map change, it
//! compares the client's files for the level with its manifest ([`verify`], `crate::join`)
//! and keeps the player out of the match until they match.

pub mod deps;
pub mod download;
pub mod store;
pub mod verify;

use std::{
    collections::{HashMap, HashSet},
    fmt,
    io::{self, BufRead, Read, Write},
    path::{Path, PathBuf},
    str::FromStr,
    sync::{
        Mutex,
        atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering},
    },
    time::UNIX_EPOCH,
};

use serde::{Deserialize, Serialize};

use crate::config::GamePaths;

/// What a server shares with joining clients.
#[derive(Serialize, Deserialize, Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
pub enum ContentMode {
    /// Nothing: clients need the server's mods themselves.
    Off,
    /// The mods (content made for this engine).
    #[default]
    Mods,
    /// The mods and the imported BF2 assets. EA's copyrighted content: an explicit choice of
    /// the server's admin, never the default.
    All,
}

impl ContentMode {
    pub fn label(self) -> &'static str {
        match self {
            ContentMode::Off => "off",
            ContentMode::Mods => "mods",
            ContentMode::All => "all",
        }
    }
}

impl fmt::Display for ContentMode {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.label())
    }
}

impl FromStr for ContentMode {
    type Err = String;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s.trim().to_ascii_lowercase().as_str() {
            "off" | "none" => Ok(ContentMode::Off),
            "mods" => Ok(ContentMode::Mods),
            "all" => Ok(ContentMode::All),
            other => Err(format!("`{other}`: expected off, mods or all")),
        }
    }
}

/// What a server tells browsers about its content (`discovery::ServerInfo::content`).
#[derive(Serialize, Deserialize, Clone, Debug, Default, PartialEq, Eq)]
pub struct ContentAdvert {
    pub mode: ContentMode,
    /// TCP port of the content endpoint.
    pub port: u16,
    /// Files and bytes a client without anything downloads for the maps the server plays
    /// (its current map and rotation). 0 while the server is still hashing.
    pub files: u32,
    pub bytes: u64,
    /// The manifest is built; until then the endpoint answers 503.
    pub ready: bool,
}

/// Bump when the manifest format changes incompatibly.
pub const MANIFEST_VERSION: u32 = 1;
/// URL path of the manifest on a server's content endpoint.
pub const MANIFEST_URL_PATH: &str = "/content/manifest.ron";
/// URL path prefix of files by hash: `/content/<hash>`.
pub const FILE_URL_PREFIX: &str = "/content/";
/// URL path of the server's identity: `/content/identity?nonce=<64 hex digits>`.
pub const IDENTITY_URL_PATH: &str = "/content/identity";

/// The content endpoint's answer to an identity request: who serves this content. The proof
/// signs the client's nonce, the manifest's [`Manifest::content_id`] and the server name
/// (purpose [`crate::join::CONTENT_PURPOSE`]), which ties the manifest to the server's key.
#[derive(Serialize, Deserialize, Clone, Debug, Default, PartialEq)]
pub struct IdentityAnswer {
    pub name: String,
    /// Empty while the manifest is being built.
    pub manifest_id: String,
    pub proof: game_auth::IdentityProof,
}

/// Refuse manifests bigger than this (a full BF2 import is about 2 MB).
pub const MAX_MANIFEST_BYTES: u64 = 32 << 20;
/// Refuse manifests with more files than this (a full BF2 import has about 17 000).
pub const MAX_FILES: usize = 250_000;
/// Refuse single files bigger than this (the biggest imported file is about 60 MB).
pub const MAX_FILE_BYTES: u64 = 2 << 30;
/// Refuse manifests sharing more than this in total (a full BF2 import is about 4 GB).
pub const MAX_TOTAL_BYTES: u64 = 64 << 30;
/// Longest path in a manifest.
pub const MAX_PATH_BYTES: usize = 240;

/// File types the game reads, and so the only ones shared: RON descriptions, glTF meshes
/// (with external buffers), textures, sounds and terrain maps. Nothing executable.
pub const ALLOWED_EXTENSIONS: &[&str] = &[
    "ron", "glb", "gltf", "bin", "dds", "png", "jpg", "jpeg", "ktx2", "wav", "ogg", "r16", "r8",
];

/// Files the game loads by a fixed path rather than through a reference in the data. Always
/// part of the common files (see [`deps`]).
pub const ALWAYS_NEEDED: &[&str] = &[
    // `render::soldiers::DEFAULT_WEAPON_ANIMATIONS` in the client.
    "objects/weapons/handheld/rurif_ak47/animations/3p.glb",
];

/// The files a server shares.
#[derive(Serialize, Deserialize, Clone, Debug, Default, PartialEq)]
pub struct Manifest {
    /// [`MANIFEST_VERSION`].
    pub version: u32,
    /// The server's [`crate::PROTOCOL_ID`]: a client with another one can't play there,
    /// so it doesn't download anything either.
    pub protocol: u64,
    /// The server's game version (`CARGO_PKG_VERSION`), for messages.
    #[serde(default)]
    pub game_version: String,
    pub mode: ContentMode,
    pub server_name: String,
    /// Levels the server plays: the current one first, then its map rotation. Clients get
    /// the files of all of them, so map changes don't need downloads.
    pub playing: Vec<String>,
    /// Level names, indexed by [`FileEntry::levels`].
    pub levels: Vec<String>,
    /// Where clients download files from instead of the server (`<url>/<hash>`), like
    /// Source's `sv_downloadurl`. The server itself stays the fallback.
    #[serde(default)]
    pub download_url: Option<String>,
    /// Highest priority first: the mods, then (mode `All`) the imported assets.
    pub layers: Vec<Layer>,
}

/// A folder of shared files: a mod, or the imported assets.
#[derive(Serialize, Deserialize, Clone, Debug, Default, PartialEq)]
pub struct Layer {
    /// Folder name (a mod's folder, or `imported`).
    pub name: String,
    /// The mod's name and description, for players.
    #[serde(default)]
    pub title: String,
    #[serde(default)]
    pub description: String,
    /// The imported BF2 assets rather than a mod.
    #[serde(default)]
    pub imported: bool,
    pub files: Vec<FileEntry>,
}

/// A shared file.
#[derive(Serialize, Deserialize, Clone, Debug, Default, PartialEq, Eq)]
pub struct FileEntry {
    /// Relative to the imported root, `/`-separated (see [`validate_path`]).
    pub path: String,
    pub size: u64,
    /// BLAKE3, 64 lowercase hex digits.
    pub hash: String,
    /// Indices into [`Manifest::levels`] of the levels that need this file; empty: every
    /// level needs it.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub levels: Vec<u32>,
}

impl Manifest {
    /// Parses and checks a manifest received from a server.
    pub fn parse(bytes: &[u8]) -> Result<Self, String> {
        if bytes.len() as u64 > MAX_MANIFEST_BYTES {
            return Err(format!("manifest too big ({} bytes)", bytes.len()));
        }
        let text = std::str::from_utf8(bytes).map_err(|_| "manifest is not UTF-8".to_string())?;
        let manifest: Manifest = ron::from_str(text).map_err(|err| format!("bad manifest: {err}"))?;
        manifest.validate()?;
        Ok(manifest)
    }

    pub fn to_ron(&self) -> String {
        ron::to_string(self).unwrap_or_default()
    }

    /// Whether this game can play on the server (same protocol).
    pub fn check_compatible(&self) -> Result<(), String> {
        if self.protocol != crate::PROTOCOL_ID {
            return Err(format!(
                "The server runs another version of the game ({}), so its content isn't downloaded.",
                if self.game_version.is_empty() { "unknown" } else { &self.game_version }
            ));
        }
        Ok(())
    }

    /// Checks everything a client relies on: the format version, sizes, paths, hashes,
    /// level references and the download URL.
    pub fn validate(&self) -> Result<(), String> {
        if self.version != MANIFEST_VERSION {
            return Err(format!(
                "manifest version {} (this game reads {MANIFEST_VERSION}): the server runs another version",
                self.version
            ));
        }
        if self.layers.len() > 256 || self.levels.len() > 4096 || self.playing.len() > 4096 {
            return Err("manifest has implausibly many layers or levels".into());
        }
        for level in self.levels.iter().chain(&self.playing) {
            validate_component(level).map_err(|err| format!("level `{level}`: {err}"))?;
        }
        if let Some(url) = &self.download_url {
            validate_download_url(url)?;
        }
        let mut names = HashSet::new();
        let (mut count, mut total) = (0usize, 0u64);
        for layer in &self.layers {
            validate_component(&layer.name).map_err(|err| format!("layer `{}`: {err}", layer.name))?;
            if !names.insert(layer.name.to_ascii_lowercase()) {
                return Err(format!("layer `{}` twice", layer.name));
            }
            let mut paths = HashSet::new();
            for file in &layer.files {
                validate_path(&file.path).map_err(|err| format!("`{}`: {err}", file.path))?;
                if !paths.insert(file.path.to_ascii_lowercase()) {
                    return Err(format!("`{}` twice in layer `{}`", file.path, layer.name));
                }
                if !is_hash(&file.hash) {
                    return Err(format!("`{}`: bad hash", file.path));
                }
                if file.size > MAX_FILE_BYTES {
                    return Err(format!("`{}` is implausibly big ({} bytes)", file.path, file.size));
                }
                if file.levels.iter().any(|&l| l as usize >= self.levels.len()) {
                    return Err(format!("`{}`: unknown level", file.path));
                }
                count += 1;
                total += file.size;
            }
        }
        if count > MAX_FILES {
            return Err(format!("manifest lists implausibly many files ({count})"));
        }
        if total > MAX_TOTAL_BYTES {
            return Err(format!("manifest shares an implausible amount of data ({})", format_bytes(total)));
        }
        Ok(())
    }

    /// Every shared file: `(layer index, file)`.
    pub fn files(&self) -> impl Iterator<Item = (usize, &FileEntry)> {
        self.layers
            .iter()
            .enumerate()
            .flat_map(|(i, layer)| layer.files.iter().map(move |f| (i, f)))
    }

    /// The files needed to play `levels`: the common ones and those of these levels.
    pub fn needed<'a>(&'a self, levels: &[String]) -> impl Iterator<Item = (usize, &'a FileEntry)> + 'a {
        let wanted: HashSet<u32> = self
            .levels
            .iter()
            .enumerate()
            .filter(|(_, name)| levels.iter().any(|l| l.eq_ignore_ascii_case(name)))
            .map(|(i, _)| i as u32)
            .collect();
        self.files()
            .filter(move |(_, f)| f.levels.is_empty() || f.levels.iter().any(|l| wanted.contains(l)))
    }

    /// Bytes of all shared files.
    pub fn total_bytes(&self) -> u64 {
        self.files().map(|(_, f)| f.size).sum()
    }

    /// Files and bytes needed for `levels`, one per hash.
    pub fn needed_size(&self, levels: &[String]) -> (usize, u64) {
        let mut seen = HashSet::new();
        self.needed(levels)
            .filter(|(_, f)| seen.insert(f.hash.as_str()))
            .fold((0, 0), |(n, bytes), (_, f)| (n + 1, bytes + f.size))
    }

    /// Serves the imported layer.
    pub fn has_imported(&self) -> bool {
        self.layers.iter().any(|l| l.imported)
    }
}

/// Why a path isn't acceptable.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PathError(pub &'static str);

impl fmt::Display for PathError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.0)
    }
}

/// Checks a shared file's path: relative to the imported root, `/`-separated, without `.`
/// or `..`, without anything Windows would read as a drive, a device or a stream, and with
/// an extension from [`ALLOWED_EXTENSIONS`].
pub fn validate_path(path: &str) -> Result<(), PathError> {
    if path.is_empty() {
        return Err(PathError("empty path"));
    }
    if path.len() > MAX_PATH_BYTES {
        return Err(PathError("path too long"));
    }
    if path.starts_with('/') {
        return Err(PathError("absolute path"));
    }
    if path.contains('\\') {
        return Err(PathError("backslash in path"));
    }
    for component in path.split('/') {
        validate_component(component)?;
    }
    let extension = extension(path).ok_or(PathError("no file extension"))?;
    if !ALLOWED_EXTENSIONS.contains(&extension.as_str()) {
        return Err(PathError("file type not shared"));
    }
    Ok(())
}

/// Checks one part of a path (or a mod folder or level name): no separators, no `.` or `..`,
/// no characters Windows forbids or treats specially, no device names.
pub fn validate_component(component: &str) -> Result<(), PathError> {
    if component.is_empty() {
        return Err(PathError("empty path component"));
    }
    if component.len() > 128 {
        return Err(PathError("path component too long"));
    }
    if component == "." || component == ".." {
        return Err(PathError("`.` or `..` in path"));
    }
    if component.chars().any(|c| c.is_control() || matches!(c, '/' | '\\' | ':' | '*' | '?' | '"' | '<' | '>' | '|')) {
        return Err(PathError("forbidden character in path"));
    }
    if component.ends_with('.') || component.ends_with(' ') || component.starts_with(' ') {
        return Err(PathError("path component ends with a dot or space"));
    }
    let stem = component.split('.').next().unwrap_or_default().to_ascii_lowercase();
    let reserved = matches!(stem.as_str(), "con" | "prn" | "aux" | "nul" | "conin$" | "conout$")
        || ((stem.starts_with("com") || stem.starts_with("lpt"))
            && stem.len() == 4
            && stem.as_bytes()[3].is_ascii_digit());
    if reserved {
        return Err(PathError("reserved device name in path"));
    }
    Ok(())
}

/// Lowercase extension of a path, if it has one.
fn extension(path: &str) -> Option<String> {
    let name = path.rsplit('/').next()?;
    let (stem, extension) = name.rsplit_once('.')?;
    (!stem.is_empty()).then(|| extension.to_ascii_lowercase())
}

/// 64 lowercase hex digits.
pub fn is_hash(s: &str) -> bool {
    s.len() == 64 && s.bytes().all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
}

/// A download URL must be plain `http(s)://host[:port]/path` without credentials, query or
/// fragment: clients append `/<hash>` to it and nothing else.
pub fn validate_download_url(url: &str) -> Result<(), String> {
    let rest = url
        .strip_prefix("http://")
        .or_else(|| url.strip_prefix("https://"))
        .ok_or_else(|| format!("download URL `{url}` must start with http:// or https://"))?;
    if url.len() > 512 || rest.is_empty() || rest.starts_with('/') {
        return Err(format!("download URL `{url}` has no host"));
    }
    if rest.chars().any(|c| c.is_whitespace() || c.is_control() || matches!(c, '?' | '#' | '@' | '\\')) {
        return Err(format!("download URL `{url}` may not have a query, fragment or credentials"));
    }
    Ok(())
}

/// `12.3 MB`.
pub fn format_bytes(bytes: u64) -> String {
    let b = bytes as f64;
    if b >= 1e9 {
        format!("{:.2} GB", b / 1e9)
    } else if b >= 1e6 {
        format!("{:.1} MB", b / 1e6)
    } else if b >= 1e3 {
        format!("{:.0} KB", b / 1e3)
    } else {
        format!("{bytes} B")
    }
}

/// Streaming BLAKE3.
#[derive(Default, Clone)]
pub struct ContentHasher(blake3::Hasher);

impl ContentHasher {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn update(&mut self, bytes: &[u8]) {
        self.0.update(bytes);
    }

    /// 64 lowercase hex digits.
    pub fn finish(&self) -> String {
        self.0.finalize().to_hex().to_string()
    }
}

/// Hash of some bytes.
pub fn hash_bytes(bytes: &[u8]) -> String {
    blake3::hash(bytes).to_hex().to_string()
}

/// Size and hash of a file. Adds the bytes read to `progress`, and stops early (with an
/// error) when `cancel` is set.
pub fn hash_file(path: &Path, progress: Option<&AtomicU64>, cancel: Option<&AtomicBool>) -> io::Result<(u64, String)> {
    let mut file = std::fs::File::open(path)?;
    let mut hasher = ContentHasher::new();
    let mut buffer = vec![0u8; 256 * 1024];
    let mut size = 0u64;
    loop {
        if cancel.is_some_and(|c| c.load(Ordering::Relaxed)) {
            return Err(io::Error::new(io::ErrorKind::Interrupted, "cancelled"));
        }
        let read = file.read(&mut buffer)?;
        if read == 0 {
            break;
        }
        hasher.update(&buffer[..read]);
        size += read as u64;
        if let Some(progress) = progress {
            progress.fetch_add(read as u64, Ordering::Relaxed);
        }
    }
    Ok((size, hasher.finish()))
}

/// Modification time in nanoseconds since 1970, for the [`HashIndex`].
fn modified_nanos(meta: &std::fs::Metadata) -> u128 {
    meta.modified()
        .ok()
        .and_then(|t| t.duration_since(UNIX_EPOCH).ok())
        .map_or(0, |d| d.as_nanos())
}

/// Remembers file hashes by path, size and modification time, so unchanged files aren't
/// hashed again. Saved as text lines: `hash size mtime path`.
#[derive(Default)]
pub struct HashIndex {
    file: Option<PathBuf>,
    entries: HashMap<PathBuf, (u64, u128, String)>,
    dirty: bool,
}

impl HashIndex {
    /// Loads the index saved in `file` (a missing or damaged file gives an empty index).
    pub fn load(file: Option<PathBuf>) -> Self {
        let mut entries = HashMap::new();
        if let Some(reader) = file.as_ref().and_then(|f| std::fs::File::open(f).ok()) {
            for line in io::BufReader::new(reader).lines().map_while(Result::ok) {
                let mut parts = line.splitn(4, '\t');
                let (Some(hash), Some(size), Some(modified), Some(path)) = (parts.next(), parts.next(), parts.next(), parts.next())
                else {
                    continue;
                };
                let (Ok(size), Ok(modified)) = (size.parse(), modified.parse()) else {
                    continue;
                };
                if is_hash(hash) {
                    entries.insert(PathBuf::from(path), (size, modified, hash.to_string()));
                }
            }
        }
        Self { file, entries, dirty: false }
    }

    /// Writes the index back if it changed, dropping files that no longer exist.
    pub fn save(&mut self) {
        let Some(file) = &self.file else {
            return;
        };
        if !self.dirty {
            return;
        }
        self.entries.retain(|path, _| path.exists());
        if let Some(dir) = file.parent() {
            let _ = std::fs::create_dir_all(dir);
        }
        // Per process: two servers may share the file.
        let temp = file.with_extension(format!("tmp{}", std::process::id()));
        let result = (|| -> io::Result<()> {
            let mut out = io::BufWriter::new(std::fs::File::create(&temp)?);
            for (path, (size, modified, hash)) in &self.entries {
                if let Some(path) = path.to_str().filter(|p| !p.contains(['\t', '\n'])) {
                    writeln!(out, "{hash}\t{size}\t{modified}\t{path}")?;
                }
            }
            out.flush()?;
            drop(out);
            std::fs::rename(&temp, file)
        })();
        match result {
            Ok(()) => self.dirty = false,
            Err(err) => bevy::log::warn!("saving {}: {err}", file.display()),
        }
    }

    /// The hash of `path`, if it is indexed and unchanged.
    pub fn lookup(&self, path: &Path, meta: &std::fs::Metadata) -> Option<&str> {
        let (size, modified, hash) = self.entries.get(path)?;
        (*size == meta.len() && *modified == modified_nanos(meta)).then_some(hash.as_str())
    }

    pub fn insert(&mut self, path: &Path, meta: &std::fs::Metadata, hash: String) {
        self.entries.insert(path.to_path_buf(), (meta.len(), modified_nanos(meta), hash));
        self.dirty = true;
    }

    /// Hashes of `files` (`None` for those that can't be read), hashing the ones that
    /// aren't indexed or changed on all cores. `progress` counts the bytes hashed.
    pub fn hash_all(
        &mut self,
        files: &[PathBuf],
        progress: &AtomicU64,
        cancel: Option<&AtomicBool>,
    ) -> Vec<Option<(u64, String)>> {
        let mut results: Vec<Option<(u64, String)>> = vec![None; files.len()];
        let mut todo = Vec::new();
        for (i, path) in files.iter().enumerate() {
            let Ok(meta) = std::fs::metadata(path) else {
                continue;
            };
            match self.lookup(path, &meta) {
                Some(hash) => results[i] = Some((meta.len(), hash.to_string())),
                None => todo.push((i, meta)),
            }
        }
        if todo.is_empty() {
            return results;
        }
        let next = AtomicUsize::new(0);
        let done = Mutex::new(Vec::new());
        let threads = std::thread::available_parallelism().map_or(4, |n| n.get()).clamp(1, 16);
        std::thread::scope(|scope| {
            for _ in 0..threads.min(todo.len()) {
                scope.spawn(|| {
                    loop {
                        let job = next.fetch_add(1, Ordering::Relaxed);
                        let Some((i, _)) = todo.get(job) else {
                            break;
                        };
                        if let Ok(hashed) = hash_file(&files[*i], Some(progress), cancel) {
                            done.lock().unwrap().push((*i, hashed));
                        }
                    }
                });
            }
        });
        let metas: HashMap<usize, std::fs::Metadata> = todo.into_iter().collect();
        for (i, (size, hash)) in done.into_inner().unwrap() {
            if let Some(meta) = metas.get(&i).filter(|m| m.len() == size) {
                self.insert(&files[i], meta, hash.clone());
            }
            results[i] = Some((size, hash));
        }
        results
    }
}

/// A file found in a layer folder.
#[derive(Clone, Debug)]
pub struct LocalFile {
    /// Relative to the layer root, `/`-separated.
    pub path: String,
    pub file: PathBuf,
    pub size: u64,
}

/// Whether a file (path relative to a layer root) is shared: a valid data path, and not a
/// server-side cache (bots' navigation grids).
pub fn is_shared(path: &str) -> bool {
    if validate_path(path).is_err() {
        return false;
    }
    let name = path.rsplit('/').next().unwrap_or(path);
    !(name.starts_with('.') || (name.starts_with("navgrid") && name.ends_with(".bin")))
}

/// Every shared file under `root`, sorted by path. Symbolic links are skipped, so a layer
/// can't reach outside its folder.
pub fn walk_layer(root: &Path) -> Vec<LocalFile> {
    let mut files = Vec::new();
    let mut stack = vec![(root.to_path_buf(), String::new())];
    while let Some((dir, prefix)) = stack.pop() {
        let Ok(entries) = std::fs::read_dir(&dir) else {
            continue;
        };
        for entry in entries.flatten() {
            let Ok(kind) = entry.file_type() else {
                continue;
            };
            let Some(name) = entry.file_name().to_str().map(str::to_string) else {
                continue;
            };
            let path = if prefix.is_empty() { name } else { format!("{prefix}/{name}") };
            if kind.is_dir() {
                let name = path.rsplit('/').next().unwrap_or_default();
                if !name.starts_with('.') && validate_component(name).is_ok() {
                    stack.push((entry.path(), path));
                }
            } else if kind.is_file() && is_shared(&path) {
                let size = entry.metadata().map_or(0, |m| m.len());
                files.push(LocalFile { path, file: entry.path(), size });
            }
        }
    }
    files.sort_by(|a, b| a.path.cmp(&b.path));
    files
}

/// How to build a server's manifest.
#[derive(Clone, Debug, Default)]
pub struct BuildOptions {
    pub mode: ContentMode,
    pub server_name: String,
    pub playing: Vec<String>,
    pub download_url: Option<String>,
    /// Where hashes are remembered between runs.
    pub index_file: Option<PathBuf>,
    /// Where the levels' files are remembered between runs (see [`deps`]).
    pub deps_cache: Option<PathBuf>,
}

/// How far building a manifest got.
#[derive(Default)]
pub struct BuildProgress {
    pub bytes_done: AtomicU64,
    pub bytes_total: AtomicU64,
}

/// A manifest and where its files are on this machine.
pub struct BuiltManifest {
    pub manifest: Manifest,
    /// Hash -> file.
    pub files: HashMap<String, PathBuf>,
}

/// Lists and hashes the files `paths` shares in `options.mode`: every enabled mod, and with
/// [`ContentMode::All`] the imported folder. Files are grouped by level (see [`deps`]).
pub fn build_manifest(paths: &GamePaths, options: &BuildOptions, progress: &BuildProgress) -> anyhow::Result<BuiltManifest> {
    let mut layers: Vec<(Layer, Vec<LocalFile>)> = Vec::new();
    if options.mode != ContentMode::Off {
        for m in &paths.mods {
            let name = m.dir.file_name().and_then(|n| n.to_str()).unwrap_or_default().to_string();
            if let Err(err) = validate_component(&name) {
                bevy::log::warn!("not sharing mod folder `{}`: {err}", m.dir.display());
                continue;
            }
            let layer = Layer {
                name,
                title: m.info.name.clone(),
                description: m.info.description.clone(),
                imported: false,
                files: Vec::new(),
            };
            layers.push((layer, walk_layer(&m.dir)));
        }
    }
    if options.mode == ContentMode::All {
        let layer = Layer {
            name: "imported".into(),
            title: "Imported BF2 assets".into(),
            imported: true,
            ..Default::default()
        };
        layers.push((layer, walk_layer(&paths.imported)));
    }
    let all: Vec<PathBuf> = layers.iter().flat_map(|(_, files)| files.iter().map(|f| f.file.clone())).collect();
    progress
        .bytes_total
        .store(layers.iter().flat_map(|(_, f)| f).map(|f| f.size).sum(), Ordering::Relaxed);
    let mut index = HashIndex::load(options.index_file.clone());
    let hashes = index.hash_all(&all, &progress.bytes_done, None);
    index.save();
    let mut hashes = hashes.into_iter();
    let mut files = HashMap::new();
    let mut local = Vec::new();
    let mut manifest = Manifest {
        version: MANIFEST_VERSION,
        protocol: crate::PROTOCOL_ID,
        game_version: env!("CARGO_PKG_VERSION").into(),
        mode: options.mode,
        server_name: options.server_name.clone(),
        playing: options.playing.clone(),
        levels: Vec::new(),
        download_url: options.download_url.clone(),
        layers: Vec::new(),
    };
    for (mut layer, found) in layers {
        let mut paths_in_layer = Vec::new();
        for file in found {
            let Some(Some((size, hash))) = hashes.next() else {
                continue;
            };
            if size > MAX_FILE_BYTES {
                bevy::log::warn!("not sharing {}: too big", file.file.display());
                continue;
            }
            files.entry(hash.clone()).or_insert_with(|| file.file.clone());
            paths_in_layer.push(file.file.clone());
            layer.files.push(FileEntry {
                path: file.path,
                size,
                hash,
                levels: Vec::new(),
            });
        }
        local.push(paths_in_layer);
        manifest.layers.push(layer);
    }
    deps::assign_levels(&mut manifest, &local, options.deps_cache.as_deref());
    manifest.validate().map_err(|err| anyhow::anyhow!(err))?;
    Ok(BuiltManifest { manifest, files })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn paths() {
        for ok in [
            "levels/sample_valley/level.ron",
            "objects/sample/meshes/crate.glb",
            "a.dds",
            "weapons/usrif_m4.patch.ron",
            "common/sound/x y.wav",
        ] {
            assert_eq!(validate_path(ok), Ok(()), "{ok}");
        }
        for bad in [
            "",
            "/etc/passwd.ron",
            "../secret.ron",
            "levels/../../x.ron",
            "levels/./x.ron",
            "levels//x.ron",
            "C:/Windows/x.dds",
            "c:x.ron",
            "levels\\x.ron",
            "file.exe",
            "script.ron.bat",
            "dir/noextension",
            "con.ron",
            "dir/COM1.dds",
            "dir/aux",
            "x.ron:stream",
            "trailing./x.ron",
            "x.ron ",
            ".ron",
            "a\u{0}b.ron",
        ] {
            assert!(validate_path(bad).is_err(), "{bad:?} accepted");
        }
        assert!(is_shared("levels/x/level.ron"));
        assert!(!is_shared("levels/x/navgrid_gpm_cq_16.bin"));
        assert!(!is_shared("README.txt"));
    }

    #[test]
    fn download_urls() {
        assert!(validate_download_url("https://cdn.example.com/bf2/content").is_ok());
        assert!(validate_download_url("http://10.0.0.2:8080").is_ok());
        for bad in ["ftp://x", "https://", "https:///x", "http://x/a?b=", "http://u:p@x/", "http://x/#a", "file:///c:/x"] {
            assert!(validate_download_url(bad).is_err(), "{bad}");
        }
    }

    fn entry(path: &str, levels: Vec<u32>) -> FileEntry {
        FileEntry {
            path: path.into(),
            size: 3,
            hash: hash_bytes(path.as_bytes()),
            levels,
        }
    }

    fn manifest() -> Manifest {
        Manifest {
            version: MANIFEST_VERSION,
            protocol: crate::PROTOCOL_ID,
            game_version: String::new(),
            mode: ContentMode::Mods,
            server_name: "test".into(),
            playing: vec!["a".into()],
            levels: vec!["a".into(), "b".into()],
            download_url: None,
            layers: vec![Layer {
                name: "sample".into(),
                files: vec![
                    entry("templates/x.ron", vec![]),
                    entry("levels/a/level.ron", vec![0]),
                    entry("levels/b/level.ron", vec![1]),
                    entry("objects/shared.glb", vec![0, 1]),
                ],
                ..Default::default()
            }],
        }
    }

    #[test]
    fn manifest_round_trip_and_needed() {
        let manifest = manifest();
        let parsed = Manifest::parse(manifest.to_ron().as_bytes()).unwrap();
        assert_eq!(parsed, manifest);
        let needed: Vec<&str> = parsed.needed(&["a".into()]).map(|(_, f)| f.path.as_str()).collect();
        assert_eq!(needed, ["templates/x.ron", "levels/a/level.ron", "objects/shared.glb"]);
        assert_eq!(parsed.needed(&[]).count(), 1);
        assert_eq!(parsed.total_bytes(), 12);
    }

    #[test]
    fn manifest_rejects_bad_input() {
        let check = |change: &dyn Fn(&mut Manifest)| {
            let mut m = manifest();
            change(&mut m);
            Manifest::parse(m.to_ron().as_bytes()).is_err()
        };
        assert!(check(&|m| m.version = 99));
        assert!(check(&|m| m.layers[0].files[0].path = "../../evil.ron".into()));
        assert!(check(&|m| m.layers[0].files[0].path = "evil.exe".into()));
        assert!(check(&|m| m.layers[0].files[0].hash = "abc".into()));
        assert!(check(&|m| m.layers[0].files[0].size = MAX_FILE_BYTES + 1));
        assert!(check(&|m| m.layers[0].files[0].levels = vec![7]));
        assert!(check(&|m| m.layers[0].name = "..".into()));
        assert!(check(&|m| m.layers[0].files.push(entry("Templates/X.ron", vec![]))));
        assert!(check(&|m| m.download_url = Some("http://x/?a".into())));
        assert!(check(&|m| m.playing = vec!["../x".into()]));
        assert!(Manifest::parse(b"(not a manifest").is_err());
        assert!(Manifest::parse(&vec![b' '; MAX_MANIFEST_BYTES as usize + 1]).is_err());
    }

    #[test]
    fn hashing_and_index() {
        let dir = std::env::temp_dir().join(format!("bf2_content_index_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join("mod/levels/x")).unwrap();
        let a = dir.join("mod/levels/x/level.ron");
        std::fs::write(&a, b"(name: \"x\")").unwrap();
        std::fs::write(dir.join("mod/levels/x/navgrid_gpm_cq_16.bin"), b"cache").unwrap();
        std::fs::write(dir.join("mod/notes.txt"), b"not shared").unwrap();
        let found = walk_layer(&dir.join("mod"));
        assert_eq!(found.len(), 1);
        assert_eq!(found[0].path, "levels/x/level.ron");
        let (size, hash) = hash_file(&a, None, None).unwrap();
        assert_eq!(size, 11);
        assert_eq!(hash, hash_bytes(b"(name: \"x\")"));
        let index_file = dir.join("index.txt");
        let mut index = HashIndex::load(Some(index_file.clone()));
        let progress = AtomicU64::new(0);
        let hashes = index.hash_all(std::slice::from_ref(&a), &progress, None);
        assert_eq!(hashes[0], Some((11, hash.clone())));
        assert_eq!(progress.load(Ordering::Relaxed), 11);
        index.save();
        // Unchanged: from the index, nothing hashed.
        let mut index = HashIndex::load(Some(index_file));
        let progress = AtomicU64::new(0);
        assert_eq!(index.hash_all(std::slice::from_ref(&a), &progress, None)[0], Some((11, hash)));
        assert_eq!(progress.load(Ordering::Relaxed), 0);
        // Changed: hashed again.
        std::fs::write(&a, b"(name: \"changed\")").unwrap();
        let hashes = index.hash_all(std::slice::from_ref(&a), &progress, None);
        assert_eq!(hashes[0].as_ref().unwrap().1, hash_bytes(b"(name: \"changed\")"));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn modes() {
        assert_eq!("ALL".parse::<ContentMode>(), Ok(ContentMode::All));
        assert_eq!("mods".parse::<ContentMode>(), Ok(ContentMode::Mods));
        assert!("everything".parse::<ContentMode>().is_err());
        assert_eq!(ContentMode::default(), ContentMode::Mods);
    }
}
