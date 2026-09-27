//! The client's content cache: files by hash, shared by every server, and per server a view
//! laid out like its layers (hard links into the store), which is mounted for the session.
//!
//! ```text
//! <cache>/
//!   content/<hash>             verified files; written only by renaming a verified download
//!   partial/<hash>             downloads in progress (resumed with a Range request)
//!   servers/<address>/<n>/...  the server's layer n (0: highest priority), hard links
//!   servers/<address>/view.txt what the view holds, to reuse it when nothing changed
//!   index.txt                  hashes of store and local files by size and time (HashIndex)
//!   used.txt                   when each stored file was last used, for the size limit
//! ```

use std::{
    collections::{HashMap, HashSet},
    io::{self, BufRead, Write},
    path::{Path, PathBuf},
    time::{Duration, SystemTime, UNIX_EPOCH},
};

use super::{ContentMode, FileEntry, Manifest, validate_component, validate_path};
use crate::{
    config::GamePaths,
    mods::{Mod, ModInfo},
};

/// A content cache folder.
#[derive(Clone, Debug)]
pub struct ContentStore {
    root: PathBuf,
}

/// A server's content, laid out for mounting.
#[derive(Clone, Debug)]
pub struct MountedContent {
    /// The server's folder name in the cache (its address).
    pub key: String,
    pub server_name: String,
    pub mode: ContentMode,
    /// Highest priority first.
    pub layers: Vec<MountedLayer>,
    /// Levels whose files are in the view.
    pub levels: Vec<String>,
    /// Every level the server shares (for a map change to one that isn't in the view).
    pub shared_levels: Vec<String>,
    pub files: usize,
    pub bytes: u64,
}

#[derive(Clone, Debug)]
pub struct MountedLayer {
    pub dir: PathBuf,
    pub name: String,
    pub title: String,
    pub description: String,
    /// The server's imported assets.
    pub imported: bool,
}

impl MountedContent {
    /// Where the game finds data while playing on the server: the server's mods, then its
    /// imported assets if it shares them, else ours. Our own mods are off: the server's
    /// content must win everywhere, or prediction and collision disagree with it.
    pub fn apply(&self, local: &GamePaths) -> GamePaths {
        GamePaths {
            imported: self
                .layers
                .iter()
                .find(|l| l.imported)
                .map_or_else(|| local.imported.clone(), |l| l.dir.clone()),
            mods: self
                .layers
                .iter()
                .filter(|l| !l.imported)
                .map(|l| Mod {
                    dir: l.dir.clone(),
                    info: ModInfo {
                        name: if l.title.is_empty() { l.name.clone() } else { l.title.clone() },
                        description: l.description.clone(),
                        enabled: true,
                        priority: 0,
                    },
                })
                .collect(),
        }
    }
}

/// A cache folder name for a server address: `127.0.0.1-16567`.
pub fn server_key(address: &str) -> String {
    let key: String = address
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() || c == '.' || c == '-' { c } else { '-' })
        .collect();
    let key = key.trim_matches(['-', '.']).to_string();
    if validate_component(&key).is_ok() { key } else { "server".into() }
}

/// `root` joined with a checked `/`-separated relative path.
fn join_checked(root: &Path, relative: &str) -> io::Result<PathBuf> {
    validate_path(relative).map_err(|err| io::Error::new(io::ErrorKind::InvalidInput, format!("{relative}: {err}")))?;
    let mut path = root.to_path_buf();
    for part in relative.split('/') {
        path.push(part);
    }
    Ok(path)
}

/// Links (or copies) every `(from, to)`. A link takes a few milliseconds on Windows (the file
/// system's filters) and a BF2 level has thousands of files: on several threads.
fn link_all(links: &[(PathBuf, PathBuf)]) -> io::Result<()> {
    let next = std::sync::atomic::AtomicUsize::new(0);
    let failed: std::sync::Mutex<Option<io::Error>> = std::sync::Mutex::new(None);
    std::thread::scope(|scope| {
        for _ in 0..8.min(links.len()) {
            scope.spawn(|| {
                while let Some((from, to)) = links.get(next.fetch_add(1, std::sync::atomic::Ordering::Relaxed)) {
                    if let Err(err) = link_or_copy(from, to) {
                        failed.lock().unwrap().get_or_insert(io::Error::new(err.kind(), format!("{}: {err}", to.display())));
                        break;
                    }
                }
            });
        }
    });
    match failed.into_inner().unwrap() {
        Some(err) => Err(err),
        None => Ok(()),
    }
}

/// Makes `to` the same file as `from`: a hard link, else a copy.
fn link_or_copy(from: &Path, to: &Path) -> io::Result<bool> {
    if std::fs::hard_link(from, to).is_ok() {
        return Ok(true);
    }
    std::fs::copy(from, to)?;
    Ok(false)
}

impl ContentStore {
    pub fn open(root: &Path) -> io::Result<Self> {
        for dir in ["content", "partial", "servers"] {
            std::fs::create_dir_all(root.join(dir))?;
        }
        Ok(Self { root: root.to_path_buf() })
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    /// A stored file. Only call with a checked hash ([`super::is_hash`]).
    pub fn file(&self, hash: &str) -> PathBuf {
        debug_assert!(super::is_hash(hash));
        self.root.join("content").join(hash)
    }

    /// Where a download in progress goes.
    pub fn partial(&self, hash: &str) -> PathBuf {
        debug_assert!(super::is_hash(hash));
        self.root.join("partial").join(hash)
    }

    /// The [`super::HashIndex`] file of the cache.
    pub fn index_file(&self) -> PathBuf {
        self.root.join("index.txt")
    }

    /// Moves a verified download into the store.
    pub fn commit(&self, hash: &str) -> io::Result<()> {
        let target = self.file(hash);
        let _ = std::fs::remove_file(&target);
        std::fs::rename(self.partial(hash), target)
    }

    /// Puts a local file known to have `entry`'s hash into the store: a hard link if the
    /// cache is on the same drive, else a copy (verified). Returns whether it was copied.
    pub fn adopt(&self, entry: &FileEntry, local: &Path) -> Result<bool, String> {
        let target = self.file(&entry.hash);
        let size = std::fs::metadata(local).map_err(|err| format!("{}: {err}", local.display()))?.len();
        if size != entry.size {
            return Err(format!("{} changed", local.display()));
        }
        if std::fs::hard_link(local, &target).is_ok() {
            return Ok(false);
        }
        let partial = self.partial(&entry.hash);
        std::fs::copy(local, &partial).map_err(|err| format!("copying {}: {err}", local.display()))?;
        let (size, hash) = super::hash_file(&partial, None, None).map_err(|err| err.to_string())?;
        if size != entry.size || hash != entry.hash {
            let _ = std::fs::remove_file(&partial);
            return Err(format!("{} changed while it was copied", local.display()));
        }
        self.commit(&entry.hash).map_err(|err| err.to_string())?;
        Ok(true)
    }

    /// Lays out `needed` (`(layer, file)`, every file in the store) as the server's layers
    /// under `servers/<key>/`, reusing the view if it already holds exactly these files.
    pub fn build_view(&self, key: &str, manifest: &Manifest, needed: &[(usize, FileEntry)], levels: &[String]) -> io::Result<MountedContent> {
        validate_component(key).map_err(|err| io::Error::new(io::ErrorKind::InvalidInput, err.to_string()))?;
        let mut listing = String::new();
        for (i, layer) in manifest.layers.iter().enumerate() {
            listing += &format!("layer\t{i}\t{}\t{}\n", layer.name, layer.imported);
        }
        let mut sorted: Vec<&(usize, FileEntry)> = needed.iter().collect();
        sorted.sort_by(|a, b| (a.0, &a.1.path).cmp(&(b.0, &b.1.path)));
        for (layer, file) in &sorted {
            listing += &format!("{layer}\t{}\t{}\n", file.path, file.hash);
        }
        let servers = self.root.join("servers");
        let mut base = servers.join(key);
        let complete = |base: &Path| {
            std::fs::read_to_string(base.join("view.txt")).is_ok_and(|old| old == listing)
                && sorted.iter().all(|(layer, file)| {
                    join_checked(&base.join(layer.to_string()), &file.path)
                        .and_then(std::fs::metadata)
                        .is_ok_and(|m| m.len() == file.size)
                })
        };
        // A map change keeps most files: update the old view if there is one.
        if !complete(&base) && !(matches!(self.update_view(&base, &listing, &sorted), Ok(true)) && complete(&base)) {
            // A folder still in use (Windows) gets a sibling.
            let mut n = 1;
            while base.exists() && std::fs::remove_dir_all(&base).is_err() {
                n += 1;
                if n > 16 {
                    return Err(io::Error::other(format!("can't clear {}", base.display())));
                }
                base = servers.join(format!("{key}-{n}"));
            }
            for i in 0..manifest.layers.len() {
                std::fs::create_dir_all(base.join(i.to_string()))?;
            }
            let mut links = Vec::with_capacity(sorted.len());
            let mut dirs = HashSet::new();
            for (layer, file) in &sorted {
                let target = join_checked(&base.join(layer.to_string()), &file.path)?;
                if let Some(dir) = target.parent()
                    && dirs.insert(dir.to_path_buf())
                {
                    std::fs::create_dir_all(dir)?;
                }
                links.push((self.file(&file.hash), target));
            }
            link_all(&links)?;
            std::fs::write(base.join("view.txt"), &listing)?;
        }
        self.touch(sorted.iter().map(|(_, f)| f.hash.as_str()));
        Ok(MountedContent {
            key: key.to_string(),
            server_name: manifest.server_name.clone(),
            mode: manifest.mode,
            layers: manifest
                .layers
                .iter()
                .enumerate()
                .map(|(i, layer)| MountedLayer {
                    dir: base.join(i.to_string()),
                    name: layer.name.clone(),
                    title: layer.title.clone(),
                    description: layer.description.clone(),
                    imported: layer.imported,
                })
                .collect(),
            levels: levels.to_vec(),
            shared_levels: manifest.levels.clone(),
            files: needed.len(),
            bytes: needed.iter().map(|(_, f)| f.size).sum(),
        })
    }

    /// Brings the view in `base` to `listing` by removing and linking only the files that
    /// changed. `Ok(false)`: there is no old view with the same layers.
    fn update_view(&self, base: &Path, listing: &str, sorted: &[&(usize, FileEntry)]) -> io::Result<bool> {
        let Ok(old) = std::fs::read_to_string(base.join("view.txt")) else {
            return Ok(false);
        };
        let header = |text: &str| -> Vec<String> { text.lines().filter(|l| l.starts_with("layer\t")).map(str::to_string).collect() };
        if header(&old) != header(listing) {
            return Ok(false);
        }
        let entries = |text: &str| -> HashMap<(usize, String), String> {
            text.lines()
                .filter(|l| !l.starts_with("layer\t"))
                .filter_map(|line| {
                    let mut parts = line.splitn(3, '\t');
                    Some(((parts.next()?.parse().ok()?, parts.next()?.to_string()), parts.next()?.to_string()))
                })
                .collect()
        };
        let (old, new) = (entries(&old), entries(listing));
        // Not reused until it is consistent again.
        std::fs::remove_file(base.join("view.txt"))?;
        for ((layer, path), hash) in &old {
            if new.get(&(*layer, path.clone())) != Some(hash) {
                match std::fs::remove_file(join_checked(&base.join(layer.to_string()), path)?) {
                    Err(err) if err.kind() != io::ErrorKind::NotFound => return Err(err),
                    _ => {}
                }
            }
        }
        let mut links = Vec::new();
        let mut dirs = HashSet::new();
        for (layer, file) in sorted {
            if old.get(&(*layer, file.path.clone())) == Some(&file.hash) {
                continue;
            }
            let target = join_checked(&base.join(layer.to_string()), &file.path)?;
            if let Some(dir) = target.parent()
                && dirs.insert(dir.to_path_buf())
            {
                std::fs::create_dir_all(dir)?;
            }
            links.push((self.file(&file.hash), target));
        }
        link_all(&links)?;
        std::fs::write(base.join("view.txt"), listing)?;
        Ok(true)
    }

    fn used_file(&self) -> PathBuf {
        self.root.join("used.txt")
    }

    fn load_used(&self) -> HashMap<String, u64> {
        let Ok(file) = std::fs::File::open(self.used_file()) else {
            return HashMap::new();
        };
        io::BufReader::new(file)
            .lines()
            .map_while(Result::ok)
            .filter_map(|line| {
                let (hash, secs) = line.split_once('\t')?;
                Some((hash.to_string(), secs.parse().ok()?))
            })
            .collect()
    }

    /// Records that these stored files were used now.
    pub fn touch<'a>(&self, hashes: impl Iterator<Item = &'a str>) {
        let now = SystemTime::now().duration_since(UNIX_EPOCH).map_or(0, |d| d.as_secs());
        let mut used = self.load_used();
        for hash in hashes {
            used.insert(hash.to_string(), now);
        }
        let _ = self.save_used(&used);
    }

    fn save_used(&self, used: &HashMap<String, u64>) -> io::Result<()> {
        let temp = self.used_file().with_extension(format!("tmp{}", std::process::id()));
        let mut out = io::BufWriter::new(std::fs::File::create(&temp)?);
        for (hash, secs) in used {
            writeln!(out, "{hash}\t{secs}")?;
        }
        out.flush()?;
        drop(out);
        std::fs::rename(temp, self.used_file())
    }

    /// Bytes of stored files.
    pub fn size(&self) -> u64 {
        std::fs::read_dir(self.root.join("content"))
            .into_iter()
            .flatten()
            .flatten()
            .filter_map(|e| e.metadata().ok())
            .filter(|m| m.is_file())
            .map(|m| m.len())
            .sum()
    }

    /// Keeps the store under `limit` bytes by deleting the files used longest ago, except
    /// `keep` (the files of the view in use, `current`). Other servers' views go first,
    /// since their links keep deleted files on disk. Downloads left unfinished for a week
    /// go too. Returns the files and bytes deleted.
    pub fn cleanup(&self, limit: u64, keep: &HashSet<String>, current: Option<&Path>) -> (usize, u64) {
        let week = Duration::from_secs(7 * 24 * 3600);
        for entry in std::fs::read_dir(self.root.join("partial")).into_iter().flatten().flatten() {
            let old = entry
                .metadata()
                .and_then(|m| m.modified())
                .is_ok_and(|t| t.elapsed().is_ok_and(|age| age > week));
            if old {
                let _ = std::fs::remove_file(entry.path());
            }
        }
        let mut files: Vec<(String, u64, u64)> = Vec::new();
        let used = self.load_used();
        for entry in std::fs::read_dir(self.root.join("content")).into_iter().flatten().flatten() {
            let Ok(meta) = entry.metadata() else {
                continue;
            };
            let name = entry.file_name().to_string_lossy().into_owned();
            let modified = meta
                .modified()
                .ok()
                .and_then(|t| t.duration_since(UNIX_EPOCH).ok())
                .map_or(0, |d| d.as_secs());
            let last = used.get(&name).copied().unwrap_or(modified);
            files.push((name, meta.len(), last));
        }
        let mut total: u64 = files.iter().map(|f| f.1).sum();
        if total <= limit {
            return (0, 0);
        }
        let current = current.and_then(|c| c.parent().map(Path::to_path_buf));
        for entry in std::fs::read_dir(self.root.join("servers")).into_iter().flatten().flatten() {
            if Some(entry.path()) != current {
                let _ = std::fs::remove_dir_all(entry.path());
            }
        }
        files.sort_by_key(|f| f.2);
        let (mut count, mut freed) = (0, 0);
        for (hash, size, _) in files {
            if total <= limit {
                break;
            }
            if keep.contains(&hash) || !super::is_hash(&hash) {
                continue;
            }
            if std::fs::remove_file(self.root.join("content").join(&hash)).is_ok() {
                total -= size;
                freed += size;
                count += 1;
            }
        }
        (count, freed)
    }
}

#[cfg(test)]
mod tests {
    use super::super::{Layer, MANIFEST_VERSION, hash_bytes};
    use super::*;

    #[test]
    fn keys() {
        assert_eq!(server_key("127.0.0.1:27800"), "127.0.0.1-27800");
        assert_eq!(server_key("[::1]:16567"), "1--16567");
        assert_eq!(server_key(":::"), "server");
    }

    #[test]
    fn views_and_cleanup() {
        let dir = std::env::temp_dir().join(format!("bf2_content_store_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let store = ContentStore::open(&dir.join("cache")).unwrap();
        let data: [&[u8]; 3] = [b"(name: \"x\")", b"mesh", b"imported texture"];
        let entry = |path: &str, bytes: &[u8]| FileEntry { path: path.into(), size: bytes.len() as u64, hash: hash_bytes(bytes), levels: vec![] };
        let entries = [entry("levels/x/level.ron", data[0]), entry("objects/x.glb", data[1]), entry("objects/t.dds", data[2])];
        // One stored by download, one adopted from a local file.
        std::fs::write(store.partial(&entries[0].hash), data[0]).unwrap();
        store.commit(&entries[0].hash).unwrap();
        std::fs::write(store.partial(&entries[1].hash), data[1]).unwrap();
        store.commit(&entries[1].hash).unwrap();
        let local = dir.join("local.dds");
        std::fs::write(&local, data[2]).unwrap();
        store.adopt(&entries[2], &local).unwrap();
        assert!(store.adopt(&FileEntry { size: 3, hash: hash_bytes(b"abc"), ..entries[2].clone() }, &local).is_err());
        let manifest = Manifest {
            version: MANIFEST_VERSION,
            server_name: "test".into(),
            layers: vec![
                Layer { name: "sample".into(), title: "Sample".into(), files: entries[..2].to_vec(), ..Default::default() },
                Layer { name: "imported".into(), imported: true, files: entries[2..].to_vec(), ..Default::default() },
            ],
            ..Default::default()
        };
        let needed = vec![(0, entries[0].clone()), (0, entries[1].clone()), (1, entries[2].clone())];
        let mounted = store.build_view("127.0.0.1-1", &manifest, &needed, &["x".into()]).unwrap();
        assert_eq!(std::fs::read(mounted.layers[0].dir.join("levels/x/level.ron")).unwrap(), data[0]);
        assert_eq!(std::fs::read(mounted.layers[1].dir.join("objects/t.dds")).unwrap(), data[2]);
        // Mounted: the server's mod over its imported folder.
        let local_paths = GamePaths { imported: dir.join("imported"), mods: vec![] };
        let paths = mounted.apply(&local_paths);
        assert_eq!(paths.imported, mounted.layers[1].dir);
        assert_eq!(paths.mods.len(), 1);
        assert_eq!(paths.mods[0].info.name, "Sample");
        assert_eq!(paths.find("objects/t.dds"), mounted.layers[1].dir.join("objects/t.dds"));
        // The same again: reused.
        let again = store.build_view("127.0.0.1-1", &manifest, &needed, &["x".into()]).unwrap();
        assert_eq!(again.layers[0].dir, mounted.layers[0].dir);
        // Another level: updated in place, the file no longer needed gone.
        let fewer = vec![(0, entries[0].clone()), (1, entries[2].clone())];
        let updated = store.build_view("127.0.0.1-1", &manifest, &fewer, &["x".into()]).unwrap();
        assert_eq!(updated.layers[0].dir, mounted.layers[0].dir);
        assert!(!updated.layers[0].dir.join("objects/x.glb").exists());
        assert!(updated.layers[1].dir.join("objects/t.dds").exists());
        let back = store.build_view("127.0.0.1-1", &manifest, &needed, &["x".into()]).unwrap();
        assert_eq!(std::fs::read(back.layers[0].dir.join("objects/x.glb")).unwrap(), data[1]);
        // Over the limit: the least recently used file goes, the kept ones stay.
        let keep: HashSet<String> = [entries[0].hash.clone()].into();
        let (count, _) = store.cleanup(data[0].len() as u64, &keep, None);
        assert_eq!(count, 2);
        assert!(store.file(&entries[0].hash).exists());
        assert!(!store.file(&entries[1].hash).exists());
        // A path leaving the view is refused.
        let evil = vec![(0, FileEntry { path: "../evil.ron".into(), ..entries[0].clone() })];
        assert!(store.build_view("evil", &manifest, &evil, &[]).is_err());
        let _ = std::fs::remove_dir_all(&dir);
    }
}
