//! Draws vehicles: one entity per part, posed from [`VehicleView`] every frame (turrets and
//! barrels at their joint angles, wheels on their springs, spinning with the speed, control
//! surfaces, landing gear and rotor blades). Each part has its outside model and, if it has
//! one, its interior (BF2's cockpits and sights), shown instead to an occupant looking out of
//! a closed seat. Models are rigged: each model file is one skinned mesh whose joints are the
//! part entities, so geometry spanning parts (track belts) follows all of them; older imports
//! draw one mesh per part through the static object pipeline. Tracks animate like BF2's:
//! the belt and wheel textures scroll, hub textures and drive sprockets turn with the
//! distance each track has run (road wheels themselves stand still).
//!
//! The outside models' lower levels of detail are rigged the same way (on the same part
//! entities) and drawn by distance, cross-fading, and the vehicle fades out past its draw
//! distance (see `unit_lods`). A wreck that is repaired (the commander's artillery) gets its
//! models back.

use std::{collections::HashMap, sync::Arc};

use bevy::{
    camera::primitives::{Aabb, MeshAabb},
    gltf::Gltf,
    math::Affine2,
    mesh::skinning::SkinnedMesh,
    prelude::*,
};
use game_shared::{
    statics::{StaticMesh, StaticMeshLods},
    vehicle::{Seated, VehicleData, VehicleHealth, joint_rotation},
};

use super::{
    materials::{Bf2Material, Bf2Materials},
    unit_lods::{UnitLod, UnitLodConfig},
};

use crate::{
    camera::ThirdPerson,
    effects::{EffectEmitter, Lasting, SpawnEffect},
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
                (
                    (run_tracks, (pose_parts, scroll_tracks)).chain(),
                    show_interior,
                    (show_wrecks, revive_wrecks).chain(),
                    play_armor_effects,
                    hide_burnt_wrecks,
                )
                    .after(VehicleViewSystems)
                    .before(TransformSystems::Propagate),
            );
    }
}

/// A destroyed vehicle whose wreck model is showing.
#[derive(Component)]
struct ShowsWreck;

/// A piece of a wreck model.
#[derive(Component)]
struct WreckPiece;

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
                // With their lower LODs, fading out where the vehicle would.
                StaticMeshLods {
                    lods: desc.wreck_lods.clone(),
                    draw_distance: desc.draw_distance,
                },
                WreckPiece,
                ChildOf(entity),
            ));
        }
    }
}

/// A wreck repaired above 0 hit points (the commander's artillery) is a vehicle again: its
/// wreck model goes, its models come back.
fn revive_wrecks(
    mut commands: Commands,
    vehicles: Query<(Entity, &VehicleData, &VehicleHealth, &VehicleParts, &Children), With<ShowsWreck>>,
    pieces: Query<(), With<WreckPiece>>,
    asset_server: Res<AssetServer>,
    config: Res<UnitLodConfig>,
) {
    for (entity, data, health, parts, children) in &vehicles {
        if health.wrecked() {
            continue;
        }
        for child in children.iter().filter(|c| pieces.contains(*c)) {
            commands.entity(child).despawn();
        }
        // The part tree hangs off the hull part; the models off the vehicle.
        for &old in parts.parts.first().into_iter().chain(&parts.models) {
            commands.entity(old).despawn();
        }
        let fresh = build_parts(&mut commands, entity, &data.0, &asset_server, config.enabled);
        commands.entity(entity).remove::<ShowsWreck>().insert(fresh);
    }
}

/// A wreck burns down to -100 % hit points while it stays; its last explosion there leaves
/// nothing behind.
fn hide_burnt_wrecks(
    vehicles: Query<(&VehicleHealth, &Children), With<ShowsWreck>>,
    mut pieces: Query<&mut Visibility, With<WreckPiece>>,
) {
    for (health, children) in &vehicles {
        if health.current > -health.max {
            continue;
        }
        for child in children.iter() {
            if let Ok(mut visibility) = pieces.get_mut(child) {
                visibility.set_if_neq(Visibility::Hidden);
            }
        }
    }
}

/// Where a vehicle's damage states stand: its hit points (percent of the maximum) when last
/// looked at and, per armor effect, the emitter of a lasting one while it runs.
#[derive(Component)]
struct ArmorEffects {
    percent: f32,
    running: Vec<Option<Entity>>,
}

/// BF2's damage states (the vehicle's armor effects): smoke and fire while the hit points are
/// under their thresholds, explosions once as they cross them, down to the wreck's last
/// explosion at -100 %.
fn play_armor_effects(
    mut commands: Commands,
    mut vehicles: Query<(Entity, &VehicleData, &VehicleHealth, &Transform, Option<&mut ArmorEffects>)>,
    mut effects: MessageWriter<SpawnEffect>,
) {
    for (entity, data, health, transform, state) in &mut vehicles {
        let desc = &data.0.desc;
        if desc.armor_effects.is_empty() {
            continue;
        }
        let percent = health.current / health.max.max(1.0) * 100.0;
        let Some(mut state) = state else {
            // Explosions from before we saw the vehicle stay in the past.
            commands.entity(entity).insert(ArmorEffects {
                percent,
                running: vec![None; desc.armor_effects.len()],
            });
            continue;
        };
        let before = std::mem::replace(&mut state.percent, percent);
        for (effect, running) in desc.armor_effects.iter().zip(&mut state.running) {
            let local = Transform::from_translation(Vec3::from_array(effect.position))
                .with_rotation(Quat::from_array(effect.rotation));
            let below = percent <= effect.hit_points;
            if effect.lasting {
                match (below, *running) {
                    (true, None) => {
                        let emitter = (local, EffectEmitter(effect.effect.clone()), Lasting, ChildOf(entity));
                        *running = Some(commands.spawn(emitter).id());
                    }
                    (false, Some(emitter)) => {
                        commands.entity(emitter).despawn();
                        *running = None;
                    }
                    _ => {}
                }
            } else if below && before > effect.hit_points {
                let at = *transform * local;
                effects.write(SpawnEffect::new(effect.effect.clone(), at.translation).with_rotation(at.rotation));
            }
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
/// far each wheel has turned. `models` keeps every model entity (also once a wreck has
/// taken the others' place).
#[derive(Component, Clone)]
struct VehicleParts {
    parts: Vec<Entity>,
    outside: Vec<Entity>,
    interior: Vec<Entity>,
    spin: Vec<f32>,
    models: Vec<Entity>,
}

/// How far each track has run, meters: per UV animation and per track wheel of the vehicle,
/// at their side (the tracks run at different speeds while turning), and the heading last
/// frame, for the turn rate.
#[derive(Component, Default)]
struct TrackTravel {
    animations: Vec<f32>,
    wheels: Vec<f32>,
    heading: Option<f32>,
}

/// A primitive whose texture BF2 animates with the track (its own copy of the material),
/// from its material's `bf2.uv_animation` extras.
#[derive(Component)]
struct UvAnimated {
    vehicle: Entity,
    material: Handle<Bf2Material>,
    /// Index into the vehicle's `uv_animations`.
    animation: usize,
    /// Turning: the UV centre and which way is forwards; scrolling: which way per UV axis.
    center: Option<Vec2>,
    spin: f32,
    flow: Vec2,
    /// The texture transform last written to the material (see `scroll_tracks`).
    written: Option<Affine2>,
}

impl UvAnimated {
    /// From a material's extras: the animation's slot in the vehicle's list, the flow, the
    /// centre and the spin.
    fn from_extras(extras: &str, desc: &game_data::VehicleDesc) -> Option<(usize, Vec2, Option<Vec2>, f32)> {
        let value: serde_json::Value = serde_json::from_str(extras).ok()?;
        let animation = &value["bf2"]["uv_animation"];
        let index = animation["index"].as_u64()? as u8;
        let slot = desc.uv_animations.iter().position(|a| a.index == index)?;
        let pair = |v: &serde_json::Value| Some(Vec2::new(v[0].as_f64()? as f32, v[1].as_f64()? as f32));
        Some((
            slot,
            pair(&animation["flow"]).unwrap_or(Vec2::ZERO),
            pair(&animation["center"]),
            animation["spin"].as_f64().unwrap_or(1.0) as f32,
        ))
    }
}

/// A rigged model of a vehicle: one skinned mesh whose joint `n` is `joints[n]` (the part
/// drawing mesh index `n`). Joints no part draws collapse out of sight. `lod`: which level of
/// detail of the model it is, drawn by distance.
#[derive(Component)]
struct VehicleRig {
    gltf: Handle<Gltf>,
    joints: HashMap<u32, Entity>,
    lod: Option<UnitLod>,
}

fn spawn_parts(
    add: On<Add, VehicleData>,
    mut commands: Commands,
    vehicles: Query<&VehicleData>,
    asset_server: Res<AssetServer>,
    config: Res<UnitLodConfig>,
) {
    let Ok(data) = vehicles.get(add.entity) else {
        return;
    };
    let parts = build_parts(&mut commands, add.entity, &data.0, &asset_server, config.enabled);
    commands.entity(add.entity).insert((Visibility::default(), parts));
}

/// Spawns a vehicle's part entities and its models (outside views with their lower LODs if
/// `lods`, interiors).
fn build_parts(
    commands: &mut Commands,
    vehicle: Entity,
    model: &game_shared::vehicle::VehicleModel,
    asset_server: &AssetServer,
    lods: bool,
) -> VehicleParts {
    let desc = &model.desc;
    let mut parts: Vec<Entity> = Vec::with_capacity(desc.parts.len());
    let (mut outside, mut interior) = (Vec::new(), Vec::new());
    for (i, part) in desc.parts.iter().enumerate() {
        // A part's parent must come before it (parts are built in order so each one's parent
        // entity already exists); a vehicle description that doesn't hold that (a forward or
        // self reference, from a bad import or a hand-edited mod) attaches to the vehicle
        // itself instead of indexing past what's been spawned so far.
        let parent = match part.parent {
            Some(p) if (p as usize) < parts.len() => parts[p as usize],
            Some(p) => {
                warn!("{}: part {i}'s parent {p} isn't earlier in the part list; attaching to the vehicle", desc.name);
                vehicle
            }
            None => vehicle,
        };
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
                // Outside views: the model and its lower LODs, each drawn in its distance
                // band, all fading out at the draw distance.
                let levels: Vec<(String, f32)> = match interior_view || !lods {
                    true => vec![(path.clone(), 0.0)],
                    false => std::iter::once((path.clone(), 0.0))
                        .chain(desc.model_lods(&path).iter().map(|l| (l.mesh.clone(), l.distance)))
                        .collect(),
                };
                let starts: Arc<[f32]> = levels.iter().map(|(_, start)| *start).collect();
                for (level, (file, _)) in levels.iter().enumerate() {
                    let rig = VehicleRig {
                        gltf: asset_server.load(format!("imported://{file}")),
                        joints: joints.clone(),
                        lod: (!interior_view).then(|| UnitLod {
                            starts: starts.clone(),
                            draw_distance: if lods { desc.model_draw_distance(&path) } else { None },
                            level,
                        }),
                    };
                    list.push(commands.spawn((Transform::IDENTITY, visibility, rig, ChildOf(vehicle))).id());
                }
            }
        }
    }
    let models = outside.iter().chain(&interior).copied().collect();
    VehicleParts {
        parts,
        outside,
        interior,
        spin: vec![0.0; model.desc.wheels.len()],
        models,
    }
}

/// Once a rig's model has loaded, draws its primitives skinned to the parts (a model that
/// isn't skinned, like a static mesh on a part, simply rides on its part).
#[allow(clippy::too_many_arguments)]
fn spawn_rigs(
    mut commands: Commands,
    rigs: Query<(Entity, &VehicleRig, &ChildOf)>,
    vehicles: Query<&VehicleData>,
    gltfs: Res<Assets<Gltf>>,
    gltf_meshes: Res<Assets<bevy::gltf::GltfMesh>>,
    skins: Res<Assets<bevy::gltf::GltfSkin>>,
    meshes: Res<Assets<Mesh>>,
    mut materials: Bf2Materials,
    asset_server: Res<AssetServer>,
) {
    for (entity, rig, child_of) in &rigs {
        let vehicle = child_of.parent();
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
        // Skinned vertices are wherever the parts put them: a box around everything any part
        // can reach (the farthest part plus the largest part-local extent of the model),
        // so the vehicle is still frustum culled.
        let reach = vehicles
            .get(vehicle)
            .map_or(0.0, |data| data.0.rest_hull.iter().map(|t| t.translation.length()).fold(0.0, f32::max));
        let extent = mesh
            .primitives
            .iter()
            .filter_map(|primitive| meshes.get(&primitive.mesh)?.compute_aabb())
            .map(|aabb| (Vec3::from(aabb.center).abs() + Vec3::from(aabb.half_extents)).length())
            .fold(0.0, f32::max);
        let bounds = Aabb::from_min_max(Vec3::splat(-(reach + extent)), Vec3::splat(reach + extent));
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
            // Track faces scroll on a material of their own.
            let animated = vehicles.get(vehicle).ok().and_then(|data| {
                let extras = primitive.material_extras.as_ref()?;
                UvAnimated::from_extras(&extras.value, &data.0.desc)
            });
            let (material, animated) = match animated {
                Some((animation, flow, center, spin)) => {
                    let material = materials.duplicate(&material);
                    let animated = UvAnimated {
                        vehicle,
                        material: material.clone(),
                        animation,
                        center,
                        spin,
                        flow,
                        written: None,
                    };
                    (material, Some(animated))
                }
                None => (material, None),
            };
            let spawned = match (&skinned, rigged) {
                (Some(skinned), true) => {
                    let mut mesh = commands.spawn((
                        Mesh3d(primitive.mesh.clone()),
                        MeshMaterial3d(material),
                        skinned.clone(),
                        bounds,
                        ChildOf(entity),
                    ));
                    if let Some(animated) = animated {
                        mesh.insert(animated);
                    }
                    mesh.id()
                }
                _ => {
                    let part = rig.joints.get(&0).copied().unwrap_or(entity);
                    commands.spawn((Mesh3d(primitive.mesh.clone()), MeshMaterial3d(material), ChildOf(part))).id()
                }
            };
            if let Some(lod) = &rig.lod {
                commands.entity(spawned).insert(lod.clone());
            }
        }
    }
}

/// Runs each track on by what its side travelled this frame: the vehicle's speed plus its
/// turn rate times the side's distance from the middle.
fn run_tracks(
    mut commands: Commands,
    time: Res<Time>,
    mut vehicles: Query<(Entity, &VehicleView, &VehicleData, Option<&mut TrackTravel>)>,
) {
    let dt = time.delta_secs();
    for (entity, view, data, travel) in &mut vehicles {
        let desc = &data.0.desc;
        if desc.uv_animations.is_empty() && desc.track_wheels.is_empty() {
            continue;
        }
        let Some(mut travel) = travel else {
            commands.entity(entity).insert(TrackTravel {
                animations: vec![0.0; desc.uv_animations.len()],
                wheels: vec![0.0; desc.track_wheels.len()],
                heading: None,
            });
            continue;
        };
        let heading = crate::vehicles::heading(view.transform.rotation);
        let turn = travel.heading.replace(heading).map_or(0.0, |before| {
            let d = (heading - before + std::f32::consts::PI).rem_euclid(std::f32::consts::TAU) - std::f32::consts::PI;
            if dt > 0.0 { d / dt } else { 0.0 }
        });
        let run = |side: f32| (view.speed + turn * side) * dt;
        for (distance, animation) in travel.animations.iter_mut().zip(&desc.uv_animations) {
            *distance += run(animation.side);
        }
        for (distance, wheel) in travel.wheels.iter_mut().zip(&desc.track_wheels) {
            *distance += run(wheel.side);
        }
    }
}

/// A track texture is written again once it has moved this far (share of a texture repeat;
/// about the same in radians for turning hubs): every write re-uploads the material's whole
/// bindless slab, so parked tanks (their speed jitters a little) mustn't write every frame.
const UV_WRITE_STEP: f32 = 1.0 / 512.0;
/// Farther than this from the camera (m), track textures move every other frame.
const UV_NEAR: f32 = 40.0;

/// Moves the track textures to where their track has run, like BF2's UV matrices: belts and
/// wheel rims scroll (wrapping where their texture repeats), hubs turn about their centres.
/// Only tracks in view, and only once they've visibly moved (see `UV_WRITE_STEP`).
fn scroll_tracks(
    mut frame: Local<u32>,
    camera: Query<&GlobalTransform, With<crate::camera::PlayerCamera>>,
    mut animated: Query<(Entity, &mut UvAnimated, &ViewVisibility, &GlobalTransform)>,
    vehicles: Query<(&VehicleData, &TrackTravel)>,
    mut materials: ResMut<Assets<Bf2Material>>,
) {
    *frame = frame.wrapping_add(1);
    let eye = camera.single().ok().map(GlobalTransform::translation);
    for (entity, mut animated, visibility, placed) in &mut animated {
        if !visibility.get() {
            continue;
        }
        let far = eye.is_some_and(|eye| placed.translation().distance_squared(eye) > UV_NEAR * UV_NEAR);
        if far && (*frame ^ entity.to_bits() as u32) & 1 == 1 {
            continue;
        }
        let Ok((data, travel)) = vehicles.get(animated.vehicle) else {
            continue;
        };
        let (Some(desc), Some(&distance)) = (
            data.0.desc.uv_animations.get(animated.animation),
            travel.animations.get(animated.animation),
        ) else {
            continue;
        };
        let transform = match (&desc.motion, animated.center) {
            (game_data::UvMotion::Scroll { size, wrap }, None) => {
                // Always forwards within one repeat of the texture: BF2's strips repeat past
                // the faces' UV, not before it (the same motion as moving backwards).
                let axis = |i: usize| {
                    if size[i] > 0.0 && wrap[i] > 0.0 {
                        (animated.flow[i] * distance / size[i]).rem_euclid(wrap[i])
                    } else {
                        0.0
                    }
                };
                Affine2::from_translation(Vec2::new(axis(0), axis(1)))
            }
            (game_data::UvMotion::Spin { radius, scale }, Some(center)) => {
                let angle = -animated.spin * distance / radius.max(0.05);
                let scale = Vec2::from_array(*scale);
                Affine2::from_translation(center)
                    * Affine2::from_scale(scale)
                    * Affine2::from_angle(angle)
                    * Affine2::from_scale(scale.recip())
                    * Affine2::from_translation(-center)
            }
            _ => continue,
        };
        if animated.written.is_some_and(|written| written.abs_diff_eq(transform, UV_WRITE_STEP)) {
            continue;
        }
        if let Some(mut material) = materials.get_mut(&animated.material) {
            material.base.uv_transform = transform;
        }
        animated.written = Some(transform);
    }
}

fn pose_parts(
    time: Res<Time>,
    mut vehicles: Query<(&VehicleView, &VehicleData, &mut VehicleParts, Option<&TrackTravel>)>,
    mut transforms: Query<&mut Transform>,
    children: Query<&Children>,
    seen: Query<&ViewVisibility>,
    mut poses: Local<Vec<Transform>>,
) {
    let dt = time.delta_secs();
    for (view, data, mut parts, travel) in &mut vehicles {
        let model = &data.0;
        let VehicleParts { parts, spin, models, .. } = &mut *parts;
        // Wheels keep turning while nobody looks, so they don't jump when seen again.
        for (wi, wheel) in model.desc.wheels.iter().enumerate() {
            if wheel.turns {
                spin[wi] = (spin[wi] - view.speed / wheel.radius.max(0.1) * dt) % std::f32::consts::TAU;
            }
        }
        // Posing moves every part and the meshes on them (transforms to propagate, meshes to
        // upload): skip vehicles none of whose meshes were seen last frame (in any view,
        // shadows included), like `soldiers::sleep_unseen`.
        let is_seen = |entity: Entity| seen.get(entity).is_ok_and(|v| v.get());
        // Meshes hang off the model entities (skinned rigs, static meshes on parts) or, when
        // a rigged model's primitive isn't skinned, straight off its part.
        let visible = models
            .iter()
            .chain(parts.iter())
            .any(|&m| is_seen(m) || children.get(m).is_ok_and(|c| c.iter().any(is_seen)));
        if !visible {
            continue;
        }
        // Each part's pose in full, written once: writing the rest pose and then turning the
        // wheels on it would change them (and everything on them) every frame.
        poses.clear();
        poses.extend((0..parts.len()).map(|i| {
            let mut local = model.rest[i];
            if let Some(angles) = model.joint_index[i].and_then(|j| view.joints.get(j)) {
                local.rotation *= joint_rotation(*angles);
            }
            local
        }));
        for (wi, wheel) in model.desc.wheels.iter().enumerate() {
            let offset = view.wheels.get(wi).copied().unwrap_or(0.0);
            if let Some(pose) = poses.get_mut(wheel.part as usize) {
                pose.translation.y -= offset;
                pose.rotation *= Quat::from_rotation_x(spin[wi]);
            }
        }
        // Drive sprockets turn with their track.
        for (wheel, distance) in model.desc.track_wheels.iter().zip(travel.map_or(&[][..], |t| t.wheels.as_slice())) {
            if let Some(pose) = poses.get_mut(wheel.part as usize) {
                let angle = (-distance / wheel.radius.max(0.05)) % std::f32::consts::TAU;
                pose.rotation *= Quat::from_rotation_x(angle);
            }
        }
        for (&entity, pose) in parts.iter().zip(poses.iter()) {
            if let Ok(mut transform) = transforms.get_mut(entity) {
                transform.set_if_neq(*pose);
            }
        }
    }
}
