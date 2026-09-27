//! Draws vehicles: one entity per part, posed from [`VehicleView`] every frame (turrets and
//! barrels at their joint angles, wheels on their springs, spinning with the speed, control
//! surfaces, landing gear and rotor blades). Each part has its outside model and, if it has
//! one, its interior (BF2's cockpits and sights), shown instead to an occupant looking out of
//! a closed seat. Models are rigged: each model file is one skinned mesh whose joints are the
//! part entities, so geometry spanning parts (track belts) follows all of them; older imports
//! draw one mesh per part through the static object pipeline.

use std::collections::HashMap;

use bevy::{
    camera::visibility::NoFrustumCulling,
    gltf::Gltf,
    mesh::skinning::SkinnedMesh,
    prelude::*,
};
use game_shared::{
    statics::StaticMesh,
    vehicle::{Seated, VehicleData, VehicleHealth, joint_rotation},
};

use super::materials::Bf2Materials;

use crate::{
    camera::ThirdPerson,
    net::LocalSoldier,
    vehicles::{VehicleView, VehicleViewSystems},
};

pub struct VehicleRenderPlugin;

impl Plugin for VehicleRenderPlugin {
    fn build(&self, app: &mut App) {
        app.add_observer(spawn_parts)
            .add_systems(Update, spawn_rigs)
            .add_systems(
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
        for &mesh in parts.outside.iter().chain(&parts.interior) {
            commands.entity(mesh).insert(Visibility::Hidden);
        }
        commands.entity(entity).remove::<VehicleParts>().insert(VehicleParts {
            outside: Vec::new(),
            interior: Vec::new(),
            ..parts.clone()
        });
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
#[derive(Component, Clone)]
struct VehicleParts {
    parts: Vec<Entity>,
    outside: Vec<Entity>,
    interior: Vec<Entity>,
    spin: Vec<f32>,
}

/// A rigged model of a vehicle: one skinned mesh whose joint `n` is `joints[n]` (the part
/// drawing mesh index `n`). Joints no part draws collapse out of sight.
#[derive(Component)]
struct VehicleRig {
    gltf: Handle<Gltf>,
    joints: HashMap<u32, Entity>,
}

fn spawn_parts(
    add: On<Add, VehicleData>,
    mut commands: Commands,
    vehicles: Query<&VehicleData>,
    asset_server: Res<AssetServer>,
) {
    let Ok(data) = vehicles.get(add.entity) else {
        return;
    };
    let model = &data.0;
    let desc = &model.desc;
    let mut parts: Vec<Entity> = Vec::with_capacity(desc.parts.len());
    let (mut outside, mut interior) = (Vec::new(), Vec::new());
    for (i, part) in desc.parts.iter().enumerate() {
        let parent = part.parent.map_or(add.entity, |p| parts[p as usize]);
        let entity = commands.spawn((model.rest[i], Visibility::default(), ChildOf(parent))).id();
        parts.push(entity);
        if desc.rigged {
            continue;
        }
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
    }
    if desc.rigged {
        // One rig per model file, or more where several parts draw the same mesh index of it
        // (the same weapon model on several pylons).
        for (interior_view, list, visibility) in [(false, &mut outside, Visibility::Inherited), (true, &mut interior, Visibility::Hidden)] {
            let mut rigs: Vec<(String, HashMap<u32, Entity>)> = Vec::new();
            for (i, part) in desc.parts.iter().enumerate() {
                let Some(path) = (if interior_view { &part.mesh_1p } else { &part.mesh }) else {
                    continue;
                };
                match rigs.iter_mut().find(|(p, joints)| p == path && !joints.contains_key(&part.mesh_index)) {
                    Some((_, joints)) => {
                        joints.insert(part.mesh_index, parts[i]);
                    }
                    None => rigs.push((path.clone(), HashMap::from([(part.mesh_index, parts[i])]))),
                }
            }
            for (path, joints) in rigs {
                let rig = VehicleRig {
                    gltf: asset_server.load(format!("imported://{path}")),
                    joints,
                };
                list.push(commands.spawn((Transform::IDENTITY, visibility, rig, ChildOf(add.entity))).id());
            }
        }
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

/// Once a rig's model has loaded, draws its primitives skinned to the parts (a model that
/// isn't skinned, like a static mesh on a part, simply rides on its part).
#[allow(clippy::too_many_arguments)]
fn spawn_rigs(
    mut commands: Commands,
    rigs: Query<(Entity, &VehicleRig)>,
    gltfs: Res<Assets<Gltf>>,
    gltf_meshes: Res<Assets<bevy::gltf::GltfMesh>>,
    skins: Res<Assets<bevy::gltf::GltfSkin>>,
    meshes: Res<Assets<Mesh>>,
    mut materials: Bf2Materials,
    asset_server: Res<AssetServer>,
) {
    for (entity, rig) in &rigs {
        if let bevy::asset::LoadState::Failed(err) = asset_server.load_state(&rig.gltf) {
            warn!("vehicle model failed to load: {err}");
            commands.entity(entity).remove::<VehicleRig>();
            continue;
        }
        let Some(gltf) = gltfs.get(&rig.gltf) else {
            continue;
        };
        let Some(mesh) = gltf.meshes.first().and_then(|m| gltf_meshes.get(m)) else {
            continue;
        };
        let skin = gltf.skins.first().and_then(|s| skins.get(s));
        if gltf.skins.first().is_some() && skin.is_none() {
            continue;
        }
        // The glTF's own materials may not be ready yet; try again next frame.
        let Some(primitive_materials) = mesh
            .primitives
            .iter()
            .map(|primitive| materials.for_primitive(primitive))
            .collect::<Option<Vec<_>>>()
        else {
            continue;
        };
        commands.entity(entity).remove::<VehicleRig>();
        let skinned = skin.map(|skin| {
            let collapsed = commands.spawn((Transform::from_scale(Vec3::ZERO), ChildOf(entity))).id();
            let joints = (0..skin.joints.len() as u32)
                .map(|n| rig.joints.get(&n).copied().unwrap_or(collapsed))
                .collect();
            SkinnedMesh {
                inverse_bindposes: skin.inverse_bind_matrices.clone(),
                joints,
            }
        });
        for (primitive, material) in mesh.primitives.iter().zip(primitive_materials) {
            let rigged = meshes
                .get(&primitive.mesh)
                .is_some_and(|m| m.attribute(Mesh::ATTRIBUTE_JOINT_INDEX).is_some());
            match (&skinned, rigged) {
                (Some(skinned), true) => {
                    commands.spawn((
                        Mesh3d(primitive.mesh.clone()),
                        MeshMaterial3d(material),
                        skinned.clone(),
                        // The skinned vertices are wherever the parts are.
                        NoFrustumCulling,
                        ChildOf(entity),
                    ));
                }
                _ => {
                    let part = rig.joints.get(&0).copied().unwrap_or(entity);
                    commands.spawn((Mesh3d(primitive.mesh.clone()), MeshMaterial3d(material), ChildOf(part)));
                }
            }
        }
    }
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
