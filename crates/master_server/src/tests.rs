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
    // Logging out takes the session's CSRF token (a forged logout leaves the session alone)...
    let mut req = Req::new(Method::Post, "/logout");
    req.headers.insert("cookie".into(), session.clone());
    assert_eq!(http::handle(&master, &req).status, 303);
    let mut still = Req::new(Method::Get, "/");
    still.headers.insert("cookie".into(), session.clone());
    assert!(http::handle(&master, &still).text().contains("Logged in as"));
    // ...and then ends it.
    let token = crate::web::csrf_token(&master, session.trim_start_matches("bf2r_session="));
    req.body = format!("csrf={token}").into_bytes();
    assert_eq!(http::handle(&master, &req).status, 303);
    let mut req = Req::new(Method::Get, "/");
    req.headers.insert("cookie".into(), session);
    assert!(!http::handle(&master, &req).text().contains("Logged in as"));
}

// Master admins: roles, two-factor authentication, the admin pages, bans.

/// A browser with a cookie jar, for walking through the web pages.
struct Browser<'a> {
    master: &'a Master,
    cookies: std::collections::BTreeMap<String, String>,
    origin: Option<String>,
}

impl<'a> Browser<'a> {
    fn new(master: &'a Master) -> Self {
        Self { master, cookies: Default::default(), origin: None }
    }

    fn request(&mut self, method: Method, path: &str, form: &[(&str, &str)]) -> Resp {
        let mut req = Req::new(method, path);
        if !self.cookies.is_empty() {
            let jar = self.cookies.iter().map(|(k, v)| format!("{k}={v}")).collect::<Vec<_>>().join("; ");
            req.headers.insert("cookie".into(), jar);
        }
        if let Some(origin) = &self.origin {
            req.headers.insert("origin".into(), origin.clone());
            req.headers.insert("host".into(), "master.example.com".into());
        }
        req.body = form.iter().map(|(k, v)| format!("{}={}", http::url_encode(k), http::url_encode(v))).collect::<Vec<_>>().join("&").into_bytes();
        let resp = http::handle(self.master, &req);
        for (name, value) in &resp.headers {
            if name != "Set-Cookie" {
                continue;
            }
            let pair = value.split(';').next().unwrap();
            let (key, value) = pair.split_once('=').unwrap();
            if value.is_empty() || value_has_max_age_zero(&resp, key) {
                self.cookies.remove(key);
            } else {
                self.cookies.insert(key.to_string(), value.to_string());
            }
        }
        resp
    }

    fn get(&mut self, path: &str) -> Resp {
        self.request(Method::Get, path, &[])
    }

    /// A form POST with the CSRF token a real page would carry: the double-submit cookie for
    /// login/registration, the pending login's for `/login/2fa`, the session's otherwise.
    fn post(&mut self, path: &str, fields: &[(&str, &str)]) -> Resp {
        let csrf = match path {
            "/login" | "/register" => self.cookies.get("bf2r_csrf").cloned().unwrap_or_default(),
            "/login/2fa" => self.cookies.get("bf2r_mfa").map(|s| crate::web::csrf_token(self.master, s)).unwrap_or_default(),
            _ => self.cookies.get("bf2r_session").map(|s| crate::web::csrf_token(self.master, s)).unwrap_or_default(),
        };
        let mut fields = fields.to_vec();
        fields.push(("csrf", &csrf));
        self.request(Method::Post, path, &fields)
    }

    fn login(&mut self, name: &str, password: &str) -> Resp {
        self.get("/login");
        self.post("/login", &[("name", name), ("password", password)])
    }

    fn logged_in(&mut self) -> bool {
        self.get("/").text().contains("Logged in as")
    }
}

fn value_has_max_age_zero(resp: &Resp, key: &str) -> bool {
    resp.headers.iter().any(|(k, v)| k == "Set-Cookie" && v.starts_with(&format!("{key}=")) && v.contains("Max-Age=0"))
}

fn location(resp: &Resp) -> &str {
    resp.headers.iter().find(|(k, _)| k == "Location").map_or("", |(_, v)| v.as_str())
}

/// The text of the first `<div class="secret"...>` on a page.
fn secret_on(page: &str) -> String {
    let start = page.find(r#"<div class="secret""#).expect("a secret on the page");
    let start = start + page[start..].find('>').unwrap() + 1;
    let end = start + page[start..].find("</div>").unwrap();
    page[start..end].to_string()
}

fn recovery_codes_on(page: &str) -> Vec<String> {
    let start = page.find(r#"<div class="codes">"#).expect("recovery codes on the page");
    let end = start + page[start..].find("</div>").unwrap();
    page[start..end].split("<span>").skip(1).map(|s| s.trim_end_matches("</span>").to_string()).collect()
}

fn account_id(master: &Master, name: &str) -> u64 {
    master.db.lock().unwrap().account_by_name(name).unwrap().unwrap().id
}

fn register(master: &Master, name: &str, password: &str) -> Session {
    let resp = post(master, "/api/v1/register", &credentials(name, password), None);
    assert_eq!(resp.status, 200, "{}", resp.text());
    serde_json::from_slice(&resp.body).unwrap()
}

fn audit_actions(master: &Master) -> Vec<String> {
    master.db.lock().unwrap().audit_log(None, None, 500).unwrap().into_iter().map(|e| e.action).collect()
}

/// Registers `name`, promotes it (as `master promote` does) and sets up two-factor
/// authentication through the pages: a browser logged in with admin powers, the TOTP secret
/// and the recovery codes.
fn enrolled_admin<'a>(master: &'a Master, name: &str) -> (Browser<'a>, Vec<u8>, Vec<String>) {
    register(master, name, "correct horse");
    let id = account_id(master, name);
    master.db.lock().unwrap().set_role(id, crate::db::Role::Admin).unwrap();
    let mut browser = Browser::new(master);
    let resp = browser.login(name, "correct horse");
    assert_eq!(location(&resp), "/account/2fa", "a promoted account is sent to set up two-factor authentication");
    let page = browser.get("/account/2fa").text();
    let secret = crate::totp::base32_decode(&secret_on(&page)).unwrap();
    let code = crate::totp::code_at(&secret, crate::totp::step_at(unix_now()));
    let resp = browser.post("/account/2fa", &[("code", &code)]);
    assert_eq!(resp.status, 200, "{}", resp.text());
    let codes = recovery_codes_on(&resp.text());
    (browser, secret, codes)
}

/// A code for a later step than any used so far (enrolment and logins use up steps).
fn next_code(master: &Master, name: &str, secret: &[u8]) -> String {
    let id = account_id(master, name);
    let last = master.db.lock().unwrap().totp_state(id).unwrap().last_step;
    let step = (last + 1).max(crate::totp::step_at(unix_now()));
    assert!(step <= crate::totp::step_at(unix_now()) + 1, "within the skew window");
    crate::totp::code_at(secret, step)
}

#[test]
fn admin_pages_need_the_role_and_two_factor() {
    let master = master();
    register(&master, "root", "correct horse");
    let pat_session = register(&master, "pat", "correct horse");
    let servers = |m: &Master| m.db.lock().unwrap().servers().unwrap().len();

    // Logged out: to the login page; a POST is refused.
    let mut anon = Browser::new(&master);
    assert_eq!(location(&anon.get("/admin")), "/login");
    assert_eq!(anon.post("/admin/servers/add", &[("name", "x")]).status, 403);
    // The game API's session tokens never reach the admin pages.
    assert_eq!(location(&get(&master, "/admin", Some(&pat_session.session_token))), "/login");

    // A player: refused, on every page and action.
    let mut root = Browser::new(&master);
    assert_eq!(location(&root.login("root", "correct horse")), "/players/root");
    for path in ["/admin", "/admin/servers", "/admin/accounts", "/admin/audit", "/admin/accounts/1"] {
        assert_eq!(root.get(path).status, 403, "{path}");
    }
    assert_eq!(root.post("/admin/servers/add", &[("name", "x")]).status, 403);
    assert_eq!(root.post("/admin/settings", &[("registration", "off")]).status, 403);
    assert_eq!(root.get("/account/2fa").status, 403, "two-factor setup is for admins");
    assert_eq!(servers(&master), 0);

    // Promoted but without two-factor authentication: still no admin powers.
    let root_id = account_id(&master, "root");
    master.db.lock().unwrap().set_role(root_id, crate::db::Role::Admin).unwrap();
    assert_eq!(location(&root.get("/admin")), "/account/2fa");
    assert_eq!(root.post("/admin/servers/add", &[("name", "x")]).status, 403);
    assert_eq!(servers(&master), 0);

    // Enrolment: the key as text, an otpauth URI and a QR code; reloading keeps the key.
    let page = root.get("/account/2fa").text();
    assert!(page.contains("otpauth://totp/") && page.contains("<svg") && !page.contains("<script"));
    let key = secret_on(&page);
    assert_eq!(secret_on(&root.get("/account/2fa").text()), key);
    let secret = crate::totp::base32_decode(&key).unwrap();
    // Stored encrypted, not as the key itself.
    let sealed: String = master.db.lock().unwrap().raw().query_row("SELECT totp_pending FROM accounts WHERE name = 'root'", [], |r| r.get(0)).unwrap();
    assert!(sealed.starts_with("c1:") && !sealed.contains(&game_auth::hex(&secret)));
    let now = crate::totp::step_at(unix_now());
    assert_eq!(root.post("/account/2fa", &[("code", &crate::totp::code_at(&secret, now + 5))]).status, 400, "a wrong code");
    let resp = root.post("/account/2fa", &[("code", &crate::totp::code_at(&secret, now))]);
    assert_eq!(resp.status, 200);
    let codes = recovery_codes_on(&resp.text());
    assert_eq!(codes.len(), 10);
    assert_eq!(master.db.lock().unwrap().recovery_codes_left(root_id).unwrap(), 10);

    // Now the session passed two-factor authentication: the admin pages open.
    let overview = root.get("/admin");
    assert_eq!(overview.status, 200, "{}", overview.text());
    assert!(overview.text().contains("ranked rounds in 24 h"));
    assert!(overview.headers.iter().any(|(k, v)| k == "Cache-Control" && v == "no-store"));

    // Ranked servers from the pages: the API key is shown once and works.
    let resp = root.post("/admin/servers/add", &[("name", "Ranked <one>")]);
    assert_eq!(resp.status, 200);
    let key = secret_on(&resp.text());
    assert!(key.starts_with("bf2r_") && resp.text().contains("Ranked &lt;one&gt;"));
    let id = master.db.lock().unwrap().servers().unwrap()[0].id;
    let server = Identity::generate();
    let beat = ServerHeartbeat { port: 16567, query_port: 16568, name: "Ranked".into(), public_key: server.public_hex(), ..Default::default() };
    assert_eq!(post(&master, "/api/v1/server/heartbeat", &beat, Some(&key)).status, 200);
    let page = root.get("/admin/servers").text();
    assert!(page.contains("127.0.0.1:16567") && page.contains(&server.fingerprint()) && !page.contains(&key), "address and key fingerprint, never the API key again");
    // Rotating: the old key stops working.
    let resp = root.post(&format!("/admin/servers/{id}/rotate"), &[]);
    let new_key = secret_on(&resp.text());
    assert_ne!(new_key, key);
    assert_eq!(post(&master, "/api/v1/server/heartbeat", &beat, Some(&key)).status, 403);
    assert_eq!(post(&master, "/api/v1/server/heartbeat", &beat, Some(&new_key)).status, 200);
    // Disabled: refused, and off the server list at once.
    assert_eq!(root.post(&format!("/admin/servers/{id}/disable"), &[]).status, 303);
    assert!(master.list.lock().unwrap().entries().is_empty());
    assert_eq!(post(&master, "/api/v1/server/heartbeat", &beat, Some(&new_key)).status, 403);
    assert_eq!(root.post(&format!("/admin/servers/{id}/enable"), &[]).status, 303);
    assert_eq!(post(&master, "/api/v1/server/heartbeat", &beat, Some(&new_key)).status, 200);

    // Forged forms: no CSRF token, a wrong one, another origin. Nothing happens.
    assert_eq!(root.request(Method::Post, "/admin/servers/add", &[("name", "forged")]).status, 400);
    assert_eq!(root.request(Method::Post, "/admin/servers/add", &[("name", "forged"), ("csrf", "0123")]).status, 400);
    root.origin = Some("https://evil.example".into());
    assert_eq!(root.post("/admin/servers/add", &[("name", "forged")]).status, 400);
    root.origin = None;
    assert_eq!(servers(&master), 1);
    // Another account's session doesn't make a valid token either.
    let mut pat = Browser::new(&master);
    pat.login("pat", "correct horse");
    let pats_token = crate::web::csrf_token(&master, &pat.cookies["bf2r_session"]);
    assert_eq!(root.request(Method::Post, "/admin/servers/add", &[("name", "forged"), ("csrf", &pats_token)]).status, 400);
    assert_eq!(pat.get("/admin/audit").status, 403);

    // Removing needs a tick.
    assert_eq!(root.post(&format!("/admin/servers/{id}/remove"), &[]).status, 400);
    assert_eq!(servers(&master), 1);
    assert_eq!(root.post(&format!("/admin/servers/{id}/remove"), &[("confirm", "yes")]).status, 303);
    assert_eq!(servers(&master), 0);
    assert_eq!(post(&master, "/api/v1/server/heartbeat", &beat, Some(&new_key)).status, 403);

    // Everything is in the audit log, which the pages show and nobody can edit.
    let actions = audit_actions(&master);
    for action in ["2fa.enrol", "server.add", "server.rotate-key", "server.disable", "server.enable", "server.remove"] {
        assert!(actions.contains(&action.to_string()), "{action} in {actions:?}");
    }
    let log = root.get("/admin/audit").text();
    assert!(log.contains("server.rotate-key") && log.contains("Ranked &lt;one&gt;"));
    let db = master.db.lock().unwrap();
    assert!(db.raw().execute("UPDATE audit SET detail = 'edited'", []).is_err());
    assert!(db.raw().execute("DELETE FROM audit", []).is_err());
    assert_eq!(db.audit_log(None, None, 500).unwrap().len(), actions.len());
    drop(db);
    let _ = codes;
}

#[test]
fn admin_login_takes_a_code_and_recovery_codes_reset_it() {
    let master = master();
    let (mut root, secret, codes) = enrolled_admin(&master, "root");
    assert_eq!(root.get("/admin").status, 200);
    assert_eq!(root.post("/logout", &[]).status, 303);
    assert!(!root.logged_in());

    // The password alone: no session yet, only the code step.
    let resp = root.login("root", "correct horse");
    assert_eq!(location(&resp), "/login/2fa");
    assert!(!root.cookies.contains_key("bf2r_session") && root.cookies.contains_key("bf2r_mfa"));
    assert_eq!(location(&root.get("/admin")), "/login");
    assert_eq!(root.get("/login/2fa").status, 200);
    // Wrong, replayed (the enrolment's step) and forged-form codes are refused.
    let now = crate::totp::step_at(unix_now());
    assert_eq!(root.post("/login/2fa", &[("code", &crate::totp::code_at(&secret, now + 10))]).status, 400);
    let root_id = account_id(&master, "root");
    let enrolment_step = master.db.lock().unwrap().totp_state(root_id).unwrap().last_step;
    assert_eq!(root.post("/login/2fa", &[("code", &crate::totp::code_at(&secret, enrolment_step))]).status, 400, "replay");
    let code = next_code(&master, "root", &secret);
    assert_eq!(root.request(Method::Post, "/login/2fa", &[("code", &code)]).status, 400, "no CSRF token");
    // The right code: an admin session.
    let resp = root.post("/login/2fa", &[("code", &code)]);
    assert_eq!(location(&resp), "/admin");
    assert_eq!(root.get("/admin").status, 200);
    let mfa_session: bool = master.db.lock().unwrap().session(&root.cookies["bf2r_session"], crate::api::WEB, unix_now()).unwrap().unwrap().1;
    assert!(mfa_session);
    // The same code again, in another browser: a replay.
    let mut other = Browser::new(&master);
    other.login("root", "correct horse");
    assert_eq!(other.post("/login/2fa", &[("code", &code)]).status, 400);

    // The game's login is unchanged for admins (no code).
    assert_eq!(post(&master, "/api/v1/login", &credentials("root", "correct horse"), None).status, 200);

    // A recovery code: logs in, turns two-factor off and sends the admin to set it up again.
    let mut lost = Browser::new(&master);
    lost.login("root", "correct horse");
    let resp = lost.post("/login/2fa", &[("code", &codes[0].to_uppercase())]);
    assert_eq!(location(&resp), "/account/2fa");
    let account = master.db.lock().unwrap().account_by_name("root").unwrap().unwrap();
    assert!(!account.totp_enabled);
    assert_eq!(master.db.lock().unwrap().recovery_codes_left(account.id).unwrap(), 0, "all the old codes are gone");
    assert_eq!(location(&lost.get("/admin")), "/account/2fa");
    assert!(audit_actions(&master).contains(&"2fa.reset".to_string()));
    // The admin session from before now lacks the two-factor setup too.
    assert_eq!(location(&root.get("/admin")), "/account/2fa");
}

#[test]
fn two_factor_attempts_are_limited() {
    let master = master();
    let (mut root, secret, _) = enrolled_admin(&master, "root");
    root.post("/logout", &[]);
    root.login("root", "correct horse");
    let wrong = crate::totp::code_at(&secret, crate::totp::step_at(unix_now()) + 20);
    for _ in 0..crate::auth::TOTP_FAILURES_SHORT {
        assert_eq!(root.post("/login/2fa", &[("code", &wrong)]).status, 400);
    }
    let right = next_code(&master, "root", &secret);
    assert_eq!(root.post("/login/2fa", &[("code", &right)]).status, 429);
    assert!(!root.cookies.contains_key("bf2r_session"));
    assert!(audit_actions(&master).iter().filter(|a| *a == "2fa.failed").count() >= crate::auth::TOTP_FAILURES_SHORT);
}

#[test]
fn bans_passwords_roles_and_settings() {
    let master = master();
    let (mut root, _, _) = enrolled_admin(&master, "root");
    let pat_session = register(&master, "pat", "correct horse");
    let pat_id = pat_session.profile.id;
    let mut pat = Browser::new(&master);
    pat.login("pat", "correct horse");
    assert!(pat.logged_in());
    let root_id = account_id(&master, "root");

    // Search and profile.
    assert!(root.get("/admin/accounts?q=pa").text().contains(&format!("/admin/accounts/{pat_id}")));
    let profile = root.get(&format!("/admin/accounts/{pat_id}"));
    assert_eq!(profile.status, 200);
    assert!(profile.text().contains("kills per death"));

    // Bans: not yourself, not another admin, and a reason is needed.
    assert_eq!(root.post(&format!("/admin/accounts/{root_id}/ban"), &[("reason", "x")]).status, 400);
    assert_eq!(root.post(&format!("/admin/accounts/{pat_id}/ban"), &[("reason", "")]).status, 400);
    assert_eq!(root.post(&format!("/admin/accounts/{pat_id}/ban"), &[("reason", "cheating"), ("days", "7")]).status, 303);
    // At once: the web session, the refresh token and the session token all stop working.
    assert!(!pat.logged_in());
    let refresh = post(&master, "/api/v1/refresh", &game_auth::api::RefreshRequest { refresh_token: pat_session.refresh_token.clone() }, None);
    assert!(refresh.status == 401 || refresh.status == 403);
    let ticket = post(&master, "/api/v1/ticket", &game_auth::api::TicketRequest { server: Identity::generate().fingerprint() }, Some(&pat_session.session_token));
    assert_eq!(ticket.status, 403, "no join ticket while banned");
    let login = post(&master, "/api/v1/login", &credentials("pat", "correct horse"), None);
    assert_eq!(login.status, 403);
    assert!(login.text().contains("banned") && login.text().contains("cheating"));
    assert!(pat.login("pat", "correct horse").text().contains("banned"));
    assert!(!pat.logged_in());
    // A wrong password doesn't reveal the ban.
    assert!(!post(&master, "/api/v1/login", &credentials("pat", "wrong horse"), None).text().contains("banned"));
    assert_eq!(root.post(&format!("/admin/accounts/{pat_id}/unban"), &[]).status, 303);
    assert_eq!(post(&master, "/api/v1/login", &credentials("pat", "correct horse"), None).status, 200);

    // A one-time password: only the web page that chooses a new one takes it.
    let resp = root.post(&format!("/admin/accounts/{pat_id}/password"), &[]);
    assert_eq!(resp.status, 200);
    let one_time = secret_on(&resp.text());
    let login = post(&master, "/api/v1/login", &credentials("pat", &one_time), None);
    assert_eq!(login.status, 403);
    assert!(login.text().contains("reset"));
    assert_eq!(location(&pat.login("pat", &one_time)), "/account");
    assert_eq!(pat.post("/account/password", &[("current", &one_time), ("password", "new horse!"), ("confirm", "different")]).status, 400);
    assert_eq!(pat.post("/account/password", &[("current", &one_time), ("password", "new horse!"), ("confirm", "new horse!")]).status, 303);
    assert_eq!(post(&master, "/api/v1/login", &credentials("pat", "new horse!"), None).status, 200);
    assert_eq!(post(&master, "/api/v1/login", &credentials("pat", &one_time), None).status, 401);

    // Log out everywhere: game logins end too.
    let game: Session = serde_json::from_slice(&post(&master, "/api/v1/login", &credentials("pat", "new horse!"), None).body).unwrap();
    assert_eq!(root.post(&format!("/admin/accounts/{pat_id}/revoke"), &[]).status, 303);
    assert_eq!(post(&master, "/api/v1/refresh", &game_auth::api::RefreshRequest { refresh_token: game.refresh_token }, None).status, 401);

    // Renaming, for moderation.
    assert_eq!(root.post(&format!("/admin/accounts/{pat_id}/rename"), &[("name", "root")]).status, 400, "taken");
    assert_eq!(root.post(&format!("/admin/accounts/{pat_id}/rename"), &[("name", "bad name")]).status, 400);
    assert_eq!(root.post(&format!("/admin/accounts/{pat_id}/rename"), &[("name", "patrick")]).status, 303);
    assert_eq!(get(&master, "/api/v1/players/patrick", None).status, 200);

    // Roles: the last admin can't demote themselves.
    assert_eq!(root.post(&format!("/admin/accounts/{root_id}/demote"), &[]).status, 400);
    assert!(master.db.lock().unwrap().account(root_id).unwrap().unwrap().is_admin());
    // Promoting ends the account's web sessions; its next login sets up two-factor first.
    pat.login("patrick", "new horse!");
    assert!(pat.logged_in());
    assert_eq!(root.post(&format!("/admin/accounts/{pat_id}/promote"), &[]).status, 400, "needs the tick");
    assert_eq!(root.post(&format!("/admin/accounts/{pat_id}/promote"), &[("confirm", "yes")]).status, 303);
    assert!(!pat.logged_in());
    assert_eq!(location(&pat.login("patrick", "new horse!")), "/account/2fa");
    assert_eq!(pat.get("/admin").status, 303);
    // Another admin can reset someone's two-factor authentication.
    master.db.lock().unwrap().set_role(pat_id, crate::db::Role::Player).unwrap();
    let (mut pat2, _, _) = enrolled_admin(&master, "second");
    let second_id = account_id(&master, "second");
    assert_eq!(pat2.get("/admin").status, 200);
    assert_eq!(root.post(&format!("/admin/accounts/{second_id}/reset-2fa"), &[("confirm", "yes")]).status, 303);
    assert!(!master.db.lock().unwrap().account(second_id).unwrap().unwrap().totp_enabled);
    assert!(!pat2.logged_in(), "their sessions ended");
    // With two admins, one can demote the other; the demoted one loses the pages at once.
    let (mut third, _, _) = enrolled_admin(&master, "third");
    let third_id = account_id(&master, "third");
    assert_eq!(root.post(&format!("/admin/accounts/{third_id}/demote"), &[]).status, 303);
    assert!(!master.db.lock().unwrap().account(third_id).unwrap().unwrap().is_admin());
    assert_ne!(third.get("/admin").status, 200);

    // Registration on and off.
    assert_eq!(root.post("/admin/settings", &[("registration", "off")]).status, 303);
    assert_eq!(post(&master, "/api/v1/register", &credentials("newbie", "correct horse"), None).status, 403);
    assert!(root.get("/admin").text().contains("closed"));
    assert_eq!(root.post("/admin/settings", &[("registration", "on")]).status, 303);
    assert_eq!(post(&master, "/api/v1/register", &credentials("newbie", "correct horse"), None).status, 200);

    let actions = audit_actions(&master);
    for action in ["ban", "unban", "password.reset", "sessions.revoke", "rename", "promote", "demote", "2fa.reset", "setting"] {
        assert!(actions.contains(&action.to_string()), "{action} in {actions:?}");
    }
    let history = root.get(&format!("/admin/accounts/{pat_id}")).text();
    assert!(history.contains("cheating") && history.contains("password.reset"));
}
