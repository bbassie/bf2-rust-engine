//! `BF2_SHOW_HITZONES=1`: draws every soldier's hit zones as the server poses them, over
//! the model, to check that they follow it.

use bevy::prelude::*;
use game_shared::{
    hitzones::{BODY, BodyPose, HEAD, LIMBS},
    soldier::Soldier,
    weapons::{Armory, Loadout},
};

use crate::prediction::SoldierRender;

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

fn draw(
    armory: Res<Armory>,
    soldiers: Query<(&SoldierRender, &Loadout), With<Soldier>>,
    bones: Query<(&Name, &GlobalTransform)>,
    mut gizmos: Gizmos,
) {
    // The drawn skeleton's bones the zones hang on, to compare.
    for (name, transform) in &bones {
        let color = match name.as_str() {
            "head" => Color::WHITE,
            "left_upperleg" | "left_lowerleg" | "left_shoulder" => Color::srgb(0.2, 0.4, 1.0),
            "right_upperleg" | "right_lowerleg" | "right_shoulder" => Color::srgb(1.0, 0.2, 1.0),
            _ => continue,
        };
        gizmos.sphere(Isometry3d::from_translation(transform.translation()), 0.04, color);
    }
    for (render, loadout) in &soldiers {
        let pose = BodyPose {
            position: render.position,
            yaw: render.yaw,
            stance: render.stance,
        };
        for zone in armory.hit_zones(&loadout.kit) {
            let (a, b) = pose.capsule(zone);
            if let Some((_, bone)) = bones.iter().find(|(n, _)| n.as_str() == zone.bone) {
                let local = |p: Vec3| Quat::from_rotation_y(-pose.yaw) * (p - pose.position);
                debug!(
                    "{:?} {}: drawn bone {:.2}, zone from {:.2} to {:.2}",
                    pose.stance,
                    zone.bone,
                    local(bone.translation()),
                    local(a),
                    local(b)
                );
            }
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
