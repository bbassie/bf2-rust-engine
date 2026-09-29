//! Your own account on the web pages, and the two-factor step of an admin's login.
//!
//! ```text
//! /account                 password, log out everywhere, two-factor status
//! POST /account/password   change the password (also after an admin's one-time password)
//! POST /account/logout-all end every other web session and game login
//! /account/2fa             admins: set up two-factor authentication (secret, otpauth URI,
//!                          QR code), then confirm with a code to get 10 recovery codes
//! POST /account/2fa/recovery  admins: new recovery codes (needs a current code)
//! /login/2fa               the code (or a recovery code) after an admin's password
//! ```
//!
//! Two-factor authentication is for master admins only: the game client's login is
//! unchanged (admins play too), and a player-only master never shows any of this. A
//! recovery code at login turns two-factor authentication off, so the admin sets it up again
//! with a new device right away (that's what losing the old one calls for).

use std::fmt::Write as _;

use game_auth::unix_now;

use crate::{
    api::{MFA_PENDING, WEB},
    auth::{check_password, hash_password},
    db::{Account, Actor, Target},
    http::{Master, Method, Req, Resp, lock},
    totp,
    web::{
        ADMIN_SESSION_SECS, MFA_COOKIE, WebSession, csrf_field, csrf_token, escape, form_ok, not_found, page, start_session,
    },
};

fn internal(err: impl std::fmt::Display) -> Resp {
    eprintln!("error: {err}");
    Resp::error(500, "internal error")
}

fn notice(text: &str) -> String {
    format!(r#"<div class="notice">{}</div>"#, escape(text))
}

fn ok_notice(text: &str) -> String {
    format!(r#"<div class="ok">{}</div>"#, escape(text))
}

fn with_status(mut resp: Resp, status: u16) -> Resp {
    resp.status = status;
    resp
}

/// The key TOTP secrets are sealed with.
pub fn seal_key(master: &Master) -> [u8; 32] {
    master.key.derive_key(totp::SEAL_CONTEXT)
}

/// Ten new recovery codes, and their hashes for storing.
fn new_recovery_codes(account: u64) -> (Vec<String>, Vec<String>) {
    let codes: Vec<String> = (0..totp::RECOVERY_CODES).map(|_| totp::new_recovery_code()).collect();
    let hashes = codes.iter().map(|c| totp::recovery_hash(account, c)).collect();
    (codes, hashes)
}

fn codes_html(codes: &[String]) -> String {
    let mut out = String::from(r#"<div class="card" style="max-width:520px"><div class="codes">"#);
    for code in codes {
        let _ = write!(out, "<span>{}</span>", escape(code));
    }
    out.push_str(
        r#"</div><p class="muted" style="margin:14px 0 0">Each works once, instead of a code from your app. Keep them somewhere safe (a password manager, or on paper): they are shown only now. Using one turns two-factor authentication off, so you set it up again with a new device.</p></div>"#,
    );
    out
}

/// Checks a TOTP code for `account` and uses its step up. `Err` with the message to show.
fn check_code(master: &Master, req: &Req, account: &Account, code: &str) -> Result<(), String> {
    if let Err(minutes) = lock(&master.limits).check_totp(account.id) {
        return Err(format!("Too many wrong codes. Try again in {minutes} minutes."));
    }
    let now = unix_now();
    let state = lock(&master.db).totp_state(account.id).map_err(|err| format!("internal error: {err}"))?;
    let secret = state.secret.as_deref().and_then(|s| totp::open(&seal_key(master), account.id, s));
    let Some(secret) = secret else {
        eprintln!("error: the TOTP secret of account {} doesn't open (master.key changed?)", account.id);
        return Err("Two-factor authentication can't be checked (the master's key changed?). Use a recovery code, or ask another admin to reset it.".into());
    };
    let used = totp::verify(&secret, code, now, state.last_step).map(|step| lock(&master.db).use_totp_step(account.id, step));
    match used {
        Some(Ok(true)) => Ok(()),
        Some(Err(err)) => Err(format!("internal error: {err}")),
        _ => {
            lock(&master.limits).totp_failed(account.id, req.ip);
            let _ = lock(&master.db).audit(&Actor::account(account, req.ip), "2fa.failed", &Target::account(account), "wrong or reused code", now);
            Err("Wrong code (or one that was already used). Wait for the next one and try again.".into())
        }
    }
}

// The login's second step.

/// The pending login of the request's `bf2r_mfa` cookie: its secret and account.
fn pending_login(master: &Master, req: &Req) -> Option<(String, Account)> {
    let secret = req.cookie(MFA_COOKIE).filter(|s| !s.is_empty())?;
    let db = lock(&master.db);
    let id = db.token_account(&secret, MFA_PENDING, unix_now()).ok()??;
    Some((secret, db.account(id).ok()??))
}

fn code_form(master: &Master, user: Option<&WebSession>, secret: &str, account: &Account, message: Option<&str>) -> Resp {
    let body = format!(
        r#"<h1>Two-factor authentication</h1><p class="sub">{} is a master admin: enter the 6-digit code from your authenticator app.</p>{}
<form method="post" action="/login/2fa"><input type="hidden" name="csrf" value="{}"><label for="code">Code</label><input id="code" name="code" maxlength="24" autocomplete="one-time-code" autofocus required>
<p class="muted" style="margin:0">Lost your device? Enter one of your recovery codes instead. That turns two-factor authentication off, and you set it up again right after.</p>
<div style="margin-top:8px"><button type="submit">Log in</button> <a class="dim" style="margin-left:12px" href="/login">Start over</a></div></form>"#,
        escape(&account.name),
        message.map(notice).unwrap_or_default(),
        csrf_token(master, secret)
    );
    let resp = page(master, "Two-factor authentication", "", user, &body);
    if message.is_some() { with_status(resp, 400) } else { resp }
}

/// `/login/2fa`: the code after an admin's password.
pub fn login_2fa(master: &Master, req: &Req, user: Option<&WebSession>) -> Resp {
    let Some((secret, account)) = pending_login(master, req) else {
        return Resp::redirect("/login");
    };
    if req.method != Method::Post {
        return code_form(master, user, &secret, &account, None);
    }
    let form = req.form();
    if !form_ok(master, req, &form, &secret) {
        return code_form(master, user, &secret, &account, Some("Your session expired; please try again."));
    }
    let now = unix_now();
    let end_pending = || {
        let _ = lock(&master.db).remove_token(&secret);
    };
    if let Some(refusal) = account.refusal(now) {
        end_pending();
        return with_status(page(master, "Log in", "", user, &notice(&refusal)), 403);
    }
    if !(account.is_admin() && account.totp_enabled) {
        // Demoted, or two-factor reset by another admin, since the password step.
        end_pending();
        return start_session(master, &account, false);
    }
    if let Err(minutes) = lock(&master.limits).check_login(req.ip) {
        return code_form(master, user, &secret, &account, Some(&format!("Too many failed attempts from your address. Try again in {minutes} minutes.")));
    }
    let code = form.get("code").map_or("", |c| c.trim());
    if totp::looks_like_code(code) {
        return match check_code(master, req, &account, code) {
            Ok(()) => {
                end_pending();
                let _ = lock(&master.db).audit(&Actor::account(&account, req.ip), "login", &Target::account(&account), "with two-factor authentication", now);
                start_session(master, &account, true)
            }
            Err(message) => {
                let status = if message.starts_with("Too many") { 429 } else { 400 };
                with_status(code_form(master, user, &secret, &account, Some(&message)), status)
            }
        };
    }
    // A recovery code.
    if let Err(minutes) = lock(&master.limits).check_totp(account.id) {
        return with_status(code_form(master, user, &secret, &account, Some(&format!("Too many wrong codes. Try again in {minutes} minutes."))), 429);
    }
    let used = lock(&master.db).use_recovery_code(account.id, &totp::recovery_hash(account.id, code));
    match used {
        Ok(true) => {
            end_pending();
            let mut db = lock(&master.db);
            if let Err(err) = db.reset_totp(account.id) {
                return internal(err);
            }
            let _ = db.audit(
                &Actor::account(&account, req.ip),
                "2fa.reset",
                &Target::account(&account),
                "own, with a recovery code at login (sets it up again next)",
                now,
            );
            drop(db);
            let account = Account { totp_enabled: false, ..account };
            start_session(master, &account, false)
        }
        Ok(false) => {
            lock(&master.limits).totp_failed(account.id, req.ip);
            let _ = lock(&master.db).audit(&Actor::account(&account, req.ip), "2fa.failed", &Target::account(&account), "wrong recovery code", now);
            code_form(master, user, &secret, &account, Some("That isn't a code from your app or one of your recovery codes."))
        }
        Err(err) => internal(err),
    }
}

// /account...

pub fn handle(master: &Master, req: &Req, user: Option<&WebSession>) -> Resp {
    let Some(session) = user else {
        return Resp::redirect("/login");
    };
    match (&req.method, req.path.as_str()) {
        (Method::Get, "/account") => account_page(master, req, session, None),
        (Method::Get, "/account/2fa") => two_factor_page(master, session, None),
        (Method::Post, path) => {
            let form = req.form();
            if !form_ok(master, req, &form, &session.secret) {
                return with_status(account_page(master, req, session, Some(Err("The form expired; please try again.".into()))), 400);
            }
            match path {
                "/account/password" => change_password(master, req, session, &form),
                "/account/logout-all" => {
                    match lock(&master.db).revoke_tokens(session.account.id, &[], Some(&session.secret)) {
                        Ok(_) => Resp::redirect("/account?done=logout-all"),
                        Err(err) => internal(err),
                    }
                }
                "/account/2fa" => confirm_two_factor(master, req, session, &form),
                "/account/2fa/recovery" => new_recovery(master, req, session, &form),
                _ => not_found(master, user),
            }
        }
        _ => not_found(master, user),
    }
}

fn account_page(master: &Master, req: &Req, session: &WebSession, message: Option<Result<String, String>>) -> Resp {
    let account = &session.account;
    let done = match req.query.get("done").map(String::as_str) {
        Some("password") => Some(Ok("Password changed. Other browsers and the game were logged out.".to_string())),
        Some("logout-all") => Some(Ok("Every other browser and game login was logged out.".to_string())),
        _ => None,
    };
    let message = match message.or(done) {
        Some(Ok(text)) => ok_notice(&text),
        Some(Err(text)) => notice(&text),
        None => String::new(),
    };
    let reset = if account.must_change_password {
        notice("An admin reset your password. Choose a new one to go on (enter the one-time password as the current one); the game refuses the one-time password.")
    } else {
        String::new()
    };
    let two_factor = if account.is_admin() {
        let left = lock(&master.db).recovery_codes_left(account.id).unwrap_or(0);
        if account.totp_enabled {
            format!(
                r#"<h2>Two-factor authentication</h2><div class="card" style="max-width:640px">On. {left} recovery codes left. <a href="/account/2fa">Details and new recovery codes</a></div>"#
            )
        } else {
            r#"<h2>Two-factor authentication</h2><div class="card" style="max-width:640px">You are a master admin: <a href="/account/2fa">set up two-factor authentication</a> to use the admin pages.</div>"#.to_string()
        }
    } else {
        String::new()
    };
    let tokens = lock(&master.db).token_counts(account.id, unix_now()).unwrap_or_default();
    let count = |kind: &str| tokens.iter().find(|(k, _)| k == kind).map_or(0, |(_, n)| *n);
    let csrf = csrf_field(master, session);
    let body = format!(
        r#"<h1>Account</h1><p class="sub">{} &middot; {}</p>{message}{reset}
<h2>Password</h2><form method="post" action="/account/password">{csrf}
<label for="current">Current password</label><input id="current" name="current" type="password" maxlength="128" autocomplete="current-password" required>
<label for="password">New password (at least 8 characters)</label><input id="password" name="password" type="password" minlength="8" maxlength="128" autocomplete="new-password" required>
<label for="confirm">New password again</label><input id="confirm" name="confirm" type="password" minlength="8" maxlength="128" autocomplete="new-password" required>
<div style="margin-top:8px"><button type="submit">Change password</button></div></form>
{two_factor}
<h2>Logins</h2><div class="card" style="max-width:640px"><p style="margin-top:0">{} browser sessions and {} game logins.</p><form class="inline" method="post" action="/account/logout-all">{csrf}<button class="plain" type="submit">Log out everywhere else</button></form></div>"#,
        escape(&account.name),
        if account.is_admin() { "master admin" } else { "player" },
        count(WEB),
        count(crate::api::REFRESH),
    );
    page(master, "Account", "/account", Some(session), &body)
}

fn change_password(master: &Master, req: &Req, session: &WebSession, form: &std::collections::HashMap<String, String>) -> Resp {
    let fail = |text: &str| with_status(account_page(master, req, session, Some(Err(text.to_string()))), 400);
    if let Err(minutes) = lock(&master.limits).check_login(req.ip) {
        return with_status(account_page(master, req, session, Some(Err(format!("Too many failed attempts. Try again in {minutes} minutes.")))), 429);
    }
    let get = |key: &str| form.get(key).map_or("", String::as_str);
    let account = &session.account;
    if !check_password(get("current"), &account.password_hash) {
        lock(&master.limits).login_failed(req.ip, &account.name);
        return fail("The current password is wrong.");
    }
    if let Err(err) = game_auth::validate_password(get("password")) {
        return fail(&err);
    }
    if get("password") != get("confirm") {
        return fail("The new passwords don't match.");
    }
    if check_password(get("password"), &account.password_hash) {
        return fail("Choose a password different from the current one.");
    }
    let db = lock(&master.db);
    let result = db
        .set_password(account.id, &hash_password(get("password")), false)
        // Everything else logged in with the old password ends: other browsers, the game.
        .and_then(|()| db.revoke_tokens(account.id, &[], Some(&session.secret)));
    if let Err(err) = result {
        return internal(err);
    }
    if account.is_admin() {
        let _ = db.audit(&session.actor(req), "password.change", &Target::account(account), "own", unix_now());
    }
    Resp::redirect("/account?done=password")
}

// Two-factor authentication.

fn admins_only(master: &Master, session: &WebSession) -> Resp {
    let body = r#"<h1>Two-factor authentication</h1><p class="sub">Two-factor authentication is for master admins: it guards the admin pages. Your account works in the game and here with your password.</p>"#;
    with_status(page(master, "Two-factor authentication", "/account", Some(session), body), 403)
}

fn two_factor_page(master: &Master, session: &WebSession, message: Option<&str>) -> Resp {
    let account = &session.account;
    if !account.is_admin() {
        return admins_only(master, session);
    }
    let message = message.map(notice).unwrap_or_default();
    let csrf = csrf_field(master, session);
    if account.totp_enabled {
        let left = lock(&master.db).recovery_codes_left(account.id).unwrap_or(0);
        let renew = if session.mfa {
            format!(
                r#"<h2>New recovery codes</h2><form method="post" action="/account/2fa/recovery">{csrf}<label for="code">A current code from your app</label><input id="code" name="code" maxlength="8" inputmode="numeric" autocomplete="one-time-code" required><p class="muted" style="margin:0">Replaces the {left} codes you have left.</p><div style="margin-top:8px"><button type="submit">Make new recovery codes</button></div></form>"#
            )
        } else {
            r#"<p class="muted">Log out and log in again with a code to manage it.</p>"#.into()
        };
        let body = format!(
            r#"<h1>Two-factor authentication</h1><p class="sub">On: logging in here asks for a code from your authenticator app. {left} recovery codes left.</p>{message}{renew}"#
        );
        let resp = page(master, "Two-factor authentication", "/account", Some(session), &body);
        return if message.is_empty() { resp } else { with_status(resp, 400) };
    }
    // Enrolment: the same secret until it is confirmed, so reloading the page doesn't
    // invalidate what the app already scanned.
    let key = seal_key(master);
    let state = match lock(&master.db).totp_state(account.id) {
        Ok(state) => state,
        Err(err) => return internal(err),
    };
    let secret = match state.pending.as_deref().and_then(|s| totp::open(&key, account.id, s)) {
        Some(secret) => secret,
        None => {
            let secret = totp::new_secret().to_vec();
            if let Err(err) = lock(&master.db).set_totp_pending(account.id, Some(&totp::seal(&key, account.id, &secret))) {
                return internal(err);
            }
            secret
        }
    };
    let b32 = totp::base32_encode(&secret);
    let uri = totp::otpauth_uri(&master.config.name, &account.name, &b32);
    let qr = totp::qr_svg(&uri, 220).map(|svg| format!(r#"<div class="qr">{svg}</div>"#)).unwrap_or_default();
    let body = format!(
        r#"<h1>Set up two-factor authentication</h1><p class="sub">You are a master admin: the admin pages need a code from an authenticator app (any TOTP app: Aegis, 2FAS, Google Authenticator, 1Password, ...) on every login.</p>{message}
<div class="row" style="align-items:flex-start"><div style="flex:0 0 auto">{qr}</div><div class="card" style="max-width:560px">
<p style="margin-top:0">1. Scan the QR code with the app, or enter this key by hand (time-based, 6 digits, 30 seconds):</p><div class="secret">{}</div>
<p class="muted">Or open this link on the device with the app:</p><div class="secret" style="font-size:12px">{}</div>
<p>2. Enter the code the app shows now.</p>
<form method="post" action="/account/2fa">{csrf}<label for="code">Code</label><input id="code" name="code" maxlength="8" inputmode="numeric" autocomplete="one-time-code" required><div style="margin-top:8px"><button type="submit">Turn on</button></div></form></div></div>"#,
        escape(&totp::grouped(&b32)),
        escape(&uri),
    );
    let resp = page(master, "Set up two-factor authentication", "/account", Some(session), &body);
    if message.is_empty() { resp } else { with_status(resp, 400) }
}

fn codes_page(master: &Master, session: &WebSession, title: &str, codes: &[String]) -> Resp {
    let body = format!(
        r#"<h1>{}</h1><div class="ok">Two-factor authentication is on. These are your recovery codes:</div>{}<p style="margin-top:22px"><a class="button" href="/admin">Continue to the admin pages</a></p>"#,
        escape(title),
        codes_html(codes)
    );
    page(master, title, "/account", Some(session), &body)
}

fn confirm_two_factor(master: &Master, req: &Req, session: &WebSession, form: &std::collections::HashMap<String, String>) -> Resp {
    let account = &session.account;
    if !account.is_admin() {
        return admins_only(master, session);
    }
    if account.totp_enabled {
        return Resp::redirect("/account/2fa");
    }
    if let Err(minutes) = lock(&master.limits).check_totp(account.id) {
        return with_status(two_factor_page(master, session, Some(&format!("Too many wrong codes. Try again in {minutes} minutes."))), 429);
    }
    let key = seal_key(master);
    let state = match lock(&master.db).totp_state(account.id) {
        Ok(state) => state,
        Err(err) => return internal(err),
    };
    let Some((sealed, secret)) = state.pending.as_deref().and_then(|s| Some((s.to_string(), totp::open(&key, account.id, s)?))) else {
        return two_factor_page(master, session, Some("Scan the new key first."));
    };
    let now = unix_now();
    let Some(step) = totp::verify(&secret, form.get("code").map_or("", String::as_str), now, 0) else {
        lock(&master.limits).totp_failed(account.id, req.ip);
        return two_factor_page(master, session, Some("That code doesn't match. Check the app's clock and try the next code."));
    };
    let (codes, hashes) = new_recovery_codes(account.id);
    let mut db = lock(&master.db);
    // The session proved the code just now: it gets admin powers (with the shorter lifetime
    // of an admin session).
    let result = db.enable_totp(account.id, &sealed, step, &hashes).and_then(|()| db.upgrade_session(&session.secret, now + ADMIN_SESSION_SECS));
    if let Err(err) = result {
        return internal(err);
    }
    let _ = db.audit(&session.actor(req), "2fa.enrol", &Target::account(account), "", now);
    drop(db);
    println!("account {} set up two-factor authentication", account.name);
    let session = WebSession { account: Account { totp_enabled: true, ..account.clone() }, secret: session.secret.clone(), mfa: true };
    codes_page(master, &session, "Two-factor authentication is on", &codes)
}

fn new_recovery(master: &Master, req: &Req, session: &WebSession, form: &std::collections::HashMap<String, String>) -> Resp {
    if !session.admin() {
        return Resp::redirect("/account/2fa");
    }
    let account = &session.account;
    if let Err(message) = check_code(master, req, account, form.get("code").map_or("", String::as_str)) {
        return two_factor_page(master, session, Some(&message));
    }
    let (codes, hashes) = new_recovery_codes(account.id);
    let mut db = lock(&master.db);
    if let Err(err) = db.set_recovery_codes(account.id, &hashes) {
        return internal(err);
    }
    let _ = db.audit(&session.actor(req), "2fa.recovery-codes", &Target::account(account), "made new recovery codes", unix_now());
    drop(db);
    codes_page(master, session, "New recovery codes", &codes)
}
