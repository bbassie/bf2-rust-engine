//! Grappling ropes and ziplines: thin cylinders along the rope, and the zipline's stand.

use bevy::prelude::*;
use game_data::RopeKind;
use game_shared::rope::{Rope, ZIPLINE_STAND};

pub struct RopeRenderPlugin;

impl Plugin for RopeRenderPlugin {
    fn build(&self, app: &mut App) {
        app.add_systems(Startup, load_rope_assets)
            .add_observer(draw_rope);
    }
}

#[derive(Resource)]
struct RopeAssets {
    /// A unit cylinder along Y, scaled to each piece.
    cylinder: Handle<Mesh>,
    rope: Handle<StandardMaterial>,
    metal: Handle<StandardMaterial>,
}

fn load_rope_assets(
    mut commands: Commands,
    mut meshes: ResMut<Assets<Mesh>>,
    mut materials: ResMut<Assets<StandardMaterial>>,
) {
    commands.insert_resource(RopeAssets {
        cylinder: meshes.add(Cylinder::new(1.0, 1.0)),
        rope: materials.add(StandardMaterial {
            base_color: Color::srgb(0.42, 0.36, 0.26),
            perceptual_roughness: 0.9,
            ..default()
        }),
        metal: materials.add(StandardMaterial {
            base_color: Color::srgb(0.3, 0.31, 0.32),
            metallic: 0.8,
            perceptual_roughness: 0.4,
            ..default()
        }),
    });
}

/// The rope's pieces, as children of the rope entity (they go when it goes).
fn draw_rope(
    add: On<Add, Rope>,
    mut commands: Commands,
    ropes: Query<&Rope>,
    assets: Option<Res<RopeAssets>>,
) {
    let (Ok(rope), Some(assets)) = (ropes.get(add.entity), assets) else {
        return;
    };
    // The rope entity carries the climbable part's transform: pieces are placed in world
    // space and parented through its inverse.
    let parent = rope.transform();
    let piece = |commands: &mut Commands,
                 from: Vec3,
                 to: Vec3,
                 radius: f32,
                 material: &Handle<StandardMaterial>| {
        let along = to - from;
        let world = Transform::from_translation((from + to) * 0.5)
            .with_rotation(Quat::from_rotation_arc(
                Vec3::Y,
                along.normalize_or(Vec3::Y),
            ))
            .with_scale(Vec3::new(radius, along.length().max(0.01), radius));
        let local = Transform::from_matrix(parent.to_matrix().inverse() * world.to_matrix());
        commands.spawn((
            Mesh3d(assets.cylinder.clone()),
            MeshMaterial3d(material.clone()),
            local,
            ChildOf(add.entity),
        ));
    };
    commands.entity(add.entity).insert(Visibility::default());
    match rope.kind {
        RopeKind::Grapple => {
            piece(&mut commands, rope.anchor, rope.top, 0.015, &assets.rope);
            piece(&mut commands, rope.top, rope.end, 0.015, &assets.rope);
        }
        RopeKind::Zipline => {
            piece(&mut commands, rope.top, rope.end, 0.01, &assets.metal);
            piece(
                &mut commands,
                rope.top - Vec3::Y * ZIPLINE_STAND,
                rope.top,
                0.04,
                &assets.metal,
            );
        }
    }
}
