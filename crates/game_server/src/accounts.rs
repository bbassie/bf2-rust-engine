//! Optional accounts on game servers (see `crates/master_server`). Off by default: LAN and
//! listen servers need nothing.
//!
//! - **Unranked** (the default): no accounts. With `master_url` (or `master_key`) set, the
//!   server still checks the tickets players offer and shows their account name and rank,
//!   but anyone can join.
//! - **Ranked** (`ranked: true` and the `api_key` the master's admin gave out): players need
//!   an account. The server sends the master a heartbeat and, when a round ends, the stats of
//!   the players with a verified account.
//!
//! Tickets are checked offline with the master's public key: `master_key` in the config, or
//! fetched once from `<master_url>/api/v1/info` and remembered in `master-keys.txt` in the
//! server's data folder (delete the line if the master's key really changed). A ticket is for
//! this server only (its audience is the server's key fingerprint) and is accepted once.

use std::{
    collections::HashMap,
    sync::{Arc, Mutex},
    time::Duration,
};

use bevy::prelude::*;
use bevy_replicon::prelude::*;
use game_auth::{
    api::{API_PREFIX, MasterInfo, PlayerRound, RoundReport, RoundResult, ServerHeartbeat, Tally},
    token::{CLOCK_LEEWAY_SECS, Claims, TokenKind, verify_token},
    unhex_array, unix_now,
};
use game_shared::{
    join::AccountsInfo,
    protocol::{Player, Score, Team},
};

use crate::{
    PlayerClient, ServerSettings,
    join::{ClientAccount, ServerIdentity},
    stats::RoundStats,
};

pub struct AccountsPlugin;

impl Plugin for AccountsPlugin {
    fn build(&self, app: &mut App) {
        app.add_systems(
            Update,
            heartbeat.run_if(resource_exists::<Accounts>).run_if(in_state(ClientState::Disconnected)),
        );
    }
}

/// What a server does with accounts (see the module docs).
#[derive(Clone, Debug, Default)]
pub struct AccountSettings {
    /// The master server's web address (`https://master.example.com`): its key, heartbeats
    /// and stats.
    pub master_url: Option<String>,
    /// The master's public key (64 hex digits), if pinned in the config.
    pub master_key: Option<String>,
    /// Registered with the master (needs `api_key`): players need an account, stats are
    /// reported.
    pub ranked: bool,
    /// The API key the master's admin gave this server.
    pub api_key: Option<String>,
    /// For the master's server list and quick join: `eu`, `us-east`, ...
    pub region: String,
}

/// Accounts on this server.
#[derive(Resource)]
pub struct Accounts {
    master_url: String,
    /// Known once pinned or fetched.
    master_key: Arc<Mutex<Option<[u8; 32]>>>,
    /// Ranked, with an API key.
    ranked: bool,
    api_key: Option<String>,
    region: String,
    /// Tickets accepted, until they expire: id -> expiry.
    seen: HashMap<String, u64>,
    /// Seconds until the next heartbeat.
    next_heartbeat: f32,
}

impl Accounts {
    /// What the join challenge tells clients; `None` until the master's key is known.
    pub fn info(&self) -> Option<AccountsInfo> {
        let key = (*self.master_key.lock().unwrap())?;
        Some(AccountsInfo {
            master_fingerprint: game_auth::fingerprint(&key),
            master_url: self.master_url.clone(),
            required: self.ranked,
        })
    }

    /// Players need an account.
    pub fn required(&self) -> bool {
        self.ranked
    }

    pub fn master_url(&self) -> String {
        if self.master_url.is_empty() { "the master server".into() } else { self.master_url.clone() }
    }

    /// Checks a ticket for this server (`fingerprint`), accepting each one once.
    pub fn check_ticket(&mut self, ticket: &str, fingerprint: &str) -> Result<Claims, String> {
        let key = (*self.master_key.lock().unwrap()).ok_or("the server doesn't know the master server's key yet")?;
        let now = unix_now();
        let claims = verify_token(&key, ticket, now, TokenKind::Ticket, Some(fingerprint)).map_err(|err| err.to_string())?;
        self.seen.retain(|_, expires| *expires + CLOCK_LEEWAY_SECS >= now);
        if self.seen.insert(claims.jti.clone(), claims.exp).is_some() {
            return Err("ticket already used".into());
        }
        Ok(claims)
    }
}

fn agent() -> ureq::Agent {
    ureq::Agent::config_builder()
        .timeout_global(Some(Duration::from_secs(10)))
        .http_status_as_error(false)
        .user_agent(format!("bf2-rust-engine-server/{}", env!("CARGO_PKG_VERSION")))
        .build()
        .into()
}

/// `master-keys.txt` in the data folder: `url key` per line.
fn pin_file() -> Option<std::path::PathBuf> {
    crate::server_config::data_dir().map(|d| d.join("master-keys.txt"))
}

fn pinned_key(url: &str) -> Option<[u8; 32]> {
    let text = std::fs::read_to_string(pin_file()?).ok()?;
    text.lines().find_map(|line| {
        let (pinned_url, key) = line.split_once(' ')?;
        (pinned_url == url).then(|| unhex_array(key.trim()))?
    })
}

fn pin_key(url: &str, key: &[u8; 32]) {
    let Some(file) = pin_file() else {
        return;
    };
    let mut text = std::fs::read_to_string(&file).unwrap_or_default();
    text.push_str(&format!("{url} {}\n", game_auth::hex(key)));
    if let Some(dir) = file.parent() {
        let _ = std::fs::create_dir_all(dir);
    }
    if let Err(err) = std::fs::write(&file, text) {
        warn!("accounts: can't remember the master's key in {}: {err}", file.display());
    }
}

/// Fetches the master's key until it answers (every 30 s), then remembers it.
fn fetch_key(url: String, slot: Arc<Mutex<Option<[u8; 32]>>>) {
    let _ = std::thread::Builder::new().name("master key".into()).spawn(move || {
        let agent = agent();
        loop {
            let answer = agent
                .get(format!("{url}{API_PREFIX}/info"))
                .call()
                .map_err(|err| err.to_string())
                .and_then(|mut r| {
                    if r.status() != 200 {
                        return Err(format!("HTTP {}", r.status()));
                    }
                    r.body_mut().read_json::<MasterInfo>().map_err(|err| err.to_string())
                });
            match answer.map(|info| unhex_array::<32>(&info.public_key)) {
                Ok(Some(key)) => {
                    info!("accounts: master server {url} has key {}; remembered", game_auth::fingerprint(&key));
                    pin_key(&url, &key);
                    *slot.lock().unwrap() = Some(key);
                    return;
                }
                Ok(None) => warn!("accounts: {url} sent a malformed key"),
                Err(err) => warn!("accounts: can't reach the master server {url} ({err}); trying again in 30 s"),
            }
            std::thread::sleep(Duration::from_secs(30));
        }
    });
}

/// Sets accounts up if the server has a master server. Called by `start_server`.
pub fn start(world: &mut World) {
    let settings = world.resource::<ServerSettings>();
    if !settings.network {
        return;
    }
    let config = settings.accounts.clone();
    let url = config.master_url.as_deref().map(|u| u.trim().trim_end_matches('/').to_string()).filter(|u| !u.is_empty());
    if url.is_none() && config.master_key.is_none() {
        if config.ranked {
            warn!("accounts: `ranked` needs `master_url`; running unranked");
        }
        return;
    }
    let url = url.unwrap_or_default();
    // S20: the master's key (and, if ranked, this server's API key) travel over `master_url`,
    // so it must be https unless it's plainly local testing (no reverse proxy in front yet).
    if !url.is_empty() && !game_auth::master_url_allowed(&url) {
        warn!("accounts: master_url must be https:// (http:// only to localhost, or anywhere in development builds); ignoring accounts");
        return;
    }
    let ranked = config.ranked && config.api_key.as_ref().is_some_and(|k| !k.trim().is_empty()) && !url.is_empty();
    if config.ranked && !ranked {
        warn!("accounts: `ranked` needs `master_url` and the `api_key` the master's admin gave this server; running unranked");
    }
    let key = match config.master_key.as_deref().map(|k| unhex_array::<32>(k.trim())) {
        Some(Some(key)) => Some(key),
        Some(None) => {
            warn!("accounts: `master_key` should be 64 hex digits; ignoring it");
            None
        }
        None => None,
    };
    let slot = Arc::new(Mutex::new(key.or_else(|| pinned_key(&url))));
    if slot.lock().unwrap().is_none() && !url.is_empty() {
        fetch_key(url.clone(), slot.clone());
    }
    info!(
        "accounts: {} with master server {} ({})",
        if ranked { "ranked (accounts required, stats reported)" } else { "unranked (accounts optional)" },
        if url.is_empty() { "pinned key" } else { &url },
        slot.lock().unwrap().map_or_else(|| "key not known yet".to_string(), |k| game_auth::fingerprint(&k))
    );
    world.insert_resource(Accounts {
        master_url: url,
        master_key: slot,
        ranked,
        api_key: config.api_key.filter(|_| ranked),
        region: config.region,
        seen: HashMap::new(),
        next_heartbeat: 0.0,
    });
}

pub fn stop(world: &mut World) {
    world.remove_resource::<Accounts>();
}

/// POSTs JSON to the master with the server's API key, on a thread; `done` gets the answer.
fn post<T: serde::Serialize + Send + 'static>(
    url: String,
    api_key: String,
    body: T,
    done: impl FnOnce(Result<String, String>) + Send + 'static,
) {
    let _ = std::thread::Builder::new().name("master post".into()).spawn(move || {
        let result = agent()
            .post(&url)
            .header("Authorization", format!("Bearer {api_key}"))
            .send_json(&body)
            .map_err(|err| err.to_string())
            .and_then(|mut response| {
                let status = response.status().as_u16();
                let text = response.body_mut().read_to_string().unwrap_or_default();
                if status == 200 { Ok(text) } else { Err(format!("HTTP {status}: {}", text.chars().take(200).collect::<String>())) }
            });
        done(result);
    });
}

/// Ranked servers tell the master every 30 s that they are there and how full they are.
fn heartbeat(
    time: Res<Time<Real>>,
    settings: Res<ServerSettings>,
    mut accounts: ResMut<Accounts>,
    identity: Option<Res<ServerIdentity>>,
    discovery: Option<Res<crate::discovery::DiscoveryResponder>>,
    level: Option<Res<game_shared::level::LoadedLevel>>,
    players: Query<&Player>,
) {
    let (Some(api_key), Some(identity)) = (accounts.api_key.clone(), identity) else {
        return;
    };
    accounts.next_heartbeat -= time.delta_secs();
    if accounts.next_heartbeat > 0.0 {
        return;
    }
    accounts.next_heartbeat = game_shared::discovery::HEARTBEAT_SECONDS;
    let bots = players.iter().filter(|p| p.is_bot).count() as u32;
    let beat = ServerHeartbeat {
        port: settings.port,
        query_port: discovery.map_or(0, |d| d.query_port()),
        name: settings.name.clone(),
        level: level.map_or_else(|| settings.level.clone(), |l| l.desc.display_name.clone()),
        mode: settings.mode.clone(),
        players: players.iter().count() as u32 - bots,
        max_players: settings.max_clients as u32,
        bots,
        public_key: identity.0.public_hex(),
        region: accounts.region.clone(),
        address: None,
    };
    let url = format!("{}{API_PREFIX}/server/heartbeat", accounts.master_url);
    post(url, api_key, beat, |result| {
        if let Err(err) = result {
            warn!("accounts: heartbeat: {err}");
        }
    });
}

fn tally<V: Copy>(map: &bevy::platform::collections::HashMap<String, V>, to: impl Fn(&String, V) -> Tally) -> Vec<Tally> {
    let mut tallies: Vec<Tally> = map.iter().map(|(name, value)| to(name, *value)).collect();
    tallies.sort_by(|a, b| a.name.cmp(&b.name));
    tallies.truncate(game_auth::api::MAX_TALLIES);
    tallies
}

/// Sends the finished round's stats of players with a verified account to the master
/// (ranked servers only). Called by `stats` when a round ends.
pub fn report_round(world: &mut World, winner: Team) {
    let Some(accounts) = world.get_resource::<Accounts>() else {
        return;
    };
    let Some(api_key) = accounts.api_key.clone() else {
        return;
    };
    let url = format!("{}{API_PREFIX}/server/round", accounts.master_url);
    let settings = world.resource::<ServerSettings>();
    let (level, mode) = (settings.level.clone(), settings.mode.clone());
    let mut players = Vec::new();
    let mut longest = 0.0f64;
    let mut rows = world.query::<(&Player, &Team, &Score, &RoundStats, &PlayerClient)>();
    let accounts_of = |world: &World, client: Entity| world.get::<ClientAccount>(client).map(|a| a.0.clone());
    let collected: Vec<_> = rows
        .iter(world)
        .filter(|(player, team, ..)| !player.is_bot && **team != Team::Spectator)
        .map(|(_, team, score, stats, client)| (*team, *score, stats.clone(), client.0))
        .collect();
    for (team, score, stats, client) in collected {
        let Some(claims) = accounts_of(world, client) else {
            continue;
        };
        longest = longest.max(stats.seconds as f64);
        players.push(PlayerRound {
            account: claims.sub,
            name: claims.name.clone(),
            team: if team == Team::Two { 2 } else { 1 },
            score: score.score,
            kills: score.kills,
            deaths: score.deaths,
            captures: stats.captures,
            seconds: stats.seconds as f64,
            kits: tally(&stats.kit_seconds, |name, seconds| Tally { name: name.clone(), seconds: seconds as f64, kills: 0 }),
            vehicles: tally(&stats.vehicle_seconds, |name, seconds| Tally { name: name.clone(), seconds: seconds as f64, kills: 0 }),
            weapons: tally(&stats.weapon_kills, |name, kills| Tally { name: name.clone(), seconds: 0.0, kills }),
        });
    }
    if players.is_empty() {
        return;
    }
    let report = RoundReport {
        round_id: game_auth::hex(&game_auth::random_bytes::<16>()),
        level,
        mode,
        winner: match winner {
            Team::One => 1,
            Team::Two => 2,
            Team::Spectator => 0,
        },
        seconds: longest,
        players,
    };
    let count = report.players.len();
    info!("accounts: reporting the round of {count} ranked players to the master server");
    post(url, api_key, report, move |result| match result.map(|text| serde_json::from_str::<RoundResult>(&text)) {
        Ok(Ok(result)) => {
            info!(
                "accounts: the master counted the round for {} players{}{}",
                result.accepted,
                if result.rejected.is_empty() { String::new() } else { format!(", refused {:?}", result.rejected) },
                result
                    .promotions
                    .iter()
                    .map(|(id, rank)| format!("; account {id} promoted to {rank}"))
                    .collect::<String>()
            );
        }
        Ok(Err(err)) => warn!("accounts: round report: strange answer ({err})"),
        Err(err) => warn!("accounts: round report: {err}"),
    });
}

#[cfg(test)]
mod tests {
    use super::*;
    use bevy::app::App;
    use game_auth::{Identity, token};

    /// A pinned key so `start` never spawns its background `fetch_key` thread (which would
    /// otherwise retry a real network request every 30 s for the rest of the test process).
    fn pinned_key() -> Option<String> {
        Some(game_auth::hex(&Identity::generate().public_key()))
    }

    #[test]
    fn refuses_an_insecure_non_loopback_master_url() {
        let mut app = App::new();
        app.insert_resource(ServerSettings {
            accounts: AccountSettings { master_url: Some("http://master.example.com".into()), master_key: pinned_key(), ranked: true, ..default() },
            ..default()
        });
        start(app.world_mut());
        // Development builds (which run the tests) allow plain http to test machines.
        let refused = app.world().get_resource::<Accounts>().is_none();
        assert_eq!(refused, !cfg!(debug_assertions), "plain http to a real host is refused in release builds");
    }

    #[test]
    fn allows_https_and_local_http() {
        for url in ["https://master.example.com", "http://127.0.0.1:16581", "http://localhost:16581"] {
            let mut app = App::new();
            app.insert_resource(ServerSettings {
                accounts: AccountSettings { master_url: Some(url.into()), master_key: pinned_key(), ..default() },
                ..default()
            });
            start(app.world_mut());
            assert!(app.world().get_resource::<Accounts>().is_some(), "{url} should be accepted");
        }
    }

    #[test]
    fn tickets_are_checked_and_used_once() {
        let master = Identity::generate();
        let server = Identity::generate();
        let mut accounts = Accounts {
            master_url: "http://master".into(),
            master_key: Arc::new(Mutex::new(Some(master.public_key()))),
            ranked: true,
            api_key: Some("key".into()),
            region: String::new(),
            seen: HashMap::new(),
            next_heartbeat: 0.0,
        };
        assert_eq!(accounts.info().unwrap().master_fingerprint, master.fingerprint());
        let now = unix_now();
        let claims = |aud: &str, jti: &str| Claims {
            v: token::TOKEN_VERSION,
            kind: TokenKind::Ticket,
            sub: 1,
            name: "alice".into(),
            rank: 0,
            rank_name: "Private".into(),
            rank_short: "Pvt".into(),
            iat: now,
            exp: now + 300,
            aud: Some(aud.into()),
            jti: jti.into(),
        };
        let ticket = token::sign(&master, &claims(&server.fingerprint(), "a"));
        assert_eq!(accounts.check_ticket(&ticket, &server.fingerprint()).unwrap().name, "alice");
        assert!(accounts.check_ticket(&ticket, &server.fingerprint()).is_err(), "replayed");
        let elsewhere = token::sign(&master, &claims(&Identity::generate().fingerprint(), "b"));
        assert!(accounts.check_ticket(&elsewhere, &server.fingerprint()).is_err(), "another server's");
        let forged = token::sign(&Identity::generate(), &claims(&server.fingerprint(), "c"));
        assert!(accounts.check_ticket(&forged, &server.fingerprint()).is_err(), "another master's");
    }
}
