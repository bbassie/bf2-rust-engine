//! Light clustering on the CPU, and only when there are lights to cluster.
//!
//! Bevy sorts the point and spot lights into a grid of clusters over the camera's frustum, so
//! that every pixel only shades the lights near it. Bevy 0.19 does that on the GPU where it
//! can: per view a handful of buffers, bind groups, a raster and several compute passes, and
//! a read-back, every frame, even when no lamp is lit and no muzzle flash lights anything.
//! Here the lights are few (the lamps within 220 m at night, the muzzle and explosion
//! lights), which the CPU clusters in microseconds, so clustering runs on the CPU, and the
//! player's camera clusters (`ClusterConfig::FixedZ`, Bevy's default) only while a point or
//! spot light could be seen; `ClusterConfig::None` otherwise. Karkand, no bots: render thread
//! 5.3 -> 4.8 ms a frame. `BF2_PERF_EXP=gpuclusters` keeps Bevy's GPU clustering, `=clusters`
//! keeps clustering on without lights.
//!
//! (`ClusterConfig::None` with GPU clustering fails in Bevy 0.19: it creates a zero-sized
//! texture.)

use bevy::{
    camera::{CameraUpdateSystems, visibility::VisibilitySystems},
    light::{
        SimulationLightSystems,
        cluster::{ClusterConfig, GlobalClusterSettings},
    },
    prelude::*,
};

use crate::camera::PlayerCamera;

pub struct LightClustersPlugin;

impl Plugin for LightClustersPlugin {
    fn build(&self, app: &mut App) {
        if !crate::perf_experiment("gpuclusters") {
            // Before the first frame renders (the settings are made in `PbrPlugin::finish`).
            app.add_systems(Startup, |mut settings: ResMut<GlobalClusterSettings>| {
                settings.gpu_clustering = None;
            });
        }
        if crate::perf_experiment("clusters") {
            return;
        }
        app.add_systems(
            PostUpdate,
            cluster_only_with_lights
                .after(VisibilitySystems::VisibilityPropagate)
                .after(CameraUpdateSystems)
                .before(SimulationLightSystems::AssignLightsToClusters),
        );
    }
}

/// Sets the player's camera to cluster lights only while one could be seen (by its
/// `InheritedVisibility`: the lamps beyond their cull distance and the unused muzzle flash
/// lights are hidden).
fn cluster_only_with_lights(
    mut commands: Commands,
    lights: Query<&InheritedVisibility, Or<(With<PointLight>, With<SpotLight>)>>,
    cameras: Query<(Entity, Option<&ClusterConfig>), With<PlayerCamera>>,
    settings: Res<GlobalClusterSettings>,
) {
    let any_light = lights.iter().any(|visible| visible.get());
    // GPU clustering can't do `None`: one cluster there.
    let off = if settings.gpu_clustering.is_some() { ClusterConfig::Single } else { ClusterConfig::None };
    for (camera, config) in &cameras {
        let clustering = !matches!(config, Some(ClusterConfig::None | ClusterConfig::Single));
        if any_light != clustering {
            let config = if any_light { ClusterConfig::default() } else { off };
            commands.entity(camera).insert(config);
        }
    }
}
