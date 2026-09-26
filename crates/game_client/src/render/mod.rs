//! Everything that draws the world.

use bevy::prelude::*;

mod environment;
mod props;
mod soldiers;
mod statics;
mod terrain;

pub struct RenderPlugin;

impl Plugin for RenderPlugin {
    fn build(&self, app: &mut App) {
        app.add_plugins((
            environment::EnvironmentPlugin,
            terrain::TerrainRenderPlugin,
            props::PropRenderPlugin,
            soldiers::SoldierRenderPlugin,
            statics::StaticRenderPlugin,
        ));
    }
}
