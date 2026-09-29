//! End-to-end tests of the API and the web pages, through [`http::handle`] (no sockets).

use std::sync::{Arc, Mutex};

use game_auth::{
    Identity,
    api::{Credentials, PlayerRound, Profile, RoundReport, RoundResult, ServerHeartbeat, Session, TicketResponse},
    token::{TokenKind, verify_token},
    unix_now,
};

use crate::{
    config::Config,
    db::Db,
    http::{self, Master, Method, Req, Resp},
    list::ServerList,
};

fn master() -> Master {
    Master {
        config: Config::default().normalized(),
        key: Identity::generate(),
        db: Mutex::new(Db::in_memory().unwrap()),
        list: Arc::new(Mutex::new(ServerList::default())),
        limits: Mutex::new(Default::default()),
        admission: game_auth::admission::Limiter::new(1000, 1000),
    }
}

fn post(master: &Master, path: &str, body: &impl serde::Serialize, bearer: Option<&str>) -> Resp {
    post_from(master, "127.0.0.1".parse().unwrap(), path, body, bearer)
}

fn post_from(master: &Master, ip: std::net::IpAddr, path: &str, body: &impl serde::Serialize, bearer: Option<&str>) -> Resp {
    let mut req = Req::new(Method::Post, path);
    req.ip = ip;
    req.body = serde_json::to_vec(body).unwrap();
    if let Some(token) = bearer {
        req.headers.insert("authorization".into(), format!("Bearer {token}"));
    }
    http::handle(master, &req)
}

fn get(master: &Master, path: &str, bearer: Option<&str>) -> Resp {
    let mut req = Req::new(Method::Get, path);
    if let Some(token) = bearer {
        req.headers.insert("authorization".into(), format!("Bearer {token}"));
    }
    http::handle(master, &req)
}

fn credentials(name: &str, password: &str) -> Credentials {
    Credentials { name: name.into(), password: password.into(), email: None }
}

#[test]
fn accounts_tokens_and_stats() {
    let master = master();
    // Register, log in, bad passwords, taken names.
    let resp = post(&master, "/api/v1/register", &credentials("alice", "correct horse"), None);
    assert_eq!(resp.status, 200, "{}", resp.text());
    let session: Session = serde_json::from_slice(&resp.body).unwrap();
    assert_eq!(session.profile.name, "alice");
    assert_eq!(session.profile.rank.name, "Private");
    assert_eq!(post(&master, "/api/v1/register", &credentials("Alice", "another pass"), None).status, 409);
    assert_eq!(post(&master, "/api/v1/register", &credentials("x", "correct horse"), None).status, 400);
    assert_eq!(post(&master, "/api/v1/register", &credentials("bob", "short"), None).status, 400);
    assert_eq!(post(&master, "/api/v1/login", &credentials("alice", "wrong horse"), None).status, 401);
    assert_eq!(post(&master, "/api/v1/login", &credentials("nobody", "wrong horse"), None).status, 401);
    let resp = post(&master, "/api/v1/login", &credentials("ALICE", "correct horse"), None);
    assert_eq!(resp.status, 200);
    let session: Session = serde_json::from_slice(&resp.body).unwrap();
    // The password isn't stored as such.
    let stored = master.db.lock().unwrap().account_by_name("alice").unwrap().unwrap().password_hash;
    assert!(stored.starts_with("$argon2id$") && !stored.contains("correct horse"));

    // The session token is the master's, short-lived, and works for /me.
    let claims = verify_token(&master.public_key(), &session.session_token, unix_now(), TokenKind::Session, None).unwrap();
    assert!(claims.exp <= unix_now() + 15 * 60);
    let me: Profile = serde_json::from_slice(&get(&master, "/api/v1/me", Some(&session.session_token)).body).unwrap();
    assert_eq!(me.name, "alice");
    assert_eq!(get(&master, "/api/v1/me", None).status, 401);
    assert_eq!(get(&master, "/api/v1/me", Some("garbage")).status, 401);
    let forged = game_auth::token::sign(&Identity::generate(), &claims);
    assert_eq!(get(&master, "/api/v1/me", Some(&forged)).status, 401);

    // Refresh tokens rotate: the old one is used up.
    let refreshed = post(&master, "/api/v1/refresh", &game_auth::api::RefreshRequest { refresh_token: session.refresh_token.clone() }, None);
    assert_eq!(refreshed.status, 200);
    let refreshed: Session = serde_json::from_slice(&refreshed.body).unwrap();
    assert_eq!(post(&master, "/api/v1/refresh", &game_auth::api::RefreshRequest { refresh_token: session.refresh_token }, None).status, 401);

    // A ranked server: heartbeat, then a round for a player who got a ticket for it.
    let api_key = crate::auth::new_api_key();
    master.db.lock().unwrap().add_server("Ranked", &api_key, unix_now()).unwrap();
    let server = Identity::generate();
    let ticket = post(&master, "/api/v1/ticket", &game_auth::api::TicketRequest { server: server.fingerprint() }, Some(&refreshed.session_token));
    assert_eq!(ticket.status, 200, "{}", ticket.text());
    let ticket: TicketResponse = serde_json::from_slice(&ticket.body).unwrap();
    let ticket_claims = verify_token(&master.public_key(), &ticket.ticket, unix_now(), TokenKind::Ticket, Some(&server.fingerprint())).unwrap();
    assert_eq!(ticket_claims.sub, me.id);
    assert!(verify_token(&master.public_key(), &ticket.ticket, unix_now(), TokenKind::Ticket, Some(&Identity::generate().fingerprint())).is_err());

    let beat = ServerHeartbeat {
        port: 16567,
        query_port: 16568,
        name: "Ranked".into(),
        level: "Strike at Karkand".into(),
        mode: "gpm_cq".into(),
        players: 1,
        max_players: 32,
        bots: 8,
        public_key: server.public_hex(),
        region: "eu".into(),
        address: None,
    };
    let round = RoundReport {
        round_id: "round1".into(),
        level: "strike_at_karkand".into(),
        mode: "gpm_cq".into(),
        winner: 1,
        seconds: 900.0,
        players: vec![PlayerRound { account: me.id, name: "alice".into(), team: 1, score: 3000, kills: 20, deaths: 5, captures: 2, seconds: 900.0, ..Default::default() }],
    };
    assert_eq!(post(&master, "/api/v1/server/round", &round, Some(&api_key)).status, 409, "no heartbeat yet");
    assert_eq!(post(&master, "/api/v1/server/heartbeat", &beat, Some("bf2r_wrong")).status, 403);
    assert_eq!(post(&master, "/api/v1/server/heartbeat", &beat, None).status, 401);
    assert_eq!(post(&master, "/api/v1/server/heartbeat", &beat, Some(&api_key)).status, 200);
    let listed: Vec<game_auth::api::ServerEntry> = serde_json::from_slice(&get(&master, "/api/v1/servers", None).body).unwrap();
    assert!(listed[0].ranked && listed[0].fingerprint == server.fingerprint());
    let quick: game_auth::api::QuickJoin = serde_json::from_slice(&get(&master, "/api/v1/quickjoin?ranked=yes&region=eu", None).body).unwrap();
    assert_eq!(quick.servers.len(), 1);

    let resp = post(&master, "/api/v1/server/round", &round, Some(&api_key));
    assert_eq!(resp.status, 200, "{}", resp.text());
    let result: RoundResult = serde_json::from_slice(&resp.body).unwrap();
    assert_eq!(result.accepted, 1);
    // 3000 score + 7 for the minutes + 10 for the win, capped at 5000: Sergeant.
    assert_eq!(result.promotions, vec![(me.id, "Sergeant".to_string())]);
    let again: RoundResult = serde_json::from_slice(&post(&master, "/api/v1/server/round", &round, Some(&api_key)).body).unwrap();
    assert!(again.duplicate && again.accepted == 0, "counted once");
    // Someone who never got a ticket for this server isn't counted.
    let bob: Session = serde_json::from_slice(&post(&master, "/api/v1/register", &credentials("bob", "hunter2hunter2"), None).body).unwrap();
    let mut other = round.clone();
    other.round_id = "round2".into();
    other.players[0].account = bob.profile.id;
    let result: RoundResult = serde_json::from_slice(&post(&master, "/api/v1/server/round", &other, Some(&api_key)).body).unwrap();
    assert_eq!((result.accepted, result.rejected.clone()), (0, vec![bob.profile.id]));
    // Nonsense is refused.
    let mut silly = round.clone();
    silly.round_id = "round3".into();
    silly.players[0].kills = 1_000_000;
    assert_eq!(post(&master, "/api/v1/server/round", &silly, Some(&api_key)).status, 400);

    let profile: Profile = serde_json::from_slice(&get(&master, "/api/v1/players/alice", None).body).unwrap();
    assert_eq!((profile.stats.kills, profile.stats.wins, profile.rank.short.as_str()), (20, 1, "Sgt"));
    assert_eq!(get(&master, "/api/v1/players/nobody", None).status, 404);
    let board: Vec<game_auth::api::LeaderboardEntry> = serde_json::from_slice(&get(&master, "/api/v1/leaderboard?sort=kills", None).body).unwrap();
    assert_eq!(board.len(), 1);
}

#[test]
fn login_rate_limit() {
    let master = master();
    post(&master, "/api/v1/register", &credentials("carol", "correct horse"), None);
    // Repeated wrong guesses against carol's name slow it down, but a request from the same
    // (test) address with the *right* password still gets through (S17): a per-name block
    // would let anyone lock carol out just by guessing wrong a few times.
    for _ in 0..10 {
        post(&master, "/api/v1/login", &credentials("carol", "nope nope"), None).status;
    }
    assert_eq!(post(&master, "/api/v1/login", &credentials("carol", "correct horse"), None).status, 200);
    // Enough failed attempts from one address blocks that address regardless of the name.
    for _ in 0..30 {
        post(&master, "/api/v1/login", &credentials("someone-else", "nope"), None);
    }
    assert_eq!(post(&master, "/api/v1/login", &credentials("carol", "correct horse"), None).status, 429);
}

/// The CSRF token a `GET /login` or `/register` handed out, from its `Set-Cookie` (the same
/// value is also in the page's hidden `csrf` field: double-submit).
fn csrf_of(resp: &Resp) -> String {
    resp.headers
        .iter()
        .find(|(k, v)| k == "Set-Cookie" && v.starts_with("bf2r_csrf="))
        .map(|(_, v)| v.split(';').next().unwrap().trim_start_matches("bf2r_csrf=").to_string())
        .expect("the form set a csrf cookie")
}

#[test]
fn web_pages() {
    let master = master();
    let home = get(&master, "/", None);
    assert_eq!(home.status, 200);
    assert!(home.text().contains("Top players"));
    assert!(home.headers.iter().any(|(k, v)| k == "Content-Security-Policy" && v.contains("default-src 'none'")));

    // A form submission with no CSRF cookie/field at all (as a cross-site forger would send)
    // is refused, not processed.
    let mut req = Req::new(Method::Post, "/register");
    req.body = b"name=nocsrf&password=correct+horse&email=".to_vec();
    assert_eq!(http::handle(&master, &req).status, 400);
    assert!(get(&master, "/api/v1/players/nocsrf", None).status != 200, "not actually registered");
    // ...and so is one with a mismatched cookie and field.
    let mut req = Req::new(Method::Post, "/register");
    req.headers.insert("cookie".into(), "bf2r_csrf=aaa".into());
    req.body = b"name=nocsrf&password=correct+horse&email=&csrf=bbb".to_vec();
    assert_eq!(http::handle(&master, &req).status, 400);

    // Register through the form (with the csrf cookie/field a browser would carry from the
    // page): a session cookie and the profile.
    let csrf = csrf_of(&get(&master, "/register", None));
    let mut req = Req::new(Method::Post, "/register");
    req.headers.insert("cookie".into(), format!("bf2r_csrf={csrf}"));
    req.body = format!("name=dave&password=correct+horse&email=&csrf={csrf}").into_bytes();
    let resp = http::handle(&master, &req);
    assert_eq!(resp.status, 303, "{}", resp.text());
    let cookie = resp.headers.iter().find(|(k, v)| k == "Set-Cookie" && v.starts_with("bf2r_session=")).unwrap().1.clone();
    assert!(cookie.contains("HttpOnly") && cookie.contains("SameSite=Strict"));
    let session = cookie.split(';').next().unwrap().to_string();
    let mut req = Req::new(Method::Get, "/players/dave");
    req.headers.insert("cookie".into(), session.clone());
    let page = http::handle(&master, &req);
    assert!(page.text().contains("Logged in as dave"));
    assert!(page.text().contains("Private"));
    // A cross-origin POST is refused even with a matching csrf cookie/field.
    let csrf = csrf_of(&get(&master, "/login", None));
    let mut req = Req::new(Method::Post, "/login");
    req.headers.insert("cookie".into(), format!("bf2r_csrf={csrf}"));
    req.headers.insert("origin".into(), "https://evil.example".into());
    req.headers.insert("host".into(), "master.example.com".into());
    req.body = format!("name=dave&password=correct+horse&csrf={csrf}").into_bytes();
    assert_eq!(http::handle(&master, &req).status, 400);
    // Names are escaped: no markup from players reaches the page.
    let csrf = csrf_of(&get(&master, "/login", None));
    let mut req = Req::new(Method::Post, "/login");
    req.headers.insert("cookie".into(), format!("bf2r_csrf={csrf}"));
    req.body = format!("name=%3Cscript%3E&password=x&csrf={csrf}").into_bytes();
    let failed = http::handle(&master, &req);
    assert_eq!(failed.status, 400);
    assert!(!failed.text().contains("<script>") && failed.text().contains("&lt;script&gt;"));
    for path in ["/leaderboard?sort=kd", "/servers", "/ranks", "/login", "/register"] {
        assert_eq!(get(&master, path, None).status, 200, "{path}");
    }
    assert_eq!(get(&master, "/players/nobody", None).status, 404);
    assert_eq!(get(&master, "/nothing", None).status, 404);
    // Logging out ends the session.
    let mut req = Req::new(Method::Post, "/logout");
    req.headers.insert("cookie".into(), session.clone());
    assert_eq!(http::handle(&master, &req).status, 303);
    let mut req = Req::new(Method::Get, "/");
    req.headers.insert("cookie".into(), session);
    assert!(!http::handle(&master, &req).text().contains("Logged in as"));
}
