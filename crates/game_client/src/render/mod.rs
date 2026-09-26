//! Everything that draws the world.

use bevy::prelude::*;

mod environment;
pub mod materials;
mod props;
mod soldiers;
mod statics;
mod terrain;

pub struct RenderPlugin;

impl Plugin for RenderPlugin {
    fn build(&self, app: &mut App) {
        app.add_plugins((
            materials::MaterialsPlugin,
            environment::EnvironmentPlugin,
            terrain::TerrainRenderPlugin,
            props::PropRenderPlugin,
            soldiers::SoldierRenderPlugin,
            statics::StaticRenderPlugin,
        ));
    }
}
