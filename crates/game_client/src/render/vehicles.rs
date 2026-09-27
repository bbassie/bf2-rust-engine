//! Draws vehicles: one entity per part, posed from [`VehicleView`] every frame (turrets and
//! barrels at their joint angles, wheels on their springs, spinning with the speed). Meshes
//! go through the static object pipeline.

use bevy::prelude::*;
use game_shared::{
    statics::StaticMesh,
    vehicle::{Seated, VehicleData, VehicleHealth, joint_rotation},
};

use crate::{
    camera::ThirdPerson,
    net::LocalSoldier,
    vehicles::{VehicleView, VehicleViewSystems},
};

pub struct VehicleRenderPlugin;

impl Plugin for VehicleRenderPlugin {
    fn build(&self, app: &mut App) {
        app.add_observer(spawn_parts).add_systems(
            PostUpdate,
            (pose_parts, hide_own_hull, show_wrecks)
                .after(VehicleViewSystems)
                .before(TransformSystems::Propagate),
        );
    }
}

/// A destroyed vehicle whose wreck model is showing.
#[derive(Component)]
struct ShowsWreck;

/// Swaps a destroyed vehicle's parts for its wreck model: piece `n` where the part drawn with
/// hull mesh index `n` is.
fn show_wrecks(
    mut commands: Commands,
    vehicles: Query<(Entity, &VehicleData, &VehicleView, &VehicleHealth, &VehicleParts), Without<ShowsWreck>>,
) {
    for (entity, data, view, health, parts) in &vehicles {
        if !health.wrecked() {
            continue;
        }
        let model = &data.0;
        let desc = &model.desc;
        commands.entity(entity).insert(ShowsWreck);
        let (Some(wreck), Some(hull_mesh)) = (&desc.wreck_mesh, desc.parts.first().and_then(|p| p.mesh.as_ref())) else {
            continue;
        };
        if let Some(&hull) = parts.parts.first() {
            commands.entity(hull).insert(Visibility::Hidden);
        }
        let transforms = model.part_transforms(&view.joints);
        for piece in 0..desc.wreck_pieces {
            let at = desc
                .parts
                .iter()
                .position(|p| p.mesh.as_ref() == Some(hull_mesh) && p.mesh_index == piece)
                .map_or(Transform::IDENTITY, |i| transforms[i]);
            commands.spawn((
                at,
                Visibility::default(),
                StaticMesh {
                    path: wreck.clone(),
                    index: piece,
                },
                ChildOf(entity),
            ));
        }
    }
}

/// Looking out of a closed hull in first person, the outside model would only block the
/// view (BF2 draws a separate cockpit model there, which we don't import yet).
fn hide_own_hull(
    third_person: Res<ThirdPerson>,
    seated: Query<&Seated, With<LocalSoldier>>,
    mut vehicles: Query<(Entity, &VehicleData, &mut Visibility), With<VehicleParts>>,
) {
    let inside = seated.single().ok().filter(|_| !third_person.0);
    for (entity, data, mut visibility) in &mut vehicles {
        let hidden = inside.is_some_and(|s| {
            s.vehicle == entity && data.0.desc.seats.get(s.seat as usize).is_some_and(|seat| !seat.open)
        });
        visibility.set_if_neq(if hidden { Visibility::Hidden } else { Visibility::Inherited });
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
