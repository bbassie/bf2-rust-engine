//! Game client.
//!
//! ```text
//! client                          # main menu
//! client --level test_range       # singleplayer on the test range, no menu
//! client --level strike_at_karkand --bots 15
//! client --host --bots 8          # listen server others can join
//! client --connect 127.0.0.1      # join a (dedicated) server
//! ```
//!
//! `--level`, `--connect`, `--host`, `--spectate`, `--scenario` and `--screenshot` start a
//! match right away; without them the client opens the main menu.

use std::{
    net::{IpAddr, SocketAddr},
    path::PathBuf,
};

use bevy::{diagnostic::FrameTimeDiagnosticsPlugin, prelude::*};
use bevy_replicon_renet::RepliconRenetPlugins;
use clap::Parser;
use game_server::{GameServerPlugin, ServerSettings};
use game_shared::{SharedPlugin, config::GamePaths};
use menu::Screen;
use net::MatchSetup;
use settings::{Settings, SettingsFile};

/// Like Bevy's `embedded_asset!` (for shaders), but a dev build uses the shader's source file
/// when it exists, so editing a shader only needs a restart of the game, not a rebuild.
macro_rules! embedded_shader {
    ($app: expr, $path: expr) => {{
        bevy::asset::embedded_asset!($app, $path);
        #[cfg(debug_assertions)]
        {
            // `file!()` is relative to the workspace root (or absolute).
            let source = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
                .join("../..")
                .join(file!())
                .parent()
                .unwrap()
                .join($path);
            if let Ok(bytes) = std::fs::read(&source) {
                let embedded = $app
                    .world_mut()
                    .resource_mut::<bevy::asset::io::embedded::EmbeddedAssetRegistry>();
                let path = bevy::asset::embedded_path!("src", $path);
                let watched = bevy::asset::io::embedded::watched_path(file!(), $path);
                embedded.insert_asset(watched, &path, bytes);
            }
        }
    }};
}

mod account;
mod announcer;
mod audio;
mod bigmap;
mod camera;
mod chat;
mod combat;
mod commander;
mod conquest_hud;
mod content;
mod deploy;
mod effects;
mod gadgets;
mod hud;
mod join;
mod local_input;
mod map_icons;
mod map_markers;
mod menu;
mod minimap;
mod mod_assets;
mod mode_hud;
mod nav_debug;
mod net;
mod prediction;
mod radio;
mod render;
mod scenario;
mod settings;
mod summary;
mod vehicle_prediction;
mod vehicle_hud;
mod vehicles;
mod wounded;

#[derive(Parser, Debug, Clone, Resource)]
#[command(version, about = "Game client")]
pub struct Cli {
    /// Connect to a server instead of playing locally.
    #[arg(long, conflicts_with = "host")]
    connect: Option<IpAddr>,
    /// Host a listen server that others can join.
    #[arg(long)]
    host: bool,
    /// With --host: accept players from other machines (listen on all interfaces).
    #[arg(long)]
    public: bool,
    #[arg(long, default_value_t = game_shared::DEFAULT_PORT)]
    port: u16,
    /// Your player name (default: the one in the settings).
    #[arg(long)]
    name: Option<String>,
    /// Level to play when hosting or in singleplayer (default `test_range`).
    #[arg(long)]
    level: Option<String>,
    /// Game mode when hosting or in singleplayer: gpm_cq (conquest), gpm_coop, gpm_rush,
    /// gpm_breakthrough, gpm_tdm, or short: conquest, coop, rush, bt, tdm.
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
    /// Folder with mods (default: ./mods or $GAME_MODS_DIR); see docs/MODDING.md.
    #[arg(long)]
    mods: Option<PathBuf>,
    /// Downloading a server's content when joining: ask, always or never (default: the
    /// setting; scenarios and screenshots: always).
    #[arg(long)]
    content: Option<content::ContentDownloads>,
    /// Content cache folder (default: `cache` next to the settings file, or
    /// $GAME_CONTENT_CACHE).
    #[arg(long)]
    content_cache: Option<PathBuf>,
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
    /// Debug: hold input packets back this many milliseconds before sending them, like a
    /// slow connection (for measuring prediction).
    #[arg(long, hide = true, default_value_t = 0)]
    input_delay: u32,
    /// Debug: don't predict the vehicle we drive (to compare).
    #[arg(long, hide = true)]
    no_vehicle_prediction: bool,
    /// Debug: draw the bots' navigation grid near the camera and their paths.
    #[arg(long, hide = true)]
    debug_nav: bool,
    /// Settings file to use (default: `settings.local.ron` if it exists, else the platform
    /// config directory). Scenarios and screenshots use the defaults unless this is given.
    #[arg(long)]
    settings: Option<PathBuf>,
}

impl Cli {
    /// The match the command line asks for, or `None` to open the main menu.
    fn match_setup(&self, settings: &Settings, menu_scenario: bool) -> Option<MatchSetup> {
        let scripted = (self.scenario.is_some() && !menu_scenario) || self.screenshot.is_some();
        let direct = self.level.is_some() || self.connect.is_some() || self.host || self.spectate || scripted;
        if !direct {
            return None;
        }
        let name = self.name.clone().unwrap_or_else(|| settings.player_name.clone());
        Some(match self.connect {
            Some(ip) => MatchSetup::Join {
                server: SocketAddr::new(ip, self.port),
                name,
                spectate: self.spectate,
            },
            None => MatchSetup::Local(ServerSettings {
                level: self.level.clone().unwrap_or_else(|| game_shared::level::TEST_RANGE.into()),
                mode: self.mode.clone(),
                size: self.size,
                bots: self.bots,
                port: self.port,
                network: self.host,
                public: self.public,
                local_player: (!self.spectate).then_some(name),
                local_team: self.team,
                coop: game_server::coop::CoopSettings {
                    human_team: self.team,
                    ..default()
                },
                ..default()
            }),
        })
    }
}

fn main() -> AppExit {
    let mut cli = Cli::parse();
    // Loading a level with many vehicles (Gulf of Oman) overflows the asset loaders' threads
    // with their default 2 MiB stacks; Bevy's task pool plugin keeps a pool made before it.
    bevy::tasks::IoTaskPool::get_or_init(|| {
        let cores = std::thread::available_parallelism().map_or(4, |n| n.get());
        bevy::tasks::TaskPoolBuilder::new()
            .num_threads((cores / 4).clamp(1, 4))
            .thread_name("IO Task Pool".into())
            .stack_size(8 << 20)
            .build()
    });
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
    let paths = GamePaths::resolve_with_mods(cli.imported.clone(), cli.mods.clone());
    let settings_file = SettingsFile::locate(cli.settings.clone(), scenario.is_some());
    let settings = settings_file.load();
    let menu_scenario = scenario.as_ref().is_some_and(|(s, _)| s.menu);
    let start = cli.match_setup(&settings, menu_scenario);

    if scenario.is_some() {
        scenario::enable_log_capture();
    }
    let mut app = App::new();
    // Converted BF2 assets live outside the game folder and are addressed as
    // `imported://levels/...`, with mods on top. Must be registered before the asset plugin.
    app.register_asset_source("imported", mod_assets::imported_source(&paths));
    app.add_plugins(
        DefaultPlugins
            .set(WindowPlugin {
                primary_window: Some(settings.window()),
                ..default()
            })
            .set(ImagePlugin {
                default_sampler: render::materials::default_sampler(),
            })
            // Captures log lines for scenario assertions (`ExpectLog`, `ForbidLog`).
            .set(bevy::log::LogPlugin {
                custom_layer: scenario::log_capture_layer,
                ..default()
            })
            // Static meshes' lightmap UVs (glTF `_LIGHTMAP_UV`; the glTF crate drops the
            // underscore before Bevy looks the name up).
            .set(
                bevy::gltf::GltfPlugin::default()
                    .add_custom_vertex_attribute("LIGHTMAP_UV", render::materials::ATTRIBUTE_LIGHTMAP_UV)
                    .add_custom_vertex_attribute("_LIGHTMAP_UV", render::materials::ATTRIBUTE_LIGHTMAP_UV),
            ),
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
        nav_debug::NavDebugPlugin,
        vehicles::ClientVehiclesPlugin,
    ))
    .add_plugins((
        announcer::AnnouncerPlugin,
        bigmap::BigMapPlugin,
        audio::AudioPlugin,
        chat::ChatPlugin,
        summary::SummaryPlugin,
        wounded::WoundedPlugin,
        radio::RadioPlugin,
        map_markers::MapMarkersPlugin,
        map_icons::MapIconsPlugin,
        commander::ClientCommanderPlugin,
        vehicle_prediction::VehiclePredictionPlugin,
        mode_hud::ModeHudPlugin,
    ))
    .add_plugins((
        // Idle until a match starts (see `net::start_match`).
        GameServerPlugin { settings: None },
        settings::SettingsPlugin,
        content::ContentPlugin,
        // Optional accounts and the join handshake (see `account`, `join`).
        (account::AccountPlugin, join::JoinPlugin),
        effects::EffectsPlugin,
        gadgets::GadgetsPlugin,
        menu::MenuPlugin {
            start: if start.is_some() { Screen::Loading } else { Screen::Menu },
        },
    ))
    .insert_resource(paths)
    .insert_resource(settings)
    .insert_resource(settings_file);

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
    if let Some(setup) = start {
        app.add_systems(Startup, move |world: &mut World| net::start_match(world, setup.clone()));
    }
    if let Some((scenario, out)) = scenario {
        app.add_plugins(scenario::ScenarioPlugin { scenario, out });
    }
    app.insert_resource(camera::ThirdPerson(cli.third_person));
    app.insert_resource(cli);
    app.run()
}
