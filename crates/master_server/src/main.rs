//! Optional master server. Nothing in the game needs it: LAN games, listen servers and
//! unranked servers work without one. It offers:
//!
//! - the **server list**: game servers announce themselves with a heartbeat, the menu's
//!   browser lists them (`list`);
//! - **accounts** with Argon2id-hashed passwords in SQLite, login rate limits, and
//!   short-lived **session tokens and join tickets** signed with the master's Ed25519 key,
//!   which game servers check offline (`api`, `game_auth::token`);
//! - **ranked servers** (registered here, with an API key) that require accounts and report
//!   **career stats**, which earn XP and **ranks** (`game_auth::ranks`);
//! - **quick join**, and **web pages** with leaderboards and profiles (`web`);
//! - **master admins** (`admin`): ranked servers, accounts (bans, one-time passwords,
//!   renames, roles), settings and an audit log on the web pages, behind two-factor
//!   authentication (`account`, `totp`).
//!
//! ```text
//! master                                   # UDP 127.0.0.1:16580, HTTP 127.0.0.1:16581
//! master --config master.ron               # see `config`
//! master add-server --name "My server"     # a ranked server: prints its API key (once)
//! master list-servers
//! master remove-server 3
//! master set-password alice                # reads the new password from standard input
//! master promote alice                     # a master admin (sets up 2FA on the next web login)
//! master demote alice
//! master reset-2fa alice                   # an admin who lost the device and recovery codes
//! master list-admins
//! ```
//!
//! Production: run it behind a reverse proxy that terminates TLS (docs/MODDING.md).

mod account;
mod admin;
mod api;
mod auth;
mod config;
mod db;
mod http;
mod list;
mod totp;
mod web;

use std::{
    net::{SocketAddr, UdpSocket},
    path::PathBuf,
    sync::{Arc, Mutex},
};

use clap::{Parser, Subcommand};
use game_auth::{Identity, unix_now};

use crate::{
    config::Config,
    db::{Actor, Db, Role, Target},
    http::Master,
};

#[derive(Parser, Debug)]
#[command(version, about = "Master server: server list, accounts, stats, ranks, web pages")]
struct Cli {
    #[command(subcommand)]
    command: Option<Command>,
    /// Config file (RON, see the docs of `config`).
    #[arg(long, global = true)]
    config: Option<PathBuf>,
    /// UDP address for heartbeats and browser lists (overrides the config).
    #[arg(long)]
    bind: Option<SocketAddr>,
    /// HTTP address for the web pages and the API (overrides the config).
    #[arg(long)]
    http: Option<SocketAddr>,
    /// Folder for the database and the signing key (overrides the config).
    #[arg(long, global = true)]
    data_dir: Option<PathBuf>,
    /// Where players reach the web pages, e.g. `https://master.example.com`.
    #[arg(long)]
    public_url: Option<String>,
}

#[derive(Subcommand, Debug)]
enum Command {
    /// Registers a ranked server and prints its API key (shown once; stored hashed).
    AddServer {
        #[arg(long)]
        name: String,
    },
    /// Lists the ranked servers.
    ListServers,
    /// Removes a ranked server (its API key stops working).
    RemoveServer { id: u64 },
    /// Sets an account's password (read from standard input).
    SetPassword { name: String },
    /// Makes an account a master admin. It gets admin powers once it has set up two-factor
    /// authentication, which its next login on the web pages asks for.
    Promote { name: String },
    /// Takes the master admin role away (its web sessions end).
    Demote { name: String },
    /// Turns an account's two-factor authentication off, e.g. for an admin who lost both the
    /// device and the recovery codes: the next web login sets it up again.
    #[command(name = "reset-2fa")]
    Reset2fa { name: String },
    /// Lists the master admins.
    ListAdmins,
}

fn main() -> std::process::ExitCode {
    match run() {
        Ok(()) => std::process::ExitCode::SUCCESS,
        Err(err) => {
            eprintln!("master: {err}");
            std::process::ExitCode::FAILURE
        }
    }
}

fn run() -> Result<(), String> {
    let cli = Cli::parse();
    let mut config = match &cli.config {
        Some(path) => Config::load(path)?,
        None => Config::default().normalized(),
    };
    if let Some(bind) = cli.bind {
        config.udp_bind = bind;
    }
    if let Some(http) = cli.http {
        config.http_bind = http;
    }
    if let Some(dir) = &cli.data_dir {
        config.data_dir = dir.clone();
    }
    if let Some(url) = &cli.public_url {
        config.public_url = url.trim().trim_end_matches('/').to_string();
    }
    let db_path = config.data_dir.join("master.sqlite");
    let mut db = Db::open(&db_path).map_err(|err| format!("{}: {err}", db_path.display()))?;
    let cli_actor = Actor::command_line();
    let account_named = |db: &Db, name: &str| db.account_by_name(name).map_err(|err| err.to_string())?.ok_or(format!("no account {name}"));
    match cli.command {
        Some(Command::AddServer { name }) => {
            let name = name.trim();
            if name.is_empty() || name.len() > 64 {
                return Err("--name: 1 to 64 characters".into());
            }
            let key = auth::new_api_key();
            let id = db.add_server(name, &key, unix_now()).map_err(|err| err.to_string())?;
            let _ = db.audit(&cli_actor, "server.add", &Target::server(id, name), "", unix_now());
            println!("ranked server {id} \"{name}\" added. Its API key (shown only now):\n\n  {key}\n");
            println!("In the game server's config: ranked: true, master_url: \"<this master's https address>\", master_api_key: \"{key}\"");
            Ok(())
        }
        Some(Command::ListServers) => {
            let servers = db.servers().map_err(|err| err.to_string())?;
            if servers.is_empty() {
                println!("no ranked servers (add one with `master add-server --name <name>`)");
            }
            for s in servers {
                let fingerprint = game_auth::unhex_array::<32>(&s.public_key).map_or_else(|| "no heartbeat yet".into(), |k| game_auth::fingerprint(&k));
                let seen = if s.last_seen == 0 { "never".to_string() } else { format!("{} s ago", unix_now().saturating_sub(s.last_seen)) };
                let days = unix_now().saturating_sub(s.created) / 86_400;
                let state = if s.disabled { "  DISABLED" } else { "" };
                println!("{:>4}  {:<32} key {fingerprint}  last seen {seen} {}, added {days} days ago{state}", s.id, s.name, s.last_address);
            }
            Ok(())
        }
        Some(Command::RemoveServer { id }) => {
            let name = db.server(id).map_err(|err| err.to_string())?.map(|s| s.name).unwrap_or_default();
            if db.remove_server(id).map_err(|err| err.to_string())? {
                let _ = db.audit(&cli_actor, "server.remove", &Target::server(id, &name), "", unix_now());
                println!("removed ranked server {id}");
                Ok(())
            } else {
                Err(format!("no ranked server {id}"))
            }
        }
        Some(Command::SetPassword { name }) => {
            let account = account_named(&db, &name)?;
            eprintln!("New password for {}:", account.name);
            let mut password = String::new();
            std::io::stdin().read_line(&mut password).map_err(|err| err.to_string())?;
            let password = password.trim_end_matches(['\r', '\n']);
            game_auth::validate_password(password)?;
            db.set_password(account.id, &auth::hash_password(password), false).map_err(|err| err.to_string())?;
            // Everything logged in with the old password ends.
            db.revoke_tokens(account.id, &[], None).map_err(|err| err.to_string())?;
            let _ = db.audit(&cli_actor, "password.set", &Target::account(&account), "", unix_now());
            println!("password of {} changed; its sessions and game logins ended", account.name);
            Ok(())
        }
        Some(Command::Promote { name }) => {
            let account = account_named(&db, &name)?;
            if account.is_admin() {
                println!("{} is already a master admin{}", account.name, if account.totp_enabled { "" } else { " (two-factor authentication not set up yet)" });
                return Ok(());
            }
            db.set_role(account.id, Role::Admin).map_err(|err| err.to_string())?;
            // The next web login goes through two-factor setup.
            db.revoke_tokens(account.id, &[api::WEB, api::MFA_PENDING], None).map_err(|err| err.to_string())?;
            let _ = db.audit(&cli_actor, "promote", &Target::account(&account), "", unix_now());
            let login = if config.public_url.is_empty() { "/login on the web pages".to_string() } else { format!("{}/login", config.public_url) };
            println!(
                "{} is now a master admin. Log in at {login}: the first login sets up two-factor authentication \
                 (an authenticator app), and the admin pages (/admin) open after that.",
                account.name
            );
            Ok(())
        }
        Some(Command::Demote { name }) => {
            let account = account_named(&db, &name)?;
            if !account.is_admin() {
                return Err(format!("{} isn't a master admin", account.name));
            }
            db.set_role(account.id, Role::Player).map_err(|err| err.to_string())?;
            db.revoke_tokens(account.id, &[api::WEB, api::MFA_PENDING], None).map_err(|err| err.to_string())?;
            let _ = db.audit(&cli_actor, "demote", &Target::account(&account), "", unix_now());
            let left = db.admin_count().map_err(|err| err.to_string())?;
            println!("{} is no longer a master admin ({left} left)", account.name);
            Ok(())
        }
        Some(Command::Reset2fa { name }) => {
            let account = account_named(&db, &name)?;
            db.reset_totp(account.id).map_err(|err| err.to_string())?;
            db.revoke_tokens(account.id, &[api::WEB, api::MFA_PENDING], None).map_err(|err| err.to_string())?;
            let _ = db.audit(&cli_actor, "2fa.reset", &Target::account(&account), "from the command line", unix_now());
            println!("two-factor authentication of {} is off; the next web login sets it up again", account.name);
            Ok(())
        }
        Some(Command::ListAdmins) => {
            let admins = db.admins().map_err(|err| err.to_string())?;
            if admins.is_empty() {
                println!("no master admins (make one with `master promote <name>`)");
            }
            for a in admins {
                let two_factor = if a.totp_enabled { "two-factor on" } else { "two-factor NOT set up yet" };
                println!("{:>6}  {:<24} {two_factor}", a.id, a.name);
            }
            Ok(())
        }
        None => serve(config, db),
    }
}

fn serve(config: Config, db: Db) -> Result<(), String> {
    let key_path = config.data_dir.join("master.key");
    let (key, created) = Identity::load_or_create(&key_path).map_err(|err| format!("{}: {err}", key_path.display()))?;
    println!(
        "{} signing key {} ({}{})",
        if created { "made a new" } else { "using" },
        key.fingerprint(),
        key_path.display(),
        if created { "; back it up: game servers pin it" } else { "" }
    );
    let udp = UdpSocket::bind(config.udp_bind).map_err(|err| format!("can't listen on UDP {}: {err}", config.udp_bind))?;
    let http = tiny_http::Server::http(config.http_bind).map_err(|err| format!("can't listen on HTTP {}: {err}", config.http_bind))?;
    println!("server list on UDP {}", config.udp_bind);
    println!(
        "web pages and API on http://{}{}",
        config.http_bind,
        if config.public_url.is_empty() { String::new() } else { format!(" (public: {})", config.public_url) }
    );
    if !config.public_url.starts_with("https://") && !config.http_bind.ip().is_loopback() {
        println!("warning: serving accounts over plain HTTP; put a TLS reverse proxy in front (docs/MODDING.md)");
    }
    if config.http_bind.ip().is_loopback() && !config.trust_proxy {
        println!(
            "warning: listening on loopback without trust_proxy: if this runs behind a reverse proxy, every \
             player's login/registration rate limit will share the proxy's address unless you set trust_proxy \
             (docs/MODDING.md)"
        );
    }
    let list = Arc::new(Mutex::new(list::ServerList::default()));
    let master = Arc::new(Master {
        config,
        key,
        db: Mutex::new(db),
        list: list.clone(),
        limits: Mutex::new(Default::default()),
        admission: game_auth::admission::Limiter::new(http::MAX_TOTAL_REQUESTS, http::MAX_REQUESTS_PER_IP),
    });
    std::thread::Builder::new()
        .name("udp".into())
        .spawn(move || list::run_udp(udp, list))
        .map_err(|err| err.to_string())?;
    let pruner = master.clone();
    std::thread::Builder::new()
        .name("prune".into())
        .spawn(move || {
            loop {
                std::thread::sleep(std::time::Duration::from_secs(600));
                let now = unix_now();
                http::lock(&pruner.limits).prune();
                let _ = http::lock(&pruner.db).remove_expired(now);
                let ticket_cutoff = now.saturating_sub(pruner.config.ticket_window_hours * 3600);
                let _ = http::lock(&pruner.db).prune_tickets(ticket_cutoff);
            }
        })
        .map_err(|err| err.to_string())?;
    http::serve(http, master, 8);
    Ok(())
}

#[cfg(test)]
mod tests;
