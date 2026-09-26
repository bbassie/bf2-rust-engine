//! Placeholder primitives used by the built-in test level.

use bevy::prelude::*;
use game_shared::level::BoxProp;

pub struct PropRenderPlugin;

impl Plugin for PropRenderPlugin {
    fn build(&self, app: &mut App) {
        app.add_observer(add_box_visual);
    }
}

fn add_box_visual(
    add: On<Add, BoxProp>,
    props: Query<&BoxProp>,
    mut commands: Commands,
    mut meshes: ResMut<Assets<Mesh>>,
    mut materials: ResMut<Assets<StandardMaterial>>,
) {
    let Ok(prop) = props.get(add.entity) else {
        return;
    };
    commands.entity(add.entity).insert((
        Mesh3d(meshes.add(Cuboid::from_size(prop.size))),
        MeshMaterial3d(materials.add(StandardMaterial {
            base_color: prop.color,
            perceptual_roughness: 0.9,
            ..default()
        })),
    ));
}
