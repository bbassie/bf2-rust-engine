//! BF2's virtual file system.
//!
//! BF2 mounts zip archives at virtual paths (`fileManager.mountArchive Objects_client.zip
//! Objects`). Lookups are case-insensitive and the *first* mount providing a path wins, which
//! is how a mod like `xpack` overrides files of its parent mod `bf2`.

use std::{
    collections::HashMap,
    fs::File,
    io::{BufReader, Read},
    path::{Path, PathBuf},
    sync::{Arc, Mutex},
};

use zip::ZipArchive;

#[derive(Debug, thiserror::Error)]
pub enum VfsError {
    #[error("file not found in virtual file system: {0}")]
    NotFound(String),
    #[error("i/o error on {path}: {source}")]
    Io {
        path: PathBuf,
        source: std::io::Error,
    },
    #[error("zip error in {path}: {source}")]
    Zip {
        path: PathBuf,
        source: zip::result::ZipError,
    },
}

type Archive = Arc<Mutex<ZipArchive<BufReader<File>>>>;

#[derive(Clone)]
enum Source {
    Zip {
        archive: Archive,
        archive_path: Arc<PathBuf>,
        index: usize,
    },
    Loose(PathBuf),
}

/// Case-insensitive, first-mount-wins file system over zips and folders.
#[derive(Default, Clone)]
pub struct Vfs {
    files: HashMap<String, Source>,
    /// Original-case path for every normalized path, for nicer listings.
    display: HashMap<String, String>,
}

/// Lowercases, converts `\` to `/`, strips leading `/`, and resolves `.` and `..`.
pub fn normalize(path: &str) -> String {
    let lowered = path.trim().replace('\\', "/").to_ascii_lowercase();
    let mut parts: Vec<&str> = Vec::new();
    for part in lowered.split('/') {
        match part {
            "" | "." => {}
            ".." => {
                parts.pop();
            }
            part => parts.push(part),
        }
    }
    parts.join("/")
}

fn join(mount_point: &str, path: &str) -> String {
    let mount_point = normalize(mount_point);
    let path = normalize(path);
    if mount_point.is_empty() {
        path
    } else {
        format!("{mount_point}/{path}")
    }
}

impl Vfs {
    pub fn new() -> Self {
        Self::default()
    }

    /// Mounts every file of a zip under `mount_point`. Paths already provided by an
    /// earlier mount keep their earlier source.
    pub fn mount_archive(&mut self, zip_path: &Path, mount_point: &str) -> Result<usize, VfsError> {
        let file = File::open(zip_path).map_err(|source| VfsError::Io {
            path: zip_path.to_owned(),
            source,
        })?;
        let archive = ZipArchive::new(BufReader::new(file)).map_err(|source| VfsError::Zip {
            path: zip_path.to_owned(),
            source,
        })?;
        let names: Vec<(usize, String)> = (0..archive.len())
            .filter_map(|i| archive.name_for_index(i).map(|n| (i, n.to_string())))
            .filter(|(_, n)| !n.ends_with('/'))
            .collect();
        let archive = Arc::new(Mutex::new(archive));
        let archive_path = Arc::new(zip_path.to_owned());
        let mut added = 0;
        for (index, name) in names {
            let virtual_path = join(mount_point, &name);
            if !self.files.contains_key(&virtual_path) {
                self.display.insert(virtual_path.clone(), format!("{mount_point}/{name}"));
                self.files.insert(
                    virtual_path,
                    Source::Zip {
                        archive: archive.clone(),
                        archive_path: archive_path.clone(),
                        index,
                    },
                );
                added += 1;
            }
        }
        Ok(added)
    }

    /// Mounts the loose files of a folder (recursively) under `mount_point`.
    pub fn mount_dir(&mut self, dir: &Path, mount_point: &str) -> Result<usize, VfsError> {
        let mut added = 0;
        let mut stack = vec![dir.to_owned()];
        while let Some(current) = stack.pop() {
            let entries = std::fs::read_dir(&current).map_err(|source| VfsError::Io {
                path: current.clone(),
                source,
            })?;
            for entry in entries.flatten() {
                let path = entry.path();
                if path.is_dir() {
                    stack.push(path);
                    continue;
                }
                let Ok(relative) = path.strip_prefix(dir) else {
                    continue;
                };
                let relative = relative.to_string_lossy();
                let virtual_path = join(mount_point, &relative);
                if !self.files.contains_key(&virtual_path) {
                    self.display
                        .insert(virtual_path.clone(), format!("{mount_point}/{relative}"));
                    self.files.insert(virtual_path, Source::Loose(path));
                    added += 1;
                }
            }
        }
        Ok(added)
    }

    pub fn exists(&self, path: &str) -> bool {
        self.files.contains_key(&normalize(path))
    }

    pub fn len(&self) -> usize {
        self.files.len()
    }

    pub fn is_empty(&self) -> bool {
        self.files.is_empty()
    }

    /// Reads a whole file.
    pub fn read(&self, path: &str) -> Result<Vec<u8>, VfsError> {
        let key = normalize(path);
        match self.files.get(&key) {
            None => Err(VfsError::NotFound(path.to_string())),
            Some(Source::Loose(file)) => std::fs::read(file).map_err(|source| VfsError::Io {
                path: file.clone(),
                source,
            }),
            Some(Source::Zip {
                archive,
                archive_path,
                index,
            }) => {
                let zip_err = |source| VfsError::Zip {
                    path: archive_path.as_ref().clone(),
                    source,
                };
                let mut archive = archive.lock().unwrap_or_else(|e| e.into_inner());
                let mut entry = archive.by_index(*index).map_err(zip_err)?;
                let mut data = Vec::with_capacity(entry.size() as usize);
                entry.read_to_end(&mut data).map_err(|source| VfsError::Io {
                    path: archive_path.join(&key),
                    source,
                })?;
                Ok(data)
            }
        }
    }

    /// Reads a text file. BF2 scripts are Windows-1252; bytes above 0x7F are mapped as Latin-1.
    pub fn read_text(&self, path: &str) -> Result<String, VfsError> {
        Ok(decode_text(&self.read(path)?))
    }

    /// All normalized paths starting with `prefix` (normalized).
    pub fn list<'a>(&'a self, prefix: &str) -> impl Iterator<Item = &'a str> + 'a {
        let prefix = normalize(prefix);
        self.files
            .keys()
            .filter(move |k| prefix.is_empty() || k.starts_with(&prefix))
            .map(String::as_str)
    }

    /// The path with its original capitalization, as found in the archive.
    pub fn display_path<'a>(&'a self, path: &'a str) -> &'a str {
        self.display
            .get(&normalize(path))
            .map(String::as_str)
            .unwrap_or(path)
    }
}

/// Decodes BF2 text (Windows-1252-ish) without failing on odd bytes.
pub fn decode_text(bytes: &[u8]) -> String {
    let bytes = bytes.strip_prefix(b"\xEF\xBB\xBF").unwrap_or(bytes);
    match std::str::from_utf8(bytes) {
        Ok(s) => s.to_string(),
        Err(_) => bytes.iter().map(|&b| b as char).collect(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn normalizes_paths() {
        assert_eq!(normalize("Objects\\Vehicles//Land\\"), "objects/vehicles/land");
        assert_eq!(normalize("./Levels/Foo"), "levels/foo");
        assert_eq!(normalize("/objects/a/../b/./c.con"), "objects/b/c.con");
        assert_eq!(join("Objects", "Common/X.con"), "objects/common/x.con");
        assert_eq!(join("", "A/B"), "a/b");
    }
}
