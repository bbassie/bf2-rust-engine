//! The REST API (see `game_auth::api` for the endpoints and types).

use game_auth::{
    api::{Credentials, MasterInfo, RefreshRequest, RoundReport, ServerEntry, ServerHeartbeat, Session, TicketRequest, TicketResponse},
    fingerprint,
    identity::normalize_fingerprint,
    token::{self, Claims, TOKEN_VERSION, TokenKind, verify_token},
    unhex_array, unix_now, validate_account_name, validate_password,
};

use crate::{
    auth::{self, check_password, dummy_hash, hash_password, new_secret},
    db::{Account, RankedServer},
    http::{Master, Method, Req, Resp, lock},
};

/// Refresh tokens of the game client.
pub const REFRESH: &str = "refresh";
/// Web sessions (cookies).
pub const WEB: &str = "web";

type Answer = Result<Resp, Resp>;

fn internal(err: impl std::fmt::Display) -> Resp {
    eprintln!("error: {err}");
    Resp::error(500, "internal error")
}

pub fn handle(master: &Master, req: &Req, path: &str) -> Answer {
    match (&req.method, path) {
        (Method::Get, "/info") => Ok(Resp::ok(&MasterInfo {
            name: master.config.name.clone(),
            public_key: master.key.public_hex(),
            fingerprint: master.key.fingerprint(),
            web_url: master.config.public_url.clone(),
        })),
        (Method::Post, "/register") => register(master, req),
        (Method::Post, "/login") => login(master, req),
        (Method::Post, "/refresh") => refresh(master, req),
        (Method::Post, "/logout") => {
            let body: RefreshRequest = req.json()?;
            lock(&master.db).remove_token(&body.refresh_token).map_err(internal)?;
            Ok(Resp::ok(&serde_json::json!({})))
        }
        (Method::Get, "/me") => {
            let account = session_account(master, req)?;
            let profile = lock(&master.db).profile(&account, &master.config.progression).map_err(internal)?;
            Ok(Resp::ok(&profile))
        }
        (Method::Post, "/ticket") => ticket(master, req),
        (Method::Get, "/leaderboard") => {
            let sort = req.query.get("sort").map_or("xp", String::as_str);
            let limit = req.query.get("limit").and_then(|l| l.parse().ok()).unwrap_or(50usize).clamp(1, 200);
            let board = lock(&master.db).leaderboard(sort, limit, &master.config.progression).map_err(internal)?;
            Ok(Resp::ok(&board))
        }
        (Method::Get, "/servers") => {
            // Paginated (S7): heartbeats are unauthenticated UDP, so the list can grow large;
            // don't hand it all out in one answer by default.
            let limit = req.query.get("limit").and_then(|l| l.parse().ok()).unwrap_or(100usize).clamp(1, 200);
            let offset = req.query.get("offset").and_then(|o| o.parse().ok()).unwrap_or(0usize);
            let page = lock(&master.list).entries().into_iter().skip(offset).take(limit).collect::<Vec<_>>();
            Ok(Resp::ok(&page))
        }
        (Method::Get, "/quickjoin") => {
            let ranked = match req.query.get("ranked").map(String::as_str) {
                Some("yes" | "true" | "1") => Some(true),
                Some("no" | "false" | "0") => Some(false),
                _ => None,
            };
            let region = req.query.get("region").cloned().unwrap_or_default();
            Ok(Resp::ok(&lock(&master.list).quick_join(ranked, &region)))
        }
        (Method::Post, "/server/heartbeat") => server_heartbeat(master, req),
        (Method::Post, "/server/round") => server_round(master, req),
        (Method::Get, other) if other.starts_with("/players/") => {
            let name = crate::http::url_decode(&other["/players/".len()..]);
            let db = lock(&master.db);
            let account = db.account_by_name(&name).map_err(internal)?.filter(|a| !a.disabled);
            let Some(account) = account else {
                return Err(Resp::error(404, "no such player"));
            };
            Ok(Resp::ok(&db.profile(&account, &master.config.progression).map_err(internal)?))
        }
        _ => Err(Resp::error(404, "no such API")),
    }
}

/// A new session token (and refresh token) for `account`.
pub fn issue_session(master: &Master, account: &Account) -> Result<Session, Resp> {
    let now = unix_now();
    let config = &master.config;
    let rank = config.progression.info(account.xp);
    let session_expires = now + config.session_minutes * 60;
    let claims = Claims {
        v: TOKEN_VERSION,
        kind: TokenKind::Session,
        sub: account.id,
        name: account.name.clone(),
        rank: rank.index,
        rank_name: rank.name.clone(),
        rank_short: rank.short.clone(),
        iat: now,
        exp: session_expires,
        aud: None,
        jti: game_auth::hex(&game_auth::random_bytes::<8>()),
    };
    let refresh_token = new_secret();
    let refresh_expires = now + config.refresh_days * 86_400;
    let db = lock(&master.db);
    db.add_token(&refresh_token, account.id, REFRESH, now, refresh_expires).map_err(internal)?;
    db.touch_login(account.id, now).map_err(internal)?;
    let _ = db.remove_expired(now);
    Ok(Session {
        session_token: token::sign(&master.key, &claims),
        session_expires,
        refresh_token,
        refresh_expires,
        profile: db.profile(account, &config.progression).map_err(internal)?,
    })
}

/// Checks name and password (with rate limits); the account on success.
pub fn authenticate(master: &Master, req: &Req, name: &str, password: &str) -> Result<Account, Resp> {
    // The address itself is blocked after too many failures (S17): this only ever punishes
    // the attacker's own traffic.
    if let Err(minutes) = lock(&master.limits).check_login(req.ip) {
        return Err(Resp::error(429, &format!("Too many failed logins from your address. Try again in {minutes} minutes.")));
    }
    // Repeated failures against one *name* only slow it down, never block it: anyone can
    // trigger those against someone else's name, so blocking would let an attacker lock the
    // real owner out just by failing a few logins.
    let delay = lock(&master.limits).name_delay(name);
    if !delay.is_zero() {
        std::thread::sleep(delay);
    }
    let account = lock(&master.db).account_by_name(name).map_err(internal)?;
    // Hashing takes a while: outside the database lock, and as long for unknown names.
    let hash = account.as_ref().map_or(dummy_hash(), |a| a.password_hash.as_str());
    let good = check_password(password, hash) && account.as_ref().is_some_and(|a| !a.disabled);
    let mut limits = lock(&master.limits);
    match account {
        Some(account) if good => {
            limits.login_succeeded(name);
            Ok(account)
        }
        _ => {
            limits.login_failed(req.ip, name);
            Err(Resp::error(401, "Wrong name or password."))
        }
    }
}

/// Validates and creates an account.
pub fn create_account(master: &Master, req: &Req, credentials: &Credentials) -> Result<Account, Resp> {
    if !master.config.allow_registration {
        return Err(Resp::error(403, "This master server doesn't take new accounts."));
    }
    let name = credentials.name.trim();
    validate_account_name(name).map_err(|err| Resp::error(400, &err))?;
    validate_password(&credentials.password).map_err(|err| Resp::error(400, &err))?;
    let email = credentials.email.as_deref().map(str::trim).filter(|e| !e.is_empty());
    if let Some(email) = email {
        auth::validate_email(email).map_err(|err| Resp::error(400, &err))?;
    }
    if !lock(&master.limits).register(req.ip) {
        return Err(Resp::error(429, "Too many new accounts from your address. Try again later."));
    }
    let hash = hash_password(&credentials.password);
    let db = lock(&master.db);
    let Some(id) = db.create_account(name, email, &hash, unix_now()).map_err(internal)? else {
        return Err(Resp::error(409, "That name is taken."));
    };
    println!("account {name} registered");
    db.account(id).map_err(internal)?.ok_or_else(|| Resp::error(500, "account vanished"))
}

fn register(master: &Master, req: &Req) -> Answer {
    let credentials: Credentials = req.json()?;
    let account = create_account(master, req, &credentials)?;
    Ok(Resp::ok(&issue_session(master, &account)?))
}

fn login(master: &Master, req: &Req) -> Answer {
    let credentials: Credentials = req.json()?;
    let account = authenticate(master, req, credentials.name.trim(), &credentials.password)?;
    Ok(Resp::ok(&issue_session(master, &account)?))
}

/// A refresh token for a new session; the old refresh token is used up.
fn refresh(master: &Master, req: &Req) -> Answer {
    let body: RefreshRequest = req.json()?;
    let account = {
        let db = lock(&master.db);
        let id = db.token_account(&body.refresh_token, REFRESH, unix_now()).map_err(internal)?;
        let account = id.map(|id| db.account(id)).transpose().map_err(internal)?.flatten();
        match account {
            Some(account) if !account.disabled => {
                db.remove_token(&body.refresh_token).map_err(internal)?;
                account
            }
            _ => return Err(Resp::error(401, "Logged out: log in again.")),
        }
    };
    Ok(Resp::ok(&issue_session(master, &account)?))
}

/// The account of the request's session token.
fn session_account(master: &Master, req: &Req) -> Result<Account, Resp> {
    let token = req.bearer().ok_or_else(|| Resp::error(401, "log in first"))?;
    let claims = verify_token(&master.public_key(), token, unix_now(), TokenKind::Session, None)
        .map_err(|err| Resp::error(401, &format!("{err}: log in again")))?;
    let account = lock(&master.db).account(claims.sub).map_err(internal)?;
    account.filter(|a| !a.disabled).ok_or_else(|| Resp::error(401, "no such account"))
}

/// A ticket for joining one game server (by its key fingerprint).
fn ticket(master: &Master, req: &Req) -> Answer {
    let account = session_account(master, req)?;
    let body: TicketRequest = req.json()?;
    let server = normalize_fingerprint(&body.server);
    if server.len() != 32 || !server.chars().all(|c| c.is_ascii_hexdigit()) {
        return Err(Resp::error(400, "server: a key fingerprint (32 hex digits)"));
    }
    // S23: each ticket upserts a row keyed by an arbitrary server fingerprint the caller
    // supplies, so an account could otherwise grow the tickets table without bound.
    if let Err(minutes) = lock(&master.limits).check_ticket(account.id) {
        return Err(Resp::error(429, &format!("Too many ticket requests. Try again in {minutes} minutes.")));
    }
    let now = unix_now();
    let rank = master.config.progression.info(account.xp);
    let expires = now + master.config.ticket_minutes * 60;
    let claims = Claims {
        v: TOKEN_VERSION,
        kind: TokenKind::Ticket,
        sub: account.id,
        name: account.name.clone(),
        rank: rank.index,
        rank_short: rank.short,
        rank_name: rank.name,
        iat: now,
        exp: expires,
        aud: Some(server.clone()),
        jti: game_auth::hex(&game_auth::random_bytes::<12>()),
    };
    lock(&master.db).note_ticket(account.id, &server, now).map_err(internal)?;
    lock(&master.limits).ticket_issued(account.id);
    Ok(Resp::ok(&TicketResponse { ticket: token::sign(&master.key, &claims), expires }))
}

/// The ranked server of the request's API key.
fn ranked_server(master: &Master, req: &Req) -> Result<RankedServer, Resp> {
    let key = req.bearer().ok_or_else(|| Resp::error(401, "API key needed"))?;
    lock(&master.db).server_by_key(key).map_err(internal)?.ok_or_else(|| Resp::error(403, "unknown API key"))
}

/// Whether `ip` is private, loopback or link-local: the connection to the master didn't come
/// from a routable address, so it's plausibly a game server reaching the master over an
/// internal network (NAT, VPN, container network, or the same machine).
fn is_internal(ip: std::net::IpAddr) -> bool {
    match ip {
        std::net::IpAddr::V4(v4) => v4.is_private() || v4.is_loopback() || v4.is_link_local(),
        std::net::IpAddr::V6(v6) => v6.is_loopback() || (v6.segments()[0] & 0xfe00) == 0xfc00,
    }
}

fn server_heartbeat(master: &Master, req: &Req) -> Answer {
    let server = ranked_server(master, req)?;
    let beat: ServerHeartbeat = req.json()?;
    let Some(key) = unhex_array::<32>(&beat.public_key) else {
        return Err(Resp::error(400, "public_key: 64 hex digits"));
    };
    // S38: the listed address must match the requester, with one exception. `address` exists
    // for a server behind NAT/a proxy whose outbound connection to the master isn't the
    // address players should use; we only honour it when the heartbeat itself arrived over a
    // non-routable path, which is what that setup looks like. A server reachable directly
    // can't use it to advertise an unrelated address.
    let ip = match beat.address.as_deref().and_then(|a| a.trim().parse().ok()) {
        Some(addr) if is_internal(req.ip) => addr,
        _ => req.ip,
    };
    let clean = |s: &str, max: usize| s.chars().filter(|c| !c.is_control()).take(max).collect::<String>();
    let entry = ServerEntry {
        address: ip.to_string(),
        port: beat.port,
        query_port: beat.query_port,
        name: clean(&beat.name, 64),
        level: clean(&beat.level, 64),
        mode: clean(&beat.mode, 32),
        players: beat.players.min(1024),
        max_players: beat.max_players.min(1024),
        bots: beat.bots.min(1024),
        ranked: true,
        region: clean(&beat.region, 24),
        fingerprint: fingerprint(&key),
    };
    lock(&master.db).server_seen(server.id, &beat.public_key.to_ascii_lowercase(), unix_now()).map_err(internal)?;
    if !lock(&master.list).beat(ip, entry) {
        return Err(Resp::error(429, "too many servers at this address"));
    }
    Ok(Resp::ok(&serde_json::json!({})))
}

/// A ranked server's round: counts it for players who got a ticket for this server lately.
fn server_round(master: &Master, req: &Req) -> Answer {
    let server = ranked_server(master, req)?;
    let report: RoundReport = req.json()?;
    report.validate().map_err(|err| Resp::error(400, &err))?;
    let Some(key) = unhex_array::<32>(&server.public_key) else {
        return Err(Resp::error(409, "send a heartbeat first (the master doesn't know this server's key yet)"));
    };
    let server_fingerprint = normalize_fingerprint(&fingerprint(&key));
    let now = unix_now();
    let since = now.saturating_sub(master.config.ticket_window_hours * 3600);
    // S19: the round is recorded and every player credited in one transaction, so a failure
    // partway through can't leave the round permanently marked done with some players missed.
    let result =
        lock(&master.db).record_round(server.id, &server_fingerprint, &report, since, now, &master.config.progression).map_err(internal)?;
    if !result.duplicate {
        for (id, rank) in &result.promotions {
            println!("account {id} promoted to {rank}");
        }
        println!(
            "server {} ({}): round {} on {} counted for {} players, {} refused",
            server.id,
            server.name,
            report.round_id,
            report.level,
            result.accepted,
            result.rejected.len()
        );
    }
    Ok(Resp::ok(&result))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn internal_addresses() {
        assert!(is_internal("127.0.0.1".parse().unwrap()));
        assert!(is_internal("10.1.2.3".parse().unwrap()));
        assert!(is_internal("192.168.1.1".parse().unwrap()));
        assert!(is_internal("169.254.1.1".parse().unwrap()));
        assert!(is_internal("::1".parse().unwrap()));
        assert!(is_internal("fc00::1".parse().unwrap()));
        assert!(!is_internal("203.0.113.5".parse().unwrap()));
        assert!(!is_internal("2001:db8::1".parse().unwrap()));
    }
}
