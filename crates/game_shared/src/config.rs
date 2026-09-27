//! Filesystem locations: the imported assets and the mods on top of them.

use std::path::{Path, PathBuf};

use bevy::prelude::*;
use serde::{Serialize, de::DeserializeOwned};

use crate::mods::{self, Mod};

/// Where the game finds data on disk.
#[derive(Resource, Clone, Debug)]
pub struct GamePaths {
    /// Root of the assets converted by `bf2-import` (`levels/`, `objects/`, ...).
    pub imported: PathBuf,
    /// Enabled mods, highest priority first. Their files add to or replace the imported
    /// ones (see [`mods`]).
    pub mods: Vec<Mod>,
}

impl GamePaths {
    /// Environment variable that overrides the imported assets folder.
    pub const IMPORTED_ENV: &str = "GAME_IMPORTED_DIR";
    /// Environment variable that overrides the mods folder.
    pub const MODS_ENV: &str = "GAME_MODS_DIR";

    /// Resolves paths from an optional CLI override, then the environment, then the default
    /// (`imported/` and `mods/` in the working directory).
    pub fn resolve(cli_imported: Option<PathBuf>) -> Self {
        Self::resolve_with_mods(cli_imported, None)
    }

    /// Like [`Self::resolve`], with an optional CLI override of the mods folder.
    pub fn resolve_with_mods(cli_imported: Option<PathBuf>, cli_mods: Option<PathBuf>) -> Self {
        let absolute = |path: PathBuf| std::path::absolute(&path).unwrap_or(path);
        let imported = cli_imported
            .or_else(|| std::env::var_os(Self::IMPORTED_ENV).map(PathBuf::from))
            .unwrap_or_else(|| PathBuf::from("imported"));
        let mods_dir = cli_mods
            .or_else(|| std::env::var_os(Self::MODS_ENV).map(PathBuf::from))
            .unwrap_or_else(|| PathBuf::from("mods"));
        // Absolute, so asset sources don't resolve them relative to the executable.
        Self {
            imported: absolute(imported),
            mods: mods::discover(&absolute(mods_dir)),
        }
    }

    /// Where files are looked for: the mods, highest priority first, then `imported`.
    pub fn roots(&self) -> Vec<&Path> {
        self.mods.iter().map(|m| m.dir.as_path()).chain([self.imported.as_path()]).collect()
    }

    /// A file relative to the imported root, from the first mod that has it, else from
    /// `imported`.
    pub fn find(&self, relative: impl AsRef<Path>) -> PathBuf {
        let relative = relative.as_ref();
        self.mods
            .iter()
            .map(|m| m.dir.join(relative))
            .find(|path| path.exists())
            .unwrap_or_else(|| self.imported.join(relative))
    }

    /// Reads a RON file relative to the imported root, from the first mod that has it (else
    /// `imported`), with the `*.patch.ron` files of the mods above it applied.
    pub fn read_ron<T: Serialize + DeserializeOwned>(&self, relative: impl AsRef<Path>) -> anyhow::Result<T> {
        mods::read_layered(&self.roots(), relative.as_ref())
    }

    /// A level's folder: the first mod with `levels/<level>/level.ron`, else `imported`.
    pub fn level_dir(&self, level: &str) -> PathBuf {
        let relative = Path::new("levels").join(level);
        self.mods
            .iter()
            .map(|m| m.dir.join(&relative))
            .find(|dir| dir.join("level.ron").is_file())
            .unwrap_or_else(|| self.imported.join(relative))
    }

    /// Every level folder in the mods and `imported`, sorted.
    pub fn level_names(&self) -> Vec<String> {
        let mut names: Vec<String> = self
            .roots()
            .into_iter()
            .flat_map(|root| std::fs::read_dir(root.join("levels")).into_iter().flatten().flatten())
            .filter(|entry| entry.path().join("level.ron").is_file())
            .map(|entry| entry.file_name().to_string_lossy().into_owned())
            .collect();
        names.sort();
        names.dedup();
        names
    }
}

/// Says which mods are on.
pub fn log_mods(paths: Option<Res<GamePaths>>) {
    for m in paths.iter().flat_map(|p| &p.mods) {
        info!("mod `{}` (priority {}) from {}", m.info.name, m.info.priority, m.dir.display());
    }
}
