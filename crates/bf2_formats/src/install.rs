//! Locating things in a Battlefield 2 installation and mounting them like the game does.

use std::path::{Path, PathBuf};

use crate::vfs::{Vfs, VfsError, decode_text, normalize};

#[derive(Debug, thiserror::Error)]
pub enum InstallError {
    #[error("{0} does not look like a Battlefield 2 installation (missing mods/bf2)")]
    NotAnInstall(PathBuf),
    #[error("mod `{0}` not found")]
    UnknownMod(String),
    #[error("level `{0}` not found")]
    UnknownLevel(String),
    #[error(transparent)]
    Vfs(#[from] VfsError),
    #[error("i/o error on {path}: {source}")]
    Io {
        path: PathBuf,
        source: std::io::Error,
    },
}

/// Which archive set to mount. The server set lacks meshes, textures and sounds.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Side {
    Client,
    Server,
    /// Client archives first, then server archives for anything only the server has.
    Both,
}

#[derive(Clone, Debug)]
pub struct Bf2Install {
    root: PathBuf,
}

#[derive(Clone, Debug)]
pub struct LevelInfo {
    /// Folder name as on disk, e.g. `Strike_at_Karkand`.
    pub name: String,
    /// Mod folder it was found in.
    pub mod_name: String,
    pub dir: PathBuf,
}

impl Bf2Install {
    /// Opens an installation folder, e.g. `C:\Program Files (x86)\EA Games\Battlefield 2`.
    pub fn open(root: impl Into<PathBuf>) -> Result<Self, InstallError> {
        let root = root.into();
        if !root.join("mods").join("bf2").is_dir() {
            return Err(InstallError::NotAnInstall(root));
        }
        Ok(Self { root })
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    pub fn mod_dir(&self, mod_name: &str) -> PathBuf {
        self.root.join("mods").join(mod_name)
    }

    /// Installed mod folder names (e.g. `bf2`, `xpack`).
    pub fn mods(&self) -> Vec<String> {
        let mut mods: Vec<String> = std::fs::read_dir(self.root.join("mods"))
            .into_iter()
            .flatten()
            .flatten()
            .filter(|e| e.path().join("Mod.desc").is_file() || e.path().join("mod.desc").is_file())
            .map(|e| e.file_name().to_string_lossy().into_owned())
            .collect();
        mods.sort();
        mods
    }

    /// Levels shipped in a mod's `Levels` folder.
    pub fn levels(&self, mod_name: &str) -> Vec<LevelInfo> {
        let dir = self.mod_dir(mod_name).join("Levels");
        let mut levels: Vec<LevelInfo> = std::fs::read_dir(&dir)
            .into_iter()
            .flatten()
            .flatten()
            .filter(|e| e.path().join("server.zip").is_file())
            .map(|e| LevelInfo {
                name: e.file_name().to_string_lossy().into_owned(),
                mod_name: mod_name.to_string(),
                dir: e.path(),
            })
            .collect();
        levels.sort_by_key(|l| l.name.to_lowercase());
        levels
    }

    /// Finds a level by case-insensitive name across all mods.
    pub fn find_level(&self, name: &str) -> Result<LevelInfo, InstallError> {
        self.mods()
            .iter()
            .flat_map(|m| self.levels(m))
            .find(|l| l.name.eq_ignore_ascii_case(name))
            .ok_or_else(|| InstallError::UnknownLevel(name.to_string()))
    }

    /// Mounts a mod's archives as listed in its `ClientArchives.con` / `ServerArchives.con`,
    /// including archives of parent mods it references (xpack mounts bf2's), then the mod's
    /// loose files (AI, python, settings, level info).
    pub fn mount_mod(&self, vfs: &mut Vfs, mod_name: &str, side: Side) -> Result<(), InstallError> {
        let mod_dir = self.mod_dir(mod_name);
        if !mod_dir.is_dir() {
            return Err(InstallError::UnknownMod(mod_name.to_string()));
        }
        let lists: &[&str] = match side {
            Side::Client => &["ClientArchives.con"],
            Side::Server => &["ServerArchives.con"],
            Side::Both => &["ClientArchives.con", "ServerArchives.con"],
        };
        for list in lists {
            let path = mod_dir.join(list);
            let text = std::fs::read(&path).map_err(|source| InstallError::Io {
                path: path.clone(),
                source,
            })?;
            for (archive, mount_point) in parse_archive_list(&decode_text(&text)) {
                // Paths starting with `mods/` are relative to the install root.
                let zip_path = if normalize(&archive).starts_with("mods/") {
                    self.root.join(&archive)
                } else {
                    mod_dir.join(&archive)
                };
                if zip_path.is_file() {
                    vfs.mount_archive(&zip_path, &mount_point)?;
                } else {
                    log::warn!("archive listed in {list} is missing: {}", zip_path.display());
                }
            }
        }
        vfs.mount_dir(&mod_dir, "")?;
        Ok(())
    }

    /// Mounts a level's `client.zip` / `server.zip` at `levels/<name>/` and overlays them on
    /// the root (booster levels ship `objects/...` files referenced by root-relative paths).
    ///
    /// Mount the level *before* its mod so level files take priority; [`Self::level_vfs`]
    /// does everything in the right order.
    pub fn mount_level(&self, vfs: &mut Vfs, level: &LevelInfo, side: Side) -> Result<(), InstallError> {
        let mount_point = format!("levels/{}", level.name);
        let zips: &[&str] = match side {
            Side::Client => &["client.zip"],
            Side::Server => &["server.zip"],
            Side::Both => &["client.zip", "server.zip"],
        };
        for zip in zips {
            let path = level.dir.join(zip);
            if path.is_file() {
                vfs.mount_archive(&path, &mount_point)?;
            }
        }
        for zip in zips {
            let path = level.dir.join(zip);
            if path.is_file() {
                vfs.mount_archive(&path, "")?;
            }
        }
        Ok(())
    }

    /// A file system with everything needed to load `level`: the level's own archives,
    /// then its mod (and the mods that one mounts).
    pub fn level_vfs(&self, level: &LevelInfo, side: Side) -> Result<Vfs, InstallError> {
        let mut vfs = Vfs::new();
        self.mount_level(&mut vfs, level, side)?;
        self.mount_mod(&mut vfs, &level.mod_name, side)?;
        Ok(vfs)
    }
}

/// Parses `fileManager.mountArchive <archive> <mount point>` lines.
pub fn parse_archive_list(text: &str) -> Vec<(String, String)> {
    text.lines()
        .filter_map(|line| {
            let mut parts = line.split_whitespace();
            let command = parts.next()?;
            if !command.eq_ignore_ascii_case("fileManager.mountArchive") {
                return None;
            }
            Some((parts.next()?.to_string(), parts.next().unwrap_or("").to_string()))
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_archive_lists() {
        let text = "fileManager.mountArchive Objects_client.zip Objects\r\n\
                    rem comment\r\n\
                    fileManager.mountArchive mods/bf2/Common_client.zip Common\r\n";
        assert_eq!(
            parse_archive_list(text),
            vec![
                ("Objects_client.zip".into(), "Objects".into()),
                ("mods/bf2/Common_client.zip".into(), "Common".into())
            ]
        );
    }
}
