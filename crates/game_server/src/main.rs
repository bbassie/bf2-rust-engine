//! Headless dedicated server.
//!
//! ```text
//! server                                        # the test range
//! server --level strike_at_karkand --bots 16
//! server --config server.ron                    # name, map rotation, admin, ... (see server_config)
//! server rcon --password secret info players    # a running server's remote console
//! ```

use std::{path::PathBuf, time::Duration};

use bevy::{
    app::{ScheduleRunnerPlugin, TerminalCtrlCHandlerPlugin},
    log::LogPlugin,
    prelude::*,
    state::app::StatesPlugin,
};
use bevy_replicon_renet::RepliconRenetPlugins;
use clap::{Parser, Subcommand};
use game_server::{GameServerPlugin, admin::rcon, server_config::ServerConfig};
use game_shared::{SharedPlugin, TICK_HZ, config::GamePaths};

#[derive(Parser, Debug)]
#[command(version, about = "Dedicated server", args_conflicts_with_subcommands = true)]
struct Cli {
    #[command(subcommand)]
    command: Option<Command>,
    /// Server config file (RON): name, map rotation, admin password, ... The options below
    /// override it.
    #[arg(long)]
    config: Option<PathBuf>,
    /// Level to start on (folder name under `imported/levels`, or `test_range`). Default:
    /// the first map of the rotation, else `test_range`. The rotation continues after it.
    #[arg(long)]
    level: Option<String>,
    /// Game mode.
    #[arg(long)]
    mode: Option<String>,
    /// Layout size: 16, 32 or 64.
    #[arg(long)]
    size: Option<u32>,
    /// UDP port.
    #[arg(long)]
    port: Option<u16>,
    #[arg(long)]
    max_players: Option<usize>,
    /// Number of bots.
    #[arg(long)]
    bots: Option<u32>,
    /// Bot skill, 0..1: aim and reaction time.
    #[arg(long)]
    bot_skill: Option<f32>,
    /// Accept players from other machines (listen on all interfaces). Without it only
    /// clients on this machine can connect.
    #[arg(long)]
    public: bool,
    /// Server name shown in browsers.
    #[arg(long)]
    name: Option<String>,
    /// Percent of the level's tickets each team starts with.
    #[arg(long)]
    ticket_ratio: Option<f32>,
    /// Seconds from death to respawn.
    #[arg(long)]
    respawn_time: Option<f32>,
    /// Bullets hurt teammates.
    #[arg(long)]
    friendly_fire: bool,
    /// Password for the remote console and `/login` in the chat.
    #[arg(long)]
    admin_password: Option<String>,
    /// TCP port of the remote console (0: off).
    #[arg(long)]
    rcon_port: Option<u16>,
    /// Folder with converted assets (default: ./imported or $GAME_IMPORTED_DIR).
    #[arg(long)]
    imported: Option<PathBuf>,
}

#[derive(Subcommand, Debug)]
enum Command {
    /// Runs commands on a server's remote console and prints the answers (`help` lists
    /// them). Reads commands from standard input when none are given.
    Rcon {
        #[arg(long, default_value = "127.0.0.1")]
        host: String,
        #[arg(long, default_value_t = rcon::DEFAULT_PORT)]
        port: u16,
        #[arg(long)]
        password: String,
        /// Commands, one per argument: `server rcon --password pw info "kick 3 spam"`.
        commands: Vec<String>,
    },
}

fn main() -> AppExit {
    let cli = Cli::parse();
    if let Some(Command::Rcon {
        host,
        port,
        password,
        commands,
    }) = &cli.command
    {
        return match rcon::run_client(host, *port, password, commands) {
            Ok(()) => AppExit::Success,
            Err(err) => {
                eprintln!("rcon {host}:{port}: {err}");
                AppExit::error()
            }
        };
    }
    let config = match &cli.config {
        Some(path) => match ServerConfig::load(path) {
            Ok(config) => config,
            Err(err) => {
                eprintln!("{err:#}");
                return AppExit::error();
            }
        },
        None => ServerConfig::default(),
    };
    let mut settings = config.into_settings();
    if let Some(level) = cli.level {
        settings.level = level;
    }
    if let Some(mode) = cli.mode {
        settings.mode = mode;
    }
    if let Some(size) = cli.size {
        settings.size = size;
    }
    if let Some(port) = cli.port {
        settings.port = port;
    }
    if let Some(max_players) = cli.max_players {
        settings.max_clients = max_players;
    }
    if let Some(bots) = cli.bots {
        settings.bots = bots;
    }
    if let Some(skill) = cli.bot_skill {
        settings.bot_skill = skill.clamp(0.0, 1.0);
    }
    if let Some(name) = cli.name {
        settings.name = name;
    }
    if let Some(ratio) = cli.ticket_ratio {
        settings.ticket_ratio = ratio.max(1.0);
    }
    if let Some(seconds) = cli.respawn_time {
        settings.respawn_seconds = seconds.max(0.0);
    }
    if let Some(password) = cli.admin_password {
        settings.admin.password = password;
    }
    if let Some(port) = cli.rcon_port {
        settings.admin.rcon_port = port;
    }
    settings.public |= cli.public;
    settings.friendly_fire |= cli.friendly_fire;

    let mut app = App::new();
    // Explicit plugin list rather than DefaultPlugins, so the server stays headless even
    // when a workspace build unifies rendering features into Bevy.
    app.add_plugins((
        MinimalPlugins.set(ScheduleRunnerPlugin::run_loop(Duration::from_secs_f64(
            1.0 / (TICK_HZ * 2.0),
        ))),
        LogPlugin::default(),
        TerminalCtrlCHandlerPlugin,
        StatesPlugin,
        TransformPlugin,
        AssetPlugin::default(),
    ))
    // Physics may expect mesh assets depending on unified features; they are CPU-only here.
    .init_asset::<Mesh>()
    .insert_resource(GamePaths::resolve(cli.imported))
    .add_plugins((
        SharedPlugin,
        RepliconRenetPlugins,
        GameServerPlugin {
            settings: Some(settings),
        },
    ));
    app.run()
}
