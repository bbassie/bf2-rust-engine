//! Filesystem locations.

use std::path::PathBuf;

use bevy::prelude::*;

/// Where the game finds data on disk.
#[derive(Resource, Clone, Debug)]
pub struct GamePaths {
    /// Root of the assets converted by `bf2-import` (`levels/`, `objects/`, ...).
    pub imported: PathBuf,
}

impl GamePaths {
    /// Environment variable that overrides the imported assets folder.
    pub const IMPORTED_ENV: &str = "GAME_IMPORTED_DIR";

    /// Resolves paths from an optional CLI override, then the environment, then the default.
    pub fn resolve(cli_imported: Option<PathBuf>) -> Self {
        let imported = cli_imported
            .or_else(|| std::env::var_os(Self::IMPORTED_ENV).map(PathBuf::from))
            .unwrap_or_else(|| PathBuf::from("imported"));
        // Absolute, so asset sources don't resolve it relative to the executable.
        let imported = std::path::absolute(&imported).unwrap_or(imported);
        Self { imported }
    }

    pub fn level_dir(&self, level: &str) -> PathBuf {
        self.imported.join("levels").join(level)
    }
}
