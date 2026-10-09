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
use game_server::ServerSettings;
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
mod client_physics;
mod combat;
mod commander;
mod conquest_hud;
mod content;
mod deploy;
mod effects;
mod gadgets;
mod hitreg;
mod hud;
mod join;
mod loadout;
mod local_input;
mod local_server;
// --- Map style (tactical minimap and maps) ---
mod map_background;
mod map_icons;
mod map_markers;
mod map_shapes;
mod objective_bar;
mod out_of_bounds;
// --- end map style ---
mod menu;
mod minimap;
mod mod_assets;
mod mode_hud;
mod nametags;
mod nav_debug;
mod net;
mod prediction;
mod quick_actions;
mod radio;
mod render;
mod scenario;
mod settings;
mod summary;
// --- Tactical map (generated minimap texture, see tactical_map.rs) ---
mod tactical_map;
mod ui_theme;
mod vehicle_prediction;
mod vehicle_hud;
mod vehicles;
// --- Voice chat ---
mod voice;
// --- end voice chat ---
mod weapon_list;
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
    /// Folder for generated data kept between runs: bots' navigation grids, tactical maps
    /// (default: the platform's cache folder, or $BF2_CACHE_DIR; see docs/ARCHITECTURE.md,
    /// "Caches").
    #[arg(long)]
    cache_dir: Option<PathBuf>,
    /// Size limit of `--cache-dir` in GB (default 2, or $BF2_CACHE_LIMIT_GB).
    #[arg(long)]
    cache_limit_gb: Option<f64>,
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
    /// Seconds between death and respawn, for a scripted run's own local server (default:
    /// `FAST_DEPLOY_SECONDS`, overridable per scenario with its `respawn_time` field). Doesn't
    /// affect `--host`/singleplayer outside a scenario (the server's own default, 10 s).
    #[arg(long, hide = true)]
    respawn_time: Option<f32>,
    /// Disables the out-of-bounds countdown for a scripted run's own local server
    /// (overridable per scenario with its `out_of_bounds` field): for a scenario whose test
    /// positions sit outside a level's combat area for reasons unrelated to what it checks
    /// (e.g. a firing range south of Karkand's playable area).
    #[arg(long, hide = true)]
    out_of_bounds: Option<bool>,
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

/// Respawn time a scripted run's own local server uses unless the scenario opts out with its
/// `respawn_time` field (or `--respawn-time`): fast enough that a `WaitSpawned` step returns in
/// about a frame instead of the real game's 10 s. A scenario that tests the deploy countdown
/// itself (its text, or a screenshot mid-countdown) sets `respawn_time: Some(10.0)` to keep it.
const FAST_DEPLOY_SECONDS: f32 = 0.5;

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
                respawn_seconds: self
                    .respawn_time
                    .unwrap_or(if scripted { FAST_DEPLOY_SECONDS } else { ServerSettings::default().respawn_seconds }),
                out_of_bounds: self.out_of_bounds.unwrap_or(ServerSettings::default().out_of_bounds),
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
                // Scripted runs (agents' scenarios and screenshots) open in the background
                // instead of taking focus from whoever is using the machine.
                primary_window: Some(Window {
                    focused: !(scenario.is_some() || cli.screenshot.is_some()),
                    ..settings.window()
                }),
                ..default()
            })
            .set(ImagePlugin {
                default_sampler: render::materials::default_sampler(),
            })
            // `BF2_PERF_EXP=cpubatch`: Bevy's CPU-built instance buffers instead of its GPU mesh
            // preprocessing. Recording the draws gets about 1.5 ms cheaper, but preparing the
            // buffers costs 9 ms (Karkand, October 2026), so not by default.
            .set(bevy::pbr::PbrPlugin {
                use_gpu_instance_buffer_builder: !perf_experiment("cpubatch"),
                ..default()
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
        out_of_bounds::OutOfBoundsPlugin,
        nametags::NameTagsPlugin,
        radio::RadioPlugin,
        map_markers::MapMarkersPlugin,
        map_icons::MapIconsPlugin,
        commander::ClientCommanderPlugin,
        vehicle_prediction::VehiclePredictionPlugin,
        mode_hud::ModeHudPlugin,
        tactical_map::TacticalMapPlugin, // --- Tactical map ---
    ))
    // --- Map style ---
    .add_plugins((map_shapes::MapShapesPlugin, map_background::MapBackgroundPlugin, objective_bar::ObjectiveBarPlugin))
    // --- end map style ---
    // Weapons: the weapon list, the melee and grenade keys, loadouts on the deploy screen.
    .add_plugins((weapon_list::WeaponListPlugin, quick_actions::QuickActionsPlugin, loadout::LoadoutPlugin))
    .add_plugins((
        // Singleplayer and hosting run the server on a thread of its own (`local_server`).
        local_server::LocalServerPlugin,
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
    .insert_resource(settings)
    .insert_resource(settings_file);
    let cache = game_shared::cache::Cache::resolve(cli.cache_dir.clone(), cli.cache_limit_gb);
    game_shared::cache::housekeeping(cache.clone(), &paths);
    if let Some(cache) = cache {
        app.insert_resource(cache);
    }
    app.insert_resource(paths);

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
    // --- Voice chat ---
    app.add_plugins(voice::VoicePlugin);
    // --- end voice chat ---
    if let Some(setup) = start {
        app.add_systems(Startup, move |world: &mut World| net::start_match(world, setup.clone()));
    }
    if let Some((scenario, out)) = scenario {
        app.add_plugins(scenario::ScenarioPlugin { scenario, out });
    }
    app.insert_resource(camera::ThirdPerson(cli.third_person));
    app.insert_resource(cli);
    single_threaded_schedules(&mut app);
    // --- Frame time: the client's physics does only what its queries need ---
    app.add_plugins(client_physics::ClientPhysicsPlugin);
    app.run()
}

/// Whether `BF2_PERF_EXP` names this frame time experiment (comma separated).
pub(crate) fn perf_experiment(name: &str) -> bool {
    std::env::var("BF2_PERF_EXP").is_ok_and(|e| e.split(',').any(|x| x.trim() == name))
}

/// Runs the main world's schedules (the frame's and the fixed tick's: the server's game rules
/// and bots when hosting, prediction) on one thread, as the dedicated server does: nearly all
/// of their systems are tiny, and handing each to a worker thread costs more than it runs, all
/// the more while the render thread keeps the workers busy. Systems with real work still split
/// it themselves (`par_iter`). Karkand, 64 players, 63 bots, release: 9.2 -> 8.3 ms a frame,
/// p95 11.3 -> 10.0 ms. `BF2_SCHEDULES=parallel` keeps Bevy's multi-threaded executor,
/// `=fixed` uses one thread for the fixed tick only.
fn single_threaded_schedules(app: &mut App) {
    use bevy::{
        app::{FixedFirst, FixedLast, FixedMain, FixedPostUpdate, FixedPreUpdate, RunFixedMainLoop},
        ecs::schedule::{InternedScheduleLabel, ScheduleLabel, SingleThreadedExecutor},
    };
    let mode = std::env::var("BF2_SCHEDULES").unwrap_or_default();
    if mode == "parallel" {
        return;
    }
    let mut labels: Vec<InternedScheduleLabel> = vec![
        RunFixedMainLoop.intern(),
        FixedMain.intern(),
        FixedFirst.intern(),
        FixedPreUpdate.intern(),
        FixedUpdate.intern(),
        FixedPostUpdate.intern(),
        FixedLast.intern(),
    ];
    if mode != "fixed" {
        labels.extend([First.intern(), PreUpdate.intern(), Update.intern(), PostUpdate.intern(), Last.intern()]);
    }
    let mut schedules = app.world_mut().resource_mut::<bevy::ecs::schedule::Schedules>();
    for label in labels {
        if let Some(schedule) = schedules.get_mut(label) {
            schedule.set_executor(SingleThreadedExecutor::new());
        }
    }
}
