//! Headless dedicated server.
//!
//! ```text
//! server                                        # the test range
//! server --level strike_at_karkand --bots 16
//! server --config server.ron                    # name, map rotation, admin, ... (see server_config)
//! server rcon --password secret info players    # a running server's remote console
//! server --content all                          # joining clients download everything
//! server export-content --out www/content       # files for a --download-url host
//! ```

use std::{path::PathBuf, time::Duration};

use bevy::{
    app::{ScheduleRunnerPlugin, TerminalCtrlCHandlerPlugin},
    ecs::schedule::{Schedules, SingleThreadedExecutor},
    log::LogPlugin,
    prelude::*,
    state::app::StatesPlugin,
};
use bevy_replicon_renet::RepliconRenetPlugins;
use clap::{Parser, Subcommand};
use game_server::{GameServerPlugin, admin::rcon, server_config::ServerConfig};
use game_shared::{SharedPlugin, TICK_HZ, config::GamePaths, content::ContentMode};

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
    /// Game mode: gpm_cq (conquest), gpm_coop, gpm_rush, gpm_breakthrough, gpm_tdm, or short:
    /// conquest, coop, rush, bt, tdm.
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
    /// Bot skill, 0..1: aim and reaction time (default: the difficulty's).
    #[arg(long)]
    bot_skill: Option<f32>,
    /// Bot difficulty: easy, normal, hard or expert (reaction, aim, tactics, awareness).
    #[arg(long)]
    bot_difficulty: Option<game_server::ai::skill::BotDifficulty>,
    /// Testing: bots of team 1, 2 (or 3: both) play with the tactics from before cover,
    /// suppression, memory and squad coordination, to compare the two.
    #[arg(long, default_value_t = 0, hide = true)]
    bot_legacy_team: u8,
    /// Testing: bots of one team at another difficulty, `2:easy`.
    #[arg(long, hide = true)]
    bot_team_difficulty: Option<String>,
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
    /// BF2's kits exactly as they are: no picking weapons from each class's pool.
    #[arg(long)]
    classic_kits: bool,
    /// Players may only pick weapons that kits of their own team carry.
    #[arg(long)]
    faction_locked_weapons: bool,
    /// Password for the remote console and `/login` in the chat. Only takes effect on
    /// servers that don't require accounts (LAN, offline, unranked): on a ranked server
    /// `/login` is refused and admin rights come from `--admin` instead.
    #[arg(long)]
    admin_password: Option<String>,
    /// TCP port of the remote console (0: off).
    #[arg(long)]
    rcon_port: Option<u16>,
    /// An account that gets admin rights on join (repeatable): `id:<account id>` matches by
    /// id, anything else matches the account name case-insensitively.
    #[arg(long = "admin")]
    admins: Vec<String>,
    /// Announce the server to this master server (`host[:port]`).
    #[arg(long)]
    master: Option<String>,
    /// What joining clients may download from this server: `off`, `mods` (default: content
    /// made for this engine) or `all` (also the imported BF2 assets: EA's copyrighted
    /// content, only if you may share it). See docs/MODDING.md.
    #[arg(long)]
    content: Option<ContentMode>,
    /// Clients download content from here first (`<url>/<hash>`, see `export-content`).
    #[arg(long)]
    download_url: Option<String>,
    /// TCP port of the content endpoint (default: the game port).
    #[arg(long)]
    content_port: Option<u16>,
    /// The server's identity key file (default: `identity.key` in the server's data folder).
    #[arg(long)]
    identity: Option<PathBuf>,
    /// Master server web address for optional accounts (`https://...`, see
    /// crates/master_server): players' account tickets are checked.
    #[arg(long)]
    master_url: Option<String>,
    /// Ranked: require accounts and report stats to the master (needs --master-url and
    /// --api-key).
    #[arg(long)]
    ranked: bool,
    /// The API key the master server's admin gave this server.
    #[arg(long)]
    api_key: Option<String>,
    /// Region for the master's server list and quick join (`eu`, `us-east`, ...).
    #[arg(long)]
    region: Option<String>,
    /// Folder with converted assets (default: ./imported or $GAME_IMPORTED_DIR).
    #[arg(long)]
    imported: Option<PathBuf>,
    /// Folder with mods (default: ./mods or $GAME_MODS_DIR); see docs/MODDING.md.
    #[arg(long)]
    mods: Option<PathBuf>,
    /// Soak test: log the server's health (entities, memory, frame times, round) every
    /// `--soak-every` seconds and quit after this many minutes (0: keep running).
    #[arg(long)]
    soak: Option<f32>,
    /// Seconds between soak reports.
    #[arg(long, default_value_t = 30.0)]
    soak_every: f32,
    /// Soak test: play the next map of the rotation after this many minutes on one (without
    /// a rotation: restart the map).
    #[arg(long)]
    soak_rotate: Option<f32>,
    /// Log where the server's time goes (per system, schedule and command flush) with each
    /// soak report. Needs a build with `--features profile` for the per-system part.
    #[arg(long)]
    profile_ticks: bool,
    /// Run each schedule's systems on several threads (Bevy's default) rather than in turn.
    #[arg(long)]
    parallel_schedules: bool,
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
    /// Writes the files the server shares to a folder, named by hash, with `manifest.ron`:
    /// upload it to a static web host or CDN and give its URL as `--download-url`.
    ExportContent {
        /// Output folder.
        #[arg(long)]
        out: PathBuf,
        /// `mods` (default) or `all`.
        #[arg(long, default_value = "mods")]
        content: ContentMode,
        #[arg(long)]
        imported: Option<PathBuf>,
        #[arg(long)]
        mods: Option<PathBuf>,
    },
}

fn main() -> AppExit {
    let cli = Cli::parse();
    if let Some(Command::ExportContent { out, content, imported, mods }) = &cli.command {
        let paths = GamePaths::resolve_with_mods(imported.clone(), mods.clone());
        return match game_server::content::export(&paths, *content, out) {
            Ok((files, bytes)) => {
                println!("{files} files ({}) written to {}", game_shared::content::format_bytes(bytes), out.display());
                AppExit::Success
            }
            Err(err) => {
                eprintln!("export-content: {err:#}");
                AppExit::error()
            }
        };
    }
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
    if let Some(difficulty) = cli.bot_difficulty {
        settings.bot_difficulty = difficulty;
        settings.bot_skill = difficulty.params().skill;
    }
    if let Some(skill) = cli.bot_skill {
        settings.bot_skill = skill.clamp(0.0, 1.0);
    }
    settings.bot_legacy_team = cli.bot_legacy_team;
    if let Some(spec) = &cli.bot_team_difficulty {
        let parsed = spec.split_once(':').and_then(|(team, d)| {
            let team: u8 = team.trim().parse().ok().filter(|t| (1..=2).contains(t))?;
            Some((team, game_server::ai::skill::BotDifficulty::parse(d)?))
        });
        match parsed {
            Some(team_difficulty) => settings.bot_team_difficulty = Some(team_difficulty),
            None => {
                eprintln!("--bot-team-difficulty: expected <team 1|2>:<easy|normal|hard|expert>, got `{spec}`");
                return AppExit::error();
            }
        }
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
    if !cli.admins.is_empty() {
        settings.admin.admins = cli.admins;
    }
    if let Some(master) = cli.master {
        settings.master_server = Some(master);
    }
    if let Some(mode) = cli.content {
        settings.content.mode = mode;
    }
    if let Some(url) = cli.download_url {
        settings.content.download_url = Some(url);
    }
    if let Some(port) = cli.content_port {
        settings.content.port = Some(port);
    }
    if let Some(file) = cli.identity {
        settings.content.identity_file = Some(file);
    }
    if let Some(url) = cli.master_url {
        settings.accounts.master_url = Some(url);
    }
    if let Some(key) = cli.api_key {
        settings.accounts.api_key = Some(key);
    }
    if let Some(region) = cli.region {
        settings.accounts.region = region;
    }
    settings.accounts.ranked |= cli.ranked;
    settings.public |= cli.public;
    settings.friendly_fire |= cli.friendly_fire;
    settings.loadouts.arsenal &= !cli.classic_kits;
    settings.loadouts.faction_locked |= cli.faction_locked_weapons;

    let mut app = App::new();
    // Explicit plugin list rather than DefaultPlugins, so the server stays headless even
    // when a workspace build unifies rendering features into Bevy.
    app.add_plugins((
        MinimalPlugins.set(ScheduleRunnerPlugin::run_loop(Duration::from_secs_f64(
            1.0 / (TICK_HZ * 2.0),
        ))),
        LogPlugin {
            custom_layer: if cli.profile_ticks { game_server::profile::layer } else { |_| None },
            ..default()
        },
        TerminalCtrlCHandlerPlugin,
        StatesPlugin,
        TransformPlugin,
        AssetPlugin::default(),
    ))
    // Physics may expect mesh assets depending on unified features; they are CPU-only here.
    .init_asset::<Mesh>()
    .insert_resource(GamePaths::resolve_with_mods(cli.imported, cli.mods))
    .add_plugins((
        SharedPlugin,
        RepliconRenetPlugins,
        GameServerPlugin {
            settings: Some(settings),
        },
    ));
    // Profiling reports with the soak's reports; without `--soak` they go on forever.
    let soak = cli.soak.or(cli.profile_ticks.then_some(0.0));
    if let Some(minutes) = soak {
        app.add_plugins(game_server::soak::SoakPlugin {
            duration: (minutes > 0.0).then(|| Duration::from_secs_f32(minutes * 60.0)),
            every: cli.soak_every,
            rotate_every: cli.soak_rotate.filter(|m| *m > 0.0).map(|m| Duration::from_secs_f32(m * 60.0)),
        });
    }
    // Single-threaded schedules: nearly every server system is tiny, and handing each one to
    // a thread cost more than running them in turn (Karkand, 32 bots: ticks 2.65 -> 1.65 ms,
    // frames 2.7 -> 1.3 ms). Systems with real work split it themselves (`par_iter`), and
    // avian runs its own schedules single-threaded for the same reason.
    if !cli.parallel_schedules {
        let mut schedules = app.world_mut().resource_mut::<Schedules>();
        for (_, schedule) in schedules.iter_mut() {
            schedule.set_executor(SingleThreadedExecutor::new());
        }
    }
    app.run()
}
