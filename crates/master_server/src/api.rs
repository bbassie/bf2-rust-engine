//! The REST API (see `game_auth::api` for the endpoints and types).

use game_auth::{
    api::{
        Credentials, MasterInfo, RefreshRequest, RoundReport, RoundResult, ServerEntry, ServerHeartbeat, Session,
        TicketRequest, TicketResponse,
    },
    fingerprint,
    identity::normalize_fingerprint,
    token::{self, Claims, TOKEN_VERSION, TokenKind, verify_token},
    unhex_array, unix_now, validate_account_name, validate_password,
};

use crate::{
    auth::{self, check_password, dummy_hash, hash_password, new_secret},
    db::{Account, RankedServer},
    http::{Master, Method, Req, Resp},
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
            master.db.lock().unwrap().remove_token(&body.refresh_token).map_err(internal)?;
            Ok(Resp::ok(&serde_json::json!({})))
        }
        (Method::Get, "/me") => {
            let account = session_account(master, req)?;
            let profile = master.db.lock().unwrap().profile(&account, &master.config.progression).map_err(internal)?;
            Ok(Resp::ok(&profile))
        }
        (Method::Post, "/ticket") => ticket(master, req),
        (Method::Get, "/leaderboard") => {
            let sort = req.query.get("sort").map_or("xp", String::as_str);
            let limit = req.query.get("limit").and_then(|l| l.parse().ok()).unwrap_or(50usize).clamp(1, 200);
            let board = master.db.lock().unwrap().leaderboard(sort, limit, &master.config.progression).map_err(internal)?;
            Ok(Resp::ok(&board))
        }
        (Method::Get, "/servers") => Ok(Resp::ok(&master.list.lock().unwrap().entries())),
        (Method::Get, "/quickjoin") => {
            let ranked = match req.query.get("ranked").map(String::as_str) {
                Some("yes" | "true" | "1") => Some(true),
                Some("no" | "false" | "0") => Some(false),
                _ => None,
            };
            let region = req.query.get("region").cloned().unwrap_or_default();
            Ok(Resp::ok(&master.list.lock().unwrap().quick_join(ranked, &region)))
        }
        (Method::Post, "/server/heartbeat") => server_heartbeat(master, req),
        (Method::Post, "/server/round") => server_round(master, req),
        (Method::Get, other) if other.starts_with("/players/") => {
            let name = crate::http::url_decode(&other["/players/".len()..]);
            let db = master.db.lock().unwrap();
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
    let db = master.db.lock().unwrap();
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
    if let Err(minutes) = master.limits.lock().unwrap().check_login(req.ip, name) {
        return Err(Resp::error(429, &format!("Too many failed logins. Try again in {minutes} minutes.")));
    }
    let account = master.db.lock().unwrap().account_by_name(name).map_err(internal)?;
    // Hashing takes a while: outside the database lock, and as long for unknown names.
    let hash = account.as_ref().map_or(dummy_hash(), |a| a.password_hash.as_str());
    let good = check_password(password, hash) && account.as_ref().is_some_and(|a| !a.disabled);
    let mut limits = master.limits.lock().unwrap();
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
    if !master.limits.lock().unwrap().register(req.ip) {
        return Err(Resp::error(429, "Too many new accounts from your address. Try again later."));
    }
    let hash = hash_password(&credentials.password);
    let db = master.db.lock().unwrap();
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
        let db = master.db.lock().unwrap();
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
    let account = master.db.lock().unwrap().account(claims.sub).map_err(internal)?;
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
    master.db.lock().unwrap().note_ticket(account.id, &server, now).map_err(internal)?;
    Ok(Resp::ok(&TicketResponse { ticket: token::sign(&master.key, &claims), expires }))
}

/// The ranked server of the request's API key.
fn ranked_server(master: &Master, req: &Req) -> Result<RankedServer, Resp> {
    let key = req.bearer().ok_or_else(|| Resp::error(401, "API key needed"))?;
    master
        .db
        .lock()
        .unwrap()
        .server_by_key(key)
        .map_err(internal)?
        .ok_or_else(|| Resp::error(403, "unknown API key"))
}

fn server_heartbeat(master: &Master, req: &Req) -> Answer {
    let server = ranked_server(master, req)?;
    let beat: ServerHeartbeat = req.json()?;
    let Some(key) = unhex_array::<32>(&beat.public_key) else {
        return Err(Resp::error(400, "public_key: 64 hex digits"));
    };
    let ip = beat.address.as_deref().and_then(|a| a.trim().parse().ok()).unwrap_or(req.ip);
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
    master.db.lock().unwrap().server_seen(server.id, &beat.public_key.to_ascii_lowercase(), unix_now()).map_err(internal)?;
    if !master.list.lock().unwrap().beat(ip, entry) {
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
    let progression = &master.config.progression;
    let mut db = master.db.lock().unwrap();
    let mut result = RoundResult::default();
    let fresh = db
        .add_round(server.id, &report.round_id, &report.level, &report.mode, report.winner, report.players.len(), now)
        .map_err(internal)?;
    if !fresh {
        result.duplicate = true;
        return Ok(Resp::ok(&result));
    }
    let mut counted = std::collections::HashSet::new();
    for player in &report.players {
        let account = db.account(player.account).map_err(internal)?;
        let joined = db.ticket_since(player.account, &server_fingerprint, since).map_err(internal)?;
        let Some(account) = account.filter(|a| !a.disabled && joined && counted.insert(a.id)) else {
            result.rejected.push(player.account);
            continue;
        };
        let won = (report.winner != 0).then_some(player.team == report.winner);
        let xp = progression.round_xp(player.score, player.seconds, won == Some(true));
        let total = db.add_player_round(player, won, xp, now).map_err(internal)?;
        if progression.rank_index(total) > progression.rank_index(account.xp) {
            let rank = progression.info(total).name;
            println!("{} promoted to {rank}", account.name);
            result.promotions.push((account.id, rank));
        }
        result.accepted += 1;
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
    Ok(Resp::ok(&result))
}
