//! Web pages for browsers, server-rendered, in the game menus' style: dark gradient, rounded
//! translucent panels, white text, blue accent. No scripts, no external requests.
//!
//! ```text
//! /                  overview: top players, servers online
//! /leaderboard       ?sort=xp|score|kills|kd|time
//! /players/<name>    profile: rank, XP to the next rank, career stats, kits, vehicles, weapons
//! /servers           servers online
//! /ranks             the ranks and the XP they need
//! /login /register   forms (POST: a session cookie)
//! /logout            POST
//! ```

use std::fmt::Write as _;

use game_auth::{
    api::{Credentials, Profile, RankInfo, Tally},
    unix_now,
};

use crate::{
    api::{WEB, authenticate, create_account},
    auth::new_secret,
    db::Account,
    http::{Master, Method, Req, Resp, url_encode},
};

/// The web session cookie.
const COOKIE: &str = "bf2r_session";
/// Web sessions last a week.
const WEB_SESSION_SECS: u64 = 7 * 86_400;

type Answer = Result<Resp, Resp>;

/// `&`, `<`, `>`, `"` and `'` escaped.
pub fn escape(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    for c in text.chars() {
        match c {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '"' => out.push_str("&quot;"),
            '\'' => out.push_str("&#39;"),
            c => out.push(c),
        }
    }
    out
}

const STYLE: &str = r#"
:root { --text: #f2f4f8; --dim: rgba(217,222,230,.6); --accent: #4d94ff; --enemy: #f25447;
  --panel: rgba(13,15,20,.92); --card: rgba(26,29,38,.85); --button: #262b36; --hover: #363c49; --track: #40454f; }
* { box-sizing: border-box; }
body { margin: 0; min-height: 100vh; color: var(--text); font: 15px/1.45 system-ui, -apple-system, "Segoe UI", Roboto, sans-serif;
  background: linear-gradient(135deg, #161b26 0%, #08090d 55%, #040506 100%) fixed; }
a { color: var(--text); text-decoration: none; } a:hover { color: var(--accent); }
.shell { display: flex; min-height: 100vh; }
nav { width: 240px; flex-shrink: 0; padding: 40px 20px 32px 32px; display: flex; flex-direction: column; gap: 4px; }
.brand { margin: 0 0 36px 12px; } .brand b { display: block; font-size: 48px; font-weight: 600; line-height: 1; }
.brand span { color: var(--accent); font-size: 13px; letter-spacing: .12em; }
nav a { padding: 9px 14px; border-radius: 8px; font-size: 19px; } nav a:hover { background: rgba(54,60,73,.7); color: var(--text); }
nav a.on { background: rgba(77,148,255,.3); }
nav .grow { flex: 1; } nav .who { color: var(--dim); font-size: 13px; padding: 0 14px; }
main { flex: 1; min-width: 0; padding: 44px 48px 40px 24px; max-width: 1100px; }
h1 { font-size: 30px; font-weight: 500; margin: 0 0 4px; } .sub { color: var(--dim); margin: 0 0 22px; }
h2 { font-size: 12px; letter-spacing: .08em; text-transform: uppercase; color: var(--dim); font-weight: 600; margin: 22px 0 8px; }
.card { background: var(--card); border-radius: 10px; padding: 16px 18px; }
.row { display: flex; gap: 16px; flex-wrap: wrap; } .row > .card { flex: 1; min-width: 220px; }
table { width: 100%; border-collapse: collapse; } th { text-align: left; font-size: 11px; letter-spacing: .08em; text-transform: uppercase; color: var(--dim); font-weight: 600; padding: 4px 10px; }
td { padding: 7px 10px; } tbody tr:hover { background: rgba(54,60,73,.5); } td.num, th.num { text-align: right; font-variant-numeric: tabular-nums; }
.badge { display: inline-block; min-width: 46px; padding: 1px 7px; margin-right: 8px; border-radius: 5px; background: rgba(77,148,255,.22); color: #cfe0ff; font-size: 12px; text-align: center; }
.ranked { background: rgba(77,148,255,.3); } .tag { display: inline-block; padding: 1px 7px; border-radius: 5px; background: var(--button); font-size: 12px; color: var(--dim); margin-left: 6px; }
.tiles { display: grid; grid-template-columns: repeat(auto-fill, minmax(150px, 1fr)); gap: 10px; }
.tile { background: var(--card); border-radius: 10px; padding: 12px 14px; } .tile b { display: block; font-size: 24px; font-weight: 500; } .tile span { color: var(--dim); font-size: 13px; }
.bar { height: 6px; border-radius: 3px; background: var(--track); overflow: hidden; margin: 10px 0 6px; } .bar i { display: block; height: 100%; background: var(--accent); border-radius: 3px; }
.hero { display: flex; align-items: center; gap: 22px; } .hero .rank { font-size: 22px; } .dim { color: var(--dim); }
form { display: flex; flex-direction: column; gap: 10px; max-width: 380px; }
label { color: var(--dim); font-size: 13px; } input { width: 100%; padding: 9px 11px; border-radius: 6px; border: 1px solid transparent; background: #1f2229; color: var(--text); font: inherit; }
input:focus { outline: none; border-color: var(--accent); }
button, .button { display: inline-block; padding: 10px 26px; border: 0; border-radius: 6px; background: var(--accent); color: var(--text); font: inherit; font-size: 17px; cursor: pointer; }
button:hover, .button:hover { background: #62a2ff; color: var(--text); } button.plain { background: var(--button); font-size: 15px; padding: 8px 14px; } button.plain:hover { background: var(--hover); }
.notice { padding: 10px 14px; border-left: 3px solid var(--enemy); background: rgba(242,84,71,.14); border-radius: 6px; margin-bottom: 14px; max-width: 640px; }
.chips a { display: inline-block; padding: 6px 12px; border-radius: 6px; background: var(--button); margin: 0 6px 6px 0; font-size: 14px; } .chips a.on { background: rgba(77,148,255,.45); }
@media (max-width: 760px) { .shell { flex-direction: column; } nav { width: auto; flex-direction: row; flex-wrap: wrap; padding: 16px; } .brand { display: none; } nav .grow { display: none; } main { padding: 16px; } }
"#;

/// A whole page.
fn page(master: &Master, title: &str, active: &str, user: Option<&Account>, body: &str) -> Resp {
    let mut nav = String::new();
    for (path, label) in [("/", "Overview"), ("/leaderboard", "Leaderboard"), ("/servers", "Servers"), ("/ranks", "Ranks")] {
        let _ = write!(nav, r#"<a href="{path}"{}>{label}</a>"#, if active == path { r#" class="on""# } else { "" });
    }
    nav.push_str(r#"<div class="grow"></div>"#);
    match user {
        Some(user) => {
            let _ = write!(
                nav,
                r#"<a href="/players/{}">My profile</a><form method="post" action="/logout" style="margin:0"><button class="plain" type="submit" style="margin:4px 14px">Log out</button></form><div class="who">Logged in as {}</div>"#,
                url_encode(&user.name),
                escape(&user.name)
            );
        }
        None => nav.push_str(r#"<a href="/login">Log in</a><a href="/register">Register</a>"#),
    }
    Resp::html(
        200,
        format!(
            r#"<!doctype html><html lang="en"><head><meta charset="utf-8"><meta name="viewport" content="width=device-width, initial-scale=1"><title>{} - {}</title><style>{STYLE}</style></head><body><div class="shell"><nav><div class="brand"><b>BF2</b><span>RUST ENGINE</span></div>{nav}</nav><main>{body}</main></div></body></html>"#,
            escape(title),
            escape(&master.config.name)
        ),
    )
}

/// The logged-in account of a web session cookie.
fn web_user(master: &Master, req: &Req) -> Option<Account> {
    let secret = req.cookie(COOKIE)?;
    let db = master.db.lock().unwrap();
    let id = db.token_account(&secret, WEB, unix_now()).ok()??;
    db.account(id).ok()?.filter(|a| !a.disabled)
}

fn session_cookie(master: &Master, secret: &str, max_age: u64) -> String {
    format!(
        "{COOKIE}={secret}; Path=/; HttpOnly; SameSite=Strict; Max-Age={max_age}{}",
        if master.config.secure_cookies() { "; Secure" } else { "" }
    )
}

/// `1h 23m`.
fn duration(seconds: f64) -> String {
    let minutes = (seconds / 60.0) as u64;
    if minutes >= 60 { format!("{}h {:02}m", minutes / 60, minutes % 60) } else { format!("{minutes}m") }
}

fn kd(kills: u32, deaths: u32) -> String {
    format!("{:.2}", kills as f64 / deaths.max(1) as f64)
}

fn rank_badge(rank: &RankInfo) -> String {
    format!(r#"<span class="badge" title="{}">{}</span>"#, escape(&rank.name), escape(&rank.short))
}

fn player_link(name: &str) -> String {
    format!(r#"<a href="/players/{}">{}</a>"#, url_encode(name), escape(name))
}

pub fn handle(master: &Master, req: &Req) -> Answer {
    let user = web_user(master, req);
    let user = user.as_ref();
    match (&req.method, req.path.as_str()) {
        (Method::Get, "/") => Ok(overview(master, user)),
        (Method::Get, "/leaderboard") => Ok(leaderboard(master, req, user)),
        (Method::Get, "/servers") => Ok(servers(master, user)),
        (Method::Get, "/ranks") => Ok(ranks(master, user)),
        (Method::Get, "/login") => Ok(login_form(master, user, None, "")),
        (Method::Get, "/register") => Ok(register_form(master, user, None, "")),
        (Method::Post, "/login") => Ok(login(master, req)),
        (Method::Post, "/register") => Ok(register(master, req)),
        (Method::Post, "/logout") => {
            if let Some(secret) = req.cookie(COOKIE) {
                let _ = master.db.lock().unwrap().remove_token(&secret);
            }
            Ok(Resp::redirect("/").with_header("Set-Cookie", session_cookie(master, "", 0)))
        }
        (Method::Get, "/favicon.ico") => Err(Resp::error(404, "none")),
        (Method::Get, path) if path.starts_with("/players/") => {
            let name = crate::http::url_decode(&path["/players/".len()..]);
            Ok(profile_page(master, &name, user))
        }
        _ => Ok(not_found(master, user)),
    }
}

fn not_found(master: &Master, user: Option<&Account>) -> Resp {
    let mut resp = page(master, "Not found", "", user, r#"<h1>Not found</h1><p class="sub">There is no such page.</p>"#);
    resp.status = 404;
    resp
}

fn overview(master: &Master, user: Option<&Account>) -> Resp {
    let (accounts, board) = {
        let db = master.db.lock().unwrap();
        (db.account_count().unwrap_or(0), db.leaderboard("xp", 10, &master.config.progression).unwrap_or_default())
    };
    let servers = master.list.lock().unwrap().entries();
    let players: u32 = servers.iter().map(|s| s.players).sum();
    let mut body = format!(
        r#"<h1>{}</h1><p class="sub">Optional accounts, stats and ranks for BF2 Rust Engine servers. Playing never needs an account: LAN and unranked servers work without one.</p>
<div class="tiles"><div class="tile"><b>{}</b><span>servers online</span></div><div class="tile"><b>{players}</b><span>players online</span></div><div class="tile"><b>{accounts}</b><span>accounts</span></div></div>"#,
        escape(&master.config.name),
        servers.len()
    );
    body.push_str("<h2>Top players</h2>");
    body.push_str(&board_table(&board));
    body.push_str("<h2>Servers</h2>");
    body.push_str(&server_table(&servers[..servers.len().min(10)]));
    if user.is_none() {
        body.push_str(r#"<p style="margin-top:22px"><a class="button" href="/register">Create an account</a> <a class="dim" style="margin-left:12px" href="/login">or log in</a></p>"#);
    }
    page(master, "Overview", "/", user, &body)
}

fn board_table(board: &[game_auth::api::LeaderboardEntry]) -> String {
    if board.is_empty() {
        return r#"<div class="card dim">Nobody has played a ranked round yet.</div>"#.into();
    }
    let mut rows = String::new();
    for e in board {
        let _ = write!(
            rows,
            r#"<tr><td class="num">{}</td><td>{}{}</td><td class="num">{}</td><td class="num">{}</td><td class="num">{}</td><td class="num">{}</td><td class="num">{}</td><td class="num">{}</td></tr>"#,
            e.position,
            rank_badge(&e.rank),
            player_link(&e.name),
            e.rank.xp,
            e.score,
            e.kills,
            e.deaths,
            kd(e.kills, e.deaths),
            duration(e.seconds)
        );
    }
    format!(
        r#"<div class="card"><table><thead><tr><th class="num">#</th><th>Player</th><th class="num">XP</th><th class="num">Score</th><th class="num">Kills</th><th class="num">Deaths</th><th class="num">K/D</th><th class="num">Time</th></tr></thead><tbody>{rows}</tbody></table></div>"#
    )
}

fn server_table(servers: &[game_auth::api::ServerEntry]) -> String {
    if servers.is_empty() {
        return r#"<div class="card dim">No servers online right now.</div>"#.into();
    }
    let mut rows = String::new();
    for s in servers {
        let name = if s.name.is_empty() { format!("{}:{}", s.address, s.port) } else { s.name.clone() };
        let _ = write!(
            rows,
            r#"<tr><td>{}{}{}</td><td>{}</td><td>{}</td><td class="num">{}/{}</td><td>{}:{}</td></tr>"#,
            escape(&name),
            if s.ranked { r#"<span class="tag ranked">ranked</span>"# } else { "" },
            if s.region.is_empty() { String::new() } else { format!(r#"<span class="tag">{}</span>"#, escape(&s.region)) },
            escape(&s.level),
            escape(&s.mode),
            s.players,
            s.max_players,
            escape(&s.address),
            s.port
        );
    }
    format!(
        r#"<div class="card"><table><thead><tr><th>Server</th><th>Map</th><th>Mode</th><th class="num">Players</th><th>Address</th></tr></thead><tbody>{rows}</tbody></table></div>"#
    )
}

fn leaderboard(master: &Master, req: &Req, user: Option<&Account>) -> Resp {
    let sort = req.query.get("sort").map_or("xp", String::as_str);
    let sort = if ["xp", "score", "kills", "kd", "time"].contains(&sort) { sort } else { "xp" };
    let board = master.db.lock().unwrap().leaderboard(sort, 100, &master.config.progression).unwrap_or_default();
    let mut chips = String::from(r#"<div class="chips">"#);
    for (key, label) in [("xp", "Rank"), ("score", "Score"), ("kills", "Kills"), ("kd", "K/D"), ("time", "Time played")] {
        let _ = write!(chips, r#"<a href="/leaderboard?sort={key}"{}>{label}</a>"#, if key == sort { r#" class="on""# } else { "" });
    }
    chips.push_str("</div>");
    let body = format!(r#"<h1>Leaderboard</h1><p class="sub">Career stats from ranked servers.</p>{chips}{}"#, board_table(&board));
    page(master, "Leaderboard", "/leaderboard", user, &body)
}

fn servers(master: &Master, user: Option<&Account>) -> Resp {
    let servers = master.list.lock().unwrap().entries();
    let body = format!(
        r#"<h1>Servers</h1><p class="sub">Servers that announce themselves to this master. Ranked servers need an account and count your stats; join them from the game's Join page.</p>{}"#,
        server_table(&servers)
    );
    page(master, "Servers", "/servers", user, &body)
}

fn ranks(master: &Master, user: Option<&Account>) -> Resp {
    let p = &master.config.progression;
    let mut rows = String::new();
    for rank in &p.ranks {
        let _ = write!(
            rows,
            r#"<tr><td><span class="badge">{}</span>{}</td><td class="num">{}</td></tr>"#,
            escape(&rank.short),
            escape(&rank.name),
            rank.xp
        );
    }
    let body = format!(
        r#"<h1>Ranks</h1><p class="sub">XP comes from ranked rounds: {} per point of score, {} per minute played and {} for a win (at most {} per round).</p><div class="card"><table><thead><tr><th>Rank</th><th class="num">XP</th></tr></thead><tbody>{rows}</tbody></table></div>"#,
        p.xp_per_score, p.xp_per_minute, p.win_bonus, p.max_round_xp
    );
    page(master, "Ranks", "/ranks", user, &body)
}

fn tally_table(title: &str, tallies: &[Tally], kills_first: bool) -> String {
    if tallies.is_empty() {
        return String::new();
    }
    let mut rows = String::new();
    for t in tallies.iter().take(8) {
        let value = if kills_first { format!("{} kills", t.kills) } else { duration(t.seconds) };
        let _ = write!(rows, r#"<tr><td>{}</td><td class="num">{value}</td></tr>"#, escape(&t.name));
    }
    format!(r#"<div class="card"><h2 style="margin-top:0">{title}</h2><table><tbody>{rows}</tbody></table></div>"#)
}

fn profile_body(profile: &Profile) -> String {
    let s = &profile.stats;
    let rank = &profile.rank;
    let next = match (&rank.next_name, rank.next_xp) {
        (Some(name), Some(xp)) => format!("{} XP to {}", xp.saturating_sub(rank.xp), escape(name)),
        _ => "Highest rank".into(),
    };
    let mut body = format!(
        r#"<div class="hero"><div><h1>{}</h1><div class="rank"><span class="badge">{}</span>{}</div></div></div>
<div class="card" style="margin-top:16px;max-width:640px"><div class="row" style="justify-content:space-between"><span>{} XP</span><span class="dim">{next}</span></div><div class="bar"><i style="width:{:.1}%"></i></div></div>
<h2>Career</h2><div class="tiles">"#,
        escape(&profile.name),
        escape(&rank.short),
        escape(&rank.name),
        rank.xp,
        rank.progress() * 100.0
    );
    for (value, label) in [
        (s.score.to_string(), "score"),
        (s.kills.to_string(), "kills"),
        (s.deaths.to_string(), "deaths"),
        (kd(s.kills, s.deaths), "kills per death"),
        (s.rounds.to_string(), "rounds"),
        (format!("{} / {}", s.wins, s.losses), "wins / losses"),
        (s.captures.to_string(), "flags captured"),
        (duration(s.seconds), "played"),
    ] {
        let _ = write!(body, r#"<div class="tile"><b>{value}</b><span>{label}</span></div>"#);
    }
    body.push_str("</div>");
    let tables = [
        tally_table("Kits", &s.kits, false),
        tally_table("Vehicles", &s.vehicles, false),
        tally_table("Weapons", &s.weapons, true),
    ]
    .concat();
    if !tables.is_empty() {
        let _ = write!(body, r#"<h2>Favourites</h2><div class="row">{tables}</div>"#);
    }
    body
}

fn profile_page(master: &Master, name: &str, user: Option<&Account>) -> Resp {
    let profile = {
        let db = master.db.lock().unwrap();
        db.account_by_name(name)
            .ok()
            .flatten()
            .filter(|a| !a.disabled)
            .and_then(|a| db.profile(&a, &master.config.progression).ok())
    };
    match profile {
        Some(profile) => page(master, &profile.name, "", user, &profile_body(&profile)),
        None => not_found(master, user),
    }
}

fn login_form(master: &Master, user: Option<&Account>, notice: Option<&str>, name: &str) -> Resp {
    let notice = notice.map(|n| format!(r#"<div class="notice">{}</div>"#, escape(n))).unwrap_or_default();
    let body = format!(
        r#"<h1>Log in</h1><p class="sub">The same account works in the game (Account page).</p>{notice}
<form method="post" action="/login"><label for="name">Name</label><input id="name" name="name" value="{}" maxlength="24" autocomplete="username" required>
<label for="password">Password</label><input id="password" name="password" type="password" maxlength="128" autocomplete="current-password" required>
<div style="margin-top:8px"><button type="submit">Log in</button> <a class="dim" style="margin-left:12px" href="/register">No account yet?</a></div></form>"#,
        escape(name)
    );
    let mut resp = page(master, "Log in", "", user, &body);
    if notice.is_empty() {
        return resp;
    }
    resp.status = 400;
    resp
}

fn register_form(master: &Master, user: Option<&Account>, notice: Option<&str>, name: &str) -> Resp {
    let notice = notice.map(|n| format!(r#"<div class="notice">{}</div>"#, escape(n))).unwrap_or_default();
    let body = format!(
        r#"<h1>Register</h1><p class="sub">An account keeps your stats and rank from ranked servers. You never need one to play.</p>{notice}
<form method="post" action="/register"><label for="name">Name (3-24 letters, digits, _ - .)</label><input id="name" name="name" value="{}" maxlength="24" autocomplete="username" required>
<label for="password">Password (at least 8 characters)</label><input id="password" name="password" type="password" minlength="8" maxlength="128" autocomplete="new-password" required>
<label for="email">Email (optional)</label><input id="email" name="email" type="email" maxlength="254" autocomplete="email">
<div style="margin-top:8px"><button type="submit">Create account</button></div></form>"#,
        escape(name)
    );
    let mut resp = page(master, "Register", "", user, &body);
    if !notice.is_empty() {
        resp.status = 400;
    }
    resp
}

/// Starts a web session: a cookie, and to the profile.
fn start_session(master: &Master, account: &Account) -> Resp {
    let secret = new_secret();
    let now = unix_now();
    if let Err(err) = master.db.lock().unwrap().add_token(&secret, account.id, WEB, now, now + WEB_SESSION_SECS) {
        eprintln!("error: {err}");
        return Resp::error(500, "internal error");
    }
    Resp::redirect(&format!("/players/{}", url_encode(&account.name))).with_header("Set-Cookie", session_cookie(master, &secret, WEB_SESSION_SECS))
}

fn message_of(resp: &Resp) -> String {
    serde_json::from_slice::<game_auth::api::ApiError>(&resp.body).map_or_else(|_| "Something went wrong.".into(), |e| e.error)
}

fn login(master: &Master, req: &Req) -> Resp {
    let form = req.form();
    let name = form.get("name").map_or("", |n| n.trim());
    let password = form.get("password").map_or("", String::as_str);
    match authenticate(master, req, name, password) {
        Ok(account) => start_session(master, &account),
        Err(resp) => login_form(master, None, Some(&message_of(&resp)), name),
    }
}

fn register(master: &Master, req: &Req) -> Resp {
    let form = req.form();
    let credentials = Credentials {
        name: form.get("name").cloned().unwrap_or_default(),
        password: form.get("password").cloned().unwrap_or_default(),
        email: form.get("email").cloned(),
    };
    match create_account(master, req, &credentials) {
        Ok(account) => start_session(master, &account),
        Err(resp) => register_form(master, None, Some(&message_of(&resp)), credentials.name.trim()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn escapes() {
        assert_eq!(escape(r#"<a href="x">'&'</a>"#), "&lt;a href=&quot;x&quot;&gt;&#39;&amp;&#39;&lt;/a&gt;");
        assert_eq!(duration(3725.0), "1h 02m");
        assert_eq!(kd(3, 0), "3.00");
    }
}
