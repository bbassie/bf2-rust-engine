//! What the server checks when a client joins or the map changes (see [`crate::join`]).
//!
//! **The required files of a level** are the files of the server's manifest the level needs
//! (its own and the common ones), one per path: where several layers have a path, the one
//! the game uses (the highest priority layer). What that covers follows from what the server
//! shares:
//!
//! - `Off`: no manifest, nothing is checked; clients play with their own content.
//! - `Mods`: the mods' files. The imported BF2 assets aren't shared, so they aren't checked:
//!   every client uses its own import.
//! - `All`: the mods' and the imported files.
//!
//! **The client's report** hashes, for each required path, the file its game would load
//! ([`GamePaths::find`] with the server's content mounted), and sends a [`digest`] of the
//! list; the per-file hashes only if the server asks. The server compares both with its
//! manifest. This catches outdated, damaged and locally changed files. It doesn't stop a
//! modified client, which can report whatever it likes.

use std::{
    collections::HashSet,
    path::PathBuf,
    sync::atomic::AtomicU64,
};

use super::{FileEntry, HashIndex, Manifest, is_hash};
use crate::config::GamePaths;

/// Hash of a missing file in a report.
pub const MISSING: [u8; 32] = [0; 32];
/// Reports listing more files than this are refused (a BF2 level with everything shared has
/// a few thousand).
pub const MAX_REQUIRED: usize = 100_000;
/// Files named in one [`crate::join::JoinVerdict::Fetch`].
pub const MAX_FETCH_LISTED: usize = 4_000;

impl Manifest {
    /// Identifies the shared files, whatever the server currently plays: the layers, their
    /// files and the levels. Both sides compute it from the manifest.
    pub fn content_id(&self) -> String {
        let mut hasher = blake3::Hasher::new();
        hasher.update(b"BF2R-MANIFEST-1\n");
        hasher.update(format!("{}\n{}\n", self.version, self.mode.label()).as_bytes());
        for level in &self.levels {
            hasher.update(level.as_bytes());
            hasher.update(b"\n");
        }
        for layer in &self.layers {
            hasher.update(format!("layer\t{}\t{}\n", layer.name, layer.imported).as_bytes());
            for file in &layer.files {
                hasher.update(format!("{}\t{}\t{}\t{:?}\n", file.path, file.size, file.hash, file.levels).as_bytes());
            }
        }
        hasher.finalize().to_hex().to_string()
    }

    /// The files `level` requires, one per path (the highest priority layer's), sorted by
    /// path.
    pub fn required(&self, level: &str) -> Vec<&FileEntry> {
        let mut seen = HashSet::new();
        let mut files: Vec<&FileEntry> = self
            .needed(&[level.to_string()])
            .filter(|(_, f)| seen.insert(f.path.to_ascii_lowercase()))
            .map(|(_, f)| f)
            .collect();
        files.sort_by(|a, b| a.path.cmp(&b.path));
        files
    }

    /// The layer the game would take `path` from (the first that has it).
    pub fn layer_of(&self, path: &str) -> Option<usize> {
        self.layers
            .iter()
            .position(|layer| layer.files.iter().any(|f| f.path.eq_ignore_ascii_case(path)))
    }
}

/// BLAKE3 over `path TAB hash` lines, in order: what a report's digest is.
pub fn digest<'a>(entries: impl IntoIterator<Item = (&'a str, &'a str)>) -> String {
    let mut hasher = blake3::Hasher::new();
    hasher.update(b"BF2R-CONTENT-1\n");
    for (path, hash) in entries {
        hasher.update(path.as_bytes());
        hasher.update(b"\t");
        hasher.update(hash.as_bytes());
        hasher.update(b"\n");
    }
    hasher.finalize().to_hex().to_string()
}

/// The digest the server expects: its own hashes.
pub fn expected_digest(required: &[&FileEntry]) -> String {
    digest(required.iter().map(|f| (f.path.as_str(), f.hash.as_str())))
}

/// 64 hex digits as bytes.
pub fn hash_bytes_of(hash: &str) -> [u8; 32] {
    if !is_hash(hash) {
        return MISSING;
    }
    game_auth::unhex_array(hash).unwrap_or(MISSING)
}

/// Bytes as 64 hex digits.
pub fn hash_hex(bytes: &[u8; 32]) -> String {
    game_auth::hex(bytes)
}

/// The digest of a client's per-file hashes for `required`.
pub fn report_digest(required: &[&FileEntry], hashes: &[[u8; 32]]) -> String {
    let hexes: Vec<String> = hashes.iter().map(hash_hex).collect();
    digest(required.iter().zip(&hexes).map(|(f, h)| (f.path.as_str(), h.as_str())))
}

/// The required files a client's hashes don't match. `None` if the list has the wrong length.
pub fn mismatches<'a>(required: &[&'a FileEntry], hashes: &[[u8; 32]]) -> Option<Vec<&'a FileEntry>> {
    if hashes.len() != required.len() {
        return None;
    }
    Some(
        required
            .iter()
            .zip(hashes)
            .filter(|(f, h)| hash_bytes_of(&f.hash) != **h)
            .map(|(f, _)| *f)
            .collect(),
    )
}

/// The client's side: hashes the files its game would load for `required` (through
/// `paths`, the server's content mounted), remembered in `index`. Returns them with their
/// digest.
pub fn local_hashes(required: &[&FileEntry], paths: &GamePaths, index: &mut HashIndex) -> (Vec<[u8; 32]>, String) {
    let files: Vec<PathBuf> = required.iter().map(|f| paths.find(&f.path)).collect();
    let progress = AtomicU64::new(0);
    let hashed = index.hash_all(&files, &progress, None);
    index.save();
    let hashes: Vec<[u8; 32]> = hashed
        .iter()
        .map(|h| h.as_ref().map_or(MISSING, |(_, hash)| hash_bytes_of(hash)))
        .collect();
    let digest = report_digest(required, &hashes);
    (hashes, digest)
}

#[cfg(test)]
mod tests {
    use super::super::{ContentMode, Layer, MANIFEST_VERSION, hash_bytes};
    use super::*;

    fn entry(path: &str, bytes: &[u8], levels: Vec<u32>) -> FileEntry {
        FileEntry { path: path.into(), size: bytes.len() as u64, hash: hash_bytes(bytes), levels }
    }

    fn manifest() -> Manifest {
        Manifest {
            version: MANIFEST_VERSION,
            mode: ContentMode::All,
            levels: vec!["a".into(), "b".into()],
            playing: vec!["a".into()],
            layers: vec![
                Layer {
                    name: "mod".into(),
                    files: vec![entry("templates/crate.ron", b"mod crate", vec![]), entry("levels/a/level.ron", b"level a", vec![0])],
                    ..Default::default()
                },
                Layer {
                    name: "imported".into(),
                    imported: true,
                    files: vec![
                        entry("templates/crate.ron", b"bf2 crate", vec![]),
                        entry("levels/b/level.ron", b"level b", vec![1]),
                        entry("objects/x.glb", b"mesh", vec![0, 1]),
                    ],
                    ..Default::default()
                },
            ],
            ..Default::default()
        }
    }

    #[test]
    fn required_files_and_ids() {
        let m = manifest();
        let a: Vec<(&str, &str)> = m.required("a").iter().map(|f| (f.path.as_str(), f.hash.as_str())).collect();
        // One per path, the mod's crate over the imported one, sorted.
        let crate_hash = hash_bytes(b"mod crate");
        assert_eq!(a.iter().map(|(p, _)| *p).collect::<Vec<_>>(), ["levels/a/level.ron", "objects/x.glb", "templates/crate.ron"]);
        assert_eq!(a[2].1, crate_hash);
        assert_eq!(m.required("b").len(), 3);
        assert_eq!(m.required("nowhere").len(), 1);
        assert_eq!(m.layer_of("templates/crate.ron"), Some(0));
        assert_eq!(m.layer_of("levels/b/level.ron"), Some(1));
        // The id ignores what the server plays now, not what it shares.
        let mut other = m.clone();
        other.playing = vec!["b".into()];
        other.server_name = "renamed".into();
        assert_eq!(other.content_id(), m.content_id());
        other.layers[0].files[0].hash = hash_bytes(b"changed");
        assert_ne!(other.content_id(), m.content_id());
    }

    #[test]
    fn reports_compare() {
        let dir = std::env::temp_dir().join(format!("bf2_content_verify_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let m = manifest();
        let required = m.required("a");
        // A client with the mod mounted over the server's imported folder.
        let (mod_dir, imported) = (dir.join("mod"), dir.join("imported"));
        for (root, path, bytes) in [
            (&mod_dir, "templates/crate.ron", b"mod crate".as_slice()),
            (&mod_dir, "levels/a/level.ron", b"level a"),
            (&imported, "templates/crate.ron", b"bf2 crate"),
            (&imported, "objects/x.glb", b"mesh"),
        ] {
            let file = root.join(path);
            std::fs::create_dir_all(file.parent().unwrap()).unwrap();
            std::fs::write(file, bytes).unwrap();
        }
        let paths = GamePaths {
            imported: imported.clone(),
            mods: vec![crate::mods::Mod { dir: mod_dir.clone(), info: Default::default() }],
        };
        let mut index = HashIndex::load(Some(dir.join("index.txt")));
        let (hashes, digest) = local_hashes(&required, &paths, &mut index);
        assert_eq!(digest, expected_digest(&required));
        assert_eq!(mismatches(&required, &hashes).unwrap().len(), 0);
        // A changed file: the digest differs and the per-file list names it.
        std::fs::write(imported.join("objects/x.glb"), b"MESH").unwrap();
        let (hashes, digest) = local_hashes(&required, &paths, &mut index);
        assert_ne!(digest, expected_digest(&required));
        let wrong: Vec<&str> = mismatches(&required, &hashes).unwrap().iter().map(|f| f.path.as_str()).collect();
        assert_eq!(wrong, ["objects/x.glb"]);
        // A missing file counts as a mismatch too; a list of the wrong length is refused.
        std::fs::remove_file(mod_dir.join("levels/a/level.ron")).unwrap();
        let (hashes, _) = local_hashes(&required, &paths, &mut index);
        assert_eq!(hashes[0], MISSING);
        assert_eq!(mismatches(&required, &hashes).unwrap().len(), 2);
        assert!(mismatches(&required, &hashes[1..]).is_none());
        let _ = std::fs::remove_dir_all(&dir);
    }
}
