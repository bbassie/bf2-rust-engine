//! Game client.
//!
//! ```text
//! client                          # singleplayer on the test range
//! client --level strike_at_karkand --bots 15
//! client --host --bots 8          # listen server others can join
//! client --connect 127.0.0.1      # join a (dedicated) server
//! ```

use std::{net::IpAddr, path::PathBuf};

use bevy::{
    asset::io::AssetSourceBuilder, diagnostic::FrameTimeDiagnosticsPlugin, prelude::*,
    window::PresentMode,
};
use bevy_replicon_renet::RepliconRenetPlugins;
use clap::Parser;
use game_server::{GameServerPlugin, ServerSettings};
use game_shared::{SharedPlugin, config::GamePaths};

mod camera;
mod combat;
mod conquest_hud;
mod deploy;
mod hud;
mod local_input;
mod minimap;
mod net;
mod prediction;
mod render;
mod scenario;
mod vehicles;

#[derive(Parser, Debug, Clone, Resource)]
#[command(version, about = "Game client")]
pub struct Cli {
    /// Connect to a server instead of playing locally.
    #[arg(long, conflicts_with = "host")]
    connect: Option<IpAddr>,
    /// Host a listen server that others can join.
    #[arg(long)]
    host: bool,
    #[arg(long, default_value_t = game_shared::DEFAULT_PORT)]
    port: u16,
    /// Your player name.
    #[arg(long, default_value = "Player")]
    name: String,
    /// Level to play when hosting or in singleplayer.
    #[arg(long, default_value = game_shared::level::TEST_RANGE)]
    level: String,
    #[arg(long, default_value = "gpm_cq")]
    mode: String,
    #[arg(long, default_value_t = 16)]
    size: u32,
    /// Number of bots when hosting or in singleplayer.
    #[arg(long, default_value_t = 7)]
    bots: u32,
    /// Folder with converted assets (default: ./imported or $GAME_IMPORTED_DIR).
    #[arg(long)]
    imported: Option<PathBuf>,
    /// Save a screenshot to this path once everything is loaded (plus
    /// `--screenshot-delay` seconds), then exit.
    #[arg(long)]
    screenshot: Option<PathBuf>,
    #[arg(long, default_value_t = 0.0)]
    screenshot_delay: f32,
    /// Run a scripted scenario (see `scenarios/`): camera moves, input, screenshots,
    /// frame time measurements. Overrides level/bots/spectate from the file.
    #[arg(long)]
    scenario: Option<PathBuf>,
    /// Where scenario screenshots and the report go (default `target/scenarios/<name>`).
    #[arg(long)]
    out: Option<PathBuf>,
    /// Your team (1 or 2) when hosting or in singleplayer.
    #[arg(long, default_value_t = 1)]
    team: u8,
    /// Watch without a soldier (free camera).
    #[arg(long)]
    spectate: bool,
    /// Spectator camera start: `x,y,z,yaw,pitch` (meters, degrees). For screenshots.
    #[arg(long, value_delimiter = ',', allow_hyphen_values = true)]
    camera: Option<Vec<f32>>,
    /// Start in third-person view (toggle with V).
    #[arg(long)]
    third_person: bool,
    /// Debug: third-person camera offset `x,y,z` relative to the view (default 0.6,0.3,3.2).
    #[arg(long, hide = true, value_delimiter = ',', allow_hyphen_values = true)]
    tp_offset: Option<Vec<f32>>,
    /// Log per-pass render timings every few seconds.
    #[arg(long)]
    diagnostics: bool,
    /// Disable sun shadows (for performance comparisons).
    #[arg(long)]
    no_shadows: bool,
    /// Disable screen-space ambient occlusion (for performance comparisons).
    #[arg(long)]
    no_ssao: bool,
    /// Debug: walk in circles and jump without any input, to exercise prediction.
    #[arg(long, hide = true)]
    debug_walk: bool,
}

fn main() -> AppExit {
    let mut cli = Cli::parse();
    let scenario = match (&cli.scenario, &cli.screenshot) {
        (Some(path), _) => match scenario::Scenario::load(path) {
            Ok(scenario) => {
                let name = path.file_stem().unwrap_or_default().to_string_lossy().into_owned();
                let out = cli.out.clone().unwrap_or_else(|| PathBuf::from("target/scenarios").join(name));
                Some((scenario, out))
            }
            Err(err) => {
                eprintln!("scenario {}: {err:#}", path.display());
                return AppExit::error();
            }
        },
        (None, Some(path)) => Some((
            scenario::Scenario::screenshot(path, cli.screenshot_delay),
            cli.out.clone().unwrap_or_default(),
        )),
        _ => None,
    };
    if let Some((scenario, _)) = &scenario {
        scenario.apply(&mut cli);
    }
    let paths = GamePaths::resolve(cli.imported.clone());

    let mut app = App::new();
    // Converted BF2 assets live outside the game folder and are addressed as
    // `imported://levels/...`. Must be registered before the asset plugin.
    app.register_asset_source(
        "imported",
        AssetSourceBuilder::platform_default(&paths.imported.to_string_lossy(), None),
    );
    app.add_plugins(
        DefaultPlugins
            .set(WindowPlugin {
                primary_window: Some(Window {
                    title: "bf2-rust-engine".into(),
                    present_mode: PresentMode::AutoNoVsync,
                    ..default()
                }),
                ..default()
            })
            .set(ImagePlugin {
                default_sampler: render::materials::default_sampler(),
            }),
    )
    .add_plugins((
        FrameTimeDiagnosticsPlugin::default(),
        SharedPlugin,
        RepliconRenetPlugins,
        net::NetPlugin,
        local_input::LocalInputPlugin,
        prediction::PredictionPlugin,
        camera::CameraPlugin,
        render::RenderPlugin,
        hud::HudPlugin,
        combat::ClientCombatPlugin,
        conquest_hud::ConquestHudPlugin,
        deploy::DeployPlugin,
        minimap::MinimapPlugin,
        vehicles::ClientVehiclesPlugin,
    ))
    .insert_resource(paths);

    if cli.diagnostics {
        app.add_plugins((
            bevy::render::diagnostic::RenderDiagnosticsPlugin,
            bevy::diagnostic::EntityCountDiagnosticsPlugin::default(),
            bevy::diagnostic::LogDiagnosticsPlugin {
                wait_duration: std::time::Duration::from_secs(5),
                ..default()
            },
        ));
    }
    if cli.connect.is_none() {
        app.add_plugins(GameServerPlugin {
            settings: ServerSettings {
                level: cli.level.clone(),
                mode: cli.mode.clone(),
                size: cli.size,
                bots: cli.bots,
                port: cli.port,
                network: cli.host,
                local_player: (!cli.spectate).then(|| cli.name.clone()),
                local_team: cli.team,
                ..default()
            },
        });
    }
    if let Some((scenario, out)) = scenario {
        app.add_plugins(scenario::ScenarioPlugin { scenario, out });
    }
    app.insert_resource(camera::ThirdPerson(cli.third_person));
    app.insert_resource(cli);
    app.run()
}
