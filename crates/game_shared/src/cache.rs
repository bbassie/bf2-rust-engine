//! Generated data kept between runs, so a level loads faster the next time: bots' navigation
//! grids (`game_server::nav`) and the tactical map's base picture (`client::tactical_map`).
//!
//! ```text
//! <cache>/                          see [`Cache::resolve`]
//!   nav/<level>/infantry-<key>.bin  the soldiers' grid
//!   nav/<level>/vehicle-<key>.bin   the land vehicles' grid
//!   tactical/<level>/base-<key>.bin the tactical map without the combat area
//! ```
//!
//! - **One folder, apart from the content.** Nothing generated goes into `imported/` or the
//!   mods, so content sharing can never offer a cache to clients, and deleting the folder only
//!   costs rebuild time. The client and a local dedicated server share it.
//! - **Content addressed.** `<key>` is a hash of everything the result is made from (the
//!   generator's version, the terrain, the colliders, the play area, the parameters), made by
//!   whoever produces the entry. Layouts with the same inputs (a level's conquest and co-op
//!   layouts often are) share one file; a changed input writes a new one. The key is stored in
//!   the file too and checked on load.
//! - **Last use is the file's modification time**, set on every hit: no index file for
//!   several processes to fight over, and nothing to get out of step after a crash.
//! - **Eviction** ([`Cache::maintain`], after each write and at startup): entries of a level
//!   and kind unused for [`STALE_AFTER`] while another of them was used since go, then the
//!   least recently used ones until the folder is under the size limit.
//! - **Atomic writes**: written to a temporary file next to the entry and renamed, so readers
//!   never see a torn file; two processes writing the same entry write the same bytes.
//! - **Compressed** in 1 MiB chunks (raw deflate with a CRC-32 each), compressed and
//!   decompressed on several threads: a 4096x4096 tactical map (64 MB) loads in a few tens of
//!   milliseconds from about 12 MB.
//!
//! [`remove_legacy`] deletes the caches older builds wrote into the content folders.

use std::{
    fs,
    io::{self, Write},
    path::{Path, PathBuf},
    sync::atomic::{AtomicU64, AtomicUsize, Ordering},
    time::{Duration, SystemTime},
};

use bevy::prelude::*;
use flate2::{Compression, Crc, Decompress, FlushDecompress, Status, write::DeflateEncoder};

use crate::config::GamePaths;

/// Environment variable that overrides the cache folder.
pub const DIR_ENV: &str = "BF2_CACHE_DIR";
/// Environment variable that overrides the size limit, in GB.
pub const LIMIT_ENV: &str = "BF2_CACHE_LIMIT_GB";
/// The size limit without `--cache-limit-gb` or [`LIMIT_ENV`].
pub const DEFAULT_LIMIT: u64 = 2_000_000_000;
/// An entry unused this long goes when another entry of its level and kind was used since.
pub const STALE_AFTER: Duration = Duration::from_secs(30 * 24 * 3600);
/// Temporary files older than this were left by a writer that died.
const TEMP_MAX_AGE: Duration = Duration::from_secs(3600);

/// Bump when the file layout changes.
const MAGIC: &[u8; 8] = b"BF2CACH1";
/// Uncompressed bytes per chunk.
const CHUNK: usize = 1 << 20;
/// Magic, key, length, chunk size, chunk count.
const HEADER: usize = 8 + 8 + 8 + 4 + 4;

/// The cache folder and its size limit. A resource on the client and the dedicated server;
/// without it nothing is cached.
#[derive(Resource, Clone, Debug)]
pub struct Cache {
    root: PathBuf,
    limit: u64,
}

/// A number of files and their size.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Usage {
    pub files: usize,
    pub bytes: u64,
}

impl Usage {
    fn add(&mut self, bytes: u64) {
        self.files += 1;
        self.bytes += bytes;
    }
}

impl std::fmt::Display for Usage {
    fn fmt(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
        write!(f, "{} files, {}", self.files, crate::content::format_bytes(self.bytes))
    }
}

/// A file in the cache folder.
#[derive(Clone, Debug)]
struct CacheFile {
    path: PathBuf,
    /// `<kind>/<level>/<name>`: entries of one level and kind.
    group: PathBuf,
    size: u64,
    used: SystemTime,
    temp: bool,
}

impl Cache {
    pub fn new(root: impl Into<PathBuf>, limit: u64) -> Self {
        Self {
            root: root.into(),
            limit,
        }
    }

    /// The folder from `dir` (`--cache-dir`), else [`DIR_ENV`], else the platform's cache
    /// folder ([`default_dir`]); the limit from `limit_gb` (`--cache-limit-gb`), else
    /// [`LIMIT_ENV`], else [`DEFAULT_LIMIT`]. None when there is no folder to use.
    pub fn resolve(dir: Option<PathBuf>, limit_gb: Option<f64>) -> Option<Self> {
        let env = |name| std::env::var_os(name).filter(|v| !v.is_empty());
        let root = dir.or_else(|| env(DIR_ENV).map(PathBuf::from)).or_else(default_dir)?;
        let limit_gb = limit_gb.or_else(|| env(LIMIT_ENV)?.to_str()?.trim().parse().ok());
        let limit = limit_gb.map_or(DEFAULT_LIMIT, |gb| (gb.max(0.0) * 1e9) as u64);
        Some(Self::new(std::path::absolute(&root).unwrap_or(root), limit))
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    pub fn limit(&self) -> u64 {
        self.limit
    }

    /// Where an entry goes: `<kind>/<level>/<name>-<key>.bin`. `kind` and `name` are fixed
    /// words (lowercase letters, digits, `_`); a level name that isn't a plain file name is
    /// made one.
    pub fn path(&self, kind: &str, level: &str, name: &str, key: u64) -> PathBuf {
        assert!(is_word(kind) && is_word(name), "cache kind `{kind}` / name `{name}`");
        self.root.join(kind).join(level_folder(level)).join(format!("{name}-{key:016x}.bin"))
    }

    /// The entry's data, if it is there (and intact). A hit counts as a use.
    pub fn load(&self, kind: &str, level: &str, name: &str, key: u64) -> Option<Vec<u8>> {
        let path = self.path(kind, level, name, key);
        let bytes = fs::read(&path).ok()?;
        match decode(&bytes, key) {
            Ok(data) => {
                touch(&path);
                Some(data)
            }
            Err(err) => {
                warn!("cache: removing {}: {err}", path.display());
                let _ = fs::remove_file(&path);
                None
            }
        }
    }

    /// Writes an entry, then keeps the folder within its limits ([`Self::maintain`]).
    pub fn store(&self, kind: &str, level: &str, name: &str, key: u64, data: &[u8]) -> io::Result<PathBuf> {
        let path = self.path(kind, level, name, key);
        write_atomic(&path, &encode(key, data))?;
        let removed = self.maintain(Some(&path));
        if removed.files > 0 {
            info!("cache: evicted {removed} from {}", self.root.display());
        }
        Ok(path)
    }

    /// The most recently used entry of a level and kind, whatever its key (tests and tools
    /// that look at what the game cached).
    pub fn newest(&self, kind: &str, level: &str, name: &str) -> Option<(u64, PathBuf)> {
        let group = Path::new(kind).join(level_folder(level)).join(name);
        let file = self.files().into_iter().filter(|f| !f.temp && f.group == group).max_by_key(|f| f.used)?;
        let key = entry_key(file.path.file_name()?.to_str()?)?;
        Some((key, file.path))
    }

    /// What the cache holds.
    pub fn usage(&self) -> Usage {
        let mut usage = Usage::default();
        for file in self.files().iter().filter(|f| !f.temp) {
            usage.add(file.size);
        }
        usage
    }

    /// Deletes every entry. Returns what went.
    pub fn clear(&self) -> Usage {
        let mut removed = Usage::default();
        let now = SystemTime::now();
        for file in self.files() {
            // A temporary file may be an entry being written right now.
            if (!file.temp || age(now, file.used) > TEMP_MAX_AGE) && fs::remove_file(&file.path).is_ok() {
                removed.add(file.size);
            }
        }
        self.remove_empty_folders();
        removed
    }

    /// Deletes leftover temporary files, entries superseded for [`STALE_AFTER`], then the
    /// least recently used entries until the cache is under its limit. `keep` (the entry just
    /// written) stays. Returns what went.
    pub fn maintain(&self, keep: Option<&Path>) -> Usage {
        let now = SystemTime::now();
        let mut removed = Usage::default();
        let mut remove = |file: &CacheFile| {
            let gone = fs::remove_file(&file.path).is_ok();
            if gone {
                removed.add(file.size);
            }
            gone
        };
        let (temps, mut entries): (Vec<CacheFile>, Vec<CacheFile>) = self.files().into_iter().partition(|f| f.temp);
        for temp in temps.iter().filter(|f| age(now, f.used) > TEMP_MAX_AGE) {
            remove(temp);
        }
        let mut newest: std::collections::HashMap<PathBuf, SystemTime> = default();
        for entry in &entries {
            let at = newest.entry(entry.group.clone()).or_insert(entry.used);
            *at = (*at).max(entry.used);
        }
        entries.retain(|entry| {
            let superseded = newest[&entry.group] > entry.used && age(now, entry.used) > STALE_AFTER;
            !(superseded && Some(entry.path.as_path()) != keep && remove(entry))
        });
        entries.sort_by_key(|entry| entry.used);
        let mut total: u64 = entries.iter().map(|e| e.size).sum();
        for entry in &entries {
            if total <= self.limit {
                break;
            }
            if Some(entry.path.as_path()) != keep && remove(entry) {
                total -= entry.size;
            }
        }
        if removed.files > 0 {
            self.remove_empty_folders();
        }
        removed
    }

    /// Every cache file: `<kind>/<level>/<file>` where the kind and the file name are ours.
    /// Anything else in the folder is left alone.
    fn files(&self) -> Vec<CacheFile> {
        let mut files = Vec::new();
        for kind in read_dir(&self.root) {
            let kind_name = kind.file_name().to_string_lossy().into_owned();
            if !is_word(&kind_name) || !kind.file_type().is_ok_and(|t| t.is_dir()) {
                continue;
            }
            for level in read_dir(&kind.path()) {
                if !level.file_type().is_ok_and(|t| t.is_dir()) {
                    continue;
                }
                for file in read_dir(&level.path()) {
                    let file_name = file.file_name().to_string_lossy().into_owned();
                    let temp = is_temp_name(&file_name);
                    if !(temp || entry_key(&file_name).is_some()) {
                        continue;
                    }
                    let Ok(meta) = file.metadata() else { continue };
                    if !meta.is_file() {
                        continue;
                    }
                    let name = file_name.split('-').next().unwrap_or_default();
                    files.push(CacheFile {
                        path: file.path(),
                        group: Path::new(&kind_name).join(level.file_name()).join(name),
                        size: meta.len(),
                        used: meta.modified().unwrap_or(SystemTime::UNIX_EPOCH),
                        temp,
                    });
                }
            }
        }
        files
    }

    /// Level folders left empty (only empty ones: `remove_dir` fails on the others).
    fn remove_empty_folders(&self) {
        for kind in read_dir(&self.root) {
            if is_word(&kind.file_name().to_string_lossy()) {
                for level in read_dir(&kind.path()) {
                    let _ = fs::remove_dir(level.path());
                }
            }
        }
    }
}

/// The platform's folder for caches: `%LOCALAPPDATA%\bf2-rust-engine\cache` on Windows,
/// `~/Library/Caches/bf2-rust-engine` on macOS, `$XDG_CACHE_HOME/bf2-rust-engine` (default
/// `~/.cache/bf2-rust-engine`) elsewhere.
pub fn default_dir() -> Option<PathBuf> {
    let env = |name| std::env::var_os(name).filter(|v| !v.is_empty()).map(PathBuf::from);
    if cfg!(windows) {
        env("LOCALAPPDATA").map(|dir| dir.join("bf2-rust-engine").join("cache"))
    } else if cfg!(target_os = "macos") {
        env("HOME").map(|home| home.join("Library/Caches/bf2-rust-engine"))
    } else {
        env("XDG_CACHE_HOME")
            .filter(|dir| dir.is_absolute())
            .or_else(|| env("HOME").map(|home| home.join(".cache")))
            .map(|dir| dir.join("bf2-rust-engine"))
    }
}

/// At startup: deletes older builds' caches from the content folders ([`remove_legacy`];
/// right away, so content sharing never offers them), then applies the cache's limits on a
/// background thread. Logs what it removed.
pub fn housekeeping(cache: Option<Cache>, paths: &GamePaths) {
    let roots: Vec<PathBuf> = paths.roots().into_iter().map(Path::to_path_buf).collect();
    let started = std::time::Instant::now();
    let legacy = remove_legacy(&roots, &paths.imported);
    if legacy.files > 0 {
        info!(
            "cache: removed {legacy} of old caches from the content folders in {:.2} s",
            started.elapsed().as_secs_f32()
        );
    }
    let Some(cache) = cache else {
        warn!("cache: no cache folder (set {DIR_ENV}); generated data isn't kept");
        return;
    };
    info!("cache: {} (limit {})", cache.root.display(), crate::content::format_bytes(cache.limit));
    let spawned = std::thread::Builder::new().name("cache housekeeping".into()).spawn(move || {
        let removed = cache.maintain(None);
        if removed.files > 0 {
            info!("cache: evicted {removed} from {}", cache.root.display());
        }
    });
    if let Err(err) = spawned {
        warn!("cache: housekeeping thread: {err}");
    }
}

/// Deletes the caches older builds wrote into the content folders: `navgrid*.bin` (and their
/// `.bin.tmp`) in every level folder of the given roots (the mods and `imported`), and
/// `<imported>/cache/tactical/*.bin` (and `.tmp`), then those two folders if that left them
/// empty. Only those exact names. Returns what went.
pub fn remove_legacy(roots: &[PathBuf], imported: &Path) -> Usage {
    let mut removed = Usage::default();
    let mut remove = |path: PathBuf| {
        let size = fs::metadata(&path).map_or(0, |m| m.len());
        if fs::remove_file(&path).is_ok() {
            removed.add(size);
        }
    };
    for root in roots {
        for level in read_dir(&root.join("levels")) {
            if !level.file_type().is_ok_and(|t| t.is_dir()) {
                continue;
            }
            for file in read_dir(&level.path()) {
                if is_legacy_navgrid(&file.file_name().to_string_lossy()) && file.file_type().is_ok_and(|t| t.is_file()) {
                    remove(file.path());
                }
            }
        }
    }
    let tactical = imported.join("cache").join("tactical");
    for file in read_dir(&tactical) {
        if is_legacy_tactical(&file.file_name().to_string_lossy()) && file.file_type().is_ok_and(|t| t.is_file()) {
            remove(file.path());
        }
    }
    let _ = fs::remove_dir(&tactical);
    let _ = fs::remove_dir(imported.join("cache"));
    removed
}

/// `navgrid.bin`, `navgrid_<words>.bin`, and the same with `.tmp` after it.
fn is_legacy_navgrid(name: &str) -> bool {
    let Some(rest) = name.strip_prefix("navgrid") else {
        return false;
    };
    let Some(middle) = rest.strip_suffix(".bin").or_else(|| rest.strip_suffix(".bin.tmp")) else {
        return false;
    };
    middle.is_empty()
        || middle
            .strip_prefix('_')
            .is_some_and(|m| !m.is_empty() && m.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'_'))
}

/// `<level>_<mode>_<size>.bin` or `.tmp`, as the tactical map wrote them.
fn is_legacy_tactical(name: &str) -> bool {
    let Some(stem) = name.strip_suffix(".bin").or_else(|| name.strip_suffix(".tmp")) else {
        return false;
    };
    !stem.is_empty() && stem.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'_' || b == b'-')
}

fn read_dir(dir: &Path) -> impl Iterator<Item = fs::DirEntry> {
    fs::read_dir(dir).into_iter().flatten().flatten()
}

fn age(now: SystemTime, then: SystemTime) -> Duration {
    now.duration_since(then).unwrap_or_default()
}

/// Lowercase letters, digits and `_`.
fn is_word(s: &str) -> bool {
    !s.is_empty() && s.bytes().all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'_')
}

/// A level name as a folder name: letters, digits, `_`, `-` and `.` are kept, anything else
/// becomes `_`.
fn level_folder(level: &str) -> String {
    let folder: String = level
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() || matches!(c, '_' | '-' | '.') { c } else { '_' })
        .collect();
    if folder.is_empty() || folder.bytes().all(|b| b == b'.') {
        format!("_{folder}")
    } else {
        folder
    }
}

/// The key of an entry's file name (`<name>-<16 hex digits>.bin`).
fn entry_key(file_name: &str) -> Option<u64> {
    let (name, rest) = file_name.split_once('-')?;
    let hex = rest.strip_suffix(".bin")?;
    (is_word(name) && hex.len() == 16 && hex.bytes().all(|b| b.is_ascii_hexdigit() && !b.is_ascii_uppercase()))
        .then(|| u64::from_str_radix(hex, 16).ok())?
}

/// `<entry file name>.<process>.<counter>.tmp`.
fn is_temp_name(file_name: &str) -> bool {
    let Some(rest) = file_name.strip_suffix(".tmp") else {
        return false;
    };
    let mut parts = rest.rsplitn(3, '.');
    let (Some(counter), Some(pid), Some(entry)) = (parts.next(), parts.next(), parts.next()) else {
        return false;
    };
    let number = |s: &str| !s.is_empty() && s.bytes().all(|b| b.is_ascii_digit());
    number(counter) && number(pid) && entry_key(entry).is_some()
}

/// Marks an entry used now.
fn touch(path: &Path) {
    let _ = fs::File::options()
        .write(true)
        .open(path)
        .and_then(|file| file.set_modified(SystemTime::now()));
}

/// Writes `bytes` to a temporary file next to `path` and renames it over `path`.
fn write_atomic(path: &Path, bytes: &[u8]) -> io::Result<()> {
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    let dir = path.parent().expect("cache entries are in a folder");
    let temp = dir.join(format!(
        "{}.{}.{}.tmp",
        path.file_name().unwrap_or_default().to_string_lossy(),
        std::process::id(),
        COUNTER.fetch_add(1, Ordering::Relaxed)
    ));
    let mut attempt = 0;
    let result = loop {
        let result = fs::create_dir_all(dir).and_then(|()| {
            let mut file = fs::File::create(&temp)?;
            file.write_all(bytes)?;
            drop(file);
            fs::rename(&temp, path)
        });
        attempt += 1;
        // Another process's `maintain` may have removed the (then empty) folder in between.
        match result {
            Err(err) if err.kind() == io::ErrorKind::NotFound && attempt < 3 => continue,
            result => break result,
        }
    };
    if result.is_err() {
        let _ = fs::remove_file(&temp);
        // Another process renamed the same entry (the same bytes) over it first, and it is
        // still open (Windows).
        if path.is_file() {
            return Ok(());
        }
    }
    result
}

fn invalid(message: impl Into<String>) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, message.into())
}

/// An entry's file: the header, a table with each chunk's compressed size and the CRC-32 of
/// its data (little-endian u32s), then the chunks, raw deflate.
pub fn encode(key: u64, data: &[u8]) -> Vec<u8> {
    let chunks: Vec<&[u8]> = data.chunks(CHUNK).collect();
    let packed = parallel(chunks.len(), |i| {
        let mut z = DeflateEncoder::new(Vec::with_capacity(chunks[i].len() / 4 + 64), Compression::fast());
        z.write_all(chunks[i]).expect("writing to memory");
        let mut crc = Crc::new();
        crc.update(chunks[i]);
        (z.finish().expect("writing to memory"), crc.sum())
    });
    let packed_len: usize = packed.iter().map(|(p, _)| p.len()).sum();
    let mut out = Vec::with_capacity(HEADER + packed.len() * 8 + packed_len);
    out.extend_from_slice(MAGIC);
    out.extend_from_slice(&key.to_le_bytes());
    out.extend_from_slice(&(data.len() as u64).to_le_bytes());
    out.extend_from_slice(&(CHUNK as u32).to_le_bytes());
    out.extend_from_slice(&(packed.len() as u32).to_le_bytes());
    for (bytes, crc) in &packed {
        out.extend_from_slice(&(bytes.len() as u32).to_le_bytes());
        out.extend_from_slice(&crc.to_le_bytes());
    }
    for (bytes, _) in &packed {
        out.extend_from_slice(bytes);
    }
    out
}

/// The data of an entry's file, if it is intact and has this key.
pub fn decode(bytes: &[u8], key: u64) -> io::Result<Vec<u8>> {
    let u32_at = |at: usize| u32::from_le_bytes(bytes[at..at + 4].try_into().unwrap());
    let u64_at = |at: usize| u64::from_le_bytes(bytes[at..at + 8].try_into().unwrap());
    if bytes.len() < HEADER || &bytes[..8] != MAGIC {
        return Err(invalid("not a cache file (or an older version)"));
    }
    if u64_at(8) != key {
        return Err(invalid("another key"));
    }
    let (len, chunk, count) = (u64_at(16), u32_at(24) as usize, u32_at(28) as usize);
    let len = usize::try_from(len).map_err(|_| invalid("too big"))?;
    if chunk == 0 || count != len.div_ceil(chunk) || bytes.len() < HEADER + count * 8 {
        return Err(invalid("bad header"));
    }
    let mut chunks = Vec::with_capacity(count);
    let mut at = HEADER + count * 8;
    for i in 0..count {
        let (size, crc) = (u32_at(HEADER + i * 8) as usize, u32_at(HEADER + i * 8 + 4));
        let end = at.checked_add(size).filter(|&end| end <= bytes.len()).ok_or_else(|| invalid("truncated"))?;
        chunks.push((&bytes[at..end], crc));
        at = end;
    }
    if at != bytes.len() {
        return Err(invalid("wrong size"));
    }
    let mut out = vec![0u8; len];
    let parts: Vec<(&mut [u8], (&[u8], u32))> = out.chunks_mut(chunk).zip(chunks).collect();
    let threads = worker_count(parts.len());
    let mut groups: Vec<Vec<(&mut [u8], (&[u8], u32))>> = (0..threads).map(|_| Vec::new()).collect();
    for (i, part) in parts.into_iter().enumerate() {
        groups[i % threads].push(part);
    }
    let inflate_all = |group: Vec<(&mut [u8], (&[u8], u32))>| -> io::Result<()> {
        for (dst, (src, crc)) in group {
            inflate(src, dst)?;
            let mut check = Crc::new();
            check.update(dst);
            if check.sum() != crc {
                return Err(invalid("checksum mismatch"));
            }
        }
        Ok(())
    };
    if threads <= 1 {
        groups.into_iter().try_for_each(inflate_all)?;
    } else {
        std::thread::scope(|scope| {
            let handles: Vec<_> = groups.into_iter().map(|group| scope.spawn(|| inflate_all(group))).collect();
            handles
                .into_iter()
                .try_for_each(|h| h.join().unwrap_or_else(|_| Err(invalid("decoder panicked"))))
        })?;
    }
    Ok(out)
}

/// Inflates `src` into exactly `dst`.
fn inflate(src: &[u8], dst: &mut [u8]) -> io::Result<()> {
    let mut z = Decompress::new(false);
    loop {
        let (read, written) = (z.total_in() as usize, z.total_out() as usize);
        let status = z
            .decompress(&src[read..], &mut dst[written..], FlushDecompress::Finish)
            .map_err(|err| invalid(err.to_string()))?;
        if status == Status::StreamEnd {
            break;
        }
        if z.total_in() as usize == read && z.total_out() as usize == written {
            return Err(invalid("truncated chunk"));
        }
    }
    if z.total_out() as usize != dst.len() || z.total_in() as usize != src.len() {
        return Err(invalid("wrong chunk size"));
    }
    Ok(())
}

fn worker_count(jobs: usize) -> usize {
    let cores = std::thread::available_parallelism().map_or(4, |n| n.get());
    jobs.min(cores).clamp(1, 16)
}

/// `f(0..n)` on up to [`worker_count`] threads, in order.
fn parallel<T: Send>(n: usize, f: impl Fn(usize) -> T + Sync) -> Vec<T> {
    let threads = worker_count(n);
    if threads <= 1 {
        return (0..n).map(f).collect();
    }
    let next = AtomicUsize::new(0);
    let mut results: Vec<(usize, T)> = std::thread::scope(|scope| {
        let handles: Vec<_> = (0..threads)
            .map(|_| {
                scope.spawn(|| {
                    let mut done = Vec::new();
                    loop {
                        let i = next.fetch_add(1, Ordering::Relaxed);
                        if i >= n {
                            break done;
                        }
                        done.push((i, f(i)));
                    }
                })
            })
            .collect();
        handles.into_iter().flat_map(|h| h.join().expect("cache worker panicked")).collect()
    });
    results.sort_by_key(|(i, _)| *i);
    results.into_iter().map(|(_, t)| t).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_cache(name: &str, limit: u64) -> Cache {
        let root = std::env::temp_dir().join(format!("bf2_cache_{name}_{}", std::process::id()));
        let _ = fs::remove_dir_all(&root);
        Cache::new(root, limit)
    }

    /// Something that compresses a little, like a picture.
    fn sample(len: usize, seed: u32) -> Vec<u8> {
        let mut x = seed.wrapping_mul(2_654_435_761).max(1);
        (0..len)
            .map(|i| {
                x ^= x << 13;
                x ^= x >> 17;
                x ^= x << 5;
                // Runs with some noise in them.
                if x % 8 == 0 { x as u8 } else { (i / 64) as u8 }
            })
            .collect()
    }

    fn set_used(path: &Path, ago: Duration) {
        let file = fs::File::options().write(true).open(path).unwrap();
        file.set_modified(SystemTime::now() - ago).unwrap();
    }

    #[test]
    fn round_trip_and_checks() {
        for len in [0, 1, 1000, CHUNK, CHUNK + 1, 5 * CHUNK + 12345] {
            let data = sample(len, len as u32);
            let bytes = encode(7, &data);
            assert_eq!(decode(&bytes, 7).unwrap(), data, "{len} bytes");
            assert!(decode(&bytes, 8).is_err(), "another key");
            assert!(decode(&bytes[..bytes.len() - 1], 7).is_err() || len == 0, "truncated");
        }
        let data = sample(3 * CHUNK, 1);
        let mut bytes = encode(1, &data);
        assert!(bytes.len() < data.len() / 2, "compressed: {} of {}", bytes.len(), data.len());
        let last = bytes.len() - 100;
        bytes[last] ^= 0x55;
        assert!(decode(&bytes, 1).is_err(), "a flipped bit");
        assert!(decode(b"BF2CACH0 and then some more bytes to be long enough", 1).is_err());
    }

    #[test]
    fn keys_name_entries() {
        let cache = temp_cache("keys", DEFAULT_LIMIT);
        // Identical inputs, one file; any difference, another.
        assert_eq!(cache.path("nav", "karkand", "infantry", 1), cache.path("nav", "karkand", "infantry", 1));
        let base = cache.path("nav", "karkand", "infantry", 1);
        for other in [
            cache.path("nav", "karkand", "infantry", 2),
            cache.path("nav", "karkand", "vehicle", 1),
            cache.path("nav", "oman", "infantry", 1),
            cache.path("tactical", "karkand", "infantry", 1),
        ] {
            assert_ne!(base, other);
        }
        assert!(base.ends_with("nav/karkand/infantry-0000000000000001.bin"));
        // Level names can't leave the folder.
        for level in ["..", "../x", "a/b", "", "c:\\x"] {
            let path = cache.path("nav", level, "infantry", 1);
            assert_eq!(path.parent().unwrap().parent().unwrap(), cache.root.join("nav"), "{level:?}");
        }
        assert_eq!(entry_key("infantry-00000000000000ff.bin"), Some(255));
        assert_eq!(entry_key("infantry-00000000000000FF.bin"), None);
        assert_eq!(entry_key("level.ron"), None);
        assert!(is_temp_name("base-00000000000000ff.bin.123.4.tmp"));
        assert!(!is_temp_name("notes.tmp"));
    }

    #[test]
    fn store_load_and_clear() {
        let cache = temp_cache("store", DEFAULT_LIMIT);
        let data = sample(2 * CHUNK + 5, 3);
        assert!(cache.load("nav", "x", "infantry", 5).is_none());
        let path = cache.store("nav", "x", "infantry", 5, &data).unwrap();
        assert_eq!(cache.load("nav", "x", "infantry", 5).unwrap(), data);
        assert!(cache.load("nav", "x", "infantry", 6).is_none());
        assert_eq!(cache.newest("nav", "x", "infantry"), Some((5, path.clone())));
        // Anything else in the folder is left alone.
        fs::write(cache.root.join("nav/x/notes.txt"), "mine").unwrap();
        fs::write(cache.root.join("readme.txt"), "mine").unwrap();
        assert_eq!(cache.usage().files, 1);
        // A damaged entry is a miss, and goes.
        fs::write(&path, b"garbage").unwrap();
        assert!(cache.load("nav", "x", "infantry", 5).is_none());
        assert!(!path.exists());
        cache.store("nav", "x", "infantry", 5, &data).unwrap();
        cache.store("tactical", "x", "base", 9, &data).unwrap();
        let usage = cache.usage();
        assert_eq!(usage.files, 2);
        assert_eq!(cache.clear(), usage);
        assert_eq!(cache.usage(), Usage::default());
        assert!(cache.root.join("nav/x/notes.txt").exists() && cache.root.join("readme.txt").exists());
        assert!(!cache.root.join("tactical/x").exists(), "empty level folders go");
        let _ = fs::remove_dir_all(&cache.root);
    }

    #[test]
    fn evicts_least_recently_used() {
        let data = sample(CHUNK, 4);
        let size = encode(0, &data).len() as u64;
        // Room for three entries.
        let cache = temp_cache("lru", size * 3 + size / 2);
        let mut paths = Vec::new();
        for (i, level) in ["a", "b", "c"].into_iter().enumerate() {
            let path = cache.store("nav", level, "infantry", i as u64, &data).unwrap();
            set_used(&path, Duration::from_secs(3600 * (10 - i as u64)));
            paths.push(path);
        }
        // `a` is the oldest, but used now.
        assert!(cache.load("nav", "a", "infantry", 0).is_some());
        let d = cache.store("nav", "d", "infantry", 3, &data).unwrap();
        assert!(paths[0].exists(), "used last");
        assert!(!paths[1].exists(), "used longest ago");
        assert!(paths[2].exists() && d.exists());
        assert!(!cache.root.join("nav/b").exists());
        // The entry just written stays even over the limit.
        let tiny = Cache::new(cache.root.clone(), 1);
        let e = tiny.store("nav", "e", "infantry", 4, &data).unwrap();
        assert!(e.exists());
        assert_eq!(tiny.usage().files, 1);
        let _ = fs::remove_dir_all(&cache.root);
    }

    #[test]
    fn drops_superseded_entries() {
        let cache = temp_cache("stale", DEFAULT_LIMIT);
        let data = sample(1000, 5);
        let old = cache.store("nav", "x", "infantry", 1, &data).unwrap();
        let alone = cache.store("nav", "y", "infantry", 1, &data).unwrap();
        let other_kind = cache.store("nav", "x", "vehicle", 1, &data).unwrap();
        for path in [&old, &alone, &other_kind] {
            set_used(path, STALE_AFTER + Duration::from_secs(3600));
        }
        let recent = cache.store("nav", "x", "infantry", 2, &data).unwrap();
        assert!(!old.exists(), "superseded and unused for long");
        assert!(alone.exists(), "the only entry of its level");
        assert!(other_kind.exists(), "another kind");
        assert!(recent.exists());
        // Leftovers of a writer that died go; one writing now stays.
        let dead = cache.root.join("nav/x/infantry-0000000000000003.bin.1.0.tmp");
        let live = cache.root.join("nav/x/infantry-0000000000000004.bin.1.1.tmp");
        fs::write(&dead, b"x").unwrap();
        fs::write(&live, b"x").unwrap();
        set_used(&dead, TEMP_MAX_AGE * 2);
        cache.maintain(None);
        assert!(!dead.exists() && live.exists());
        let _ = fs::remove_dir_all(&cache.root);
    }

    #[test]
    fn concurrent_writers_and_readers() {
        let cache = temp_cache("concurrent", DEFAULT_LIMIT);
        let data: Vec<Vec<u8>> = (0..4).map(|i| sample(CHUNK * 2 + i * 1000, i as u32)).collect();
        std::thread::scope(|scope| {
            for t in 0..8 {
                let (cache, data) = (&cache, &data);
                scope.spawn(move || {
                    for round in 0..6 {
                        let i = (t + round) % data.len();
                        // Same entries from several threads at once, and reads in between:
                        // a read sees nothing or the whole entry.
                        cache.store("tactical", "x", "base", i as u64, &data[i]).unwrap();
                        for (j, expected) in data.iter().enumerate() {
                            if let Some(got) = cache.load("tactical", "x", "base", j as u64) {
                                assert_eq!(&got, expected);
                            }
                        }
                    }
                });
            }
        });
        assert_eq!(cache.usage().files, data.len());
        assert!(cache.files().iter().all(|f| !f.temp), "no temporary files left");
        let _ = fs::remove_dir_all(&cache.root);
    }

    #[test]
    fn removes_only_legacy_caches() {
        let root = std::env::temp_dir().join(format!("bf2_cache_legacy_{}", std::process::id()));
        let _ = fs::remove_dir_all(&root);
        let (imported, mod_dir) = (root.join("imported"), root.join("mods/m"));
        let keep = [
            imported.join("levels/a/level.ron"),
            imported.join("levels/a/navgrid.ron"),
            imported.join("levels/a/navgrid_x.bin.bak"),
            imported.join("levels/a/navgrid-x.bin"),
            imported.join("cache/tactical/notes.txt"),
            mod_dir.join("levels/b/level.ron"),
        ];
        let gone = [
            imported.join("levels/a/navgrid_gpm_cq_16.bin"),
            imported.join("levels/a/navgrid_vehicle_gpm_cq_64.bin"),
            imported.join("levels/a/navgrid.bin"),
            imported.join("levels/a/navgrid_gpm_cq_16.bin.tmp"),
            imported.join("cache/tactical/a_gpm_cq_16.bin"),
            imported.join("cache/tactical/a_gpm_cq_16.tmp"),
            mod_dir.join("levels/b/navgrid_gpm_coop_16.bin"),
        ];
        for file in keep.iter().chain(&gone) {
            fs::create_dir_all(file.parent().unwrap()).unwrap();
            fs::write(file, b"1234").unwrap();
        }
        let removed = remove_legacy(&[mod_dir.clone(), imported.clone()], &imported);
        assert_eq!(removed, Usage { files: gone.len(), bytes: 4 * gone.len() as u64 });
        assert!(keep.iter().all(|f| f.exists()) && gone.iter().all(|f| !f.exists()));
        // An emptied cache folder goes.
        fs::remove_file(imported.join("cache/tactical/notes.txt")).unwrap();
        remove_legacy(&[imported.clone()], &imported);
        assert!(!imported.join("cache").exists());
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn resolves_folder_and_limit() {
        let cache = Cache::resolve(Some(PathBuf::from("some/dir")), Some(0.5)).unwrap();
        assert!(cache.root().is_absolute() && cache.root().ends_with("some/dir"));
        assert_eq!(cache.limit(), 500_000_000);
    }
}
