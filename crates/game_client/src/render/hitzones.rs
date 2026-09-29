//! `BF2_SHOW_HITZONES=1`: draws every soldier's hit zones over the model, posed as the server
//! judges the shots fired looking at this moment (the same capsules the tracers stop in), to
//! check that they follow it.

use bevy::prelude::*;
use game_shared::hitzones::{BODY, HEAD, LIMBS};

use crate::combat::DrawnTargets;

pub struct HitZoneDebugPlugin;

impl Plugin for HitZoneDebugPlugin {
    fn build(&self, app: &mut App) {
        if std::env::var_os("BF2_SHOW_HITZONES").is_some() {
            app.add_systems(Startup, draw_on_top)
                .add_systems(PostUpdate, draw.after(crate::prediction::RenderStateSystems));
        }
    }
}

fn draw_on_top(mut store: ResMut<GizmoConfigStore>) {
    store.config_mut::<DefaultGizmoConfigGroup>().0.depth_bias = -1.0;
}

fn draw(drawn: DrawnTargets, mut gizmos: Gizmos) {
    for target in drawn.collect() {
        for zone in target.zones {
            let (a, b) = drawn.capsule(&target, zone);
            let color = match zone.material {
                HEAD => Color::srgb(1.0, 0.1, 0.1),
                BODY => Color::srgb(1.0, 0.85, 0.1),
                LIMBS => Color::srgb(0.2, 1.0, 0.3),
                _ => Color::srgb(1.0, 0.5, 0.0),
            };
            let axis = b - a;
            let rotation = Quat::from_rotation_arc(Vec3::Y, axis.normalize_or(Vec3::Y));
            gizmos.primitive_3d(
                &Capsule3d::new(zone.radius, axis.length()),
                Isometry3d::new((a + b) * 0.5, rotation),
                color,
            );
        }
    }
}
