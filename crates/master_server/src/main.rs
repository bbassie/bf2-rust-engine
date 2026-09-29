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
//! - **quick join**, and **web pages** with leaderboards and profiles (`web`).
//!
//! ```text
//! master                                   # UDP 127.0.0.1:16580, HTTP 127.0.0.1:16581
//! master --config master.ron               # see `config`
//! master add-server --name "My server"     # a ranked server: prints its API key (once)
//! master list-servers
//! master remove-server 3
//! master set-password alice                # reads the new password from standard input
//! ```
//!
//! Production: run it behind a reverse proxy that terminates TLS (docs/MODDING.md).

mod api;
mod auth;
mod config;
mod db;
mod http;
mod list;
mod web;

use std::{
    net::{SocketAddr, UdpSocket},
    path::PathBuf,
    sync::{Arc, Mutex},
};

use clap::{Parser, Subcommand};
use game_auth::{Identity, unix_now};

use crate::{config::Config, db::Db, http::Master};

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
    let db = Db::open(&db_path).map_err(|err| format!("{}: {err}", db_path.display()))?;
    match cli.command {
        Some(Command::AddServer { name }) => {
            let name = name.trim();
            if name.is_empty() || name.len() > 64 {
                return Err("--name: 1 to 64 characters".into());
            }
            let key = auth::new_api_key();
            let id = db.add_server(name, &key, unix_now()).map_err(|err| err.to_string())?;
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
                println!("{:>4}  {:<32} key {fingerprint}  last seen {seen}, added {days} days ago", s.id, s.name);
            }
            Ok(())
        }
        Some(Command::RemoveServer { id }) => {
            if db.remove_server(id).map_err(|err| err.to_string())? {
                println!("removed ranked server {id}");
                Ok(())
            } else {
                Err(format!("no ranked server {id}"))
            }
        }
        Some(Command::SetPassword { name }) => {
            let account = db.account_by_name(&name).map_err(|err| err.to_string())?.ok_or(format!("no account {name}"))?;
            eprintln!("New password for {}:", account.name);
            let mut password = String::new();
            std::io::stdin().read_line(&mut password).map_err(|err| err.to_string())?;
            let password = password.trim_end_matches(['\r', '\n']);
            game_auth::validate_password(password)?;
            db.set_password(account.id, &auth::hash_password(password)).map_err(|err| err.to_string())?;
            println!("password of {} changed", account.name);
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
