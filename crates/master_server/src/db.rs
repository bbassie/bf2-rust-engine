//! The master's SQLite database: accounts, login tokens, ranked servers, career stats.
//!
//! Secrets are stored hashed: passwords with Argon2id (see `auth`), refresh tokens, web
//! sessions and server API keys with BLAKE3 (they are 256-bit random, so a fast hash is
//! enough).

use std::path::Path;

use game_auth::{
    api::{CareerStats, LeaderboardEntry, PlayerRound, Profile, Tally},
    ranks::Progression,
};
use rusqlite::{Connection, OptionalExtension, params};

pub struct Db {
    conn: Connection,
}

/// An account row.
#[derive(Clone, Debug)]
pub struct Account {
    pub id: u64,
    pub name: String,
    pub password_hash: String,
    pub created: u64,
    pub xp: u64,
    pub disabled: bool,
}

/// A ranked server row.
#[derive(Clone, Debug)]
pub struct RankedServer {
    pub id: u64,
    pub name: String,
    pub public_key: String,
    pub created: u64,
    pub last_seen: u64,
}

const SCHEMA: &str = "
PRAGMA foreign_keys = ON;
CREATE TABLE IF NOT EXISTS accounts (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    name TEXT NOT NULL,
    name_key TEXT NOT NULL UNIQUE,
    email TEXT,
    password_hash TEXT NOT NULL,
    created INTEGER NOT NULL,
    last_login INTEGER NOT NULL DEFAULT 0,
    xp INTEGER NOT NULL DEFAULT 0,
    disabled INTEGER NOT NULL DEFAULT 0
);
CREATE TABLE IF NOT EXISTS tokens (
    token_hash TEXT PRIMARY KEY,
    account_id INTEGER NOT NULL REFERENCES accounts(id) ON DELETE CASCADE,
    kind TEXT NOT NULL,
    created INTEGER NOT NULL,
    expires INTEGER NOT NULL
);
CREATE TABLE IF NOT EXISTS careers (
    account_id INTEGER PRIMARY KEY REFERENCES accounts(id) ON DELETE CASCADE,
    rounds INTEGER NOT NULL DEFAULT 0,
    wins INTEGER NOT NULL DEFAULT 0,
    losses INTEGER NOT NULL DEFAULT 0,
    score INTEGER NOT NULL DEFAULT 0,
    kills INTEGER NOT NULL DEFAULT 0,
    deaths INTEGER NOT NULL DEFAULT 0,
    captures INTEGER NOT NULL DEFAULT 0,
    seconds REAL NOT NULL DEFAULT 0,
    last_played INTEGER NOT NULL DEFAULT 0
);
CREATE TABLE IF NOT EXISTS tallies (
    account_id INTEGER NOT NULL REFERENCES accounts(id) ON DELETE CASCADE,
    kind TEXT NOT NULL,
    name TEXT NOT NULL,
    seconds REAL NOT NULL DEFAULT 0,
    kills INTEGER NOT NULL DEFAULT 0,
    PRIMARY KEY (account_id, kind, name)
);
CREATE TABLE IF NOT EXISTS servers (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    name TEXT NOT NULL,
    api_key_hash TEXT NOT NULL UNIQUE,
    public_key TEXT NOT NULL DEFAULT '',
    created INTEGER NOT NULL,
    last_seen INTEGER NOT NULL DEFAULT 0
);
CREATE TABLE IF NOT EXISTS rounds (
    server_id INTEGER NOT NULL REFERENCES servers(id) ON DELETE CASCADE,
    round_id TEXT NOT NULL,
    level TEXT NOT NULL,
    mode TEXT NOT NULL,
    winner INTEGER NOT NULL,
    players INTEGER NOT NULL,
    ended INTEGER NOT NULL,
    PRIMARY KEY (server_id, round_id)
);
CREATE TABLE IF NOT EXISTS tickets (
    account_id INTEGER NOT NULL REFERENCES accounts(id) ON DELETE CASCADE,
    server TEXT NOT NULL,
    issued INTEGER NOT NULL,
    PRIMARY KEY (account_id, server)
);
";

/// Hash of a random secret (token, API key) for storage.
pub fn secret_hash(secret: &str) -> String {
    blake3::hash(secret.as_bytes()).to_hex().to_string()
}

fn to_i64(v: u64) -> i64 {
    v.min(i64::MAX as u64) as i64
}

impl Db {
    pub fn open(path: &Path) -> rusqlite::Result<Self> {
        if let Some(dir) = path.parent() {
            let _ = std::fs::create_dir_all(dir);
        }
        let conn = Connection::open(path)?;
        conn.pragma_update(None, "journal_mode", "WAL")?;
        conn.busy_timeout(std::time::Duration::from_secs(5))?;
        conn.execute_batch(SCHEMA)?;
        Ok(Self { conn })
    }

    #[cfg(test)]
    pub fn in_memory() -> rusqlite::Result<Self> {
        let conn = Connection::open_in_memory()?;
        conn.execute_batch(SCHEMA)?;
        Ok(Self { conn })
    }

    // Accounts.

    pub fn create_account(&self, name: &str, email: Option<&str>, password_hash: &str, now: u64) -> rusqlite::Result<Option<u64>> {
        let inserted = self.conn.execute(
            "INSERT OR IGNORE INTO accounts (name, name_key, email, password_hash, created) VALUES (?1, ?2, ?3, ?4, ?5)",
            params![name, name.to_ascii_lowercase(), email, password_hash, to_i64(now)],
        )?;
        if inserted == 0 {
            return Ok(None);
        }
        let id = self.conn.last_insert_rowid() as u64;
        self.conn.execute("INSERT INTO careers (account_id) VALUES (?1)", params![id as i64])?;
        Ok(Some(id))
    }

    fn account_where(&self, clause: &str, value: &dyn rusqlite::ToSql) -> rusqlite::Result<Option<Account>> {
        self.conn
            .query_row(
                &format!("SELECT id, name, password_hash, created, xp, disabled FROM accounts WHERE {clause}"),
                [value],
                |r| {
                    Ok(Account {
                        id: r.get::<_, i64>(0)? as u64,
                        name: r.get(1)?,
                        password_hash: r.get(2)?,
                        created: r.get::<_, i64>(3)? as u64,
                        xp: r.get::<_, i64>(4)? as u64,
                        disabled: r.get::<_, i64>(5)? != 0,
                    })
                },
            )
            .optional()
    }

    pub fn account_by_name(&self, name: &str) -> rusqlite::Result<Option<Account>> {
        self.account_where("name_key = ?1", &name.to_ascii_lowercase())
    }

    pub fn account(&self, id: u64) -> rusqlite::Result<Option<Account>> {
        self.account_where("id = ?1", &(id as i64))
    }

    pub fn touch_login(&self, id: u64, now: u64) -> rusqlite::Result<()> {
        self.conn.execute("UPDATE accounts SET last_login = ?2 WHERE id = ?1", params![id as i64, to_i64(now)])?;
        Ok(())
    }

    pub fn set_password(&self, id: u64, password_hash: &str) -> rusqlite::Result<()> {
        self.conn.execute("UPDATE accounts SET password_hash = ?2 WHERE id = ?1", params![id as i64, password_hash])?;
        Ok(())
    }

    pub fn account_count(&self) -> rusqlite::Result<u64> {
        self.conn.query_row("SELECT COUNT(*) FROM accounts", [], |r| r.get::<_, i64>(0)).map(|n| n as u64)
    }

    // Refresh tokens and web sessions.

    pub fn add_token(&self, secret: &str, account: u64, kind: &str, now: u64, expires: u64) -> rusqlite::Result<()> {
        self.conn.execute(
            "INSERT INTO tokens (token_hash, account_id, kind, created, expires) VALUES (?1, ?2, ?3, ?4, ?5)",
            params![secret_hash(secret), account as i64, kind, to_i64(now), to_i64(expires)],
        )?;
        Ok(())
    }

    /// The account of a valid token of `kind`.
    pub fn token_account(&self, secret: &str, kind: &str, now: u64) -> rusqlite::Result<Option<u64>> {
        self.conn
            .query_row(
                "SELECT account_id FROM tokens WHERE token_hash = ?1 AND kind = ?2 AND expires > ?3",
                params![secret_hash(secret), kind, to_i64(now)],
                |r| r.get::<_, i64>(0),
            )
            .optional()
            .map(|id| id.map(|id| id as u64))
    }

    pub fn remove_token(&self, secret: &str) -> rusqlite::Result<bool> {
        Ok(self.conn.execute("DELETE FROM tokens WHERE token_hash = ?1", params![secret_hash(secret)])? > 0)
    }

    pub fn remove_expired(&self, now: u64) -> rusqlite::Result<()> {
        self.conn.execute("DELETE FROM tokens WHERE expires <= ?1", params![to_i64(now)])?;
        Ok(())
    }

    // Tickets: which accounts joined which server lately (for accepting stats).

    pub fn note_ticket(&self, account: u64, server: &str, now: u64) -> rusqlite::Result<()> {
        self.conn.execute(
            "INSERT INTO tickets (account_id, server, issued) VALUES (?1, ?2, ?3)
             ON CONFLICT(account_id, server) DO UPDATE SET issued = excluded.issued",
            params![account as i64, server, to_i64(now)],
        )?;
        Ok(())
    }

    pub fn ticket_since(&self, account: u64, server: &str, since: u64) -> rusqlite::Result<bool> {
        self.conn
            .query_row(
                "SELECT 1 FROM tickets WHERE account_id = ?1 AND server = ?2 AND issued >= ?3",
                params![account as i64, server, to_i64(since)],
                |_| Ok(()),
            )
            .optional()
            .map(|r| r.is_some())
    }

    // Ranked servers.

    pub fn add_server(&self, name: &str, api_key: &str, now: u64) -> rusqlite::Result<u64> {
        self.conn.execute(
            "INSERT INTO servers (name, api_key_hash, created) VALUES (?1, ?2, ?3)",
            params![name, secret_hash(api_key), to_i64(now)],
        )?;
        Ok(self.conn.last_insert_rowid() as u64)
    }

    fn server_row(r: &rusqlite::Row) -> rusqlite::Result<RankedServer> {
        Ok(RankedServer {
            id: r.get::<_, i64>(0)? as u64,
            name: r.get(1)?,
            public_key: r.get(2)?,
            created: r.get::<_, i64>(3)? as u64,
            last_seen: r.get::<_, i64>(4)? as u64,
        })
    }

    pub fn server_by_key(&self, api_key: &str) -> rusqlite::Result<Option<RankedServer>> {
        self.conn
            .query_row(
                "SELECT id, name, public_key, created, last_seen FROM servers WHERE api_key_hash = ?1",
                params![secret_hash(api_key)],
                Self::server_row,
            )
            .optional()
    }

    pub fn servers(&self) -> rusqlite::Result<Vec<RankedServer>> {
        let mut statement = self.conn.prepare("SELECT id, name, public_key, created, last_seen FROM servers ORDER BY id")?;
        let rows = statement.query_map([], Self::server_row)?;
        rows.collect()
    }

    pub fn remove_server(&self, id: u64) -> rusqlite::Result<bool> {
        Ok(self.conn.execute("DELETE FROM servers WHERE id = ?1", params![id as i64])? > 0)
    }

    pub fn server_seen(&self, id: u64, public_key: &str, now: u64) -> rusqlite::Result<()> {
        self.conn.execute(
            "UPDATE servers SET public_key = ?2, last_seen = ?3 WHERE id = ?1",
            params![id as i64, public_key, to_i64(now)],
        )?;
        Ok(())
    }

    // Stats.

    /// Records a round once: `false` if this server reported it before.
    pub fn add_round(&self, server: u64, round_id: &str, level: &str, mode: &str, winner: u8, players: usize, now: u64) -> rusqlite::Result<bool> {
        Ok(self.conn.execute(
            "INSERT OR IGNORE INTO rounds (server_id, round_id, level, mode, winner, players, ended) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
            params![server as i64, round_id, level, mode, winner as i64, players as i64, to_i64(now)],
        )? > 0)
    }

    /// Adds a player's round to their career and XP. Returns the new XP.
    pub fn add_player_round(&mut self, round: &PlayerRound, won: Option<bool>, xp: u64, now: u64) -> rusqlite::Result<u64> {
        let tx = self.conn.transaction()?;
        let (wins, losses) = match won {
            Some(true) => (1, 0),
            Some(false) => (0, 1),
            None => (0, 0),
        };
        tx.execute(
            "UPDATE careers SET rounds = rounds + 1, wins = wins + ?2, losses = losses + ?3, score = score + ?4,
             kills = kills + ?5, deaths = deaths + ?6, captures = captures + ?7, seconds = seconds + ?8, last_played = ?9
             WHERE account_id = ?1",
            params![
                round.account as i64,
                wins,
                losses,
                round.score as i64,
                round.kills as i64,
                round.deaths as i64,
                round.captures as i64,
                round.seconds,
                to_i64(now)
            ],
        )?;
        for (kind, tallies) in [("kit", &round.kits), ("vehicle", &round.vehicles), ("weapon", &round.weapons)] {
            for tally in tallies {
                tx.execute(
                    "INSERT INTO tallies (account_id, kind, name, seconds, kills) VALUES (?1, ?2, ?3, ?4, ?5)
                     ON CONFLICT(account_id, kind, name) DO UPDATE SET seconds = seconds + excluded.seconds, kills = kills + excluded.kills",
                    params![round.account as i64, kind, tally.name, tally.seconds, tally.kills as i64],
                )?;
            }
        }
        tx.execute("UPDATE accounts SET xp = xp + ?2 WHERE id = ?1", params![round.account as i64, to_i64(xp)])?;
        let total: i64 = tx.query_row("SELECT xp FROM accounts WHERE id = ?1", params![round.account as i64], |r| r.get(0))?;
        tx.commit()?;
        Ok(total as u64)
    }

    fn tallies(&self, account: u64, kind: &str) -> rusqlite::Result<Vec<Tally>> {
        let mut statement = self.conn.prepare(
            "SELECT name, seconds, kills FROM tallies WHERE account_id = ?1 AND kind = ?2 ORDER BY seconds DESC, kills DESC LIMIT 12",
        )?;
        let rows = statement.query_map(params![account as i64, kind], |r| {
            Ok(Tally { name: r.get(0)?, seconds: r.get(1)?, kills: r.get::<_, i64>(2)? as u32 })
        })?;
        rows.collect()
    }

    pub fn career(&self, account: u64) -> rusqlite::Result<CareerStats> {
        let mut stats = self
            .conn
            .query_row(
                "SELECT rounds, wins, losses, score, kills, deaths, captures, seconds, last_played FROM careers WHERE account_id = ?1",
                params![account as i64],
                |r| {
                    Ok(CareerStats {
                        rounds: r.get::<_, i64>(0)? as u32,
                        wins: r.get::<_, i64>(1)? as u32,
                        losses: r.get::<_, i64>(2)? as u32,
                        score: r.get(3)?,
                        kills: r.get::<_, i64>(4)? as u32,
                        deaths: r.get::<_, i64>(5)? as u32,
                        captures: r.get::<_, i64>(6)? as u32,
                        seconds: r.get(7)?,
                        last_played: r.get::<_, i64>(8)? as u64,
                        ..Default::default()
                    })
                },
            )
            .optional()?
            .unwrap_or_default();
        stats.kits = self.tallies(account, "kit")?;
        stats.vehicles = self.tallies(account, "vehicle")?;
        stats.weapons = self.tallies(account, "weapon")?;
        stats.weapons.sort_by(|a, b| b.kills.cmp(&a.kills));
        Ok(stats)
    }

    pub fn profile(&self, account: &Account, progression: &Progression) -> rusqlite::Result<Profile> {
        Ok(Profile {
            id: account.id,
            name: account.name.clone(),
            created: account.created,
            rank: progression.info(account.xp),
            stats: self.career(account.id)?,
        })
    }

    /// The best players by `sort`: `xp` (default), `score`, `kills`, `kd` or `time`.
    pub fn leaderboard(&self, sort: &str, limit: usize, progression: &Progression) -> rusqlite::Result<Vec<LeaderboardEntry>> {
        let order = match sort {
            "score" => "c.score DESC",
            "kills" => "c.kills DESC",
            "kd" => "(CAST(c.kills AS REAL) / MAX(c.deaths, 1)) DESC, c.kills DESC",
            "time" => "c.seconds DESC",
            _ => "a.xp DESC, c.score DESC",
        };
        let mut statement = self.conn.prepare(&format!(
            "SELECT a.name, a.xp, c.score, c.kills, c.deaths, c.seconds, c.rounds FROM accounts a JOIN careers c ON c.account_id = a.id
             WHERE a.disabled = 0 AND c.rounds > 0 ORDER BY {order} LIMIT ?1"
        ))?;
        let rows = statement.query_map(params![limit.min(500) as i64], |r| {
            Ok((
                r.get::<_, String>(0)?,
                r.get::<_, i64>(1)? as u64,
                r.get::<_, i64>(2)?,
                r.get::<_, i64>(3)? as u32,
                r.get::<_, i64>(4)? as u32,
                r.get::<_, f64>(5)?,
                r.get::<_, i64>(6)? as u32,
            ))
        })?;
        let mut entries = Vec::new();
        for (i, row) in rows.enumerate() {
            let (name, xp, score, kills, deaths, seconds, rounds) = row?;
            entries.push(LeaderboardEntry {
                position: i as u32 + 1,
                name,
                rank: progression.info(xp),
                score,
                kills,
                deaths,
                seconds,
                rounds,
            });
        }
        Ok(entries)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn accounts_tokens_rounds() {
        let mut db = Db::in_memory().unwrap();
        let progression = Progression::default();
        let id = db.create_account("Alice", None, "hash", 1).unwrap().unwrap();
        assert!(db.create_account("alice", None, "hash", 1).unwrap().is_none(), "names are case-insensitive");
        assert_eq!(db.account_by_name("ALICE").unwrap().unwrap().id, id);
        db.add_token("secret", id, "refresh", 10, 100).unwrap();
        assert_eq!(db.token_account("secret", "refresh", 50).unwrap(), Some(id));
        assert_eq!(db.token_account("secret", "web", 50).unwrap(), None);
        assert_eq!(db.token_account("secret", "refresh", 100).unwrap(), None, "expired");
        assert!(db.remove_token("secret").unwrap());
        let server = db.add_server("Test", "bf2r_key", 1).unwrap();
        assert_eq!(db.server_by_key("bf2r_key").unwrap().unwrap().id, server);
        assert!(db.server_by_key("other").unwrap().is_none());
        assert!(db.add_round(server, "r1", "karkand", "gpm_cq", 1, 1, 5).unwrap());
        assert!(!db.add_round(server, "r1", "karkand", "gpm_cq", 1, 1, 5).unwrap(), "counted once");
        let round = PlayerRound {
            account: id,
            name: "Alice".into(),
            team: 1,
            score: 40,
            kills: 5,
            deaths: 2,
            captures: 1,
            seconds: 600.0,
            kits: vec![Tally { name: "assault".into(), seconds: 600.0, kills: 0 }],
            vehicles: vec![],
            weapons: vec![Tally { name: "M16A2".into(), seconds: 0.0, kills: 5 }],
        };
        assert_eq!(db.add_player_round(&round, Some(true), 55, 6).unwrap(), 55);
        assert_eq!(db.add_player_round(&round, Some(false), 45, 7).unwrap(), 100);
        let career = db.career(id).unwrap();
        assert_eq!((career.rounds, career.wins, career.losses, career.kills, career.score), (2, 1, 1, 10, 80));
        assert_eq!(career.kits[0].seconds, 1200.0);
        assert_eq!(career.weapons[0].kills, 10);
        let board = db.leaderboard("kd", 10, &progression).unwrap();
        assert_eq!(board[0].name, "Alice");
        db.note_ticket(id, "fp", 100).unwrap();
        assert!(db.ticket_since(id, "fp", 50).unwrap());
        assert!(!db.ticket_since(id, "fp", 150).unwrap());
        assert!(!db.ticket_since(id, "other", 0).unwrap());
    }
}
