//! `bf2-import`: converts content from a Battlefield 2 installation you own into the game's
//! own formats. Nothing is uploaded or redistributed; output stays on your machine.
//!
//! ```text
//! bf2-import --bf2 "C:\Program Files (x86)\EA Games\Battlefield 2" list
//! bf2-import --bf2 ... level strike_at_karkand
//! bf2-import --bf2 ... level --all
//! bf2-import --bf2 ... light --all     # only the world lighting and lamps of imported levels
//! bf2-import --bf2 ... kits --bf2-mod bf2   # every kit, its unlocks and their weapons
//! bf2-import --bf2 ... check          # parse every mesh and collision mesh, report failures
//! bf2-import --bf2 ... --progress-json level --base-game --resume   # what the game's first-run screen runs
//! ```
//!
//! Without `--out` the output goes where the game looks: `imported/` in the data folder
//! (`game_data::paths`: the working directory of a checkout, else the per-user data folder, or
//! `--data-dir`).
//!
//! `--progress-json` prints one JSON object per line on standard output for a front end (the
//! game's first-run screen runs this program rather than reading BF2 files itself); logs stay
//! on standard error:
//!
//! ```text
//! {"event":"plan","total":15,"skipped":2,"levels":[{"name":"Strike_at_Karkand","mod":"bf2"},...]}
//! {"event":"stage","stage":"soldiers"}
//! {"event":"level","index":0,"total":15,"name":"Strike_at_Karkand","mod":"bf2"}
//! {"event":"level_done","index":0,"name":"Strike_at_Karkand","ok":true,"seconds":21.3,"warnings":0,"error":null}
//! {"event":"done","ok":13,"failed":0,"skipped":2,"seconds":300.1}
//! {"event":"error","message":"..."}       (the run failed as a whole; exit code 1)
//! ```
//!
//! `level --resume` skips levels a previous run into the same `--out` finished (listed in
//! `<out>/import-state.json`), so a cancelled import continues where it stopped.

use std::{
    io::Write,
    path::{Path, PathBuf},
    sync::atomic::{AtomicBool, Ordering},
    time::Instant,
};

use anyhow::{Context, Result, bail};
use bf2_formats::{
    Bf2Install, Side, Vfs,
    localization::Localization,
    collision::CollisionMesh,
    mesh::{MeshKind, VisMesh},
};
use clap::{Parser, Subcommand};
use rayon::prelude::*;

mod ai;
mod audio;
mod commander;
mod coords;
mod dds;
mod destruction;
mod effects;
mod glb;
mod hitzones;
mod lamps;
mod level;
mod lighting;
mod lods;
mod meshes;
mod roads;
mod soldiers;
mod sounds;
mod terrain;
mod ui_icons;
mod vegetation;
mod vehicle_hud;
mod vehicles;
mod weapons;

#[derive(Parser)]
#[command(version, about = "Convert content from a Battlefield 2 installation")]
struct Cli {
    /// Battlefield 2 installation folder (or set BF2_DIR).
    #[arg(long, env = "BF2_DIR")]
    bf2: PathBuf,
    /// Output folder for converted assets (default: `imported` in the data folder).
    #[arg(long)]
    out: Option<PathBuf>,
    /// The game's data folder, holding `imported/` and `mods/` (default: the working
    /// directory when it has them, else the per-user data folder, or $BF2_DATA_DIR).
    #[arg(long)]
    data_dir: Option<PathBuf>,
    /// Print progress as JSON lines on standard output (see the module doc).
    #[arg(long, global = true)]
    progress_json: bool,
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// List installed mods and levels.
    List {
        /// As one JSON object: `{"root","mods":[{"name","title","base","levels":[...]}]}`.
        #[arg(long)]
        json: bool,
    },
    /// Import levels (terrain, static objects, game mode layouts and the meshes they use).
    Level {
        /// Level folder names (case-insensitive).
        names: Vec<String>,
        /// Import every level of every installed mod, or (with `--bf2-mod`) of just that one.
        #[arg(long)]
        all: bool,
        /// Import every level of the base game (`bf2` and `xpack`, Special Forces).
        #[arg(long, conflicts_with = "all")]
        base_game: bool,
        /// Skip levels a previous run into the same `--out` finished
        /// (`<out>/import-state.json`).
        #[arg(long)]
        resume: bool,
        /// Only look at this installed BF2 mod's levels (e.g. `AIX2`). Also scopes the
        /// soldier bodies this run imports, so a mod's own soldiers don't get duplicated into
        /// a namespaced `--out` alongside every other installed mod's.
        #[arg(long = "bf2-mod")]
        bf2_mod: Option<String>,
        /// Namespace every imported level so it can't collide with a vanilla one of the same
        /// folder name (BF2 mods sometimes reuse a base level's name, e.g. AIX 2's own
        /// `Dalian_plant`): writes `<namespace>_<level>` instead of `<level>`, and prefixes
        /// the menu display name with `--mod-title`. See `docs/MODDING.md#bf2-mods` for the
        /// full design (point `--out` at a `mods/<namespace>` folder of its own, on top of a
        /// vanilla `imported/`).
        #[arg(long)]
        namespace: Option<String>,
        /// Display name prefix used with `--namespace`, e.g. "AIX 2" for "AIX 2: Archipelago".
        /// Defaults to `--namespace` itself.
        #[arg(long = "mod-title")]
        mod_title: Option<String>,
    },
    /// Import soldier bodies (skinned mesh, skeleton, animations). Also done by `level`.
    Soldiers {
        /// Only this installed BF2 mod's soldiers (default: the base game, `xpack` merged over
        /// `bf2` the way it's mounted in-game; other installed mods, e.g. `AIX2`, are never
        /// imported unless named here).
        #[arg(long = "bf2-mod")]
        bf2_mod: Option<String>,
    },
    /// Import only the bots' hints (strategic areas, weapon templates) of imported levels.
    /// Also done by `level`.
    Ai {
        names: Vec<String>,
        #[arg(long)]
        all: bool,
        #[arg(long = "bf2-mod")]
        bf2_mod: Option<String>,
    },
    /// Update only the world lighting (`sky.con`) in imported levels' `level.ron`. Also
    /// done by `level`.
    Light {
        names: Vec<String>,
        #[arg(long)]
        all: bool,
        #[arg(long = "bf2-mod")]
        bf2_mod: Option<String>,
    },
    /// Import every kit of a BF2 mod with its unlocks and the weapons they carry (models,
    /// sounds, effects, icons), without a level: the weapon pools of loadouts
    /// (`game_shared::arsenal`). `level` imports the kits its level uses the same way.
    Kits {
        #[arg(long = "bf2-mod", default_value = "bf2")]
        bf2_mod: String,
    },
    /// Parse every mesh and collision mesh of a mod and report failures.
    Check {
        #[arg(long, default_value = "bf2")]
        r#mod: String,
    },
}

/// `--progress-json` is on.
static PROGRESS: AtomicBool = AtomicBool::new(false);

/// Prints a progress event (with `--progress-json`). A front end that went away (closed
/// pipe) ends the import.
fn progress(event: serde_json::Value) {
    if !PROGRESS.load(Ordering::Relaxed) {
        return;
    }
    let mut out = std::io::stdout().lock();
    if writeln!(out, "{event}").and_then(|()| out.flush()).is_err() {
        std::process::exit(1);
    }
}

fn main() -> std::process::ExitCode {
    env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("info")).init();
    let cli = Cli::parse();
    PROGRESS.store(cli.progress_json, Ordering::Relaxed);
    match run(cli) {
        Ok(()) => std::process::ExitCode::SUCCESS,
        Err(err) => {
            log::error!("{err:#}");
            progress(serde_json::json!({ "event": "error", "message": format!("{err:#}") }));
            std::process::ExitCode::FAILURE
        }
    }
}

fn run(cli: Cli) -> Result<()> {
    let install = Bf2Install::open(&cli.bf2)?;
    let out_dir = cli
        .out
        .clone()
        .unwrap_or_else(|| game_data::paths::DataDirs::resolve(cli.data_dir.clone(), None, None).imported);
    let started_run = Instant::now();

    match cli.command {
        Command::List { json } => {
            if json {
                let mods: Vec<serde_json::Value> = install
                    .mods()
                    .into_iter()
                    .map(|m| {
                        let levels: Vec<String> = install.levels(&m).into_iter().map(|l| l.name).collect();
                        serde_json::json!({
                            "name": m,
                            "title": mod_title_of(&install, &m),
                            "base": Bf2Install::is_base_mod(&m),
                            "levels": levels,
                        })
                    })
                    .collect();
                println!("{}", serde_json::json!({ "root": install.root(), "mods": mods }));
            } else {
                for mod_name in install.mods() {
                    println!("{mod_name}");
                    for level in install.levels(&mod_name) {
                        println!("  {}", level.name);
                    }
                }
            }
        }
        Command::Level {
            names,
            all,
            base_game,
            resume,
            bf2_mod,
            namespace,
            mod_title,
        } => {
            let mods = bf2_mod.clone().map(|m| vec![m]).unwrap_or_else(|| install.mods());
            let levels: Vec<bf2_formats::LevelInfo> = if all {
                mods.iter().flat_map(|m| install.levels(m)).collect()
            } else if base_game {
                // `bf2` first: a level both ship comes from the base game once.
                let mut base: Vec<String> = install.mods().into_iter().filter(|m| Bf2Install::is_base_mod(m)).collect();
                base.sort_by_key(|m| !m.eq_ignore_ascii_case("bf2"));
                let mut levels: Vec<bf2_formats::LevelInfo> = Vec::new();
                for level in base.iter().flat_map(|m| install.levels(m)) {
                    if !levels.iter().any(|l| l.name.eq_ignore_ascii_case(&level.name)) {
                        levels.push(level);
                    }
                }
                levels
            } else {
                if names.is_empty() {
                    bail!("name at least one level, or pass --all or --base-game");
                }
                names
                    .iter()
                    .map(|n| find_level(&install, n, bf2_mod.as_deref()))
                    .collect::<Result<Vec<_>, _>>()?
            };
            if namespace.is_none() {
                if let Some(level) = levels.iter().find(|l| !Bf2Install::is_base_mod(&l.mod_name)) {
                    bail!(
                        "{} is from the {} mod: import it into its own folder with --out mods/<name> --namespace <name>                          (or pick the base game's level with --bf2-mod bf2 / xpack)",
                        level.name,
                        level.mod_name
                    );
                }
            }
            let out = out_dir.as_path();
            std::fs::create_dir_all(out)?;
            write_readme(out)?;
            let mut state = ImportState::load(out);
            let all_count = levels.len();
            let levels: Vec<_> = if resume {
                levels.into_iter().filter(|l| !state.is_done(l)).collect()
            } else {
                levels
            };
            let skipped = all_count - levels.len();
            progress(serde_json::json!({
                "event": "plan",
                "total": levels.len(),
                "skipped": skipped,
                "levels": levels.iter().map(|l| serde_json::json!({ "name": l.name, "mod": l.mod_name })).collect::<Vec<_>>(),
            }));
            if let Some(namespace) = &namespace {
                write_mod_ron(out, namespace, mod_title.as_deref(), bf2_mod.as_deref())?;
            }
            let (mut ok, mut failed) = (0, 0);
            if !levels.is_empty() {
                progress(serde_json::json!({ "event": "stage", "stage": "soldiers" }));
                let soldier_mods = bf2_mod.clone().map(|m| vec![m]).unwrap_or_else(|| default_soldier_mods(&install));
                import_soldiers(&install, out, &soldier_mods);
            }
            let localization = Localization::load(&install, "english");
            log::info!("{} localized strings", localization.len());
            let mod_title = mod_title.as_deref().or(namespace.as_deref());
            let total = levels.len();
            for (index, level) in levels.into_iter().enumerate() {
                let started = Instant::now();
                log::info!("importing {} ({})", level.name, level.mod_name);
                progress(serde_json::json!({
                    "event": "level", "index": index, "total": total, "name": level.name, "mod": level.mod_name,
                }));
                let result = level::import_level(&install, &level, &localization, out, namespace.as_deref(), mod_title);
                let seconds = started.elapsed().as_secs_f32();
                match &result {
                    Ok(report) => {
                        ok += 1;
                        state.mark_done(out, &level);
                        progress(serde_json::json!({
                            "event": "level_done", "index": index, "name": level.name, "ok": true,
                            "seconds": seconds, "warnings": report.failed_meshes.len(), "error": null,
                        }));
                    }
                    Err(err) => {
                        failed += 1;
                        progress(serde_json::json!({
                            "event": "level_done", "index": index, "name": level.name, "ok": false,
                            "seconds": seconds, "warnings": 0, "error": format!("{err:#}"),
                        }));
                    }
                }
                match result {
                    Ok(report) => {
                        log::info!(
                            "{}: {} statics, {} roads, {} kits, {} weapons, {} vehicles, {} templates, {} mesh files, modes [{}] in {:.1}s",
                            level.name,
                            report.statics,
                            report.roads,
                            report.kits,
                            report.weapons,
                            report.vehicles,
                            report.templates,
                            report.meshes,
                            report.game_modes.join(", "),
                            started.elapsed().as_secs_f32()
                        );
                        for failure in report.failed_meshes.iter().take(20) {
                            log::warn!("  mesh failed: {failure}");
                        }
                        if !report.missing_templates.is_empty() {
                            log::debug!("  missing templates: {}", report.missing_templates.join(", "));
                        }
                    }
                    Err(err) => log::error!("{}: {err:#}", level.name),
                }
            }
            progress(serde_json::json!({
                "event": "done", "ok": ok, "failed": failed, "skipped": skipped,
                "seconds": started_run.elapsed().as_secs_f32(),
            }));
        }
        Command::Soldiers { bf2_mod } => {
            let mods = bf2_mod.map(|m| vec![m]).unwrap_or_else(|| default_soldier_mods(&install));
            import_soldiers(&install, &out_dir, &mods)
        }
        Command::Ai { names, all, bf2_mod } => {
            let mods = bf2_mod.clone().map(|m| vec![m]).unwrap_or_else(|| install.mods());
            let levels = if all {
                mods.iter().flat_map(|m| install.levels(m)).collect()
            } else {
                names
                    .iter()
                    .map(|n| find_level(&install, n, bf2_mod.as_deref()))
                    .collect::<Result<Vec<_>, _>>()?
            };
            for level in levels {
                match ai::import_only(&install, &level, &out_dir) {
                    Ok((areas, weapons)) => log::info!("{}: {areas} strategic areas, {weapons} weapon templates", level.name),
                    Err(err) => log::warn!("{}: {err:#}", level.name),
                }
            }
        }
        Command::Light { names, all, bf2_mod } => {
            let mods = bf2_mod.clone().map(|m| vec![m]).unwrap_or_else(|| install.mods());
            let levels = if all {
                mods.iter().flat_map(|m| install.levels(m)).collect()
            } else {
                if names.is_empty() {
                    bail!("name at least one level, or pass --all");
                }
                names
                    .iter()
                    .map(|n| find_level(&install, n, bf2_mod.as_deref()))
                    .collect::<Result<Vec<_>, _>>()?
            };
            for level in levels {
                match lighting::import_only(&install, &level, &out_dir) {
                    Ok(Some(l)) => log::info!(
                        "{}: static sky {:?} sun {:?}, terrain GI {:?} sun {:?} (lightmap sun x{:.2}){}",
                        level.name,
                        l.static_sky,
                        l.static_sun,
                        l.terrain_gi,
                        l.terrain_sun,
                        l.terrain_sun_scale,
                        if l.dark_adapted.is_some() { ", faked HDR" } else { "" }
                    ),
                    Ok(None) => log::info!("{}: no world lighting in its scripts", level.name),
                    Err(err) => log::warn!("{}: {err:#}", level.name),
                }
            }
        }
        Command::Kits { bf2_mod } => {
            progress(serde_json::json!({ "event": "stage", "stage": format!("kits ({bf2_mod})") }));
            import_kits(&install, &out_dir, &bf2_mod)?;
            progress(serde_json::json!({
                "event": "done", "ok": 0, "failed": 0, "skipped": 0, "seconds": started_run.elapsed().as_secs_f32(),
            }));
        }
        Command::Check { r#mod } => check(&install, &r#mod)?,
    }
    Ok(())
}

/// Finds a level by name in the `--bf2-mod` mod, or in every mod with the base game first.
fn find_level(install: &Bf2Install, name: &str, bf2_mod: Option<&str>) -> Result<bf2_formats::LevelInfo, bf2_formats::InstallError> {
    match bf2_mod {
        Some(m) => install.find_level_in(name, &[m.to_string()]),
        None => install.find_level(name),
    }
}

/// The mod(s) to import soldiers from when `--bf2-mod` wasn't given: the base game only, and
/// just `xpack` when it's installed rather than both `bf2` and `xpack` separately, since
/// mounting `xpack` already pulls in `bf2`'s own archives (`Bf2Install::mount_mod`) the same
/// way the game does. Importing `bf2` and `xpack` as two separate passes (as plain
/// `install.mods()` would, alongside every other installed mod) mounts `bf2` alone first, which
/// is missing Special Forces-only content such as the rope/zipline clips
/// (`soldiers::ROPE_ANIMATIONS`); `soldiers::import_all`'s dedup by body name then keeps that
/// incomplete `bf2` pass and skips the fuller one `xpack` would have produced. Other installed
/// mods (e.g. `AIX2`) are never imported by default, matching `level`'s refusal to import a
/// non-base mod's level without `--namespace`.
fn default_soldier_mods(install: &Bf2Install) -> Vec<String> {
    let mut mods = install.mods();
    mods.retain(|m| Bf2Install::is_base_mod(m));
    if mods.iter().any(|m| m.eq_ignore_ascii_case("xpack")) {
        mods.retain(|m| m.eq_ignore_ascii_case("xpack"));
    }
    mods
}

fn import_soldiers(install: &Bf2Install, out: &std::path::Path, mods: &[String]) {
    let started = Instant::now();
    match soldiers::import_all(install, out, mods) {
        Ok(names) => log::info!(
            "soldiers: {} in {:.1}s",
            names.join(", "),
            started.elapsed().as_secs_f32()
        ),
        Err(err) => log::error!("soldiers: {err:#}"),
    }
}

/// `bf2-import kits`: every `Kit` template under the mod's `objects/kits`.
fn import_kits(install: &Bf2Install, out: &std::path::Path, mod_name: &str) -> Result<()> {
    let started = Instant::now();
    std::fs::create_dir_all(out)?;
    write_readme(out)?;
    let mut vfs = Vfs::new();
    install
        .mount_mod(&mut vfs, mod_name, Side::Both)
        .with_context(|| format!("mounting mod {mod_name}"))?;
    let localization = Localization::load(install, "english");
    let mut interp = bf2_formats::con::Interpreter::new(&vfs);
    let mut files: Vec<String> = vfs
        .list("objects/kits")
        .filter(|p| p.ends_with(".con"))
        .map(str::to_string)
        .collect();
    files.sort();
    for file in &files {
        interp.run(file, &[]);
    }
    let mut kits: Vec<String> = interp
        .world
        .templates
        .iter()
        .filter(|(_, t)| t.ty.eq_ignore_ascii_case("Kit"))
        .map(|(name, _)| name.clone())
        .collect();
    kits.sort();
    let converter = meshes::MeshConverter::new(&vfs, out);
    let (kit_count, weapon_count) = weapons::import(&mut interp, &converter, &localization, &kits, out)?;
    effects::import_weapon_effects(&mut interp, &converter, &kits, out);
    log::info!(
        "{mod_name}: {kit_count} kits, {weapon_count} weapons in {:.1}s",
        started.elapsed().as_secs_f32()
    );
    Ok(())
}

fn check(install: &Bf2Install, mod_name: &str) -> Result<()> {
    let mut vfs = Vfs::new();
    install
        .mount_mod(&mut vfs, mod_name, Side::Both)
        .with_context(|| format!("mounting {mod_name}"))?;
    let mut paths: Vec<&str> = vfs.list("objects").collect();
    paths.sort_unstable();

    let meshes: Vec<&str> = paths
        .iter()
        .copied()
        .filter(|p| MeshKind::from_path(p).is_some())
        .collect();
    let failures: Vec<String> = meshes
        .par_iter()
        .filter_map(|path| {
            let data = vfs.read(path).ok()?;
            VisMesh::parse(&data, MeshKind::from_path(path)?)
                .err()
                .map(|e| format!("{path}: {e}"))
        })
        .collect();
    println!("visible meshes: {} ok, {} failed", meshes.len() - failures.len(), failures.len());
    for f in failures.iter().take(30) {
        println!("  {f}");
    }

    let collisions: Vec<&str> = paths
        .iter()
        .copied()
        .filter(|p| p.ends_with(".collisionmesh"))
        .collect();
    let failures: Vec<String> = collisions
        .par_iter()
        .filter_map(|path| {
            let data = vfs.read(path).ok()?;
            CollisionMesh::parse(&data).err().map(|e| format!("{path}: {e}"))
        })
        .collect();
    println!(
        "collision meshes: {} ok, {} failed",
        collisions.len() - failures.len(),
        failures.len()
    );
    for f in failures.iter().take(30) {
        println!("  {f}");
    }
    Ok(())
}

fn write_readme(out: &std::path::Path) -> Result<()> {
    std::fs::write(
        out.join("README.txt"),
        "Converted from a local Battlefield 2 installation by bf2-import.\n\
         This is EA's copyrighted content: for personal use on this machine only.\n\
         Do not commit, upload or redistribute these files.\n",
    )?;
    Ok(())
}

/// The title in a BF2 mod's `Mod.desc` (`<title> Allied Intent Xtended 2.0 </title>`), else
/// the folder name.
fn mod_title_of(install: &Bf2Install, mod_name: &str) -> String {
    let dir = install.mod_dir(mod_name);
    let text = std::fs::read(dir.join("Mod.desc"))
        .or_else(|_| std::fs::read(dir.join("mod.desc")))
        .map(|bytes| String::from_utf8_lossy(&bytes).into_owned())
        .unwrap_or_default();
    let lower = text.to_ascii_lowercase();
    lower
        .find("<title>")
        .and_then(|start| {
            let start = start + "<title>".len();
            let end = start + lower[start..].find("</title>")?;
            Some(text[start..end].trim().to_string())
        })
        // The base game's are localization keys (`MODDESC_BF2_TITLE`).
        .filter(|t| !t.is_empty() && !t.starts_with("MODDESC_"))
        .unwrap_or_else(|| mod_name.to_string())
}

/// A namespaced import is a mod of the game: gives its folder a `mod.ron` (unless it has one)
/// so the game finds it (`game_shared::mods`).
fn write_mod_ron(out: &Path, namespace: &str, title: Option<&str>, bf2_mod: Option<&str>) -> Result<()> {
    let path = out.join("mod.ron");
    if path.exists() {
        return Ok(());
    }
    let name = title.unwrap_or(namespace);
    let source = bf2_mod.map_or(String::new(), |m| format!(" from the BF2 mod `mods/{m}`"));
    let description = format!(
        "Imported{source} by bf2-import: its levels are namespaced `{namespace}_*`. Converted from a local BF2 installation: personal use only, do not redistribute."
    );
    std::fs::write(&path, format!("(\n    name: {name:?},\n    description: {description:?},\n    priority: 10,\n)\n"))?;
    Ok(())
}

/// Levels finished in an output folder, for `level --resume`: `<out>/import-state.json`.
#[derive(Default, serde::Serialize, serde::Deserialize)]
struct ImportState {
    /// `<bf2 mod>/<level folder>`, lowercase.
    done: Vec<String>,
}

impl ImportState {
    const FILE: &str = "import-state.json";

    fn load(out: &Path) -> Self {
        std::fs::read(out.join(Self::FILE))
            .ok()
            .and_then(|bytes| serde_json::from_slice(&bytes).ok())
            .unwrap_or_default()
    }

    fn key(level: &bf2_formats::LevelInfo) -> String {
        format!("{}/{}", level.mod_name, level.name).to_lowercase()
    }

    fn is_done(&self, level: &bf2_formats::LevelInfo) -> bool {
        self.done.contains(&Self::key(level))
    }

    fn mark_done(&mut self, out: &Path, level: &bf2_formats::LevelInfo) {
        let key = Self::key(level);
        if !self.done.contains(&key) {
            self.done.push(key);
        }
        let write = serde_json::to_vec_pretty(self)
            .map_err(anyhow::Error::from)
            .and_then(|bytes| Ok(std::fs::write(out.join(Self::FILE), bytes)?));
        if let Err(err) = write {
            log::warn!("{}: {err:#}", out.join(Self::FILE).display());
        }
    }
}
