//! The master server's config file, `master --config master.ron`. Every field is optional.
//!
//! ```ron
//! (
//!     name: "BF2 Rust master",
//!     // The server list for game browsers (UDP heartbeats and list queries).
//!     udp_bind: "0.0.0.0:16580",
//!     // The web pages and the REST API (plain HTTP: put a TLS reverse proxy in front).
//!     http_bind: "127.0.0.1:16581",
//!     // Where players reach the web pages (links, cookies): the proxy's https address.
//!     public_url: "https://master.example.com",
//!     // Behind a reverse proxy: take the client's address from X-Forwarded-For, but only
//!     // when the request's immediate peer is the proxy itself (loopback by default, since
//!     // that's how the documented deployment runs); otherwise X-Forwarded-For is ignored,
//!     // so a client that reaches this port directly can't spoof its address to dodge rate
//!     // limits. Only needed if the proxy isn't on this machine.
//!     trust_proxy: true,
//!     trusted_proxies: ["127.0.0.1", "::1"],
//!     // Folder for the database (master.sqlite) and the signing key (master.key).
//!     data_dir: "/var/lib/bf2-master",
//!     session_minutes: 15,
//!     ticket_minutes: 5,
//!     refresh_days: 30,
//!     // Stats only count for players who got a ticket for the reporting server this recently.
//!     ticket_window_hours: 12,
//!     allow_registration: true,
//!     // XP and ranks (see game_auth::ranks): BF2's by default.
//!     progression: (
//!         xp_per_score: 1.0,
//!         xp_per_minute: 0.5,
//!         win_bonus: 10,
//!         max_round_xp: 5000,
//!         ranks: [
//!             (name: "Private", short: "Pvt", xp: 0),
//!             (name: "Corporal", short: "Cpl", xp: 800),
//!             (name: "Sergeant", short: "Sgt", xp: 2500),
//!         ],
//!     ),
//! )
//! ```

use std::{
    net::{IpAddr, SocketAddr},
    path::{Path, PathBuf},
};

use game_auth::ranks::Progression;
use serde::Deserialize;

#[derive(Deserialize, Debug, Clone)]
#[serde(default)]
pub struct Config {
    pub name: String,
    pub udp_bind: SocketAddr,
    pub http_bind: SocketAddr,
    /// Where people reach the web pages, for links (no trailing slash).
    pub public_url: String,
    pub trust_proxy: bool,
    /// Peer addresses allowed to set `X-Forwarded-For`/`X-Real-IP` when `trust_proxy` is on.
    /// Empty (the default): only a proxy on this machine (loopback) is trusted, which matches
    /// the documented reverse-proxy deployment. Set this if the proxy runs elsewhere (a
    /// separate host or container).
    pub trusted_proxies: Vec<IpAddr>,
    pub data_dir: PathBuf,
    pub session_minutes: u64,
    pub ticket_minutes: u64,
    pub refresh_days: u64,
    pub ticket_window_hours: u64,
    pub allow_registration: bool,
    pub progression: Progression,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            name: "BF2 Rust master".into(),
            udp_bind: "127.0.0.1:16580".parse().unwrap(),
            http_bind: "127.0.0.1:16581".parse().unwrap(),
            public_url: String::new(),
            trust_proxy: false,
            trusted_proxies: Vec::new(),
            data_dir: PathBuf::from("master-data"),
            session_minutes: 15,
            ticket_minutes: 5,
            refresh_days: 30,
            ticket_window_hours: 12,
            allow_registration: true,
            progression: Progression::default(),
        }
    }
}

impl Config {
    /// Reads a config file; relative paths in it are relative to the file.
    pub fn load(path: &Path) -> Result<Self, String> {
        let text = std::fs::read_to_string(path).map_err(|err| format!("reading {}: {err}", path.display()))?;
        let mut config: Self = ron::Options::default()
            .with_default_extension(ron::extensions::Extensions::IMPLICIT_SOME)
            .from_str(&text)
            .map_err(|err| format!("{}: {err}", path.display()))?;
        if !config.data_dir.has_root() {
            config.data_dir = path.parent().unwrap_or(Path::new(".")).join(&config.data_dir);
        }
        Ok(config.normalized())
    }

    pub fn normalized(mut self) -> Self {
        self.public_url = self.public_url.trim().trim_end_matches('/').to_string();
        self.session_minutes = self.session_minutes.clamp(1, 24 * 60);
        self.ticket_minutes = self.ticket_minutes.clamp(1, 60);
        self.refresh_days = self.refresh_days.clamp(1, 365);
        self.ticket_window_hours = self.ticket_window_hours.clamp(1, 24 * 30);
        self.progression.normalize();
        self
    }

    /// Cookies only over https.
    pub fn secure_cookies(&self) -> bool {
        self.public_url.starts_with("https://")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn example_parses() {
        let doc = include_str!("config.rs");
        let start = doc.find("//! ```ron\n").unwrap() + "//! ```ron\n".len();
        let end = start + doc[start..].find("//! ```").unwrap();
        let example: String = doc[start..end]
            .lines()
            .map(|l| l.trim_start_matches("//!").trim_start_matches(' '))
            .collect::<Vec<_>>()
            .join("\n");
        let path = std::env::temp_dir().join(format!("bf2_master_config_{}.ron", std::process::id()));
        std::fs::write(&path, example).unwrap();
        let config = Config::load(&path).unwrap();
        assert_eq!(config.progression.ranks.len(), 3);
        assert_eq!(config.http_bind.port(), 16581);
        assert!(config.secure_cookies());
        assert_eq!(config.data_dir, PathBuf::from("/var/lib/bf2-master"));
        assert_eq!(config.trusted_proxies, vec!["127.0.0.1".parse::<IpAddr>().unwrap(), "::1".parse().unwrap()]);
        let _ = std::fs::remove_file(path);
    }
}
