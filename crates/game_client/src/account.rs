//! Optional accounts on a master server (`crates/master_server`): login and registration,
//! the profile (rank, XP, career stats), join tickets for servers that take accounts, and
//! quick join. Nothing needs it: without a master server (`master_url` in the settings) or
//! without logging in, everything works as before.
//!
//! The client keeps its refresh token (not the password) in `account.ron` in the platform
//! config folder (`%APPDATA%\bf2-rust-engine` on Windows), or next to `--settings` when that
//! is given; scripted runs keep it in memory. Session tokens live only in memory. Logging out
//! revokes the refresh token on the master.

use std::{
    path::PathBuf,
    sync::{Arc, Mutex},
    time::Duration,
};

use bevy::prelude::*;
use game_auth::{
    api::{API_PREFIX, ApiError, Credentials, MasterInfo, Profile, QuickJoin, RefreshRequest, Session, TicketRequest, TicketResponse},
    unix_now,
};
use serde::{Deserialize, Serialize};

use crate::{
    Cli,
    settings::{Settings, SettingsFile},
};

pub struct AccountPlugin;

impl Plugin for AccountPlugin {
    fn build(&self, app: &mut App) {
        app.add_systems(Startup, load_account).add_systems(Update, follow_master_url);
    }
}

/// What `account.ron` keeps.
#[derive(Serialize, Deserialize, Clone, Debug, Default)]
#[serde(default)]
struct StoredAccount {
    master_url: String,
    name: String,
    refresh_token: String,
    /// The master's key fingerprint when we logged in.
    master_fingerprint: String,
}

/// The account, shared with background requests. The second lock is held while a refresh
/// token is being used: the master rotates it, so two requests must not use it at once.
#[derive(Resource, Clone, Default)]
pub struct Account(Arc<Mutex<AccountState>>, Arc<Mutex<()>>);

#[derive(Default)]
pub struct AccountState {
    /// The master's web address, from the settings.
    pub master_url: Option<String>,
    pub master: Option<MasterInfo>,
    pub name: String,
    refresh_token: Option<String>,
    /// Session token and when it expires.
    session: Option<(String, u64)>,
    pub profile: Option<Profile>,
    /// A request is running.
    pub busy: bool,
    /// The last request's problem, for the account page.
    pub error: Option<String>,
    /// Bumped on every change, so pages rebuild.
    pub version: u32,
    file: Option<PathBuf>,
}

impl AccountState {
    pub fn logged_in(&self) -> bool {
        self.refresh_token.is_some()
    }

    fn save(&self) {
        let Some(file) = &self.file else {
            return;
        };
        let stored = StoredAccount {
            master_url: self.master_url.clone().unwrap_or_default(),
            name: self.name.clone(),
            refresh_token: self.refresh_token.clone().unwrap_or_default(),
            master_fingerprint: self.master.as_ref().map(|m| m.fingerprint.clone()).unwrap_or_default(),
        };
        if let Some(dir) = file.parent() {
            let _ = std::fs::create_dir_all(dir);
        }
        if let Err(err) = std::fs::write(file, ron::ser::to_string_pretty(&stored, Default::default()).unwrap_or_default()) {
            warn!("account: can't save {}: {err}", file.display());
        }
    }
}

fn agent() -> ureq::Agent {
    ureq::Agent::config_builder()
        .timeout_connect(Some(Duration::from_secs(5)))
        .timeout_global(Some(Duration::from_secs(15)))
        .http_status_as_error(false)
        .user_agent(format!("bf2-rust-engine/{}", env!("CARGO_PKG_VERSION")))
        .build()
        .into()
}

/// Reads a JSON answer, or the master's error message.
fn read<T: serde::de::DeserializeOwned>(response: Result<ureq::http::Response<ureq::Body>, ureq::Error>) -> Result<T, String> {
    let mut response = response.map_err(|err| format!("can't reach the master server ({err})"))?;
    let status = response.status().as_u16();
    let text = response.body_mut().with_config().limit(4 << 20).read_to_string().map_err(|err| err.to_string())?;
    if status == 200 {
        serde_json::from_str(&text).map_err(|err| format!("unexpected answer from the master server ({err})"))
    } else {
        Err(serde_json::from_str::<ApiError>(&text).map_or_else(|_| format!("the master server says HTTP {status}"), |e| e.error))
    }
}

impl Account {
    pub fn state(&self) -> std::sync::MutexGuard<'_, AccountState> {
        self.0.lock().unwrap()
    }

    fn url(&self, path: &str) -> Option<String> {
        self.state().master_url.as_ref().map(|base| format!("{base}{API_PREFIX}{path}"))
    }

    /// Runs `work` on a thread with the account marked busy; errors go to the page.
    fn background(&self, work: impl FnOnce(&Account) -> Result<(), String> + Send + 'static) {
        {
            let mut state = self.state();
            if state.busy {
                return;
            }
            state.busy = true;
            state.error = None;
            state.version += 1;
        }
        let account = self.clone();
        let _ = std::thread::Builder::new().name("account".into()).spawn(move || {
            let result = work(&account);
            let mut state = account.state();
            state.busy = false;
            state.error = result.err();
            if let Some(err) = &state.error {
                warn!("account: {err}");
            }
            state.version += 1;
        });
    }

    fn take_session(&self, session: Session) {
        let mut state = self.state();
        info!("account: logged in as {} ({})", session.profile.name, session.profile.rank.name);
        state.name = session.profile.name.clone();
        state.refresh_token = Some(session.refresh_token);
        state.session = Some((session.session_token, session.session_expires));
        state.profile = Some(session.profile);
        state.save();
        state.version += 1;
    }

    fn fetch_master(&self) -> Result<MasterInfo, String> {
        let url = self.url("/info").ok_or("no master server set")?;
        let info: MasterInfo = read(agent().get(url).call())?;
        self.state().master = Some(info.clone());
        Ok(info)
    }

    /// Logs in (or registers) in the background.
    pub fn login(&self, name: String, password: String, register: bool, email: Option<String>) {
        self.background(move |account| {
            account.fetch_master()?;
            let url = account.url(if register { "/register" } else { "/login" }).ok_or("no master server set")?;
            let session: Session = read(agent().post(url).send_json(&Credentials { name, password, email }))?;
            account.take_session(session);
            Ok(())
        });
    }

    /// Logs out: revokes the refresh token and forgets it.
    pub fn logout(&self) {
        let (url, token) = {
            let mut state = self.state();
            let token = state.refresh_token.take();
            state.session = None;
            state.profile = None;
            state.save();
            state.version += 1;
            (state.master_url.clone(), token)
        };
        if let (Some(url), Some(token)) = (url, token) {
            let _ = std::thread::Builder::new().name("logout".into()).spawn(move || {
                let _ = agent().post(format!("{url}{API_PREFIX}/logout")).send_json(&RefreshRequest { refresh_token: token });
            });
        }
    }

    /// A valid session token, refreshing it if needed (blocking).
    fn session_token(&self) -> Result<String, String> {
        let _refreshing = self.1.lock().unwrap();
        let (session, refresh) = {
            let state = self.state();
            (state.session.clone(), state.refresh_token.clone())
        };
        if let Some((token, expires)) = session
            && expires > unix_now() + 30
        {
            return Ok(token);
        }
        let refresh_token = refresh.ok_or("not logged in")?;
        let url = self.url("/refresh").ok_or("no master server set")?;
        match read::<Session>(agent().post(url).send_json(&RefreshRequest { refresh_token })) {
            Ok(session) => {
                let token = session.session_token.clone();
                self.take_session(session);
                Ok(token)
            }
            Err(err) => {
                // The master doesn't know the refresh token any more: logged out.
                if err.contains("log in") {
                    let mut state = self.state();
                    state.refresh_token = None;
                    state.session = None;
                    state.save();
                    state.version += 1;
                }
                Err(err)
            }
        }
    }

    /// Fetches the profile again in the background (after logging in, or on the page).
    pub fn refresh_profile(&self) {
        if !self.state().logged_in() {
            return;
        }
        self.background(|account| {
            if account.state().master.is_none() {
                account.fetch_master()?;
            }
            let token = account.session_token()?;
            let url = account.url("/me").ok_or("no master server set")?;
            let profile: Profile = read(agent().get(url).header("Authorization", format!("Bearer {token}")).call())?;
            let mut state = account.state();
            state.profile = Some(profile);
            state.version += 1;
            Ok(())
        });
    }

    /// A join ticket for the server with this key fingerprint, if we are logged in to the
    /// master it trusts (blocking; for the join handshake). `Ok(None)`: no account to offer.
    pub fn ticket(&self, server_fingerprint: &str, master_fingerprint: &str) -> Result<Option<String>, String> {
        if !self.state().logged_in() {
            return Ok(None);
        }
        let ours = match self.state().master.as_ref().map(|m| m.fingerprint.clone()) {
            Some(fingerprint) => fingerprint,
            None => self.fetch_master()?.fingerprint,
        };
        let normalize = game_auth::identity::normalize_fingerprint;
        if normalize(&ours) != normalize(master_fingerprint) {
            info!("account: the server trusts another master server ({master_fingerprint}); joining without a ticket");
            return Ok(None);
        }
        let token = self.session_token()?;
        let url = self.url("/ticket").ok_or("no master server set")?;
        let answer: TicketResponse = read(
            agent()
                .post(url)
                .header("Authorization", format!("Bearer {token}"))
                .send_json(&TicketRequest { server: server_fingerprint.to_string() }),
        )?;
        Ok(Some(answer.ticket))
    }

    /// Servers for quick join from the master (blocking). `ranked`: only (un)ranked ones.
    pub fn quick_join(&self, ranked: Option<bool>) -> Result<QuickJoin, String> {
        let filter = match ranked {
            Some(true) => "?ranked=yes",
            Some(false) => "?ranked=no",
            None => "",
        };
        let url = self.url(&format!("/quickjoin{filter}")).ok_or("no master server set")?;
        read(agent().get(url).call())
    }
}

/// `account.ron`: next to `--settings` if given, else in the platform config folder; none
/// in scripted runs.
fn account_file(cli: &Cli, settings_file: &SettingsFile) -> Option<PathBuf> {
    if let Some(explicit) = &cli.settings {
        return Some(explicit.parent().map_or_else(|| PathBuf::from("account.ron"), |d| d.join("account.ron")));
    }
    if cli.scenario.is_some() || cli.screenshot.is_some() || settings_file.0.is_none() {
        return None;
    }
    crate::settings::config_dir().map(|d| d.join("bf2-rust-engine").join("account.ron"))
}

fn load_account(mut commands: Commands, cli: Res<Cli>, settings_file: Res<SettingsFile>, settings: Res<Settings>) {
    let file = account_file(&cli, &settings_file);
    let stored: StoredAccount = file
        .as_ref()
        .and_then(|f| std::fs::read_to_string(f).ok())
        .and_then(|text| ron::from_str(&text).ok())
        .unwrap_or_default();
    let master_url = clean_url(settings.master_url.as_deref());
    // A token for another master is no good.
    let same_master = master_url.as_deref() == Some(stored.master_url.as_str());
    let state = AccountState {
        master_url: master_url.clone(),
        name: stored.name,
        refresh_token: (same_master && !stored.refresh_token.is_empty()).then_some(stored.refresh_token),
        file,
        ..default()
    };
    let account = Account(Arc::new(Mutex::new(state)), default());
    if account.state().logged_in() {
        account.refresh_profile();
    }
    commands.insert_resource(account);
}

/// `https://host[:port][/path]` without a trailing slash; `None` for nothing or garbage.
pub fn clean_url(url: Option<&str>) -> Option<String> {
    let url = url?.trim().trim_end_matches('/');
    let rest = url.strip_prefix("https://").or_else(|| url.strip_prefix("http://"))?;
    (!rest.is_empty() && !rest.contains(char::is_whitespace)).then(|| url.to_string())
}

/// The master URL changed in the settings: log out of the old one.
fn follow_master_url(settings: Res<Settings>, account: Option<Res<Account>>) {
    let Some(account) = account else {
        return;
    };
    if !settings.is_changed() {
        return;
    }
    let url = clean_url(settings.master_url.as_deref());
    let changed = account.state().master_url != url;
    if changed {
        let mut state = account.state();
        state.master_url = url;
        state.master = None;
        state.refresh_token = None;
        state.session = None;
        state.profile = None;
        state.error = None;
        state.save();
        state.version += 1;
    }
}
