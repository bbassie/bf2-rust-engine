//! First-person view model: BF2's first-person arms and weapon, animated with the weapon's
//! first-person set, drawn by a second camera so it never clips into walls.

use bevy::{
    anti_alias::smaa::Smaa,
    app::AnimationSystems,
    camera::visibility::RenderLayers,
    gltf::{Gltf, GltfMesh},
    light::NotShadowCaster,
    platform::collections::HashMap,
    prelude::*,
    world_serialization::WorldInstanceReady,
};
use game_data::SoldierDesc;
use avian3d::prelude::{SpatialQuery, SpatialQueryFilter};
use game_shared::{
    config::GamePaths,
    physics::GameLayer,
    level::LoadedLevel,
    protocol::Team,
    soldier::SoldierMotion,
    weapons::{Armory, Inventory, Loadout},
};

use super::environment::Sun;
use crate::{
    camera::{PlayerCamera, ThirdPerson},
    combat::CombatFeedback,
    local_input::InputHistory,
    net::{LocalPlayer, LocalSoldier},
};

/// Render layer of everything in the view model.
pub const VIEW_MODEL_LAYER: usize = 1;

pub struct ViewModelPlugin;

impl Plugin for ViewModelPlugin {
    fn build(&self, app: &mut App) {
        app.init_resource::<ViewModelAssets>()
            .add_observer(add_view_model_camera)
            .add_systems(
                Update,
                (
                    load_arms.run_if(resource_exists_and_changed::<LoadedLevel>),
                    attach_arms,
                    attach_weapon,
                    animate_view_model,
                )
                    .chain(),
            )
            .add_systems(Update, (light_view_model, match_main_camera_msaa))
            .add_systems(
                PostUpdate,
                align_to_camera_bone
                    .after(AnimationSystems)
                    .before(TransformSystems::Propagate),
            );
    }
}

#[derive(Resource, Default)]
struct ViewModelAssets {
    /// First-person arms per team.
    arms: [Option<Handle<Gltf>>; 2],
    /// Weapon first-person animation sets by path.
    sets: HashMap<String, Handle<Gltf>>,
    graphs: HashMap<String, ViewAnimations>,
}

#[derive(Clone)]
struct ViewAnimations {
    graph: Handle<AnimationGraph>,
    clips: HashMap<String, AnimationNodeIndex>,
}

#[derive(Component)]
struct ViewModelCamera;

/// The arms scene under the view model camera.
#[derive(Component)]
struct ViewModelRoot {
    team: usize,
}

/// Found in the arms scene once it spawned.
#[derive(Component)]
struct ViewRig {
    player: Entity,
    camera_bone: Entity,
    bones: [Option<Entity>; 16],
}

#[derive(Component, Default)]
struct ViewState {
    weapon: String,
    parts: Vec<Entity>,
    parts_ready: bool,
    weapon_gltf: Option<Handle<Gltf>>,
    /// Weapon and animation set the animations are for.
    animated_weapon: String,
    set: String,
    base: String,
    /// One-shot clip playing (fire, reload, deploy) and its node.
    one_shot: Option<AnimationNodeIndex>,
    shots_seen: u32,
    was_reloading: bool,
}

fn add_view_model_camera(add: On<Add, PlayerCamera>, mut commands: Commands) {
    commands.spawn((
        ViewModelCamera,
        Camera3d::default(),
        Camera {
            order: 1,
            clear_color: ClearColorConfig::None,
            ..default()
        },
        Projection::from(PerspectiveProjection {
            fov: 60f32.to_radians(),
            near: 0.01,
            far: 10.0,
            ..default()
        }),
        RenderLayers::layer(VIEW_MODEL_LAYER),
        ChildOf(add.entity),
    ));
}

/// Both cameras draw into one image, and Bevy only shares a camera's intermediate textures
/// with cameras of the same MSAA setting (otherwise this camera would draw over a stale
/// copy of the world). Without MSAA (for SSAO) this camera renders last, so its SMAA pass
/// smooths the world and the weapon at once.
fn match_main_camera_msaa(
    mut commands: Commands,
    main: Query<&Msaa, (With<PlayerCamera>, Changed<Msaa>)>,
    view_camera: Query<Entity, With<ViewModelCamera>>,
) {
    let (Ok(msaa), Ok(view_camera)) = (main.single(), view_camera.single()) else {
        return;
    };
    let mut view_camera = commands.entity(view_camera);
    view_camera.insert(*msaa);
    if *msaa == Msaa::Off {
        view_camera.insert(Smaa::default());
    } else {
        view_camera.remove::<Smaa>();
    }
}

fn load_arms(
    level: Res<LoadedLevel>,
    paths: Res<GamePaths>,
    asset_server: Res<AssetServer>,
    mut assets: ResMut<ViewModelAssets>,
) {
    for (index, slot) in assets.arms.iter_mut().enumerate() {
        *slot = level
            .desc
            .teams
            .get(index)
            .and_then(|team| team.kits.first())
            .and_then(|kit| {
                let path = paths.imported.join("soldiers").join(format!("{}.ron", kit.soldier));
                game_data::read_ron::<SoldierDesc>(&path).ok()
            })
            .and_then(|desc| desc.mesh_1p)
            .map(|path| asset_server.load(format!("imported://{path}")));
    }
}

/// Spawns the local team's arms under the view model camera.
#[allow(clippy::type_complexity)]
fn attach_arms(
    mut commands: Commands,
    assets: Res<ViewModelAssets>,
    gltfs: Res<Assets<Gltf>>,
    camera: Query<Entity, With<ViewModelCamera>>,
    local: Query<&Team, With<LocalPlayer>>,
    roots: Query<(Entity, &ViewModelRoot)>,
) {
    let (Ok(camera), Ok(team)) = (camera.single(), local.single()) else {
        return;
    };
    let team = match team {
        Team::One => 0,
        Team::Two => 1,
        Team::Spectator => return,
    };
    if let Ok((entity, root)) = roots.single() {
        if root.team == team {
            return;
        }
        commands.entity(entity).despawn();
    }
    let Some(scene) = assets.arms[team].as_ref().and_then(|h| gltfs.get(h)).and_then(|g| g.default_scene.clone()) else {
        return;
    };
    commands
        .spawn((
            ViewModelRoot { team },
            ViewState::default(),
            WorldAssetRoot(scene),
            Visibility::Hidden,
            ChildOf(camera),
        ))
        .observe(
            |ready: On<WorldInstanceReady>,
             mut commands: Commands,
             children: Query<&Children>,
             players: Query<(), With<AnimationPlayer>>,
             meshes: Query<(), With<Mesh3d>>,
             names: Query<&Name>| {
                let mut player = None;
                let mut camera_bone = None;
                let mut bones = [None; 16];
                for entity in children.iter_descendants(ready.entity) {
                    if players.contains(entity) {
                        player = Some(entity);
                    }
                    if meshes.contains(entity) {
                        commands
                            .entity(entity)
                            .insert((RenderLayers::layer(VIEW_MODEL_LAYER), NotShadowCaster));
                    }
                    let Ok(name) = names.get(entity) else { continue };
                    if name.as_str().eq_ignore_ascii_case("camerabone") {
                        camera_bone = Some(entity);
                    } else if let Some(n) = name.as_str().strip_prefix("mesh").and_then(|n| n.parse::<usize>().ok())
                        && (1..=16).contains(&n)
                    {
                        bones[n - 1] = Some(entity);
                    }
                }
                if let (Some(player), Some(camera_bone)) = (player, camera_bone) {
                    commands.entity(ready.entity).insert(ViewRig {
                        player,
                        camera_bone,
                        bones,
                    });
                }
            },
        );
}

fn local_weapon<'a>(armory: &'a Armory, loadout: &Loadout, inventory: &Inventory) -> Option<&'a game_data::WeaponDesc> {
    armory
        .weapon(loadout.weapons.get(inventory.active as usize)?)
        .map(|w| w.as_ref())
}

/// Puts the active weapon's first-person parts on the arms' weapon bones.
#[allow(clippy::type_complexity)]
fn attach_weapon(
    mut commands: Commands,
    armory: Res<Armory>,
    asset_server: Res<AssetServer>,
    gltfs: Res<Assets<Gltf>>,
    gltf_meshes: Res<Assets<GltfMesh>>,
    soldier: Query<(&Loadout, &Inventory), With<LocalSoldier>>,
    mut roots: Query<(&ViewRig, &mut ViewState)>,
) {
    let (Ok((loadout, inventory)), Ok((rig, mut state))) = (soldier.single(), roots.single_mut()) else {
        return;
    };
    let Some(weapon) = local_weapon(&armory, loadout, inventory) else {
        return;
    };
    if state.weapon != weapon.name {
        for part in state.parts.drain(..) {
            commands.entity(part).try_despawn();
        }
        state.weapon = weapon.name.clone();
        state.parts_ready = false;
        state.weapon_gltf = weapon
            .mesh_1p
            .as_ref()
            .map(|path| asset_server.load(format!("imported://{path}")));
    }
    if state.parts_ready {
        return;
    }
    let Some(handle) = &state.weapon_gltf else {
        state.parts_ready = true;
        return;
    };
    let Some(gltf) = gltfs.get(handle) else {
        return;
    };
    for (index, mesh) in gltf.meshes.iter().enumerate() {
        let (Some(bone), Some(mesh)) = (rig.bones.get(index).copied().flatten(), gltf_meshes.get(mesh)) else {
            continue;
        };
        for primitive in &mesh.primitives {
            let material: Handle<StandardMaterial> = primitive
                .material
                .as_ref()
                .and_then(|m| m.path())
                .and_then(|path| {
                    let label = format!("{}/std", path.label()?);
                    Some(asset_server.load(path.clone().with_label(label)))
                })
                .unwrap_or_default();
            let part = commands
                .spawn((
                    Mesh3d(primitive.mesh.clone()),
                    MeshMaterial3d(material),
                    RenderLayers::layer(VIEW_MODEL_LAYER),
                    NotShadowCaster,
                    ChildOf(bone),
                ))
                .id();
            state.parts.push(part);
        }
    }
    state.parts_ready = true;
}

fn view_animations(
    assets: &mut ViewModelAssets,
    set: &str,
    asset_server: &AssetServer,
    gltfs: &Assets<Gltf>,
    graphs: &mut Assets<AnimationGraph>,
) -> Option<ViewAnimations> {
    if let Some(found) = assets.graphs.get(set) {
        return Some(found.clone());
    }
    let handle = assets
        .sets
        .entry(set.to_string())
        .or_insert_with(|| asset_server.load(format!("imported://{set}")))
        .clone();
    let gltf = gltfs.get(&handle)?;
    let mut graph = AnimationGraph::new();
    let clips = gltf
        .named_animations
        .iter()
        .map(|(name, clip)| (name.to_string(), graph.add_clip(clip.clone(), 1.0, graph.root)))
        .collect();
    let animations = ViewAnimations {
        graph: graphs.add(graph),
        clips,
    };
    assets.graphs.insert(set.to_string(), animations.clone());
    Some(animations)
}

#[allow(clippy::too_many_arguments, clippy::type_complexity)]
fn animate_view_model(
    mut commands: Commands,
    mut assets: ResMut<ViewModelAssets>,
    armory: Res<Armory>,
    asset_server: Res<AssetServer>,
    gltfs: Res<Assets<Gltf>>,
    mut graphs: ResMut<Assets<AnimationGraph>>,
    feedback: Res<CombatFeedback>,
    history: Res<InputHistory>,
    third_person: Res<ThirdPerson>,
    soldier: Query<(&Loadout, &Inventory, &SoldierMotion), (With<LocalSoldier>, Without<game_shared::vehicle::Seated>)>,
    mut roots: Query<(&ViewRig, &mut ViewState, &mut Visibility)>,
    mut players: Query<&mut AnimationPlayer>,
) {
    let Ok((rig, mut state, mut visibility)) = roots.single_mut() else {
        return;
    };
    let Ok((loadout, inventory, motion)) = soldier.single() else {
        visibility.set_if_neq(Visibility::Hidden);
        return;
    };
    let weapon = local_weapon(&armory, loadout, inventory);
    let set = weapon.and_then(|w| w.animations_1p.clone());
    let (Some(weapon), Some(set)) = (weapon, set) else {
        // Nothing to hold (or no first-person animations for it).
        visibility.set_if_neq(Visibility::Hidden);
        return;
    };
    visibility.set_if_neq(if third_person.0 { Visibility::Hidden } else { Visibility::Inherited });
    let Some(animations) = view_animations(&mut assets, &set, &asset_server, &gltfs, &mut graphs) else {
        return;
    };
    let Ok(mut player) = players.get_mut(rig.player) else {
        return;
    };
    let clip = |name: &str| animations.clips.get(name).copied();

    let mut one_shot = None;
    if state.set != set {
        commands.entity(rig.player).insert(AnimationGraphHandle(animations.graph.clone()));
        state.set = set;
    }
    if state.animated_weapon != weapon.name {
        state.animated_weapon = weapon.name.clone();
        state.shots_seen = feedback.shots_fired;
        state.was_reloading = false;
        one_shot = clip("deploy");
    }
    let input = history.latest().copied().unwrap_or_default();
    let zoomed = input.pressed(game_shared::input::Buttons::AIM);
    if feedback.shots_fired != state.shots_seen {
        state.shots_seen = feedback.shots_fired;
        one_shot = if zoomed { clip("zoom_fire").or(clip("fire")) } else { clip("fire") };
    }
    if feedback.reloading && !state.was_reloading {
        one_shot = clip("reload");
    }
    state.was_reloading = feedback.reloading;

    if let Some(node) = one_shot {
        player.stop_all();
        player.play(node);
        state.one_shot = Some(node);
        state.base.clear();
        return;
    }
    if let Some(node) = state.one_shot {
        if !player.animation(node).is_none_or(|a| a.is_finished()) {
            return;
        }
        state.one_shot = None;
    }

    let speed = Vec2::new(motion.velocity.x, motion.velocity.z).length();
    let base = match (zoomed, speed > 5.0, speed > 0.5) {
        (true, _, true) => "zoom_run",
        (true, _, false) => "zoom_stand",
        (false, true, _) => "sprint",
        (false, false, true) => "run",
        _ => "stand",
    };
    if state.base != base {
        let node = clip(base).or_else(|| clip("stand"));
        if let Some(node) = node {
            player.stop_all();
            player.play(node).repeat();
        }
        state.base = base.to_string();
    }
}

/// Keeps BF2's camera bone at the eye: the arms move so `Camerabone` sits at the camera.
fn align_to_camera_bone(
    mut roots: Query<(&ViewRig, &mut Transform), With<ViewModelRoot>>,
    bones: Query<&Transform, Without<ViewModelRoot>>,
    parents: Query<&ChildOf>,
) {
    for (rig, mut root_transform) in &mut roots {
        // Camera bone relative to the scene root (through the `firstperson` node).
        let mut relative = Transform::IDENTITY;
        let mut entity = rig.camera_bone;
        loop {
            let Ok(local) = bones.get(entity) else { break };
            relative = *local * relative;
            let Ok(parent) = parents.get(entity) else { break };
            if bones.get(parent.parent()).is_err() {
                break;
            }
            entity = parent.parent();
        }
        let inverse = Transform::from_matrix(relative.to_matrix().inverse());
        *root_transform = inverse;
    }
}

/// Sunlight for the view model. The level's sun only lights the world layer (so the view
/// model camera needs no shadow maps of its own); this one follows it without shadows and
/// fades out while the camera is in the shade.
#[derive(Component)]
struct ViewModelSun {
    visibility: f32,
}

#[allow(clippy::type_complexity)]
fn light_view_model(
    mut commands: Commands,
    time: Res<Time>,
    spatial: SpatialQuery,
    sun: Query<(&DirectionalLight, &Transform), (With<Sun>, Without<ViewModelSun>)>,
    camera: Query<&GlobalTransform, With<PlayerCamera>>,
    mut lights: Query<(Entity, &mut DirectionalLight, &mut Transform, &mut ViewModelSun)>,
) {
    let Ok((sun_light, sun_transform)) = sun.single() else {
        for (entity, ..) in &lights {
            commands.entity(entity).despawn();
        }
        return;
    };
    let Ok((_, mut light, mut transform, mut state)) = lights.single_mut() else {
        commands.spawn((
            ViewModelSun { visibility: 1.0 },
            DirectionalLight {
                shadow_maps_enabled: false,
                ..sun_light.clone()
            },
            *sun_transform,
            RenderLayers::layer(VIEW_MODEL_LAYER),
        ));
        return;
    };
    let lit = camera.single().map_or(true, |camera| {
        let filter = SpatialQueryFilter::from_mask(GameLayer::World);
        spatial
            .cast_ray(camera.translation(), -sun_transform.forward(), 400.0, true, &filter)
            .is_none()
    });
    let target = if lit { 1.0 } else { 0.0 };
    state.visibility += (target - state.visibility) * (1.0 - (-8.0 * time.delta_secs()).exp());
    light.illuminance = sun_light.illuminance * state.visibility;
    light.color = sun_light.color;
    *transform = *sun_transform;
}
