//! Everything that draws the world.

use bevy::prelude::*;

mod blend;
pub mod environment;
mod flags;
pub mod materials;
mod props;
mod soldiers;
mod statics;
mod terrain;
pub mod viewmodel;

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
        ));
    }
}
