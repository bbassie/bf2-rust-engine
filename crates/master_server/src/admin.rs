//! The master admin pages: only for an admin account with two-factor authentication whose
//! web session passed it at login ([`WebSession::admin`]). Every POST also checks the
//! `Origin` header and the session's CSRF token, and every action goes to the audit log.
//!
//! ```text
//! /admin                    overview: counts, registration on/off, rank settings, recent log
//! /admin/servers            ranked servers: add (the API key is shown once), rotate a key,
//!                           disable/enable, remove; address and last heartbeat
//! /admin/accounts?q=        search accounts
//! /admin/accounts/<id>      profile and stats; ban/unban, one-time password, log out
//!                           everywhere, rename, promote/demote, reset two-factor
//! /admin/audit              the audit log (read-only: the database refuses edits)
//! ```
//!
//! Admins can't ban themselves or another admin (demote first), can't demote the last admin,
//! and reset their own two-factor authentication with a recovery code at login rather than
//! here. Promoting or demoting ends the account's web sessions, so the change shows at once
//! (a promoted admin sets up two-factor authentication on the next login); banning ends
//! every session and game login, and bans are checked on every request, ticket and refresh.

use std::{collections::HashMap, fmt::Write as _};

use game_auth::{fingerprint, unhex_array, unix_now, validate_account_name};

use crate::{
    api::{MFA_PENDING, REGISTRATION_SETTING, WEB, registration_open},
    auth::{hash_password, new_api_key, new_one_time_password},
    db::{Account, AuditEntry, Ban, RankedServer, Role, Target},
    http::{Master, Method, Req, Resp, lock, url_encode},
    web::{WebSession, ago, csrf_field, duration, escape, form_ok, format_time, kd, not_found, page},
};

fn internal(err: impl std::fmt::Display) -> Resp {
    eprintln!("error: {err}");
    Resp::error(500, "internal error")
}

fn with_status(mut resp: Resp, status: u16) -> Resp {
    resp.status = status;
    resp
}

/// A message above a page: `Ok` in blue, `Err` in red.
type Flash = Option<Result<String, String>>;

fn flash_html(flash: &Flash) -> String {
    match flash {
        Some(Ok(text)) => format!(r#"<div class="ok">{}</div>"#, escape(text)),
        Some(Err(text)) => format!(r#"<div class="notice">{}</div>"#, escape(text)),
        None => String::new(),
    }
}

/// Messages after a redirect (`?done=`): fixed texts, nothing from the URL reaches the page.
fn done_message(req: &Req) -> Flash {
    let text = match req.query.get("done")?.as_str() {
        "registration" => "Registration setting saved.",
        "server-disabled" => "Server disabled: its API key is refused until it's enabled again, and it left the server list.",
        "server-enabled" => "Server enabled.",
        "server-removed" => "Server removed: its API key no longer works.",
        "promoted" => "Promoted to master admin. They set up two-factor authentication on their next web login; until then they have no admin powers.",
        "demoted" => "No longer a master admin. Their web sessions ended.",
        "banned" => "Banned: every session and game login ended, and they can't log in, refresh or get join tickets.",
        "unbanned" => "Ban lifted.",
        "revoked" => "Every session and game login of this account ended.",
        "renamed" => "Renamed.",
        "2fa-reset" => "Two-factor authentication reset: they set it up again on their next login. Their web sessions ended.",
        _ => return None,
    };
    Some(Ok(text.to_string()))
}

fn forbidden(master: &Master, user: Option<&WebSession>, text: &str) -> Resp {
    let body = format!(r#"<h1>Admin</h1><div class="notice">{}</div>"#, escape(text));
    with_status(page(master, "Admin", "", user, &body), 403)
}

fn subnav(active: &str) -> String {
    let mut out = String::from(r#"<div class="chips">"#);
    for (path, label) in [("/admin", "Overview"), ("/admin/servers", "Ranked servers"), ("/admin/accounts", "Accounts"), ("/admin/audit", "Audit log")] {
        let _ = write!(out, r#"<a href="{path}"{}>{label}</a>"#, if active == path { r#" class="on""# } else { "" });
    }
    out.push_str("</div>");
    out
}

fn admin_page(master: &Master, session: &WebSession, title: &str, sub: &str, active: &str, flash: &Flash, body: &str) -> Resp {
    let body = format!(r#"<h1>{}</h1><p class="sub">{sub}</p>{}{}{body}"#, escape(title), subnav(active), flash_html(flash));
    let resp = page(master, title, "/admin", Some(session), &body);
    match flash {
        Some(Err(_)) => with_status(resp, 400),
        _ => resp,
    }
}

/// `/admin...`: role, two-factor authentication and (for POSTs) CSRF first.
pub fn handle(master: &Master, req: &Req, user: Option<&WebSession>) -> Resp {
    let get = req.method == Method::Get;
    let Some(session) = user else {
        return if get { Resp::redirect("/login") } else { forbidden(master, user, "Log in first.") };
    };
    if !session.account.is_admin() {
        return forbidden(master, user, "The admin pages are for master admins.");
    }
    if !session.account.totp_enabled {
        return if get { Resp::redirect("/account/2fa") } else { forbidden(master, user, "Set up two-factor authentication first.") };
    }
    if !session.mfa {
        return forbidden(master, user, "This session didn't pass two-factor authentication. Log out and log in again with a code.");
    }
    let path = req.path.as_str();
    let id_in = |prefix: &str| path.strip_prefix(prefix).and_then(|rest| rest.split('/').next()).and_then(|id| id.parse::<u64>().ok());
    match req.method {
        Method::Get => match path {
            "/admin" => overview(master, req, session, done_message(req)),
            "/admin/servers" => servers_page(master, session, done_message(req), ""),
            "/admin/accounts" => accounts_page(master, req, session),
            "/admin/audit" => audit_page(master, req, session),
            _ => match id_in("/admin/accounts/") {
                Some(id) => account_page(master, req, session, id, done_message(req), ""),
                None => not_found(master, user),
            },
        },
        Method::Post => {
            let form = req.form();
            if !form_ok(master, req, &form, &session.secret) {
                return with_status(forbidden(master, user, "The form expired or came from another site; reload the page and try again."), 400);
            }
            if path == "/admin/settings" {
                return settings(master, req, session, &form);
            }
            if path == "/admin/servers/add" {
                return add_server(master, req, session, &form);
            }
            let action = path.rsplit('/').next().unwrap_or("");
            if let Some(id) = id_in("/admin/servers/") {
                return server_action(master, req, session, id, action, &form);
            }
            if let Some(id) = id_in("/admin/accounts/") {
                return account_action(master, req, session, id, action, &form);
            }
            not_found(master, user)
        }
        Method::Other => not_found(master, user),
    }
}

// Overview and settings.

fn overview(master: &Master, _req: &Req, session: &WebSession, flash: Flash) -> Resp {
    let now = unix_now();
    let (counts, recent) = {
        let db = lock(&master.db);
        (db.counts(now).unwrap_or_default(), db.audit_log(None, None, 10).unwrap_or_default())
    };
    let online = lock(&master.list).entries();
    let ranked_online = online.iter().filter(|s| s.ranked).count();
    let players: u32 = online.iter().map(|s| s.players).sum();
    let mut tiles = String::from(r#"<div class="tiles">"#);
    for (value, label) in [
        (counts.accounts.to_string(), "accounts".to_string()),
        (counts.new_accounts_24h.to_string(), "new in 24 h".to_string()),
        (counts.admins.to_string(), "master admins".to_string()),
        (counts.banned.to_string(), "banned".to_string()),
        (counts.ranked_servers.to_string(), format!("ranked servers ({} disabled)", counts.disabled_servers)),
        (online.len().to_string(), format!("servers online ({ranked_online} ranked)")),
        (players.to_string(), "players online".to_string()),
        (counts.rounds_24h.to_string(), "ranked rounds in 24 h".to_string()),
    ] {
        let _ = write!(tiles, r#"<div class="tile"><b>{value}</b><span>{label}</span></div>"#);
    }
    tiles.push_str("</div>");
    let open = registration_open(master);
    let csrf = csrf_field(master, session);
    let p = &master.config.progression;
    let body = format!(
        r#"{tiles}
<h2>Settings</h2><div class="card" style="max-width:720px">
<form class="inline" method="post" action="/admin/settings">{csrf}<input type="hidden" name="registration" value="{}"><span>New accounts: <b>{}</b> <span class="muted">(master.ron default: {})</span></span><button class="plain small" type="submit">{}</button></form>
<p class="muted" style="margin:12px 0 0">Ranks and XP come from <code>progression</code> in master.ron (restart to change): {} ranks up to {} at {} XP; {} XP per point of score, {} per minute, {} for a win, at most {} per round. <a href="/ranks">The rank table</a></p></div>
<h2>Recent admin actions</h2>{}<p><a href="/admin/audit">The whole audit log</a></p>"#,
        if open { "off" } else { "on" },
        if open { "open" } else { "closed" },
        if master.config.allow_registration { "open" } else { "closed" },
        if open { "Close registration" } else { "Open registration" },
        p.ranks.len(),
        escape(&p.ranks.last().map(|r| r.name.clone()).unwrap_or_default()),
        p.ranks.last().map_or(0, |r| r.xp),
        p.xp_per_score,
        p.xp_per_minute,
        p.win_bonus,
        p.max_round_xp,
        audit_table(&recent, now),
    );
    admin_page(master, session, "Admin", "Master admin pages. Every action here is written to the audit log.", "/admin", &flash, &body)
}

fn settings(master: &Master, req: &Req, session: &WebSession, form: &HashMap<String, String>) -> Resp {
    let value = match form.get("registration").map(String::as_str) {
        Some("on") => "on",
        Some("off") => "off",
        _ => return overview(master, req, session, Some(Err("Unknown setting.".into()))),
    };
    let db = lock(&master.db);
    if let Err(err) = db.set_setting(REGISTRATION_SETTING, value) {
        return internal(err);
    }
    let _ = db.audit(&session.actor(req), "setting", &Target::setting("registration"), value, unix_now());
    Resp::redirect("/admin?done=registration")
}

// Ranked servers.

/// The server fingerprint from a stored public key, or a note.
fn key_fingerprint(public_key: &str) -> String {
    unhex_array::<32>(public_key).map_or_else(|| "no heartbeat yet".into(), |k| fingerprint(&k))
}

fn servers_page(master: &Master, session: &WebSession, flash: Flash, shown_key: &str) -> Resp {
    let now = unix_now();
    let servers = lock(&master.db).servers().unwrap_or_default();
    let csrf = csrf_field(master, session);
    let mut rows = String::new();
    for s in &servers {
        let online = s.last_seen > 0 && now.saturating_sub(s.last_seen) < crate::list::TIMEOUT.as_secs();
        let state = if s.disabled {
            r#"<span class="pill red">disabled</span>"#
        } else if online {
            r#"<span class="pill blue">online</span>"#
        } else {
            r#"<span class="pill">offline</span>"#
        };
        let toggle = if s.disabled { ("enable", "Enable") } else { ("disable", "Disable") };
        let _ = write!(
            rows,
            r#"<tr><td class="num">{}</td><td>{} {state}</td><td>{}</td><td title="{}">{}</td><td class="muted">{}</td><td>{}</td><td><div class="actions">
<form method="post" action="/admin/servers/{id}/rotate">{csrf}<button class="plain small" type="submit">Rotate key</button></form>
<form method="post" action="/admin/servers/{id}/{}">{csrf}<button class="plain small" type="submit">{}</button></form>
<form class="inline" method="post" action="/admin/servers/{id}/remove">{csrf}<label class="muted"><input type="checkbox" name="confirm" value="yes" required> sure</label><button class="danger small" type="submit">Remove</button></form></div></td></tr>"#,
            s.id,
            escape(&s.name),
            if s.last_address.is_empty() { "-".to_string() } else { escape(&s.last_address) },
            escape(&format_time(s.last_seen)),
            ago(s.last_seen, now),
            escape(&key_fingerprint(&s.public_key)),
            escape(&format_time(s.created).replace(" UTC", "")),
            toggle.0,
            toggle.1,
            id = s.id,
        );
    }
    let table = if servers.is_empty() {
        r#"<div class="card dim">No ranked servers yet.</div>"#.to_string()
    } else {
        format!(
            r#"<div class="card"><table><thead><tr><th class="num">ID</th><th>Name</th><th>Address</th><th>Last heartbeat</th><th>Key fingerprint</th><th>Added</th><th></th></tr></thead><tbody>{rows}</tbody></table></div>"#
        )
    };
    let body = format!(
        r#"{shown_key}{table}
<h2>Add a ranked server</h2><form class="inline" method="post" action="/admin/servers/add">{csrf}<input name="name" maxlength="64" placeholder="Server name" required><button type="submit">Add</button></form>
<p class="muted">Ranked servers need accounts and report stats. Removing a server also deletes its round history; disabling keeps it and refuses the key.</p>"#
    );
    admin_page(master, session, "Ranked servers", "Game servers registered here with an API key.", "/admin/servers", &flash, &body)
}

/// A just-made API key, shown once.
fn key_box(master: &Master, id: u64, name: &str, key: &str, what: &str) -> String {
    let url = if master.config.public_url.is_empty() { "<this master's https address>".to_string() } else { master.config.public_url.clone() };
    format!(
        r#"<div class="ok">{what} API key of ranked server {id} "{}" (shown only now; the master keeps only its hash):</div><div class="secret" style="max-width:720px;margin-bottom:10px">{}</div>
<p class="muted" style="margin-top:0">In the game server's config: <code>ranked: true, master_url: "{}", master_api_key: "{}"</code></p>"#,
        escape(name),
        escape(key),
        escape(&url),
        escape(key)
    )
}

fn add_server(master: &Master, req: &Req, session: &WebSession, form: &HashMap<String, String>) -> Resp {
    let name = form.get("name").map_or("", |n| n.trim());
    if name.is_empty() || name.chars().count() > 64 || name.chars().any(char::is_control) {
        return servers_page(master, session, Some(Err("Server names are 1 to 64 characters.".into())), "");
    }
    let key = new_api_key();
    let now = unix_now();
    let db = lock(&master.db);
    let id = match db.add_server(name, &key, now) {
        Ok(id) => id,
        Err(err) => return internal(err),
    };
    let _ = db.audit(&session.actor(req), "server.add", &Target::server(id, name), "", now);
    drop(db);
    println!("ranked server {id} \"{name}\" added by {}", session.account.name);
    servers_page(master, session, None, &key_box(master, id, name, &key, "The"))
}

/// Takes a server off the live list (it's listed by the address of its last heartbeat).
fn delist(master: &Master, server: &RankedServer) {
    if let Ok(addr) = server.last_address.parse::<std::net::SocketAddr>() {
        lock(&master.list).bye(addr.ip(), addr.port());
    }
}

fn server_action(master: &Master, req: &Req, session: &WebSession, id: u64, action: &str, form: &HashMap<String, String>) -> Resp {
    let now = unix_now();
    let server = lock(&master.db).server(id).ok().flatten();
    let Some(server) = server else {
        return servers_page(master, session, Some(Err(format!("There is no ranked server {id}."))), "");
    };
    let target = Target::server(server.id, &server.name);
    let actor = session.actor(req);
    match action {
        "rotate" => {
            let key = new_api_key();
            let db = lock(&master.db);
            if let Err(err) = db.set_server_key(id, &key) {
                return internal(err);
            }
            let _ = db.audit(&actor, "server.rotate-key", &target, "", now);
            drop(db);
            servers_page(master, session, None, &key_box(master, id, &server.name, &key, "The new"))
        }
        "disable" | "enable" => {
            let disable = action == "disable";
            let db = lock(&master.db);
            if let Err(err) = db.set_server_disabled(id, disable) {
                return internal(err);
            }
            let _ = db.audit(&actor, if disable { "server.disable" } else { "server.enable" }, &target, "", now);
            drop(db);
            if disable {
                delist(master, &server);
            }
            Resp::redirect(&format!("/admin/servers?done=server-{action}d"))
        }
        "remove" => {
            if form.get("confirm").map(String::as_str) != Some("yes") {
                return servers_page(master, session, Some(Err("Tick \"sure\" to remove a server.".into())), "");
            }
            let db = lock(&master.db);
            if let Err(err) = db.remove_server(id) {
                return internal(err);
            }
            let _ = db.audit(&actor, "server.remove", &target, &format!("key {}", key_fingerprint(&server.public_key)), now);
            drop(db);
            delist(master, &server);
            Resp::redirect("/admin/servers?done=server-removed")
        }
        _ => not_found(master, Some(session)),
    }
}

// Accounts.

fn role_pills(account: &Account, now: u64) -> String {
    let mut out = String::new();
    if account.is_admin() {
        out.push_str(r#" <span class="pill blue">admin</span>"#);
        if !account.totp_enabled {
            out.push_str(r#" <span class="pill">no 2FA yet</span>"#);
        }
    }
    if account.active_ban(now).is_some() {
        out.push_str(r#" <span class="pill red">banned</span>"#);
    }
    if account.disabled {
        out.push_str(r#" <span class="pill red">disabled</span>"#);
    }
    out
}

fn accounts_page(master: &Master, req: &Req, session: &WebSession) -> Resp {
    let now = unix_now();
    let query = req.query.get("q").map_or("", |q| q.trim());
    let found = lock(&master.db).search_accounts(query, 100).unwrap_or_default();
    let mut rows = String::new();
    for a in &found {
        let rank = master.config.progression.info(a.xp);
        let _ = write!(
            rows,
            r#"<tr><td class="num">{}</td><td><a href="/admin/accounts/{}">{}</a>{}</td><td><span class="badge">{}</span>{}</td><td class="muted">{}</td><td class="muted">{}</td></tr>"#,
            a.id,
            a.id,
            escape(&a.name),
            role_pills(a, now),
            escape(&rank.short),
            a.xp,
            escape(&format_time(a.created).replace(" UTC", "")),
            ago(a.last_login, now),
        );
    }
    let table = if found.is_empty() {
        r#"<div class="card dim">No accounts found.</div>"#.to_string()
    } else {
        format!(
            r#"<div class="card"><table><thead><tr><th class="num">ID</th><th>Name</th><th>Rank, XP</th><th>Created</th><th>Last login</th></tr></thead><tbody>{rows}</tbody></table></div>"#
        )
    };
    let body = format!(
        r#"<form class="inline" method="get" action="/admin/accounts" style="margin-bottom:14px"><input name="q" value="{}" maxlength="64" placeholder="Name or id"><button class="plain" type="submit">Search</button></form>{table}"#,
        escape(query)
    );
    let sub = if query.is_empty() { "The newest accounts; search by name or id." } else { "Accounts matching the search (admins first)." };
    admin_page(master, session, "Accounts", sub, "/admin/accounts", &None, &body)
}

fn account_page(master: &Master, _req: &Req, session: &WebSession, id: u64, flash: Flash, extra: &str) -> Resp {
    let now = unix_now();
    let (account, profile, tokens, codes_left, history) = {
        let db = lock(&master.db);
        let Some(account) = db.account(id).ok().flatten() else {
            drop(db);
            return not_found(master, Some(session));
        };
        let profile = db.profile(&account, &master.config.progression).ok();
        let tokens = db.token_counts(id, now).unwrap_or_default();
        let codes_left = db.recovery_codes_left(id).unwrap_or(0);
        let history = db.audit_log(None, Some(("account", id)), 20).unwrap_or_default();
        (account, profile, tokens, codes_left, history)
    };
    let csrf = csrf_field(master, session);
    let count = |kind: &str| tokens.iter().find(|(k, _)| k == kind).map_or(0, |(_, n)| *n);
    let myself = account.id == session.account.id;
    let a = |action: &str| format!("/admin/accounts/{id}/{action}");

    let ban_line = match &account.ban {
        Some(ban) if account.active_ban(now).is_some() => format!(
            r#"<tr><td>Banned</td><td><b>{}</b> by {} on {}, {}</td></tr>"#,
            escape(&ban.reason),
            escape(&ban.by),
            escape(&format_time(ban.at)),
            if ban.expires == 0 { "permanently".to_string() } else { format!("until {}", escape(&format_time(ban.expires))) }
        ),
        _ => String::new(),
    };
    let two_factor = match (account.is_admin(), account.totp_enabled) {
        (_, true) => format!("on, {codes_left} recovery codes left"),
        (true, false) => "not set up yet (no admin powers until then)".into(),
        (false, false) => "off".into(),
    };
    let info = format!(
        r#"<div class="card" style="max-width:720px"><table><tbody>
<tr><td>Role</td><td>{}</td></tr><tr><td>Two-factor</td><td>{two_factor}</td></tr>{ban_line}
<tr><td>Created</td><td>{}</td></tr><tr><td>Last login</td><td>{}</td></tr><tr><td>Email</td><td>{}</td></tr>
<tr><td>Logged in</td><td>{} browser sessions, {} game logins</td></tr>{}
</tbody></table></div>"#,
        if account.is_admin() { "master admin" } else { "player" },
        escape(&format_time(account.created)),
        escape(&format_time(account.last_login)),
        account.email.as_deref().map(escape).unwrap_or_else(|| "-".into()),
        count(WEB),
        count(crate::api::REFRESH),
        if account.must_change_password { r#"<tr><td>Password</td><td>one-time password set: they choose a new one at the next login</td></tr>"# } else { "" },
    );

    let stats = profile.map(|p| {
        let s = &p.stats;
        let mut tiles = String::from(r#"<h2>Stats</h2><div class="tiles">"#);
        for (value, label) in [
            (format!("{} ({})", escape(&p.rank.short), p.rank.xp), "rank (XP)".to_string()),
            (s.rounds.to_string(), "rounds".into()),
            (format!("{} / {}", s.wins, s.losses), "wins / losses".into()),
            (s.score.to_string(), "score".into()),
            (s.kills.to_string(), "kills".into()),
            (kd(s.kills, s.deaths), "kills per death".into()),
            (duration(s.seconds), "played".into()),
            (ago(s.last_played, now), "last played".into()),
        ] {
            let _ = write!(tiles, r#"<div class="tile"><b>{value}</b><span>{label}</span></div>"#);
        }
        tiles.push_str("</div>");
        tiles
    });

    let mut actions = String::new();
    let _ = write!(
        actions,
        r#"<div class="card"><h2 style="margin-top:0">Display name</h2><form class="inline" method="post" action="{}">{csrf}<input name="name" value="{}" maxlength="24" required><button class="plain small" type="submit">Rename</button></form></div>"#,
        a("rename"),
        escape(&account.name)
    );
    if !myself {
        if account.active_ban(now).is_some() {
            let _ = write!(
                actions,
                r#"<div class="card"><h2 style="margin-top:0">Ban</h2><form class="inline" method="post" action="{}">{csrf}<button class="plain small" type="submit">Lift the ban</button></form></div>"#,
                a("unban")
            );
        } else if !account.is_admin() {
            let _ = write!(
                actions,
                r#"<div class="card"><h2 style="margin-top:0">Ban</h2><form method="post" action="{}">{csrf}<label for="reason">Reason (shown to them)</label><input id="reason" name="reason" maxlength="200" required><label for="days">Days (empty: permanent)</label><input id="days" name="days" type="number" min="1" max="3650"><div><button class="danger small" type="submit">Ban</button></div></form></div>"#,
                a("ban")
            );
        }
        let _ = write!(
            actions,
            r#"<div class="card"><h2 style="margin-top:0">Password</h2><p class="muted" style="margin-top:0">Sets a one-time password to hand over: it only opens the web page that chooses a new one. Ends their logins.</p><form class="inline" method="post" action="{}">{csrf}<button class="plain small" type="submit">Set a one-time password</button></form></div>"#,
            a("password")
        );
    }
    let _ = write!(
        actions,
        r#"<div class="card"><h2 style="margin-top:0">Logins</h2><form class="inline" method="post" action="{}">{csrf}<button class="plain small" type="submit">Log out everywhere{}</button></form></div>"#,
        a("revoke"),
        if myself { " else" } else { "" }
    );
    let role_form = if account.is_admin() {
        format!(
            r#"<form class="inline" method="post" action="{}">{csrf}<button class="danger small" type="submit">Remove master admin</button></form>"#,
            a("demote")
        )
    } else {
        format!(
            r#"<form class="inline" method="post" action="{}">{csrf}<label class="muted"><input type="checkbox" name="confirm" value="yes" required> sure</label><button class="plain small" type="submit">Make master admin</button></form>"#,
            a("promote")
        )
    };
    let reset_2fa = if account.totp_enabled && !myself {
        format!(
            r#"<form class="inline" method="post" action="{}" style="margin-top:8px">{csrf}<label class="muted"><input type="checkbox" name="confirm" value="yes" required> sure</label><button class="danger small" type="submit">Reset two-factor</button></form>"#,
            a("reset-2fa")
        )
    } else {
        String::new()
    };
    let _ = write!(actions, r#"<div class="card"><h2 style="margin-top:0">Role</h2>{role_form}{reset_2fa}</div>"#);

    let body = format!(
        r#"{extra}{info}{}<h2>Actions</h2><div class="row" style="align-items:flex-start">{actions}</div><h2>Admin actions on this account</h2>{}<p><a href="/players/{}">Public profile</a></p>"#,
        stats.unwrap_or_default(),
        audit_table(&history, now),
        url_encode(&account.name)
    );
    let title = format!("{}{}", account.name, if myself { " (you)" } else { "" });
    let sub = format!("Account #{}{}", account.id, role_pills(&account, now));
    admin_page(master, session, &title, &sub, "/admin/accounts", &flash, &body)
}

fn account_action(master: &Master, req: &Req, session: &WebSession, id: u64, action: &str, form: &HashMap<String, String>) -> Resp {
    let now = unix_now();
    let account = lock(&master.db).account(id).ok().flatten();
    let Some(account) = account else {
        return not_found(master, Some(session));
    };
    let myself = account.id == session.account.id;
    let refuse = |text: &str| account_page(master, req, session, id, Some(Err(text.to_string())), "");
    let target = Target::account(&account);
    let actor = session.actor(req);
    let done = |what: &str| Resp::redirect(&format!("/admin/accounts/{id}?done={what}"));
    let db_result = |result: rusqlite::Result<()>| result.map_err(internal);
    match action {
        "rename" => {
            let name = form.get("name").map_or("", |n| n.trim());
            if name == account.name {
                return done("renamed");
            }
            if let Err(err) = validate_account_name(name) {
                return refuse(&err);
            }
            // (The database lock is released before `refuse`, which renders a page that takes it.)
            let renamed = lock(&master.db).rename(id, name);
            match renamed {
                Ok(true) => {}
                Ok(false) => return refuse("That name is taken."),
                Err(err) => return internal(err),
            }
            let _ = lock(&master.db).audit(&actor, "rename", &target, &format!("{} -> {name}", account.name), now);
            done("renamed")
        }
        "ban" => {
            if myself {
                return refuse("You can't ban yourself.");
            }
            if account.is_admin() {
                return refuse("Remove their admin role first.");
            }
            let reason = form.get("reason").map_or("", |r| r.trim());
            if reason.is_empty() || reason.chars().count() > 200 || reason.chars().any(char::is_control) {
                return refuse("A ban needs a reason (up to 200 characters).");
            }
            let days = form.get("days").map_or("", |d| d.trim());
            let expires = match days {
                "" | "0" => 0,
                days => match days.parse::<u64>() {
                    Ok(days @ 1..=3650) => now + days * 86_400,
                    _ => return refuse("Days: 1 to 3650, or empty for a permanent ban."),
                },
            };
            let ban = Ban { reason: reason.to_string(), expires, by: session.account.name.clone(), at: now };
            let db = lock(&master.db);
            // Takes effect at once: every session and game login ends, and the ban is checked
            // on every request, refresh and ticket.
            if let Err(resp) = db_result(db.set_ban(id, Some(&ban)).and_then(|()| db.revoke_tokens(id, &[], None).map(|_| ()))) {
                return resp;
            }
            let until = if expires == 0 { "permanent".to_string() } else { format!("until {}", format_time(expires)) };
            let _ = db.audit(&actor, "ban", &target, &format!("{until}: {reason}"), now);
            drop(db);
            println!("account {} banned by {} ({until})", account.name, session.account.name);
            done("banned")
        }
        "unban" => {
            let db = lock(&master.db);
            if let Err(resp) = db_result(db.set_ban(id, None)) {
                return resp;
            }
            let _ = db.audit(&actor, "unban", &target, "", now);
            done("unbanned")
        }
        "password" => {
            if myself {
                return refuse("Change your own password on the Account page.");
            }
            let password = new_one_time_password();
            let db = lock(&master.db);
            if let Err(resp) = db_result(db.set_password(id, &hash_password(&password), true).and_then(|()| db.revoke_tokens(id, &[], None).map(|_| ()))) {
                return resp;
            }
            let _ = db.audit(&actor, "password.reset", &target, "one-time password", now);
            drop(db);
            let login = if master.config.public_url.is_empty() { "/login".to_string() } else { format!("{}/login", master.config.public_url) };
            let extra = format!(
                r#"<div class="ok">One-time password for {} (shown only now). Hand it over privately: it works once, on {}, where they choose a new password. The game refuses it until then.</div><div class="secret" style="max-width:420px;margin-bottom:14px">{}</div>"#,
                escape(&account.name),
                escape(&login),
                escape(&password)
            );
            account_page(master, req, session, id, None, &extra)
        }
        "revoke" => {
            let keep = if myself { Some(session.secret.as_str()) } else { None };
            let db = lock(&master.db);
            let ended = match db.revoke_tokens(id, &[], keep) {
                Ok(ended) => ended,
                Err(err) => return internal(err),
            };
            let _ = db.audit(&actor, "sessions.revoke", &target, &format!("{ended} ended"), now);
            done("revoked")
        }
        "promote" => {
            if account.is_admin() {
                return refuse("Already a master admin.");
            }
            if form.get("confirm").map(String::as_str) != Some("yes") {
                return refuse("Tick \"sure\" to make someone a master admin.");
            }
            if let Some(refusal) = account.refusal(now) {
                return refuse(&format!("Not while they can't log in: {refusal}"));
            }
            let db = lock(&master.db);
            // Their next web login sets up two-factor authentication first.
            if let Err(resp) = db_result(db.set_role(id, Role::Admin).and_then(|()| db.revoke_tokens(id, &[WEB, MFA_PENDING], None).map(|_| ()))) {
                return resp;
            }
            let _ = db.audit(&actor, "promote", &target, "", now);
            drop(db);
            println!("account {} promoted to master admin by {}", account.name, session.account.name);
            done("promoted")
        }
        "demote" => {
            if !account.is_admin() {
                return refuse("Not a master admin.");
            }
            // Counted and changed under one lock, so two admins demoting each other at the same
            // time can't leave none.
            let db = lock(&master.db);
            match db.admin_count() {
                Ok(n) if n <= 1 => {
                    drop(db);
                    return refuse("This is the last master admin: promote someone else first.");
                }
                Ok(_) => {}
                Err(err) => return internal(err),
            }
            if let Err(resp) = db_result(db.set_role(id, Role::Player).and_then(|()| db.revoke_tokens(id, &[WEB, MFA_PENDING], None).map(|_| ()))) {
                return resp;
            }
            let _ = db.audit(&actor, "demote", &target, if myself { "themselves" } else { "" }, now);
            drop(db);
            println!("account {} is no longer a master admin ({})", account.name, session.account.name);
            if myself { Resp::redirect("/") } else { done("demoted") }
        }
        "reset-2fa" => {
            if myself {
                return refuse("Reset your own two-factor authentication with a recovery code when you log in.");
            }
            if !account.totp_enabled {
                return refuse("Two-factor authentication isn't set up.");
            }
            if form.get("confirm").map(String::as_str) != Some("yes") {
                return refuse("Tick \"sure\" to reset two-factor authentication.");
            }
            let mut db = lock(&master.db);
            if let Err(resp) = db_result(db.reset_totp(id)) {
                return resp;
            }
            if let Err(resp) = db_result(db.revoke_tokens(id, &[WEB, MFA_PENDING], None).map(|_| ())) {
                return resp;
            }
            let _ = db.audit(&actor, "2fa.reset", &target, "by another admin", now);
            done("2fa-reset")
        }
        _ => not_found(master, Some(session)),
    }
}

// The audit log.

fn audit_table(entries: &[AuditEntry], now: u64) -> String {
    if entries.is_empty() {
        return r#"<div class="card dim">Nothing yet.</div>"#.into();
    }
    let mut rows = String::new();
    for e in entries {
        let target = match (e.target_kind.as_str(), e.target_id) {
            ("account", Some(id)) => format!(r#"<a href="/admin/accounts/{id}">{}</a>"#, escape(&e.target)),
            ("server", Some(id)) => format!("server {id} {}", escape(&e.target)),
            ("setting", _) => format!("setting {}", escape(&e.target)),
            _ => escape(&e.target),
        };
        let actor = match e.actor_id {
            Some(id) => format!(r#"<a href="/admin/accounts/{id}">{}</a>"#, escape(&e.actor)),
            None => escape(&e.actor),
        };
        let _ = write!(
            rows,
            r#"<tr><td class="muted" title="{}">{}</td><td>{actor}<div class="muted">{}</div></td><td>{}</td><td>{target}</td><td>{}</td></tr>"#,
            escape(&format_time(e.at)),
            ago(e.at, now),
            escape(&e.ip),
            escape(&e.action),
            escape(&e.detail),
        );
    }
    format!(
        r#"<div class="card"><table><thead><tr><th>When</th><th>Who</th><th>Action</th><th>Target</th><th>Detail</th></tr></thead><tbody>{rows}</tbody></table></div>"#
    )
}

const AUDIT_PAGE: usize = 100;

fn audit_page(master: &Master, req: &Req, session: &WebSession) -> Resp {
    let now = unix_now();
    let before = req.query.get("before").and_then(|b| b.parse::<u64>().ok());
    let entries = lock(&master.db).audit_log(before, None, AUDIT_PAGE).unwrap_or_default();
    let older = match entries.last() {
        Some(last) if entries.len() == AUDIT_PAGE => format!(r#"<p><a class="button" href="/admin/audit?before={}">Older</a></p>"#, last.id),
        _ => String::new(),
    };
    let newer = if before.is_some() { r#"<p><a href="/admin/audit">Newest</a></p>"# } else { "" };
    let body = format!("{newer}{}{older}", audit_table(&entries, now));
    admin_page(
        master,
        session,
        "Audit log",
        "Every admin action and admin login, newest first. The log can't be edited or deleted, not even here.",
        "/admin/audit",
        &None,
        &body,
    )
}
