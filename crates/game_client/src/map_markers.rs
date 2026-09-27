//! Markers the minimap and the big map show besides flags and teammates: spotted enemies,
//! squad orders, the commander's assets and what they're doing. Rebuilt every frame by
//! `radio` and `commander`.

use bevy::prelude::*;

use crate::{
    camera::CameraSystems,
    prediction::RenderStateSystems,
    vehicles::VehicleViewSystems,
};

pub struct MapMarkersPlugin;

impl Plugin for MapMarkersPlugin {
    fn build(&self, app: &mut App) {
        app.init_resource::<MapMarkers>()
            .configure_sets(
                PostUpdate,
                MarkerSystems
                    .after(RenderStateSystems)
                    .after(VehicleViewSystems)
                    .after(CameraSystems),
            )
            .add_systems(PostUpdate, clear.before(MarkerSystems));
    }
}

/// Systems that add [`MapMarkers`], in `PostUpdate` once soldiers and vehicles moved.
#[derive(SystemSet, Debug, Clone, PartialEq, Eq, Hash)]
pub struct MarkerSystems;

#[derive(Clone, Debug)]
pub struct MapMarker {
    /// What it marks: an icon follows it.
    pub key: Entity,
    pub position: Vec3,
    pub color: Color,
    /// Diameter on the minimap, logical pixels (the big map draws them a bit larger).
    pub size: f32,
    /// Shown next to it on the big map.
    pub label: Option<String>,
}

#[derive(Resource, Default)]
pub struct MapMarkers(pub Vec<MapMarker>);

fn clear(mut markers: ResMut<MapMarkers>) {
    markers.0.clear();
}
