//! Everything that draws the world.

use bevy::prelude::*;

mod blend;
mod destruction;
pub mod environment;
mod far_trees;
mod flags;
pub mod materials;
mod hitzones;
mod projectiles;
mod props;
mod ropes;
pub mod scope;
mod soldiers;
mod statics;
mod terrain;
mod vegetation;
mod vehicles;
pub mod viewmodel;
mod water;

pub struct RenderPlugin;

impl Plugin for RenderPlugin {
    fn build(&self, app: &mut App) {
        app.add_plugins((
            materials::MaterialsPlugin,
            environment::EnvironmentPlugin,
            flags::FlagRenderPlugin,
            terrain::TerrainRenderPlugin,
            props::PropRenderPlugin,
            soldiers::SoldierRenderPlugin,
            statics::StaticRenderPlugin,
            viewmodel::ViewModelPlugin,
            vehicles::VehicleRenderPlugin,
            scope::ScopePlugin,
        ))
        .add_plugins((destruction::DestructionRenderPlugin, projectiles::ProjectileRenderPlugin, hitzones::HitZoneDebugPlugin))
        .add_plugins((vegetation::VegetationRenderPlugin, water::WaterPlugin, ropes::RopeRenderPlugin))
        .add_plugins(far_trees::FarTreesPlugin);
    }
}
