//! Draws vehicles: one entity per part, posed from [`VehicleView`] every frame (turrets and
//! barrels at their joint angles, wheels on their springs, spinning with the speed, control
//! surfaces, landing gear and rotor blades). Each part has its outside model and, if it has
//! one, its interior (BF2's cockpits and sights), shown instead to an occupant looking out of
//! a closed seat. Meshes go through the static object pipeline.

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
            (pose_parts, show_interior, show_wrecks)
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

/// Looking out of a closed seat in first person, the interior replaces the outside model
/// (which would only block the view; without an interior nothing is drawn).
fn show_interior(
    third_person: Res<ThirdPerson>,
    seated: Query<&Seated, With<LocalSoldier>>,
    vehicles: Query<(Entity, &VehicleData, &VehicleParts)>,
    mut visibility: Query<&mut Visibility>,
) {
    let inside = seated.single().ok().filter(|_| !third_person.0);
    for (entity, data, parts) in &vehicles {
        let inside = inside.is_some_and(|s| {
            s.vehicle == entity && data.0.desc.seats.get(s.seat as usize).is_some_and(|seat| !seat.open)
        });
        let (outside, interior) = match inside {
            true => (Visibility::Hidden, Visibility::Inherited),
            false => (Visibility::Inherited, Visibility::Hidden),
        };
        for (list, wanted) in [(&parts.outside, outside), (&parts.interior, interior)] {
            for &mesh in list {
                if let Ok(mut visibility) = visibility.get_mut(mesh) {
                    visibility.set_if_neq(wanted);
                }
            }
        }
    }
}

/// The part entities of a vehicle, in part order, their outside and interior meshes, and how
/// far each wheel has turned.
#[derive(Component)]
struct VehicleParts {
    parts: Vec<Entity>,
    outside: Vec<Entity>,
    interior: Vec<Entity>,
    spin: Vec<f32>,
}

fn spawn_parts(add: On<Add, VehicleData>, mut commands: Commands, vehicles: Query<&VehicleData>) {
    let Ok(data) = vehicles.get(add.entity) else {
        return;
    };
    let model = &data.0;
    let mut parts: Vec<Entity> = Vec::with_capacity(model.desc.parts.len());
    let (mut outside, mut interior) = (Vec::new(), Vec::new());
    for (i, part) in model.desc.parts.iter().enumerate() {
        let parent = part.parent.map_or(add.entity, |p| parts[p as usize]);
        let entity = commands.spawn((model.rest[i], Visibility::default(), ChildOf(parent))).id();
        let meshes = [(&part.mesh, &mut outside, Visibility::Inherited), (&part.mesh_1p, &mut interior, Visibility::Hidden)];
        for (mesh, list, visibility) in meshes {
            if let Some(path) = mesh {
                list.push(
                    commands
                        .spawn((
                            Transform::IDENTITY,
                            visibility,
                            StaticMesh {
                                path: path.clone(),
                                index: part.mesh_index,
                            },
                            ChildOf(entity),
                        ))
                        .id(),
                );
            }
        }
        parts.push(entity);
    }
    commands.entity(add.entity).insert((
        Visibility::default(),
        VehicleParts {
            parts,
            outside,
            interior,
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
        let VehicleParts { parts, spin, .. } = &mut *parts;
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
