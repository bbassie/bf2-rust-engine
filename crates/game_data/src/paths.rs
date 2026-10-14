//! Where the game's data lives on disk: the converted BF2 content (`imported/`) and the mods.
//! Shared by the game (`game_shared::config::GamePaths`), the dedicated server and
//! `bf2-import`, so all three agree without being told.
//!
//! The **data root** holds `imported/` and `mods/`. It is, in order:
//!
//! 1. `--data-dir` (or `$BF2_DATA_DIR`): everything goes there ([`Layout::Custom`]);
//! 2. the working directory when it has `imported/` or `mods/` ([`Layout::WorkingDir`]): a
//!    repository checkout (which ships `mods/`), so development keeps using the repo's
//!    folders exactly as before;
//! 3. the executable's folder when it has `imported/` ([`Layout::Portable`]): a game folder
//!    carried around with its content (on a USB stick, say);
//! 4. the platform's per-user data folder ([`Layout::User`]): `%LOCALAPPDATA%\bf2-rust-engine`
//!    on Windows, `$XDG_DATA_HOME/bf2-rust-engine` (default `~/.local/share/bf2-rust-engine`)
//!    on Linux, `~/Library/Application Support/bf2-rust-engine` on macOS. A packaged game
//!    unzipped anywhere imports into this one.
//!
//! `--imported`/`$GAME_IMPORTED_DIR` and `--mods`/`$GAME_MODS_DIR` still override the two
//! folders one by one. Settings (`%APPDATA%`) and caches (`game_shared::cache`) have their
//! own platform folders.

use std::path::{Path, PathBuf};

/// The folder name under the platform folders.
pub const APP_DIR: &str = "bf2-rust-engine";
/// Environment variable naming the data root (like `--data-dir`).
pub const DATA_DIR_ENV: &str = "BF2_DATA_DIR";
/// Environment variable that overrides the imported assets folder.
pub const IMPORTED_ENV: &str = "GAME_IMPORTED_DIR";
/// Environment variable that overrides the mods folder.
pub const MODS_ENV: &str = "GAME_MODS_DIR";

/// Why the data root is where it is.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Layout {
    /// `--data-dir` or `$BF2_DATA_DIR`.
    Custom,
    /// `imported/` or `mods/` in the working directory (a repository checkout).
    WorkingDir,
    /// `imported/` next to the executable.
    Portable,
    /// The platform's per-user data folder.
    User,
}

impl Layout {
    pub fn label(self) -> &'static str {
        match self {
            Layout::Custom => "data folder from --data-dir",
            Layout::WorkingDir => "working directory",
            Layout::Portable => "next to the game",
            Layout::User => "user data folder",
        }
    }
}

/// The resolved folders. Paths are absolute.
#[derive(Clone, Debug)]
pub struct DataDirs {
    pub layout: Layout,
    pub root: PathBuf,
    pub imported: PathBuf,
    pub mods: PathBuf,
}

impl DataDirs {
    /// Resolves the folders (see the module doc). `data_dir` is `--data-dir`; `imported` and
    /// `mods` are the per-folder overrides.
    pub fn resolve(data_dir: Option<PathBuf>, imported: Option<PathBuf>, mods: Option<PathBuf>) -> Self {
        let cwd = std::env::current_dir().unwrap_or_default();
        let exe_dir = std::env::current_exe().ok().and_then(|exe| exe.parent().map(Path::to_path_buf));
        Self::resolve_from(
            data_dir.or_else(|| env(DATA_DIR_ENV)),
            imported.or_else(|| env(IMPORTED_ENV)),
            mods.or_else(|| env(MODS_ENV)),
            &cwd,
            exe_dir.as_deref(),
            user_data_dir(),
        )
    }

    /// [`Self::resolve`] with the environment variables already applied and the folders given.
    pub fn resolve_from(
        data_dir: Option<PathBuf>,
        imported: Option<PathBuf>,
        mods: Option<PathBuf>,
        cwd: &Path,
        exe_dir: Option<&Path>,
        user: Option<PathBuf>,
    ) -> Self {
        let (layout, root) = if let Some(dir) = data_dir {
            (Layout::Custom, cwd.join(dir))
        } else if cwd.join("imported").is_dir() || cwd.join("mods").is_dir() {
            (Layout::WorkingDir, cwd.to_path_buf())
        } else if let Some(exe_dir) = exe_dir.filter(|d| d.join("imported").is_dir()) {
            (Layout::Portable, exe_dir.to_path_buf())
        } else if let Some(user) = user {
            (Layout::User, user)
        } else {
            (Layout::WorkingDir, cwd.to_path_buf())
        };
        let imported = imported.map_or_else(|| root.join("imported"), |p| cwd.join(p));
        let mods = mods.map_or_else(|| root.join("mods"), |p| cwd.join(p));
        Self { layout, root: absolute(root), imported: absolute(imported), mods: absolute(mods) }
    }

    /// Whether `imported/` has at least one level.
    pub fn has_imported_levels(&self) -> bool {
        std::fs::read_dir(self.imported.join("levels"))
            .into_iter()
            .flatten()
            .flatten()
            .any(|entry| entry.path().join("level.ron").is_file())
    }
}

/// The platform's per-user data folder for the game (see the module doc).
pub fn user_data_dir() -> Option<PathBuf> {
    if cfg!(windows) {
        env("LOCALAPPDATA").map(|dir| dir.join(APP_DIR))
    } else if cfg!(target_os = "macos") {
        env("HOME").map(|home| home.join("Library/Application Support").join(APP_DIR))
    } else {
        env("XDG_DATA_HOME")
            .filter(|dir| dir.is_absolute())
            .or_else(|| env("HOME").map(|home| home.join(".local/share")))
            .map(|dir| dir.join(APP_DIR))
    }
}

fn env(name: &str) -> Option<PathBuf> {
    std::env::var_os(name).filter(|v| !v.is_empty()).map(PathBuf::from)
}

fn absolute(path: PathBuf) -> PathBuf {
    std::path::absolute(&path).unwrap_or(path)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("bf2_paths_{name}_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn picks_the_layout() {
        let base = temp("layout");
        let repo = base.join("repo");
        let exe = base.join("game");
        let user = base.join("user");
        for dir in [&repo, &exe] {
            std::fs::create_dir_all(dir).unwrap();
        }
        // Nothing anywhere: the user folder.
        let dirs = DataDirs::resolve_from(None, None, None, &repo, Some(&exe), Some(user.clone()));
        assert_eq!(dirs.layout, Layout::User);
        assert_eq!(dirs.imported, user.join("imported"));
        // Imported content next to the executable: portable.
        std::fs::create_dir_all(exe.join("imported")).unwrap();
        let dirs = DataDirs::resolve_from(None, None, None, &repo, Some(&exe), Some(user.clone()));
        assert_eq!(dirs.layout, Layout::Portable);
        assert_eq!(dirs.mods, exe.join("mods"));
        // A checkout (has `mods/`): the working directory wins.
        std::fs::create_dir_all(repo.join("mods")).unwrap();
        let dirs = DataDirs::resolve_from(None, None, None, &repo, Some(&exe), Some(user.clone()));
        assert_eq!(dirs.layout, Layout::WorkingDir);
        assert_eq!(dirs.imported, repo.join("imported"));
        // --data-dir beats everything, and a per-folder override beats it.
        let dirs = DataDirs::resolve_from(Some(base.join("d")), Some(PathBuf::from("x")), None, &repo, Some(&exe), Some(user));
        assert_eq!(dirs.layout, Layout::Custom);
        assert_eq!(dirs.imported, repo.join("x"));
        assert_eq!(dirs.mods, base.join("d").join("mods"));
        let _ = std::fs::remove_dir_all(&base);
    }
}
