//! `bf2-import`: converts content from a Battlefield 2 installation you own into the game's
//! own formats. Nothing is uploaded or redistributed; output stays on your machine.
//!
//! ```text
//! bf2-import --bf2 "C:\Program Files (x86)\EA Games\Battlefield 2" list
//! bf2-import --bf2 ... level strike_at_karkand
//! bf2-import --bf2 ... level --all
//! bf2-import --bf2 ... check          # parse every mesh and collision mesh, report failures
//! ```

use std::{path::PathBuf, time::Instant};

use anyhow::{Context, Result, bail};
use bf2_formats::{
    Bf2Install, Side, Vfs,
    collision::CollisionMesh,
    mesh::{MeshKind, VisMesh},
};
use clap::{Parser, Subcommand};
use rayon::prelude::*;

mod coords;
mod dds;
mod glb;
mod level;
mod meshes;
mod terrain;

#[derive(Parser)]
#[command(version, about = "Convert content from a Battlefield 2 installation")]
struct Cli {
    /// Battlefield 2 installation folder (or set BF2_DIR).
    #[arg(long, env = "BF2_DIR")]
    bf2: PathBuf,
    /// Output folder for converted assets.
    #[arg(long, default_value = "imported")]
    out: PathBuf,
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// List installed mods and levels.
    List,
    /// Import levels (terrain, static objects, game mode layouts and the meshes they use).
    Level {
        /// Level folder names (case-insensitive).
        names: Vec<String>,
        /// Import every level of every installed mod.
        #[arg(long)]
        all: bool,
    },
    /// Parse every mesh and collision mesh of a mod and report failures.
    Check {
        #[arg(long, default_value = "bf2")]
        r#mod: String,
    },
}

fn main() -> Result<()> {
    env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("info")).init();
    let cli = Cli::parse();
    let install = Bf2Install::open(&cli.bf2)?;

    match cli.command {
        Command::List => {
            for mod_name in install.mods() {
                println!("{mod_name}");
                for level in install.levels(&mod_name) {
                    println!("  {}", level.name);
                }
            }
        }
        Command::Level { names, all } => {
            let levels = if all {
                install.mods().iter().flat_map(|m| install.levels(m)).collect()
            } else {
                if names.is_empty() {
                    bail!("name at least one level, or pass --all");
                }
                names
                    .iter()
                    .map(|n| install.find_level(n))
                    .collect::<Result<Vec<_>, _>>()?
            };
            std::fs::create_dir_all(&cli.out)?;
            write_readme(&cli.out)?;
            for level in levels {
                let started = Instant::now();
                log::info!("importing {} ({})", level.name, level.mod_name);
                match level::import_level(&install, &level, &cli.out) {
                    Ok(report) => {
                        log::info!(
                            "{}: {} statics, {} templates, {} mesh files, modes [{}] in {:.1}s",
                            level.name,
                            report.statics,
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
        }
        Command::Check { r#mod } => check(&install, &r#mod)?,
    }
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
