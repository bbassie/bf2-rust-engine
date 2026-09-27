//! Draws vehicles: one entity per part, posed from [`VehicleView`] every frame (turrets and
//! barrels at their joint angles, wheels on their springs, spinning with the speed). Meshes
//! go through the static object pipeline.

use bevy::prelude::*;
use game_shared::{
    statics::StaticMesh,
    vehicle::{VehicleData, joint_rotation},
};

use crate::vehicles::{VehicleView, VehicleViewSystems};

pub struct VehicleRenderPlugin;

impl Plugin for VehicleRenderPlugin {
    fn build(&self, app: &mut App) {
        app.add_observer(spawn_parts).add_systems(
            PostUpdate,
            pose_parts
                .after(VehicleViewSystems)
                .before(TransformSystems::Propagate),
        );
    }
}

/// The part entities of a vehicle, in part order, and how far each wheel has turned.
#[derive(Component)]
struct VehicleParts {
    parts: Vec<Entity>,
    spin: Vec<f32>,
}

fn spawn_parts(add: On<Add, VehicleData>, mut commands: Commands, vehicles: Query<&VehicleData>) {
    let Ok(data) = vehicles.get(add.entity) else {
        return;
    };
    let model = &data.0;
    let mut parts: Vec<Entity> = Vec::with_capacity(model.desc.parts.len());
    for (i, part) in model.desc.parts.iter().enumerate() {
        let parent = part.parent.map_or(add.entity, |p| parts[p as usize]);
        let mut entity = commands.spawn((model.rest[i], Visibility::default(), ChildOf(parent)));
        if let Some(mesh) = &part.mesh {
            entity.insert(StaticMesh {
                path: mesh.clone(),
                index: part.mesh_index,
            });
        }
        parts.push(entity.id());
    }
    commands.entity(add.entity).insert((
        Visibility::default(),
        VehicleParts {
            parts,
            spin: vec![0.0; model.desc.wheels.len()],
        },
    ));
}

fn pose_parts(
    time: Res<Time>,
    mut vehicles: Query<(&VehicleView, &VehicleData, &mut VehicleParts)>,
    mut transforms: Query<&mut Transform>,
) {
    let dt = time.delta_secs();
    for (view, data, mut parts) in &mut vehicles {
        let model = &data.0;
        let VehicleParts { parts, spin } = &mut *parts;
        for (i, &entity) in parts.iter().enumerate() {
            let mut local = model.rest[i];
            if let Some(angles) = model.joint_index[i].and_then(|j| view.joints.get(j)) {
                local.rotation *= joint_rotation(*angles);
            }
            if let Ok(mut transform) = transforms.get_mut(entity) {
                transform.set_if_neq(local);
            }
        }
        for (wi, wheel) in model.desc.wheels.iter().enumerate() {
            let angle = &mut spin[wi];
            *angle = (*angle - view.speed / wheel.radius.max(0.1) * dt) % std::f32::consts::TAU;
            let offset = view.wheels.get(wi).copied().unwrap_or(0.0);
            let Some(&entity) = parts.get(wheel.part as usize) else {
                continue;
            };
            if let Ok(mut transform) = transforms.get_mut(entity) {
                transform.translation.y -= offset;
                transform.rotation *= Quat::from_rotation_x(*angle);
            }
        }
    }
}
