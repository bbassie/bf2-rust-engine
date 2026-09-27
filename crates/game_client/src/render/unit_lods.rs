//! Levels of detail and draw distances of vehicles and soldiers, like the static objects'
//! (see `statics`): each LOD mesh is drawn within its distance band (Bevy's
//! [`VisibilityRange`], cross-fading by dithering around each switch), and the whole unit
//! fades out past BF2's cull distance for player control objects. The distances come from
//! the import; zooming in reaches further (switch distances times the zoom, draw distances
//! times its square root) and the view distance setting scales the draw distances, with the
//! same factors as the statics. `BF2_UNIT_LODS=off` draws vehicles and soldiers at full
//! detail at any distance (for comparisons).

use std::sync::Arc;

use bevy::{camera::visibility::VisibilityRange, prelude::*};

use super::statics::{LodScales, lod_ranges};

pub struct UnitLodPlugin;

impl Plugin for UnitLodPlugin {
    fn build(&self, app: &mut App) {
        app.insert_resource(UnitLodConfig::from_env()).add_systems(PostUpdate, apply_unit_lods);
    }
}

/// Whether vehicles and soldiers use their LODs and draw distances (`BF2_UNIT_LODS`).
#[derive(Resource, Clone, Copy, Debug)]
pub struct UnitLodConfig {
    pub enabled: bool,
}

impl UnitLodConfig {
    fn from_env() -> Self {
        let enabled = !matches!(
            std::env::var("BF2_UNIT_LODS").as_deref().map(str::to_ascii_lowercase).as_deref(),
            Ok("off" | "0" | "false" | "no")
        );
        if !enabled {
            info!("vehicle and soldier LODs and draw distances are off (BF2_UNIT_LODS)");
        }
        Self { enabled }
    }
}

/// How far a small part of a unit with a model of its own (a soldier's weapon) is drawn:
/// BF2 culls parts under 0.8 of their object's cull radius on their own, with half the
/// minimum cull distance and half the (non-PCO) cull constant squared (`C = max(√(5π) · 8 ·
/// r, 40)` on high; see docs/formats/meshes.md §2.12), measured to its cull sphere. `r` is
/// half the diagonal of its model's box; `None` if it goes with its object.
pub fn small_part_draw_distance(radius: f32, object_cull_radius: f32) -> Option<f32> {
    const DISTANCE_CULL_CONST: f32 = 8.0;
    const MIN_CULL_DISTANCE: f32 = 80.0;
    if !(radius > 0.0) || radius >= 0.8 * object_cull_radius {
        return None;
    }
    let cull = ((5.0 * std::f32::consts::PI).sqrt() * DISTANCE_CULL_CONST * radius).max(MIN_CULL_DISTANCE * 0.5);
    Some((cull * cull + radius * radius).sqrt())
}

/// On a mesh of a vehicle or soldier: it is level `level` of LODs starting at `starts` (the
/// first at 0) of something drawn up to `draw_distance`, before scaling.
#[derive(Component, Clone, Debug)]
pub struct UnitLod {
    pub starts: Arc<[f32]>,
    pub draw_distance: Option<f32>,
    pub level: usize,
}

impl UnitLod {
    fn range(&self, scales: &LodScales) -> Option<VisibilityRange> {
        lod_ranges(&self.starts, self.draw_distance, scales.lod, scales.draw)
            .into_iter()
            .nth(self.level)
            .flatten()
    }
}

/// Gives new LOD meshes their visibility range, and applies changed scales (view distance
/// setting, zoom) to all of them.
fn apply_unit_lods(
    mut commands: Commands,
    config: Res<UnitLodConfig>,
    scales: Res<LodScales>,
    added: Query<(Entity, &UnitLod), Added<UnitLod>>,
    mut existing: Query<(&UnitLod, &mut VisibilityRange)>,
) {
    if !config.enabled {
        return;
    }
    if scales.is_changed() {
        for (lod, mut range) in &mut existing {
            if let Some(new) = lod.range(&scales) {
                range.set_if_neq(new);
            }
        }
    }
    for (entity, lod) in &added {
        if let Some(range) = lod.range(&scales) {
            commands.entity(entity).insert(range);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn small_parts_fade_out_on_their_own() {
        // An M16 (half diagonal 0.51 m) in a soldier's hands (cull radius 2.45 m): 40 m.
        let d = small_part_draw_distance(0.51, 2.45).unwrap();
        assert!((d - 40.003).abs() < 0.01, "{d}");
        // Big parts go with their object.
        assert_eq!(small_part_draw_distance(2.0, 2.45), None);
        // Larger small parts reach further than the minimum.
        let d = small_part_draw_distance(1.5, 4.0).unwrap();
        assert!((d - 47.58).abs() < 0.05, "{d}");
    }
}
