//! The master's SQLite database: accounts, login tokens, ranked servers, career stats, and
//! the admin side (roles, two-factor authentication, bans, settings, the audit log).
//!
//! Secrets are stored hashed: passwords with Argon2id (see `auth`), refresh tokens, web
//! sessions and server API keys with BLAKE3 (they are 256-bit random, so a fast hash is
//! enough), recovery codes with BLAKE3 too (80 random bits, see `totp`). TOTP secrets can't
//! be hashed (the master computes codes from them), so they are encrypted (`totp::seal`).
//!
//! **Schema versions.** [`SCHEMA`] is the original schema (version 0; `IF NOT EXISTS`, so a
//! no-op on an existing database). [`MIGRATIONS`] upgrade it one version at a time, each in a
//! transaction that also bumps `PRAGMA user_version`. [`Db::open`] runs whatever is missing,
//! so an existing database upgrades in place on the first start of a newer master, and an
//! older master refuses a newer database rather than misreading it.

use std::path::Path;

use game_auth::{
    api::{CareerStats, LeaderboardEntry, Profile, RoundReport, RoundResult, Tally},
    ranks::Progression,
};
#[cfg(test)]
use game_auth::api::PlayerRound;
use rusqlite::{Connection, OptionalExtension, params};

pub struct Db {
    conn: Connection,
}

/// What an account may do on the master.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Role {
    Player,
    /// A master admin: the admin pages, once two-factor authentication is set up and the web
    /// session passed it.
    Admin,
}

impl Role {
    pub fn as_str(self) -> &'static str {
        match self {
            Role::Player => "player",
            Role::Admin => "admin",
        }
    }

    fn parse(text: &str) -> Self {
        if text == "admin" { Role::Admin } else { Role::Player }
    }
}

/// An account's ban.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Ban {
    pub reason: String,
    /// Seconds since 1970; 0 for a permanent ban.
    pub expires: u64,
    /// Who banned (an admin's name).
    pub by: String,
    pub at: u64,
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
    pub email: Option<String>,
    pub last_login: u64,
    pub role: Role,
    /// Two-factor authentication is set up.
    pub totp_enabled: bool,
    /// The last ban set, if any (it may have run out: see [`Account::active_ban`]).
    pub ban: Option<Ban>,
    /// An admin set a one-time password: the next login has to choose a new one.
    pub must_change_password: bool,
}

impl Account {
    /// The ban in force at `now`, if any.
    pub fn active_ban(&self, now: u64) -> Option<&Ban> {
        self.ban.as_ref().filter(|b| b.expires == 0 || b.expires > now)
    }

    /// Why this account can't log in, refresh or get tickets at `now`, if it can't.
    pub fn refusal(&self, now: u64) -> Option<String> {
        if self.disabled {
            return Some("This account is disabled.".into());
        }
        let ban = self.active_ban(now)?;
        let until = if ban.expires == 0 { String::new() } else { format!(" until {}", crate::web::format_time(ban.expires)) };
        Some(format!("This account is banned{until}. Reason: {}", ban.reason))
    }

    pub fn is_admin(&self) -> bool {
        self.role == Role::Admin
    }
}

/// A ranked server row.
#[derive(Clone, Debug)]
pub struct RankedServer {
    pub id: u64,
    pub name: String,
    pub public_key: String,
    pub created: u64,
    pub last_seen: u64,
    /// Its API key is refused while disabled.
    pub disabled: bool,
    /// `ip:port` players reach it on, from its last heartbeat.
    pub last_address: String,
}

/// Who did something, for the audit log.
#[derive(Clone, Debug)]
pub struct Actor {
    pub id: Option<u64>,
    pub name: String,
    pub ip: String,
}

impl Actor {
    /// The `master` command line on the master's own machine.
    pub fn command_line() -> Self {
        Self { id: None, name: "(command line)".into(), ip: String::new() }
    }

    pub fn account(account: &Account, ip: std::net::IpAddr) -> Self {
        Self { id: Some(account.id), name: account.name.clone(), ip: ip.to_string() }
    }
}

/// What an audit row is about.
#[derive(Clone, Debug, Default)]
pub struct Target {
    /// `account`, `server`, `setting` or empty.
    pub kind: &'static str,
    pub id: Option<u64>,
    /// A name for reading the log (the account's or server's name at the time).
    pub name: String,
}

impl Target {
    pub fn account(account: &Account) -> Self {
        Self { kind: "account", id: Some(account.id), name: account.name.clone() }
    }

    pub fn server(id: u64, name: &str) -> Self {
        Self { kind: "server", id: Some(id), name: name.to_string() }
    }

    pub fn setting(name: &str) -> Self {
        Self { kind: "setting", id: None, name: name.to_string() }
    }
}

/// An audit log row.
#[derive(Clone, Debug)]
pub struct AuditEntry {
    pub id: u64,
    pub at: u64,
    pub actor_id: Option<u64>,
    pub actor: String,
    pub ip: String,
    pub action: String,
    pub target_kind: String,
    pub target_id: Option<u64>,
    pub target: String,
    pub detail: String,
}

/// Numbers for the admin overview.
#[derive(Clone, Debug, Default)]
pub struct Counts {
    pub accounts: u64,
    pub admins: u64,
    pub banned: u64,
    pub new_accounts_24h: u64,
    pub ranked_servers: u64,
    pub disabled_servers: u64,
    pub rounds_24h: u64,
}

/// An account's two-factor state.
#[derive(Clone, Debug, Default)]
pub struct TotpState {
    /// Sealed (`totp::seal`).
    pub secret: Option<String>,
    /// A secret shown on the enrolment page and not confirmed with a code yet (sealed).
    pub pending: Option<String>,
    pub last_step: u64,
}

/// Version 0, the original schema.
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

/// Schema upgrades: `MIGRATIONS[n]` takes a database from version `n` to `n + 1`. Never edit
/// one that shipped; add another.
pub const MIGRATIONS: &[&str] = &[
    // 1: master admins (role, two-factor authentication, recovery codes), bans, one-time
    // passwords, web sessions that passed 2FA, ranked servers that can be disabled and show
    // their address, runtime settings, and the append-only audit log.
    "
ALTER TABLE accounts ADD COLUMN role TEXT NOT NULL DEFAULT 'player';
ALTER TABLE accounts ADD COLUMN totp_secret TEXT;
ALTER TABLE accounts ADD COLUMN totp_pending TEXT;
ALTER TABLE accounts ADD COLUMN totp_last_step INTEGER NOT NULL DEFAULT 0;
ALTER TABLE accounts ADD COLUMN banned INTEGER NOT NULL DEFAULT 0;
ALTER TABLE accounts ADD COLUMN ban_reason TEXT NOT NULL DEFAULT '';
ALTER TABLE accounts ADD COLUMN ban_expires INTEGER NOT NULL DEFAULT 0;
ALTER TABLE accounts ADD COLUMN ban_by TEXT NOT NULL DEFAULT '';
ALTER TABLE accounts ADD COLUMN ban_at INTEGER NOT NULL DEFAULT 0;
ALTER TABLE accounts ADD COLUMN must_change_password INTEGER NOT NULL DEFAULT 0;
ALTER TABLE tokens ADD COLUMN mfa INTEGER NOT NULL DEFAULT 0;
CREATE INDEX IF NOT EXISTS tokens_account ON tokens(account_id);
ALTER TABLE servers ADD COLUMN disabled INTEGER NOT NULL DEFAULT 0;
ALTER TABLE servers ADD COLUMN last_address TEXT NOT NULL DEFAULT '';
CREATE INDEX IF NOT EXISTS rounds_ended ON rounds(ended);
CREATE TABLE recovery_codes (
    account_id INTEGER NOT NULL REFERENCES accounts(id) ON DELETE CASCADE,
    code_hash TEXT NOT NULL,
    PRIMARY KEY (account_id, code_hash)
);
CREATE TABLE settings (
    key TEXT PRIMARY KEY,
    value TEXT NOT NULL
);
CREATE TABLE audit (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    at INTEGER NOT NULL,
    actor_id INTEGER,
    actor TEXT NOT NULL,
    ip TEXT NOT NULL DEFAULT '',
    action TEXT NOT NULL,
    target_kind TEXT NOT NULL DEFAULT '',
    target_id INTEGER,
    target TEXT NOT NULL DEFAULT '',
    detail TEXT NOT NULL DEFAULT ''
);
CREATE INDEX audit_target ON audit(target_kind, target_id);
CREATE TRIGGER audit_no_update BEFORE UPDATE ON audit BEGIN SELECT RAISE(ABORT, 'the audit log is append-only'); END;
CREATE TRIGGER audit_no_delete BEFORE DELETE ON audit BEGIN SELECT RAISE(ABORT, 'the audit log is append-only'); END;
",
];

/// The schema version this master writes.
pub const SCHEMA_VERSION: u32 = MIGRATIONS.len() as u32;

/// Brings `conn` up to [`SCHEMA_VERSION`]; the version it was at.
fn migrate(conn: &mut Connection) -> rusqlite::Result<u32> {
    conn.execute_batch(SCHEMA)?;
    let from: u32 = conn.query_row("PRAGMA user_version", [], |r| r.get(0))?;
    if from > SCHEMA_VERSION {
        return Err(rusqlite::Error::InvalidParameterName(format!(
            "the database is schema version {from}, newer than this master understands ({SCHEMA_VERSION}); run a newer master"
        )));
    }
    for (version, sql) in MIGRATIONS.iter().enumerate().skip(from as usize) {
        let tx = conn.transaction()?;
        tx.execute_batch(sql)?;
        tx.pragma_update(None, "user_version", version as u32 + 1)?;
        tx.commit()?;
    }
    Ok(from)
}

const ACCOUNT_COLUMNS: &str = "id, name, password_hash, created, xp, disabled, email, last_login, role, totp_secret IS NOT NULL, \
     banned, ban_reason, ban_expires, ban_by, ban_at, must_change_password";

fn account_row(r: &rusqlite::Row) -> rusqlite::Result<Account> {
    let banned = r.get::<_, i64>(10)? != 0;
    Ok(Account {
        id: r.get::<_, i64>(0)? as u64,
        name: r.get(1)?,
        password_hash: r.get(2)?,
        created: r.get::<_, i64>(3)? as u64,
        xp: r.get::<_, i64>(4)? as u64,
        disabled: r.get::<_, i64>(5)? != 0,
        email: r.get(6)?,
        last_login: r.get::<_, i64>(7)? as u64,
        role: Role::parse(&r.get::<_, String>(8)?),
        totp_enabled: r.get(9)?,
        ban: if banned {
            Some(Ban { reason: r.get(11)?, expires: r.get::<_, i64>(12)? as u64, by: r.get(13)?, at: r.get::<_, i64>(14)? as u64 })
        } else {
            None
        },
        must_change_password: r.get::<_, i64>(15)? != 0,
    })
}

/// SQL: account `a` isn't under a ban at `:now`.
const NOT_BANNED: &str = "NOT (a.banned = 1 AND (a.ban_expires = 0 OR a.ban_expires > :now))";

const SERVER_COLUMNS: &str = "id, name, public_key, created, last_seen, disabled, last_address";

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
        let mut conn = Connection::open(path)?;
        conn.pragma_update(None, "journal_mode", "WAL")?;
        // Readable by the master's own user only: it holds password hashes and the sealed
        // TOTP secrets. New -wal and -shm files get the database file's mode, but ones left
        // by an older master keep theirs, so all three are set.
        #[cfg(unix)]
        for suffix in ["", "-wal", "-shm"] {
            use std::os::unix::fs::PermissionsExt;
            let file = std::path::PathBuf::from(format!("{}{suffix}", path.display()));
            if file.exists() {
                let _ = std::fs::set_permissions(&file, std::fs::Permissions::from_mode(0o600));
            }
        }
        conn.busy_timeout(std::time::Duration::from_secs(5))?;
        let from = migrate(&mut conn)?;
        if from != SCHEMA_VERSION {
            println!("database {}: schema upgraded from version {from} to {SCHEMA_VERSION}", path.display());
        }
        Ok(Self { conn })
    }

    #[cfg(test)]
    pub fn in_memory() -> rusqlite::Result<Self> {
        let mut conn = Connection::open_in_memory()?;
        migrate(&mut conn)?;
        Ok(Self { conn })
    }

    /// The database's schema version.
    #[cfg(test)]
    pub fn schema_version(&self) -> rusqlite::Result<u32> {
        self.conn.query_row("PRAGMA user_version", [], |r| r.get(0))
    }

    #[cfg(test)]
    pub fn raw(&self) -> &Connection {
        &self.conn
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
        self.conn.query_row(&format!("SELECT {ACCOUNT_COLUMNS} FROM accounts WHERE {clause}"), [value], account_row).optional()
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

    /// Sets a password; `one_time` makes the next login choose a new one.
    pub fn set_password(&self, id: u64, password_hash: &str, one_time: bool) -> rusqlite::Result<()> {
        self.conn.execute(
            "UPDATE accounts SET password_hash = ?2, must_change_password = ?3 WHERE id = ?1",
            params![id as i64, password_hash, one_time as i64],
        )?;
        Ok(())
    }

    pub fn account_count(&self) -> rusqlite::Result<u64> {
        self.conn.query_row("SELECT COUNT(*) FROM accounts", [], |r| r.get::<_, i64>(0)).map(|n| n as u64)
    }

    /// Accounts whose name contains `query` (or whose id it is, `42` or `#42`): admins first,
    /// then by name. An empty query lists the newest accounts.
    pub fn search_accounts(&self, query: &str, limit: usize) -> rusqlite::Result<Vec<Account>> {
        let query = query.trim();
        let limit = limit.min(500) as i64;
        if query.is_empty() {
            let mut statement = self.conn.prepare(&format!("SELECT {ACCOUNT_COLUMNS} FROM accounts ORDER BY id DESC LIMIT ?1"))?;
            let rows = statement.query_map(params![limit], account_row)?;
            return rows.collect();
        }
        let escaped = query.to_ascii_lowercase().replace('\\', "\\\\").replace('%', "\\%").replace('_', "\\_");
        let id = query.strip_prefix('#').unwrap_or(query).parse::<i64>().unwrap_or(-1);
        let mut statement = self.conn.prepare(&format!(
            "SELECT {ACCOUNT_COLUMNS} FROM accounts WHERE name_key LIKE ?1 ESCAPE '\\' OR id = ?2 ORDER BY role = 'admin' DESC, name_key LIMIT ?3"
        ))?;
        let rows = statement.query_map(params![format!("%{escaped}%"), id, limit], account_row)?;
        rows.collect()
    }

    pub fn set_role(&self, id: u64, role: Role) -> rusqlite::Result<()> {
        self.conn.execute("UPDATE accounts SET role = ?2 WHERE id = ?1", params![id as i64, role.as_str()])?;
        Ok(())
    }

    pub fn admins(&self) -> rusqlite::Result<Vec<Account>> {
        let mut statement = self.conn.prepare(&format!("SELECT {ACCOUNT_COLUMNS} FROM accounts WHERE role = 'admin' ORDER BY name_key"))?;
        let rows = statement.query_map([], account_row)?;
        rows.collect()
    }

    pub fn admin_count(&self) -> rusqlite::Result<u64> {
        self.conn.query_row("SELECT COUNT(*) FROM accounts WHERE role = 'admin'", [], |r| r.get::<_, i64>(0)).map(|n| n as u64)
    }

    /// Renames an account; `false` if another account has that name (in any case).
    pub fn rename(&self, id: u64, name: &str) -> rusqlite::Result<bool> {
        let key = name.to_ascii_lowercase();
        let taken = self
            .conn
            .query_row("SELECT 1 FROM accounts WHERE name_key = ?1 AND id != ?2", params![key, id as i64], |_| Ok(()))
            .optional()?
            .is_some();
        if taken {
            return Ok(false);
        }
        self.conn.execute("UPDATE accounts SET name = ?2, name_key = ?3 WHERE id = ?1", params![id as i64, name, key])?;
        Ok(true)
    }

    /// Bans (`Some`) or unbans (`None`) an account.
    pub fn set_ban(&self, id: u64, ban: Option<&Ban>) -> rusqlite::Result<()> {
        match ban {
            Some(ban) => self.conn.execute(
                "UPDATE accounts SET banned = 1, ban_reason = ?2, ban_expires = ?3, ban_by = ?4, ban_at = ?5 WHERE id = ?1",
                params![id as i64, ban.reason, to_i64(ban.expires), ban.by, to_i64(ban.at)],
            )?,
            None => self.conn.execute("UPDATE accounts SET banned = 0 WHERE id = ?1", params![id as i64])?,
        };
        Ok(())
    }

    // Two-factor authentication.

    pub fn totp_state(&self, id: u64) -> rusqlite::Result<TotpState> {
        self.conn
            .query_row("SELECT totp_secret, totp_pending, totp_last_step FROM accounts WHERE id = ?1", params![id as i64], |r| {
                Ok(TotpState { secret: r.get(0)?, pending: r.get(1)?, last_step: r.get::<_, i64>(2)? as u64 })
            })
            .optional()
            .map(Option::unwrap_or_default)
    }

    pub fn set_totp_pending(&self, id: u64, sealed: Option<&str>) -> rusqlite::Result<()> {
        self.conn.execute("UPDATE accounts SET totp_pending = ?2 WHERE id = ?1", params![id as i64, sealed])?;
        Ok(())
    }

    /// Turns two-factor authentication on with `sealed` (confirmed with the code of `step`,
    /// which is thereby used up) and replaces the recovery codes.
    pub fn enable_totp(&mut self, id: u64, sealed: &str, step: u64, recovery_hashes: &[String]) -> rusqlite::Result<()> {
        let tx = self.conn.transaction()?;
        tx.execute(
            "UPDATE accounts SET totp_secret = ?2, totp_pending = NULL, totp_last_step = ?3 WHERE id = ?1",
            params![id as i64, sealed, to_i64(step)],
        )?;
        replace_recovery_codes(&tx, id, recovery_hashes)?;
        tx.commit()
    }

    pub fn set_recovery_codes(&mut self, id: u64, hashes: &[String]) -> rusqlite::Result<()> {
        let tx = self.conn.transaction()?;
        replace_recovery_codes(&tx, id, hashes)?;
        tx.commit()
    }

    /// Marks the code of `step` used; `false` if that step (or a later one) already was, so
    /// two requests racing with the same code can't both pass.
    pub fn use_totp_step(&self, id: u64, step: u64) -> rusqlite::Result<bool> {
        Ok(self.conn.execute(
            "UPDATE accounts SET totp_last_step = ?2 WHERE id = ?1 AND totp_last_step < ?2",
            params![id as i64, to_i64(step)],
        )? == 1)
    }

    /// Uses up a recovery code; `false` if it isn't one of the account's (any more).
    pub fn use_recovery_code(&self, id: u64, hash: &str) -> rusqlite::Result<bool> {
        Ok(self.conn.execute("DELETE FROM recovery_codes WHERE account_id = ?1 AND code_hash = ?2", params![id as i64, hash])? == 1)
    }

    pub fn recovery_codes_left(&self, id: u64) -> rusqlite::Result<u64> {
        self.conn
            .query_row("SELECT COUNT(*) FROM recovery_codes WHERE account_id = ?1", params![id as i64], |r| r.get::<_, i64>(0))
            .map(|n| n as u64)
    }

    /// Turns two-factor authentication off (an admin enrols again on the next login).
    pub fn reset_totp(&mut self, id: u64) -> rusqlite::Result<()> {
        let tx = self.conn.transaction()?;
        tx.execute(
            "UPDATE accounts SET totp_secret = NULL, totp_pending = NULL, totp_last_step = 0 WHERE id = ?1",
            params![id as i64],
        )?;
        replace_recovery_codes(&tx, id, &[])?;
        tx.commit()
    }

    // Refresh tokens, web sessions and pending two-factor logins.

    pub fn add_token(&self, secret: &str, account: u64, kind: &str, now: u64, expires: u64) -> rusqlite::Result<()> {
        self.add_session(secret, account, kind, now, expires, false)
    }

    /// A token that may have passed two-factor authentication (`mfa`): web sessions.
    pub fn add_session(&self, secret: &str, account: u64, kind: &str, now: u64, expires: u64, mfa: bool) -> rusqlite::Result<()> {
        self.conn.execute(
            "INSERT INTO tokens (token_hash, account_id, kind, created, expires, mfa) VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
            params![secret_hash(secret), account as i64, kind, to_i64(now), to_i64(expires), mfa as i64],
        )?;
        Ok(())
    }

    /// The account of a valid token of `kind`.
    pub fn token_account(&self, secret: &str, kind: &str, now: u64) -> rusqlite::Result<Option<u64>> {
        Ok(self.session(secret, kind, now)?.map(|(id, _)| id))
    }

    /// The account of a valid token of `kind`, and whether it passed two-factor
    /// authentication.
    pub fn session(&self, secret: &str, kind: &str, now: u64) -> rusqlite::Result<Option<(u64, bool)>> {
        self.conn
            .query_row(
                "SELECT account_id, mfa FROM tokens WHERE token_hash = ?1 AND kind = ?2 AND expires > ?3",
                params![secret_hash(secret), kind, to_i64(now)],
                |r| Ok((r.get::<_, i64>(0)? as u64, r.get::<_, i64>(1)? != 0)),
            )
            .optional()
    }

    /// Marks a session as having passed two-factor authentication; it ends by `expires` at
    /// the latest.
    pub fn upgrade_session(&self, secret: &str, expires: u64) -> rusqlite::Result<()> {
        self.conn.execute(
            "UPDATE tokens SET mfa = 1, expires = MIN(expires, ?2) WHERE token_hash = ?1",
            params![secret_hash(secret), to_i64(expires)],
        )?;
        Ok(())
    }

    /// Ends an account's tokens of these kinds (every kind if empty), except `keep` (a token
    /// secret: the web session doing this). How many ended.
    pub fn revoke_tokens(&self, account: u64, kinds: &[&str], keep: Option<&str>) -> rusqlite::Result<usize> {
        let keep = keep.map(secret_hash).unwrap_or_default();
        if kinds.is_empty() {
            return self.conn.execute("DELETE FROM tokens WHERE account_id = ?1 AND token_hash != ?2", params![account as i64, keep]);
        }
        let mut ended = 0;
        for kind in kinds {
            ended += self.conn.execute(
                "DELETE FROM tokens WHERE account_id = ?1 AND kind = ?2 AND token_hash != ?3",
                params![account as i64, kind, keep],
            )?;
        }
        Ok(ended)
    }

    /// An account's valid tokens per kind.
    pub fn token_counts(&self, account: u64, now: u64) -> rusqlite::Result<Vec<(String, u64)>> {
        let mut statement =
            self.conn.prepare("SELECT kind, COUNT(*) FROM tokens WHERE account_id = ?1 AND expires > ?2 GROUP BY kind ORDER BY kind")?;
        let rows = statement.query_map(params![account as i64, to_i64(now)], |r| Ok((r.get(0)?, r.get::<_, i64>(1)? as u64)))?;
        rows.collect()
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

    /// Forgets tickets issued before `before` (S23): only recent ones matter (stats acceptance
    /// looks back `ticket_window_hours`), and a logged-in account can otherwise grow this
    /// table forever by requesting tickets for arbitrary server fingerprints.
    pub fn prune_tickets(&self, before: u64) -> rusqlite::Result<()> {
        self.conn.execute("DELETE FROM tickets WHERE issued < ?1", params![to_i64(before)])?;
        Ok(())
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
            disabled: r.get::<_, i64>(5)? != 0,
            last_address: r.get(6)?,
        })
    }

    pub fn server_by_key(&self, api_key: &str) -> rusqlite::Result<Option<RankedServer>> {
        self.conn
            .query_row(&format!("SELECT {SERVER_COLUMNS} FROM servers WHERE api_key_hash = ?1"), params![secret_hash(api_key)], Self::server_row)
            .optional()
    }

    pub fn server(&self, id: u64) -> rusqlite::Result<Option<RankedServer>> {
        self.conn.query_row(&format!("SELECT {SERVER_COLUMNS} FROM servers WHERE id = ?1"), params![id as i64], Self::server_row).optional()
    }

    pub fn servers(&self) -> rusqlite::Result<Vec<RankedServer>> {
        let mut statement = self.conn.prepare(&format!("SELECT {SERVER_COLUMNS} FROM servers ORDER BY id"))?;
        let rows = statement.query_map([], Self::server_row)?;
        rows.collect()
    }

    pub fn remove_server(&self, id: u64) -> rusqlite::Result<bool> {
        Ok(self.conn.execute("DELETE FROM servers WHERE id = ?1", params![id as i64])? > 0)
    }

    /// Gives a ranked server a new API key (the old one stops working); `false` if unknown.
    pub fn set_server_key(&self, id: u64, api_key: &str) -> rusqlite::Result<bool> {
        Ok(self.conn.execute("UPDATE servers SET api_key_hash = ?2 WHERE id = ?1", params![id as i64, secret_hash(api_key)])? > 0)
    }

    pub fn set_server_disabled(&self, id: u64, disabled: bool) -> rusqlite::Result<bool> {
        Ok(self.conn.execute("UPDATE servers SET disabled = ?2 WHERE id = ?1", params![id as i64, disabled as i64])? > 0)
    }

    pub fn server_seen(&self, id: u64, public_key: &str, address: &str, now: u64) -> rusqlite::Result<()> {
        self.conn.execute(
            "UPDATE servers SET public_key = ?2, last_address = ?3, last_seen = ?4 WHERE id = ?1",
            params![id as i64, public_key, address, to_i64(now)],
        )?;
        Ok(())
    }

    // Settings changed at runtime on the admin pages (the config file has the defaults).

    pub fn setting(&self, key: &str) -> rusqlite::Result<Option<String>> {
        self.conn.query_row("SELECT value FROM settings WHERE key = ?1", params![key], |r| r.get(0)).optional()
    }

    pub fn set_setting(&self, key: &str, value: &str) -> rusqlite::Result<()> {
        self.conn.execute(
            "INSERT INTO settings (key, value) VALUES (?1, ?2) ON CONFLICT(key) DO UPDATE SET value = excluded.value",
            params![key, value],
        )?;
        Ok(())
    }

    // The audit log: append-only (triggers refuse UPDATE and DELETE).

    pub fn audit(&self, actor: &Actor, action: &str, target: &Target, detail: &str, now: u64) -> rusqlite::Result<()> {
        self.conn.execute(
            "INSERT INTO audit (at, actor_id, actor, ip, action, target_kind, target_id, target, detail) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)",
            params![
                to_i64(now),
                actor.id.map(|id| id as i64),
                actor.name,
                actor.ip,
                action,
                target.kind,
                target.id.map(|id| id as i64),
                target.name,
                detail
            ],
        )?;
        Ok(())
    }

    /// The newest audit rows (older than row `before`, if given), optionally only those
    /// about one target (`kind`, `id`).
    pub fn audit_log(&self, before: Option<u64>, target: Option<(&str, u64)>, limit: usize) -> rusqlite::Result<Vec<AuditEntry>> {
        let mut statement = self.conn.prepare(
            "SELECT id, at, actor_id, actor, ip, action, target_kind, target_id, target, detail FROM audit
             WHERE id < ?1 AND (?2 = '' OR (target_kind = ?2 AND target_id = ?3)) ORDER BY id DESC LIMIT ?4",
        )?;
        let (kind, id) = target.unwrap_or(("", 0));
        let rows = statement.query_map(params![to_i64(before.unwrap_or(u64::MAX)), kind, id as i64, limit.min(500) as i64], |r| {
            Ok(AuditEntry {
                id: r.get::<_, i64>(0)? as u64,
                at: r.get::<_, i64>(1)? as u64,
                actor_id: r.get::<_, Option<i64>>(2)?.map(|id| id as u64),
                actor: r.get(3)?,
                ip: r.get(4)?,
                action: r.get(5)?,
                target_kind: r.get(6)?,
                target_id: r.get::<_, Option<i64>>(7)?.map(|id| id as u64),
                target: r.get(8)?,
                detail: r.get(9)?,
            })
        })?;
        rows.collect()
    }

    /// Numbers for the admin overview.
    pub fn counts(&self, now: u64) -> rusqlite::Result<Counts> {
        let count = |sql: &str, params: &[&dyn rusqlite::ToSql]| self.conn.query_row(sql, params, |r| r.get::<_, i64>(0)).map(|n| n as u64);
        let day_ago = to_i64(now.saturating_sub(86_400));
        let now = to_i64(now);
        Ok(Counts {
            accounts: count("SELECT COUNT(*) FROM accounts", &[])?,
            admins: count("SELECT COUNT(*) FROM accounts WHERE role = 'admin'", &[])?,
            banned: count("SELECT COUNT(*) FROM accounts WHERE banned = 1 AND (ban_expires = 0 OR ban_expires > ?1)", &[&now])?,
            new_accounts_24h: count("SELECT COUNT(*) FROM accounts WHERE created >= ?1", &[&day_ago])?,
            ranked_servers: count("SELECT COUNT(*) FROM servers", &[])?,
            disabled_servers: count("SELECT COUNT(*) FROM servers WHERE disabled = 1", &[])?,
            rounds_24h: count("SELECT COUNT(*) FROM rounds WHERE ended >= ?1", &[&day_ago])?,
        })
    }

    // Stats.

    /// Records a round and credits the players who get counted for it, all in one
    /// transaction (S19): the round is only marked reported if every player's credit also
    /// commits, so a failure partway through leaves nothing counted rather than the round
    /// permanently marked done with some players missed (a retry would then see it as
    /// already reported and skip them for good). `duplicate` on the result if this server
    /// reported this round before; nothing is re-applied in that case.
    pub fn record_round(
        &mut self,
        server: u64,
        server_fingerprint: &str,
        report: &RoundReport,
        since: u64,
        now: u64,
        progression: &Progression,
    ) -> rusqlite::Result<RoundResult> {
        let tx = self.conn.transaction()?;
        let mut result = RoundResult::default();
        let fresh = tx.execute(
            "INSERT OR IGNORE INTO rounds (server_id, round_id, level, mode, winner, players, ended) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
            params![server as i64, report.round_id, report.level, report.mode, report.winner as i64, report.players.len() as i64, to_i64(now)],
        )? > 0;
        if fresh {
            let mut counted = std::collections::HashSet::new();
            for player in &report.players {
                let account = tx
                    .query_row(
                        "SELECT id, xp, disabled OR (banned = 1 AND (ban_expires = 0 OR ban_expires > ?2)) FROM accounts WHERE id = ?1",
                        params![player.account as i64, to_i64(now)],
                        |r| Ok((r.get::<_, i64>(0)? as u64, r.get::<_, i64>(1)? as u64, r.get::<_, i64>(2)? != 0)),
                    )
                    .optional()?;
                let joined = tx
                    .query_row(
                        "SELECT 1 FROM tickets WHERE account_id = ?1 AND server = ?2 AND issued >= ?3",
                        params![player.account as i64, server_fingerprint, to_i64(since)],
                        |_| Ok(()),
                    )
                    .optional()?
                    .is_some();
                let usable = account.filter(|(id, _, disabled)| !disabled && joined && !counted.contains(id));
                let Some((id, xp, _)) = usable else {
                    result.rejected.push(player.account);
                    continue;
                };
                counted.insert(id);
                let won = (report.winner != 0).then_some(player.team == report.winner);
                let (wins, losses) = match won {
                    Some(true) => (1, 0),
                    Some(false) => (0, 1),
                    None => (0, 0),
                };
                let gained = progression.round_xp(player.score, player.seconds, won == Some(true));
                tx.execute(
                    "UPDATE careers SET rounds = rounds + 1, wins = wins + ?2, losses = losses + ?3, score = score + ?4,
                     kills = kills + ?5, deaths = deaths + ?6, captures = captures + ?7, seconds = seconds + ?8, last_played = ?9
                     WHERE account_id = ?1",
                    params![id as i64, wins, losses, player.score as i64, player.kills as i64, player.deaths as i64, player.captures as i64, player.seconds, to_i64(now)],
                )?;
                for (kind, tallies) in [("kit", &player.kits), ("vehicle", &player.vehicles), ("weapon", &player.weapons)] {
                    for tally in tallies {
                        tx.execute(
                            "INSERT INTO tallies (account_id, kind, name, seconds, kills) VALUES (?1, ?2, ?3, ?4, ?5)
                             ON CONFLICT(account_id, kind, name) DO UPDATE SET seconds = seconds + excluded.seconds, kills = kills + excluded.kills",
                            params![id as i64, kind, tally.name, tally.seconds, tally.kills as i64],
                        )?;
                    }
                }
                tx.execute("UPDATE accounts SET xp = xp + ?2 WHERE id = ?1", params![id as i64, to_i64(gained)])?;
                let total = xp + gained;
                if progression.rank_index(total) > progression.rank_index(xp) {
                    result.promotions.push((id, progression.info(total).name));
                }
                result.accepted += 1;
            }
        } else {
            result.duplicate = true;
        }
        tx.commit()?;
        Ok(result)
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
             WHERE a.disabled = 0 AND {NOT_BANNED} AND c.rounds > 0 ORDER BY {order} LIMIT :limit"
        ))?;
        let named = rusqlite::named_params! { ":limit": limit.min(500) as i64, ":now": to_i64(game_auth::unix_now()) };
        let rows = statement.query_map(named, |r| {
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

/// Replaces an account's recovery codes (inside the caller's transaction).
fn replace_recovery_codes(conn: &Connection, id: u64, hashes: &[String]) -> rusqlite::Result<()> {
    conn.execute("DELETE FROM recovery_codes WHERE account_id = ?1", params![id as i64])?;
    for hash in hashes {
        conn.execute("INSERT OR IGNORE INTO recovery_codes (account_id, code_hash) VALUES (?1, ?2)", params![id as i64, hash])?;
    }
    Ok(())
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

        db.note_ticket(id, "fp", 100).unwrap();
        assert!(db.ticket_since(id, "fp", 50).unwrap());
        assert!(!db.ticket_since(id, "fp", 150).unwrap());
        assert!(!db.ticket_since(id, "other", 0).unwrap());

        let player = PlayerRound {
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
        let report = RoundReport { round_id: "r1".into(), level: "karkand".into(), mode: "gpm_cq".into(), winner: 1, seconds: 600.0, players: vec![player.clone()] };
        let result = db.record_round(server, "fp", &report, 50, 6, &progression).unwrap();
        assert_eq!((result.accepted, result.duplicate, result.rejected.len()), (1, false, 0));
        let again = db.record_round(server, "fp", &report, 50, 7, &progression).unwrap();
        assert!(again.duplicate && again.accepted == 0, "counted once, and nothing is re-applied for a duplicate");

        let mut second = report.clone();
        second.round_id = "r2".into();
        second.winner = 2; // Alice's team (1) lost this one.
        assert_eq!(db.record_round(server, "fp", &second, 50, 8, &progression).unwrap().accepted, 1);

        let career = db.career(id).unwrap();
        assert_eq!((career.rounds, career.wins, career.losses, career.kills, career.score), (2, 1, 1, 10, 80));
        assert_eq!(career.kits[0].seconds, 1200.0);
        assert_eq!(career.weapons[0].kills, 10);
        let board = db.leaderboard("kd", 10, &progression).unwrap();
        assert_eq!(board[0].name, "Alice");

        // No ticket for this server: not counted, and the round itself isn't wasted (a real
        // round from an account that does have one still gets to be "fresh").
        let bob = db.create_account("Bob", None, "hash", 1).unwrap().unwrap();
        let mut third = report.clone();
        third.round_id = "r3".into();
        third.players[0].account = bob;
        let result3 = db.record_round(server, "fp", &third, 50, 9, &progression).unwrap();
        assert_eq!((result3.accepted, result3.rejected), (0, vec![bob]));
    }

    #[test]
    fn an_old_database_upgrades_in_place() {
        let path = std::env::temp_dir().join(format!("bf2_master_migrate_{}.sqlite", std::process::id()));
        let _ = std::fs::remove_file(&path);
        {
            // A database as the first master wrote it: the original schema, version 0.
            let old = Connection::open(&path).unwrap();
            old.execute_batch(SCHEMA).unwrap();
            old.execute("INSERT INTO accounts (name, name_key, password_hash, created, xp) VALUES ('Alice', 'alice', 'hash', 5, 900)", []).unwrap();
            old.execute("INSERT INTO careers (account_id, kills) VALUES (1, 7)", []).unwrap();
            old.execute("INSERT INTO tokens (token_hash, account_id, kind, created, expires) VALUES (?1, 1, 'refresh', 1, 99999999999)", [secret_hash("tok")]).unwrap();
            old.execute("INSERT INTO servers (name, api_key_hash, created) VALUES ('Old', ?1, 1)", [secret_hash("bf2r_old")]).unwrap();
            let version: u32 = old.query_row("PRAGMA user_version", [], |r| r.get(0)).unwrap();
            assert_eq!(version, 0);
        }
        let mut db = Db::open(&path).unwrap();
        assert_eq!(db.schema_version().unwrap(), SCHEMA_VERSION);
        let alice = db.account_by_name("alice").unwrap().unwrap();
        assert_eq!((alice.xp, alice.role, alice.totp_enabled, alice.ban.is_none()), (900, Role::Player, false, true));
        assert_eq!(db.career(alice.id).unwrap().kills, 7);
        assert_eq!(db.token_account("tok", "refresh", 10).unwrap(), Some(alice.id), "logins survive");
        let server = db.server_by_key("bf2r_old").unwrap().unwrap();
        assert!(!server.disabled && server.last_address.is_empty());
        // The new tables work.
        db.set_role(alice.id, Role::Admin).unwrap();
        db.enable_totp(alice.id, "c1:00", 3, &["h".into()]).unwrap();
        db.audit(&Actor::command_line(), "promote", &Target::account(&alice), "", 10).unwrap();
        drop(db);
        // Opening again changes nothing.
        let db = Db::open(&path).unwrap();
        assert_eq!(db.schema_version().unwrap(), SCHEMA_VERSION);
        assert!(db.account(alice.id).unwrap().unwrap().totp_enabled);
        assert_eq!(db.audit_log(None, None, 10).unwrap().len(), 1);
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            for suffix in ["", "-wal", "-shm"] {
                let mode = std::fs::metadata(format!("{}{suffix}", path.display())).unwrap().permissions().mode();
                assert_eq!(mode & 0o777, 0o600, "{suffix}: readable by the master only");
            }
        }
        // A database from a newer master is refused rather than misread.
        db.raw().pragma_update(None, "user_version", SCHEMA_VERSION + 1).unwrap();
        drop(db);
        assert!(Db::open(&path).is_err());
        for suffix in ["", "-wal", "-shm"] {
            let _ = std::fs::remove_file(format!("{}{suffix}", path.display()));
        }
    }

    #[test]
    fn bans_and_two_factor_state() {
        let mut db = Db::in_memory().unwrap();
        let id = db.create_account("Bob", None, "hash", 1).unwrap().unwrap();
        let ban = Ban { reason: "cheating".into(), expires: 100, by: "root".into(), at: 1 };
        db.set_ban(id, Some(&ban)).unwrap();
        let bob = db.account(id).unwrap().unwrap();
        assert!(bob.refusal(50).unwrap().contains("cheating"));
        assert!(bob.refusal(100).is_none(), "ran out");
        db.set_ban(id, Some(&Ban { expires: 0, ..ban })).unwrap();
        assert!(db.account(id).unwrap().unwrap().refusal(u64::MAX / 2).is_some(), "permanent");
        db.set_ban(id, None).unwrap();
        assert!(db.account(id).unwrap().unwrap().refusal(50).is_none());

        db.enable_totp(id, "c1:00", 10, &["a".into(), "b".into()]).unwrap();
        assert!(!db.use_totp_step(id, 10).unwrap(), "the enrolment's step is used");
        assert!(db.use_totp_step(id, 11).unwrap());
        assert!(!db.use_totp_step(id, 11).unwrap(), "once");
        assert!(db.use_recovery_code(id, "a").unwrap());
        assert!(!db.use_recovery_code(id, "a").unwrap(), "once");
        assert_eq!(db.recovery_codes_left(id).unwrap(), 1);
        db.reset_totp(id).unwrap();
        assert!(!db.account(id).unwrap().unwrap().totp_enabled);
        assert_eq!(db.recovery_codes_left(id).unwrap(), 0);

        db.add_session("s1", id, "web", 1, 1000, false).unwrap();
        db.add_session("s2", id, "web", 1, 1000, true).unwrap();
        db.add_token("r1", id, "refresh", 1, 1000).unwrap();
        assert_eq!(db.session("s2", "web", 5).unwrap(), Some((id, true)));
        db.upgrade_session("s1", 500).unwrap();
        assert_eq!(db.session("s1", "web", 5).unwrap(), Some((id, true)));
        assert_eq!(db.session("s1", "web", 600).unwrap(), None, "an upgraded session gets the shorter lifetime");
        assert_eq!(db.revoke_tokens(id, &["web"], Some("s2")).unwrap(), 1);
        assert_eq!(db.revoke_tokens(id, &[], None).unwrap(), 2);
        assert!(db.token_counts(id, 5).unwrap().is_empty());
    }
}
