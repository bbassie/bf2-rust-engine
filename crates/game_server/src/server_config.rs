//! The dedicated server's config file, `server --config server.ron`. Every field is
//! optional; command line options override the file.
//!
//! ```ron
//! (
//!     name: "My BF2 server",
//!     port: 16567,
//!     // Accept players from other machines (otherwise only this one can join).
//!     public: false,
//!     max_players: 32,
//!     bots: 12,
//!     bot_skill: 0.5,
//!     friendly_fire: false,
//!     respawn_time: 10.0,
//!     // Percent of each level's tickets.
//!     ticket_ratio: 100.0,
//!     // Remote console (`server rcon`) and `/login` in the chat; empty turns both off.
//!     admin_password: "secret",
//!     rcon_port: 4711,
//!     rcon_public: false,
//!     motd: "Welcome! Be nice.\nVisit example.com",
//!     // Relative to this file. Default: the `server` folder in the user's config directory.
//!     stats_file: "stats.ron",
//!     ban_file: "bans.ron",
//!     // Co-op maps (mode "gpm_coop"): the humans' team, the percentage of all soldiers on
//!     // the bots' team (50: even teams) and the bots' skill there.
//!     coop_team: 1,
//!     coop_bot_ratio: 50.0,
//!     coop_bot_skill: 0.4,
//!     // Announce the server to a master server (off by default; see crates/master_server).
//!     master_server: "127.0.0.1:16580",
//!     // What joining clients download from this server (docs/MODDING.md): Off, Mods (the
//!     // default: content made for this engine) or All (also the imported BF2 assets, which
//!     // are EA's copyrighted content: only if you may share them).
//!     content: Mods,
//!     // Clients fetch files from here first (a static host or CDN filled by
//!     // `server export-content`); this server stays the fallback.
//!     download_url: "https://cdn.example.com/bf2-content",
//!     // TCP port of the content endpoint (default: `port`).
//!     content_port: 16567,
//!     rotation: [
//!         (level: "strike_at_karkand", size: 32),
//!         (level: "dalian_plant", mode: "gpm_cq", size: 64, bots: 24),
//!         (level: "strike_at_karkand", mode: "gpm_coop", size: 16, bots: 16),
//!     ],
//! )
//! ```

use std::path::{Path, PathBuf};

use serde::Deserialize;

use game_shared::content::ContentMode;

use crate::{ServerSettings, admin::AdminSettings, content::ContentSettings, coop::CoopSettings, rotation::MapEntry};

#[derive(Deserialize, Debug, Clone)]
#[serde(default)]
pub struct ServerConfig {
    pub name: String,
    pub port: u16,
    pub public: bool,
    pub max_players: usize,
    pub bots: u32,
    pub bot_skill: f32,
    pub friendly_fire: bool,
    /// Seconds between death and respawn.
    pub respawn_time: f32,
    pub ticket_ratio: f32,
    pub admin_password: String,
    pub rcon_port: u16,
    pub rcon_public: bool,
    pub motd: String,
    /// `None` keeps stats in memory only.
    pub stats_file: Option<PathBuf>,
    /// `None` keeps bans in memory only.
    pub ban_file: Option<PathBuf>,
    /// Co-op: the humans' team (1 or 2).
    pub coop_team: u8,
    /// Co-op: percent of all soldiers on the bots' team.
    pub coop_bot_ratio: f32,
    /// Co-op: bot skill 0..1 (default `bot_skill`).
    pub coop_bot_skill: Option<f32>,
    /// Master server to announce this server to, `host[:port]`.
    pub master_server: Option<String>,
    /// What joining clients may download: `Off`, `Mods` or `All`.
    pub content: ContentMode,
    /// Clients download from here first: `<url>/<hash>`.
    pub download_url: Option<String>,
    /// TCP port of the content endpoint (default: `port`).
    pub content_port: Option<u16>,
    pub rotation: Vec<MapEntry>,
}

impl Default for ServerConfig {
    fn default() -> Self {
        let settings = ServerSettings::default();
        let admin = AdminSettings::default();
        let data = data_dir();
        Self {
            name: settings.name,
            port: settings.port,
            public: false,
            max_players: settings.max_clients,
            bots: 0,
            bot_skill: settings.bot_skill,
            friendly_fire: settings.friendly_fire,
            respawn_time: settings.respawn_seconds,
            ticket_ratio: settings.ticket_ratio,
            admin_password: String::new(),
            rcon_port: admin.rcon_port,
            rcon_public: false,
            motd: String::new(),
            stats_file: data.as_ref().map(|d| d.join("stats.ron")),
            ban_file: data.map(|d| d.join("bans.ron")),
            coop_team: 1,
            coop_bot_ratio: 50.0,
            coop_bot_skill: None,
            master_server: None,
            content: ContentMode::default(),
            download_url: None,
            content_port: None,
            rotation: Vec::new(),
        }
    }
}

/// `server` in the platform config directory (`%APPDATA%\bf2-rust-engine\server` on
/// Windows, `~/.config/bf2-rust-engine/server` on Linux).
pub fn data_dir() -> Option<PathBuf> {
    let env = |name| std::env::var_os(name).filter(|v| !v.is_empty()).map(PathBuf::from);
    let base = if cfg!(windows) {
        env("APPDATA")
    } else if cfg!(target_os = "macos") {
        env("HOME").map(|home| home.join("Library/Application Support"))
    } else {
        env("XDG_CONFIG_HOME").or_else(|| env("HOME").map(|home| home.join(".config")))
    };
    base.map(|b| b.join("bf2-rust-engine").join("server"))
}

impl ServerConfig {
    /// Reads a config file. Relative file names in it are relative to the file.
    pub fn load(path: &Path) -> anyhow::Result<Self> {
        let text = std::fs::read_to_string(path)
            .map_err(|err| anyhow::anyhow!("reading {}: {err}", path.display()))?;
        let options = ron::Options::default()
            .with_default_extension(ron::extensions::Extensions::IMPLICIT_SOME);
        let mut config: Self = options
            .from_str(&text)
            .map_err(|err| anyhow::anyhow!("{}: {err}", path.display()))?;
        let dir = path.parent().unwrap_or(Path::new("."));
        for file in [&mut config.stats_file, &mut config.ban_file].into_iter().flatten() {
            if file.is_relative() {
                *file = dir.join(&*file);
            }
        }
        Ok(config)
    }

    /// Server settings starting on the first map of the rotation (the test range without).
    pub fn into_settings(self) -> ServerSettings {
        let first = self.rotation.first().cloned().unwrap_or_default();
        ServerSettings {
            level: first.level,
            mode: first.mode,
            size: first.size,
            bots: self.bots,
            max_clients: self.max_players,
            port: self.port,
            network: true,
            public: self.public,
            local_player: None,
            local_team: 1,
            respawn_seconds: self.respawn_time.max(0.0),
            friendly_fire: self.friendly_fire,
            bot_skill: self.bot_skill.clamp(0.0, 1.0),
            name: self.name,
            ticket_ratio: self.ticket_ratio.max(1.0),
            rotation: self.rotation,
            admin: AdminSettings {
                password: self.admin_password,
                rcon_port: self.rcon_port,
                rcon_public: self.rcon_public,
                motd: self.motd,
                ban_file: self.ban_file,
                stats_file: self.stats_file,
            },
            coop: CoopSettings {
                human_team: if self.coop_team == 2 { 2 } else { 1 },
                bot_ratio: self.coop_bot_ratio.clamp(0.0, 100.0),
                bot_skill: self.coop_bot_skill.map(|s| s.clamp(0.0, 1.0)),
            },
            master_server: self.master_server.filter(|m| !m.trim().is_empty()),
            content: ContentSettings {
                mode: self.content,
                port: self.content_port,
                download_url: self.download_url.filter(|u| !u.trim().is_empty()),
                cache_dir: None,
            },
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn example_parses() {
        let doc = include_str!("server_config.rs");
        let start = doc.find("//! ```ron\n").unwrap() + "//! ```ron\n".len();
        let end = start + doc[start..].find("//! ```").unwrap();
        let example: String = doc[start..end]
            .lines()
            .map(|l| l.trim_start_matches("//!").trim_start_matches(' '))
            .collect::<Vec<_>>()
            .join("\n");
        let path = std::env::temp_dir().join("bf2_server_config_test.ron");
        std::fs::write(&path, example).unwrap();
        let config = ServerConfig::load(&path).unwrap();
        assert_eq!(config.rotation.len(), 3);
        assert_eq!(config.coop_bot_skill, Some(0.4));
        assert_eq!(config.rotation[0].mode, "gpm_cq");
        assert_eq!(config.rotation[1].bots, Some(24));
        assert!(config.stats_file.unwrap().ends_with("stats.ron"));
        let settings = ServerConfig::load(&path).unwrap().into_settings();
        assert_eq!(settings.level, "strike_at_karkand");
        assert_eq!(settings.size, 32);
        assert_eq!(settings.admin.motd.lines().count(), 2);
        assert_eq!(settings.content.mode, ContentMode::Mods);
        assert_eq!(settings.content.port, Some(16567));
    }
}
