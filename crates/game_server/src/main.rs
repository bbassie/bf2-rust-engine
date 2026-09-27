//! Headless dedicated server.

use std::{path::PathBuf, time::Duration};

use bevy::{
    app::{ScheduleRunnerPlugin, TerminalCtrlCHandlerPlugin},
    log::LogPlugin,
    prelude::*,
    state::app::StatesPlugin,
};
use bevy_replicon_renet::RepliconRenetPlugins;
use clap::Parser;
use game_server::{GameServerPlugin, ServerSettings};
use game_shared::{SharedPlugin, TICK_HZ, config::GamePaths};

#[derive(Parser, Debug)]
#[command(version, about = "Dedicated server")]
struct Cli {
    /// Level to run (folder name under `imported/levels`, or `test_range`).
    #[arg(long, default_value = game_shared::level::TEST_RANGE)]
    level: String,
    /// Game mode.
    #[arg(long, default_value = "gpm_cq")]
    mode: String,
    /// Layout size: 16, 32 or 64.
    #[arg(long, default_value_t = 64)]
    size: u32,
    /// UDP port.
    #[arg(long, default_value_t = game_shared::DEFAULT_PORT)]
    port: u16,
    #[arg(long, default_value_t = 64)]
    max_players: usize,
    /// Number of bots.
    #[arg(long, default_value_t = 0)]
    bots: u32,
    /// Bot skill, 0..1: aim and reaction time.
    #[arg(long, default_value_t = 0.5)]
    bot_skill: f32,
    /// Accept players from other machines (listen on all interfaces). Without it only
    /// clients on this machine can connect.
    #[arg(long)]
    public: bool,
    /// Folder with converted assets (default: ./imported or $GAME_IMPORTED_DIR).
    #[arg(long)]
    imported: Option<PathBuf>,
}

fn main() -> AppExit {
    let cli = Cli::parse();

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
            settings: Some(ServerSettings {
                level: cli.level,
                mode: cli.mode,
                size: cli.size,
                bots: cli.bots,
                bot_skill: cli.bot_skill.clamp(0.0, 1.0),
                max_clients: cli.max_players,
                port: cli.port,
                network: true,
                public: cli.public,
                local_player: None,
                ..default()
            }),
        },
    ));
    app.run()
}
