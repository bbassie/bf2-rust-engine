//! The master server's REST API: JSON over HTTP(S) under [`API_PREFIX`].
//!
//! ```text
//! GET  /api/v1/info                      MasterInfo: name, public key (for game servers)
//! POST /api/v1/register                  Credentials -> Session
//! POST /api/v1/login                     Credentials -> Session
//! POST /api/v1/refresh                   RefreshRequest -> Session (the refresh token rotates)
//! POST /api/v1/logout                    RefreshRequest -> {}
//! GET  /api/v1/me                        (session) Profile
//! POST /api/v1/ticket                    (session) TicketRequest -> TicketResponse
//! GET  /api/v1/players/<name>            Profile (public)
//! GET  /api/v1/leaderboard?sort=&limit=  [LeaderboardEntry]
//! GET  /api/v1/servers                   [ServerEntry]
//! GET  /api/v1/quickjoin?ranked=&region= QuickJoin
//! POST /api/v1/server/heartbeat          (server API key) ServerHeartbeat -> {}
//! POST /api/v1/server/round              (server API key) RoundReport -> RoundResult
//! ```
//!
//! `(session)`: `Authorization: Bearer <session token>`. `(server API key)`:
//! `Authorization: Bearer <API key>` of a ranked server. Errors are [`ApiError`] with an
//! HTTP status: 400 bad input, 401 not logged in, 403 not allowed, 404, 409 name taken,
//! 429 too many attempts.

use serde::{Deserialize, Serialize};

pub const API_PREFIX: &str = "/api/v1";
/// Longest request body the master reads.
pub const MAX_BODY_BYTES: usize = 256 * 1024;
/// Players in one round report.
pub const MAX_ROUND_PLAYERS: usize = 256;
/// Entries per tally (kits, vehicles, weapons) in a round report.
pub const MAX_TALLIES: usize = 64;
/// A round is at most this long (a day).
pub const MAX_ROUND_SECONDS: f64 = 86_400.0;

#[derive(Serialize, Deserialize, Clone, Debug, Default, PartialEq)]
pub struct ApiError {
    pub error: String,
}

#[derive(Serialize, Deserialize, Clone, Debug, Default, PartialEq)]
pub struct MasterInfo {
    pub name: String,
    /// The key that signs account tokens (64 hex digits).
    pub public_key: String,
    pub fingerprint: String,
    /// Where people register in a browser.
    #[serde(default)]
    pub web_url: String,
}

#[derive(Serialize, Deserialize, Clone, Debug, Default, PartialEq)]
pub struct Credentials {
    pub name: String,
    pub password: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub email: Option<String>,
}

/// A login: a short-lived session token for the API and a refresh token (kept by the client)
/// for the next one.
#[derive(Serialize, Deserialize, Clone, Debug, Default, PartialEq)]
pub struct Session {
    pub session_token: String,
    pub session_expires: u64,
    pub refresh_token: String,
    pub refresh_expires: u64,
    pub profile: Profile,
}

#[derive(Serialize, Deserialize, Clone, Debug, Default, PartialEq)]
pub struct RefreshRequest {
    pub refresh_token: String,
}

#[derive(Serialize, Deserialize, Clone, Debug, Default, PartialEq)]
pub struct TicketRequest {
    /// The game server's key fingerprint.
    pub server: String,
}

#[derive(Serialize, Deserialize, Clone, Debug, Default, PartialEq)]
pub struct TicketResponse {
    pub ticket: String,
    pub expires: u64,
}

#[derive(Serialize, Deserialize, Clone, Debug, Default, PartialEq)]
pub struct RankInfo {
    pub index: u32,
    pub name: String,
    pub short: String,
    pub xp: u64,
    /// XP the current rank needed.
    pub rank_xp: u64,
    pub next_name: Option<String>,
    pub next_xp: Option<u64>,
}

impl RankInfo {
    /// How far to the next rank, 0..1 (1 at the top).
    pub fn progress(&self) -> f32 {
        match self.next_xp {
            Some(next) if next > self.rank_xp => {
                (self.xp.saturating_sub(self.rank_xp) as f64 / (next - self.rank_xp) as f64).clamp(0.0, 1.0) as f32
            }
            _ => 1.0,
        }
    }
}

/// Seconds and kills with a kit, vehicle or weapon.
#[derive(Serialize, Deserialize, Clone, Debug, Default, PartialEq)]
pub struct Tally {
    pub name: String,
    #[serde(default)]
    pub seconds: f64,
    #[serde(default)]
    pub kills: u32,
}

#[derive(Serialize, Deserialize, Clone, Debug, Default, PartialEq)]
pub struct CareerStats {
    pub rounds: u32,
    pub wins: u32,
    pub losses: u32,
    pub score: i64,
    pub kills: u32,
    pub deaths: u32,
    pub captures: u32,
    pub seconds: f64,
    pub kits: Vec<Tally>,
    pub vehicles: Vec<Tally>,
    pub weapons: Vec<Tally>,
    /// Seconds since 1970; 0 if never.
    pub last_played: u64,
}

#[derive(Serialize, Deserialize, Clone, Debug, Default, PartialEq)]
pub struct Profile {
    pub id: u64,
    pub name: String,
    /// Seconds since 1970.
    pub created: u64,
    pub rank: RankInfo,
    pub stats: CareerStats,
}

#[derive(Serialize, Deserialize, Clone, Debug, Default, PartialEq)]
pub struct LeaderboardEntry {
    pub position: u32,
    pub name: String,
    pub rank: RankInfo,
    pub score: i64,
    pub kills: u32,
    pub deaths: u32,
    pub seconds: f64,
    pub rounds: u32,
}

/// A game server the master knows.
#[derive(Serialize, Deserialize, Clone, Debug, Default, PartialEq)]
pub struct ServerEntry {
    /// IP as the master sees it (or as the server says, behind NAT).
    pub address: String,
    pub port: u16,
    /// Discovery port, for the server's details and ping (`game_shared::discovery`).
    pub query_port: u16,
    pub name: String,
    pub level: String,
    pub mode: String,
    pub players: u32,
    pub max_players: u32,
    pub bots: u32,
    /// Registered with the master: needs an account, reports stats.
    pub ranked: bool,
    /// As the server's admin says: `eu`, `us-east`, ...
    pub region: String,
    /// The server's key fingerprint, if it said.
    pub fingerprint: String,
}

impl ServerEntry {
    pub fn free_slots(&self) -> u32 {
        self.max_players.saturating_sub(self.players)
    }
}

/// Servers for quick join, best first. The client pings them and takes the closest.
#[derive(Serialize, Deserialize, Clone, Debug, Default, PartialEq)]
pub struct QuickJoin {
    pub servers: Vec<ServerEntry>,
    /// Players on all listed servers.
    pub players: u32,
}

/// A ranked server's heartbeat (every 30 s): it is alive, and how full it is.
#[derive(Serialize, Deserialize, Clone, Debug, Default, PartialEq)]
pub struct ServerHeartbeat {
    pub port: u16,
    pub query_port: u16,
    pub name: String,
    pub level: String,
    pub mode: String,
    pub players: u32,
    pub max_players: u32,
    pub bots: u32,
    /// The server's identity key (64 hex digits).
    pub public_key: String,
    #[serde(default)]
    pub region: String,
    /// The address players use, if not the one the heartbeat comes from (NAT, proxies).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub address: Option<String>,
}

/// What a ranked server reports when a round ends: the players with a verified account.
#[derive(Serialize, Deserialize, Clone, Debug, Default, PartialEq)]
pub struct RoundReport {
    /// Random, unique per round: a report sent twice counts once.
    pub round_id: String,
    pub level: String,
    pub mode: String,
    /// 1 or 2; 0 for a draw.
    pub winner: u8,
    pub seconds: f64,
    pub players: Vec<PlayerRound>,
}

#[derive(Serialize, Deserialize, Clone, Debug, Default, PartialEq)]
pub struct PlayerRound {
    /// Account id (the ticket's `sub`).
    pub account: u64,
    pub name: String,
    /// 1 or 2.
    pub team: u8,
    pub score: i32,
    pub kills: u32,
    pub deaths: u32,
    pub captures: u32,
    pub seconds: f64,
    #[serde(default)]
    pub kits: Vec<Tally>,
    #[serde(default)]
    pub vehicles: Vec<Tally>,
    #[serde(default)]
    pub weapons: Vec<Tally>,
}

impl RoundReport {
    /// Plausibility checks the master applies (a ranked server is trusted, but not with
    /// nonsense).
    pub fn validate(&self) -> Result<(), String> {
        if self.round_id.is_empty() || self.round_id.len() > 64 || !self.round_id.chars().all(|c| c.is_ascii_alphanumeric() || c == '-') {
            return Err("bad round id".into());
        }
        if self.level.len() > 128 || self.mode.len() > 32 {
            return Err("level or mode too long".into());
        }
        if self.winner > 2 {
            return Err("bad winner".into());
        }
        if !(0.0..=MAX_ROUND_SECONDS).contains(&self.seconds) {
            return Err("implausible round length".into());
        }
        if self.players.len() > MAX_ROUND_PLAYERS {
            return Err("too many players".into());
        }
        for p in &self.players {
            let tallies = [&p.kits, &p.vehicles, &p.weapons];
            if p.name.len() > 64
                || !(1..=2).contains(&p.team)
                || !(0.0..=self.seconds + 60.0).contains(&p.seconds)
                || p.kills > 5_000
                || p.deaths > 5_000
                || p.captures > 1_000
                || p.score.abs() > 100_000
                || tallies.iter().any(|t| t.len() > MAX_TALLIES || t.iter().any(|e| e.name.len() > 64 || e.kills > 5_000 || !(0.0..=MAX_ROUND_SECONDS).contains(&e.seconds)))
            {
                return Err(format!("implausible numbers for account {}", p.account));
            }
        }
        Ok(())
    }
}

#[derive(Serialize, Deserialize, Clone, Debug, Default, PartialEq)]
pub struct RoundResult {
    /// Players counted.
    pub accepted: u32,
    /// Accounts left out (unknown, or without a ticket for this server lately).
    pub rejected: Vec<u64>,
    /// Players promoted by this round: account, new rank name.
    pub promotions: Vec<(u64, String)>,
    /// The report was sent before and counted then.
    pub duplicate: bool,
}
