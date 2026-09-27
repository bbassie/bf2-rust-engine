//! The team and squad layers of the bots, and what bots know about weapons. The soldiers
//! themselves are in [`crate::bots`].
//!
//! - [`strategy`]: a commander per team values the strategic areas around the flags and
//!   gives every squad an order (attack or defend a flag).
//! - [`squad`]: leaders lead, members follow in formation; kits and spawns for dead bots.
//! - [`tactics`]: cover, flanking spots, grenade arcs, safe strafing.
//! - [`skill`]: difficulty and per-bot personality.
//! - [`stats`]: per-minute statistics.
//!
//! Level data comes from the importer ([`game_data::LevelAiDesc`], BF2's strategic areas,
//! and [`game_data::AiWeaponsDesc`], BF2's weapon templates); both are optional.

pub mod skill;
pub mod squad;
pub mod stats;
pub mod strategy;
pub mod tactics;

use bevy::prelude::*;
use game_data::{AiWeaponDesc, AiWeaponsDesc, FireKind, FiringPose, LevelAiDesc, WeaponDesc};
use game_shared::{config::GamePaths, level::LoadedLevel};

/// The AI hints of the loaded level.
#[derive(Resource, Default)]
pub struct AiData {
    pub level: LevelAiDesc,
    pub weapons: AiWeaponsDesc,
}

impl AiData {
    /// How bots use a weapon: BF2's template if imported, otherwise a guess from the weapon.
    pub fn weapon(&self, weapon: &WeaponDesc) -> AiWeaponDesc {
        if let Some(desc) = self.weapons.weapons.get(&weapon.name) {
            return desc.clone();
        }
        let thrown = weapon.fire.kind == FireKind::Thrown;
        let zoomed = weapon.zoom_factors.iter().any(|z| *z > 0.5);
        let max_range = match (thrown, weapon.projectiles_per_shot > 1, zoomed) {
            (true, ..) => 45.0,
            (_, true, _) => 30.0,
            (_, _, true) => 150.0,
            _ => 70.0,
        };
        AiWeaponDesc {
            min_range: if thrown { 12.0 } else { 0.0 },
            max_range,
            optimal_range: max_range * 0.5,
            pose: if zoomed { FiringPose::Prone } else { FiringPose::Crouching },
            infantry_strength: 5.0,
            thrown,
            explosion_radius: weapon.projectile.explosion_radius,
        }
    }
}

fn read<T: serde::de::DeserializeOwned>(path: std::path::PathBuf) -> Option<T> {
    if !path.exists() {
        return None;
    }
    game_data::read_ron(&path).map_err(|err| warn!("ai: {err}")).ok()
}

/// Loads the level's AI hints when a level is loaded.
pub fn load_ai_data(mut commands: Commands, level: Res<LoadedLevel>, paths: Option<Res<GamePaths>>) {
    let level_ai: LevelAiDesc = level.dir.as_ref().and_then(|dir| read(dir.join("ai.ron"))).unwrap_or_default();
    let weapons: AiWeaponsDesc = paths
        .as_ref()
        .and_then(|paths| read(paths.imported.join("ai").join("weapons.ron")))
        .unwrap_or_default();
    info!(
        "ai: {} layouts with strategic areas, {} weapon templates",
        level_ai.layouts.len(),
        weapons.weapons.len()
    );
    commands.insert_resource(AiData {
        level: level_ai,
        weapons,
    });
}
