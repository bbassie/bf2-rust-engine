//! Working out what to download from a server's [`Manifest`], and downloading it into a
//! [`ContentStore`] with resume, several at a time, verifying every file before it is used.
//! The transport is behind [`Fetch`] (the client uses HTTP; tests use memory).

use std::{
    collections::{HashMap, HashSet},
    io::{self, Read, Write},
    path::{Path, PathBuf},
    sync::{
        Mutex,
        atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering},
    },
    time::Duration,
};

use super::{FileEntry, HashIndex, Manifest, is_hash, store::ContentStore};

/// An HTTP-like GET.
pub trait Fetch: Send + Sync {
    /// Requests `url` from byte `offset` on (a `Range: bytes=<offset>-` request when it isn't
    /// 0). `size` is the file's expected size, for timeouts.
    fn get(&self, url: &str, offset: u64, size: u64) -> Result<FetchResponse, String>;
}

pub struct FetchResponse {
    /// 200: the whole file; 206: from `range_start`; anything else fails.
    pub status: u16,
    /// Where a 206 answer starts (its `Content-Range`).
    pub range_start: Option<u64>,
    pub body: Box<dyn Read>,
}

/// How far checking and downloading got; shared with the UI.
#[derive(Default)]
pub struct Progress {
    pub files_total: AtomicUsize,
    pub files_done: AtomicUsize,
    pub bytes_total: AtomicU64,
    pub bytes_done: AtomicU64,
    /// Set to stop as soon as possible.
    pub cancel: AtomicBool,
}

impl Progress {
    pub fn cancelled(&self) -> bool {
        self.cancel.load(Ordering::Relaxed)
    }

    pub fn reset(&self, files: usize, bytes: u64) {
        self.files_total.store(files, Ordering::Relaxed);
        self.files_done.store(0, Ordering::Relaxed);
        self.bytes_total.store(bytes, Ordering::Relaxed);
        self.bytes_done.store(0, Ordering::Relaxed);
    }
}

/// What joining a server takes.
#[derive(Clone, Debug, Default)]
pub struct Plan {
    /// Levels planned for (the server's current one and its rotation).
    pub levels: Vec<String>,
    /// The files to mount: `(layer, file)`.
    pub needed: Vec<(usize, FileEntry)>,
    /// Files to download, one per hash.
    pub download: Vec<FileEntry>,
    /// Files we have locally (our own import or mods) with the right hash, to copy or link
    /// into the store.
    pub adopt: Vec<(FileEntry, PathBuf)>,
    /// Bytes of the needed files, once per hash.
    pub needed_bytes: u64,
    pub download_bytes: u64,
}

/// Works out what's needed for `levels`: files already in the store (verified) or among our
/// own files (`local_roots`: our mods and imported folder, compared by hash) aren't
/// downloaded. Hashes local files as needed (remembered in `index`); `progress` counts that.
pub fn plan(
    manifest: &Manifest,
    levels: &[String],
    store: &ContentStore,
    local_roots: &[PathBuf],
    index: &mut HashIndex,
    progress: &Progress,
) -> Result<Plan, String> {
    let needed: Vec<(usize, FileEntry)> = manifest.needed(levels).map(|(layer, f)| (layer, f.clone())).collect();
    let mut unique: Vec<&FileEntry> = Vec::new();
    let mut seen = HashSet::new();
    for (_, entry) in &needed {
        if seen.insert(entry.hash.as_str()) {
            unique.push(entry);
        }
    }
    // Candidates: the stored file, and local files of the right size.
    let mut candidates: Vec<PathBuf> = Vec::new();
    let mut of_entry: Vec<Vec<usize>> = Vec::with_capacity(unique.len());
    for entry in &unique {
        let mut mine = Vec::new();
        let stored = store.file(&entry.hash);
        let paths = std::iter::once(stored).chain(local_roots.iter().map(|root| {
            let mut path = root.clone();
            path.extend(entry.path.split('/'));
            path
        }));
        for path in paths {
            if std::fs::metadata(&path).is_ok_and(|m| m.is_file() && m.len() == entry.size) {
                mine.push(candidates.len());
                candidates.push(path);
            }
        }
        of_entry.push(mine);
    }
    let bytes: u64 = candidates.iter().filter_map(|c| std::fs::metadata(c).ok()).map(|m| m.len()).sum();
    progress.reset(candidates.len(), bytes);
    let hashes = index.hash_all(&candidates, &progress.bytes_done, Some(&progress.cancel));
    index.save();
    if progress.cancelled() {
        return Err("cancelled".into());
    }
    let mut plan = Plan {
        levels: levels.to_vec(),
        needed_bytes: unique.iter().map(|f| f.size).sum(),
        ..Default::default()
    };
    for (entry, mine) in unique.iter().zip(&of_entry) {
        let matching = mine
            .iter()
            .find(|&&i| hashes[i].as_ref().is_some_and(|(size, hash)| *size == entry.size && *hash == entry.hash));
        let stored = store.file(&entry.hash);
        match matching.map(|&i| &candidates[i]) {
            Some(path) if *path == stored => {}
            Some(path) => {
                // A damaged stored file makes way for our good copy.
                let _ = std::fs::remove_file(&stored);
                plan.adopt.push(((*entry).clone(), path.clone()));
            }
            None => {
                if stored.exists() {
                    bevy::log::warn!("content cache: {} is damaged, downloading it again", stored.display());
                    let _ = std::fs::remove_file(&stored);
                }
                plan.download_bytes += entry.size;
                plan.download.push((*entry).clone());
            }
        }
    }
    plan.needed = needed;
    Ok(plan)
}

/// Checks a downloaded file before it goes into the store (after its hash matched).
pub type Validate = dyn Fn(&FileEntry, &Path) -> Result<(), String> + Sync;

/// Attempts per source before trying the next one.
const ATTEMPTS: u32 = 3;

/// Downloads one file into the store: from each of `bases` in turn (`<base>/<hash>`),
/// resuming a partial download, verifying its hash (and `validate`) before it goes in.
pub fn download_file(
    fetch: &dyn Fetch,
    bases: &[String],
    entry: &FileEntry,
    store: &ContentStore,
    progress: &Progress,
    validate: &Validate,
) -> Result<(), String> {
    if !is_hash(&entry.hash) {
        return Err(format!("{}: bad hash", entry.path));
    }
    let partial = store.partial(&entry.hash);
    let partial_len = || std::fs::metadata(&partial).map_or(0, |m| m.len());
    // What an earlier session got counts as done.
    let mut counted = partial_len().min(entry.size);
    progress.bytes_done.fetch_add(counted, Ordering::Relaxed);
    let mut last_error = String::from("no source");
    for base in bases {
        let url = format!("{}/{}", base.trim_end_matches('/'), entry.hash);
        let mut attempt = 0;
        while attempt < ATTEMPTS {
            if progress.cancelled() {
                return Err("cancelled".into());
            }
            attempt += 1;
            let mut offset = partial_len();
            if offset > entry.size {
                let _ = std::fs::remove_file(&partial);
                progress.bytes_done.fetch_sub(counted, Ordering::Relaxed);
                counted = 0;
                offset = 0;
            }
            // Hashed while it arrives: opening a file just written costs a virus scan on Windows.
            let mut streamed = None;
            if offset < entry.size || entry.size == 0 && !partial.exists() {
                match fetch_into(fetch, &url, entry, offset, &partial, progress, &mut counted) {
                    Ok(hash) => streamed = hash,
                    Err(FetchError::Retry(err)) => {
                        last_error = format!("{url}: {err}");
                        std::thread::sleep(Duration::from_millis(250 * attempt as u64));
                        continue;
                    }
                    Err(FetchError::NextSource(err)) => {
                        last_error = format!("{url}: {err}");
                        break;
                    }
                }
                if partial_len() < entry.size {
                    // The connection ended early: resume.
                    last_error = format!("{url}: connection closed early");
                    continue;
                }
            }
            let hashed = match streamed {
                Some(hash) => Ok((partial_len(), hash)),
                None => super::hash_file(&partial, None, Some(&progress.cancel)),
            };
            let verified = hashed
                .map_err(|err| err.to_string())
                .and_then(|(size, hash)| {
                    if size == entry.size && hash == entry.hash {
                        Ok(())
                    } else {
                        Err(format!("{url}: not the file the manifest lists (did the server's files change?)"))
                    }
                })
                .and_then(|()| validate(entry, &partial));
            match verified {
                Ok(()) => {
                    store.commit(&entry.hash).map_err(|err| format!("{}: {err}", partial.display()))?;
                    progress.files_done.fetch_add(1, Ordering::Relaxed);
                    return Ok(());
                }
                Err(err) => {
                    let _ = std::fs::remove_file(&partial);
                    progress.bytes_done.fetch_sub(counted, Ordering::Relaxed);
                    counted = 0;
                    last_error = err;
                    // This source serves something else: try the next.
                    break;
                }
            }
        }
    }
    Err(format!("{}: {last_error}", entry.path))
}

enum FetchError {
    /// Try again (resuming).
    Retry(String),
    /// This source doesn't have it.
    NextSource(String),
}

/// Fetches (the rest of) a file into `partial`. Returns its hash if it is complete.
fn fetch_into(
    fetch: &dyn Fetch,
    url: &str,
    entry: &FileEntry,
    offset: u64,
    partial: &Path,
    progress: &Progress,
    counted: &mut u64,
) -> Result<Option<String>, FetchError> {
    let response = fetch.get(url, offset, entry.size).map_err(FetchError::Retry)?;
    let append = match response.status {
        206 if response.range_start == Some(offset) => true,
        200 => false,
        206 => return Err(FetchError::Retry("answered another range".into())),
        416 => {
            let _ = std::fs::remove_file(partial);
            progress.bytes_done.fetch_sub(*counted, Ordering::Relaxed);
            *counted = 0;
            return Err(FetchError::Retry("range not satisfiable".into()));
        }
        404 | 403 | 410 => return Err(FetchError::NextSource(format!("HTTP {}", response.status))),
        status => return Err(FetchError::Retry(format!("HTTP {status}"))),
    };
    let io = |err: io::Error| FetchError::Retry(err.to_string());
    let mut file = if append {
        std::fs::OpenOptions::new().append(true).open(partial).map_err(io)?
    } else {
        // From the start after all.
        progress.bytes_done.fetch_sub(*counted, Ordering::Relaxed);
        *counted = 0;
        std::fs::File::create(partial).map_err(io)?
    };
    let start = if append { offset } else { 0 };
    let mut hasher = super::ContentHasher::new();
    if append {
        let mut existing = std::fs::File::open(partial).map_err(io)?.take(start);
        let mut buffer = vec![0u8; 256 * 1024];
        loop {
            match existing.read(&mut buffer).map_err(io)? {
                0 => break,
                read => hasher.update(&buffer[..read]),
            }
        }
    }
    let mut written = start;
    let mut body = response.body;
    let mut buffer = vec![0u8; 128 * 1024];
    loop {
        if progress.cancelled() {
            return Err(FetchError::Retry("cancelled".into()));
        }
        let read = match body.read(&mut buffer) {
            Ok(0) => break,
            Ok(read) => read,
            Err(err) if err.kind() == io::ErrorKind::Interrupted => continue,
            Err(err) => {
                file.flush().map_err(io)?;
                return Err(FetchError::Retry(err.to_string()));
            }
        };
        if written + read as u64 > entry.size {
            drop(file);
            let _ = std::fs::remove_file(partial);
            progress.bytes_done.fetch_sub(*counted, Ordering::Relaxed);
            *counted = 0;
            return Err(FetchError::NextSource("sent more than the file's size".into()));
        }
        file.write_all(&buffer[..read]).map_err(io)?;
        hasher.update(&buffer[..read]);
        written += read as u64;
        *counted += read as u64;
        progress.bytes_done.fetch_add(read as u64, Ordering::Relaxed);
    }
    file.flush().map_err(io)?;
    Ok((written == entry.size).then(|| hasher.finish()))
}

/// Downloads `files` with `workers` at a time. Stops at the first file that can't be had
/// and returns its error.
pub fn download_all(
    fetch: &dyn Fetch,
    bases: &[String],
    files: &[FileEntry],
    store: &ContentStore,
    progress: &Progress,
    workers: usize,
    validate: &Validate,
) -> Result<(), String> {
    progress.reset(files.len(), files.iter().map(|f| f.size).sum());
    // Big files first, so the last moments aren't one big file on one connection.
    let mut order: Vec<&FileEntry> = files.iter().collect();
    order.sort_by_key(|f| std::cmp::Reverse(f.size));
    let next = AtomicUsize::new(0);
    let failed: Mutex<Option<String>> = Mutex::new(None);
    let stop = AtomicBool::new(false);
    std::thread::scope(|scope| {
        for _ in 0..workers.clamp(1, 16).min(order.len().max(1)) {
            scope.spawn(|| {
                while !stop.load(Ordering::Relaxed) && !progress.cancelled() {
                    let Some(entry) = order.get(next.fetch_add(1, Ordering::Relaxed)) else {
                        break;
                    };
                    if let Err(err) = download_file(fetch, bases, entry, store, progress, validate) {
                        stop.store(true, Ordering::Relaxed);
                        failed.lock().unwrap().get_or_insert(err);
                    }
                }
            });
        }
    });
    if progress.cancelled() {
        return Err("cancelled".into());
    }
    match failed.into_inner().unwrap() {
        Some(err) => Err(err),
        None => Ok(()),
    }
}

/// Puts the local files of a plan into the store. Returns the bytes copied (the rest were
/// hard links).
pub fn adopt_all(plan: &Plan, store: &ContentStore, progress: &Progress) -> Result<u64, String> {
    let mut copied = 0;
    let mut done = HashSet::new();
    for (entry, local) in &plan.adopt {
        if progress.cancelled() {
            return Err("cancelled".into());
        }
        if done.insert(entry.hash.clone()) && store.adopt(entry, local)? {
            copied += entry.size;
        }
    }
    Ok(copied)
}

/// A [`Fetch`] serving files from memory by URL, for tests: `breaks` cuts the first answer
/// for a URL off after this many bytes, like a dropped connection.
#[derive(Default)]
pub struct MemoryFetch {
    pub files: HashMap<String, Vec<u8>>,
    pub breaks: Option<usize>,
    /// Ignore `Range` (answer 200 with everything).
    pub no_ranges: bool,
    pub requests: Mutex<Vec<(String, u64)>>,
}

impl Fetch for MemoryFetch {
    fn get(&self, url: &str, offset: u64, _size: u64) -> Result<FetchResponse, String> {
        let first = {
            let mut requests = self.requests.lock().unwrap();
            let first = !requests.iter().any(|(u, _)| u == url);
            requests.push((url.to_string(), offset));
            first
        };
        let Some(bytes) = self.files.get(url) else {
            return Ok(FetchResponse { status: 404, range_start: None, body: Box::new(io::empty()) });
        };
        let (status, start) = if offset > 0 && !self.no_ranges {
            if offset >= bytes.len() as u64 {
                return Ok(FetchResponse { status: 416, range_start: None, body: Box::new(io::empty()) });
            }
            (206, offset as usize)
        } else {
            (200, 0)
        };
        let mut body = bytes[start..].to_vec();
        if first && let Some(cut) = self.breaks {
            body.truncate(cut);
        }
        Ok(FetchResponse { status, range_start: (status == 206).then_some(start as u64), body: Box::new(io::Cursor::new(body)) })
    }
}

#[cfg(test)]
mod tests {
    use super::super::{Layer, MANIFEST_VERSION, hash_bytes};
    use super::*;

    fn setup(name: &str) -> (PathBuf, ContentStore) {
        let dir = std::env::temp_dir().join(format!("bf2_content_dl_{name}_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let store = ContentStore::open(&dir.join("cache")).unwrap();
        (dir, store)
    }

    fn entry(path: &str, bytes: &[u8]) -> FileEntry {
        FileEntry { path: path.into(), size: bytes.len() as u64, hash: hash_bytes(bytes), levels: vec![] }
    }

    fn ok(_: &FileEntry, _: &Path) -> Result<(), String> {
        Ok(())
    }

    #[test]
    fn resumes_and_verifies() {
        let (dir, store) = setup("resume");
        let data: Vec<u8> = (0..300_000u32).map(|i| (i * 7 % 251) as u8).collect();
        let file = entry("objects/big.glb", &data);
        let url = format!("http://server/content/{}", file.hash);
        let fetch = MemoryFetch { files: [(url.clone(), data.clone())].into(), breaks: Some(100_000), ..Default::default() };
        let progress = Progress::default();
        download_file(&fetch, &["http://server/content".into()], &file, &store, &progress, &ok).unwrap();
        assert_eq!(std::fs::read(store.file(&file.hash)).unwrap(), data);
        // The second request resumed where the first broke off.
        let requests = fetch.requests.lock().unwrap().clone();
        assert_eq!(requests, [(url.clone(), 0), (url.clone(), 100_000)]);
        assert_eq!(progress.bytes_done.load(Ordering::Relaxed), data.len() as u64);

        // A partial download from an earlier session is resumed, not started over.
        let other: Vec<u8> = (0..5000u32).map(|i| (i % 13) as u8).collect();
        let second = entry("objects/other.dds", &other);
        std::fs::write(store.partial(&second.hash), &other[..1234]).unwrap();
        let url2 = format!("http://server/content/{}", second.hash);
        let fetch = MemoryFetch { files: [(url2.clone(), other.clone())].into(), ..Default::default() };
        download_file(&fetch, &["http://server/content".into()], &second, &store, &Progress::default(), &ok).unwrap();
        assert_eq!(fetch.requests.lock().unwrap()[0], (url2, 1234));
        assert_eq!(std::fs::read(store.file(&second.hash)).unwrap(), other);

        // A server ignoring Range still works (starts over).
        let third = entry("objects/third.dds", b"0123456789");
        std::fs::write(store.partial(&third.hash), b"01234").unwrap();
        let fetch = MemoryFetch {
            files: [(format!("http://server/content/{}", third.hash), b"0123456789".to_vec())].into(),
            no_ranges: true,
            ..Default::default()
        };
        download_file(&fetch, &["http://server/content".into()], &third, &store, &Progress::default(), &ok).unwrap();
        assert_eq!(std::fs::read(store.file(&third.hash)).unwrap(), b"0123456789");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn rejects_wrong_content_and_falls_back() {
        let (dir, store) = setup("wrong");
        let file = entry("levels/x/level.ron", b"(name: \"x\")");
        // The CDN serves something else; the server has it right.
        let fetch = MemoryFetch {
            files: [
                (format!("http://cdn/{}", file.hash), b"(name: \"evil\")".to_vec()),
                (format!("http://server/content/{}", file.hash), b"(name: \"x\")".to_vec()),
            ]
            .into(),
            ..Default::default()
        };
        let bases = ["http://cdn".to_string(), "http://server/content".to_string()];
        download_file(&fetch, &bases, &file, &store, &Progress::default(), &ok).unwrap();
        assert_eq!(std::fs::read(store.file(&file.hash)).unwrap(), b"(name: \"x\")");
        // Only wrong content anywhere: refused, nothing stored.
        let other = entry("levels/x/other.ron", b"good");
        let fetch = MemoryFetch { files: [(format!("http://server/content/{}", other.hash), b"evil".to_vec())].into(), ..Default::default() };
        assert!(download_file(&fetch, &bases[1..], &other, &store, &Progress::default(), &ok).is_err());
        assert!(!store.file(&other.hash).exists());
        assert!(!store.partial(&other.hash).exists());
        // More bytes than the manifest says: refused.
        let big = entry("x.dds", b"1234");
        let fetch = MemoryFetch { files: [(format!("http://server/content/{}", big.hash), b"123456789".to_vec())].into(), ..Default::default() };
        assert!(download_file(&fetch, &bases[1..], &big, &store, &Progress::default(), &ok).is_err());
        // A validator saying no: refused.
        let wav = entry("sound.wav", b"not a wav");
        let fetch = MemoryFetch { files: [(format!("http://server/content/{}", wav.hash), b"not a wav".to_vec())].into(), ..Default::default() };
        let no = |_: &FileEntry, _: &Path| -> Result<(), String> { Err("bad sound".into()) };
        assert!(download_file(&fetch, &bases[1..], &wav, &store, &Progress::default(), &no).is_err());
        assert!(!store.file(&wav.hash).exists());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn plans_with_local_files_and_downloads_the_rest() {
        let (dir, store) = setup("plan");
        let files: [(&str, &[u8]); 3] = [("levels/a/level.ron", b"(level a)"), ("objects/x.glb", b"mesh x"), ("objects/y.dds", b"texture y")];
        let entries: Vec<FileEntry> = files.iter().map(|(p, b)| entry(p, b)).collect();
        // We have y already (our own import), and an outdated x.
        let local = dir.join("imported");
        std::fs::create_dir_all(local.join("objects")).unwrap();
        std::fs::write(local.join("objects/y.dds"), b"texture y").unwrap();
        std::fs::write(local.join("objects/x.glb"), b"mesh X").unwrap();
        let manifest = Manifest {
            version: MANIFEST_VERSION,
            levels: vec!["a".into()],
            layers: vec![Layer { name: "m".into(), files: entries.clone(), ..Default::default() }],
            ..Default::default()
        };
        let mut index = HashIndex::load(Some(store.index_file()));
        let progress = Progress::default();
        let plan = plan(&manifest, &["a".into()], &store, std::slice::from_ref(&local), &mut index, &progress).unwrap();
        let downloads: Vec<&str> = plan.download.iter().map(|f| f.path.as_str()).collect();
        assert_eq!(downloads, ["levels/a/level.ron", "objects/x.glb"]);
        assert_eq!(plan.adopt.len(), 1);
        assert_eq!(plan.download_bytes, 15);
        let fetch = MemoryFetch {
            files: files.iter().map(|(_, b)| (format!("http://s/content/{}", hash_bytes(b)), b.to_vec())).collect(),
            ..Default::default()
        };
        adopt_all(&plan, &store, &progress).unwrap();
        download_all(&fetch, &["http://s/content".into()], &plan.download, &store, &progress, 4, &ok).unwrap();
        // Again: nothing left to download.
        let again = super::plan(&manifest, &["a".into()], &store, std::slice::from_ref(&local), &mut index, &progress).unwrap();
        assert!(again.download.is_empty() && again.adopt.is_empty());
        assert_eq!(again.needed.len(), 3);
        // A damaged stored file is noticed and fetched again.
        std::fs::write(store.file(&entries[1].hash), b"mesh Z").unwrap();
        let damaged = super::plan(&manifest, &["a".into()], &store, std::slice::from_ref(&local), &mut index, &progress).unwrap();
        assert_eq!(damaged.download.len(), 1);
        let _ = std::fs::remove_dir_all(&dir);
    }
}
