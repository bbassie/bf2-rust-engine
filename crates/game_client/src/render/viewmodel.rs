//! First-person view model: BF2's first-person arms and weapon, animated with the weapon's
//! first-person set.
//!
//! It is drawn by the player camera, shrunk towards the eye so it never clips into walls: a
//! uniform scale about the eye changes nothing on screen, only the depth. The model hangs
//! under a [`ViewModelAnchor`] on the camera whose scale is [`VIEW_MODEL_SHRINK`], and in x
//! and y also the ratio of the camera's field of view to BF2's first-person one
//! ([`VIEW_MODEL_FOV`]), so it looks as if a 60° camera drew it at any field of view and
//! zoom. A second camera cost about 1.4 ms of the render thread and 0.5 ms of the main
//! thread a frame (its own view, passes and post-processing). The model is lit like the
//! world, sun shadows included; `bf2_material.wgsl` keeps screen-space ambient occlusion off
//! it (materials flagged [`Bf2Layers::VIEW_MODEL`]).

use bevy::{
    app::AnimationSystems,
    camera::visibility::RenderLayers,
    gltf::{Gltf, GltfMesh},
    light::NotShadowCaster,
    platform::collections::HashMap,
    prelude::*,
    world_serialization::WorldInstanceReady,
};
use game_data::SoldierDesc;
use game_shared::{
    config::GamePaths,
    level::LoadedLevel,
    protocol::Team,
    soldier::SoldierMotion,
    weapons::{Armory, Inventory, Loadout},
};

use super::{
    blend::{BlendLayer, Clip, Play},
    materials::{Bf2Layers, Bf2Material, Bf2Materials},
    scope::Zoom,
};
use crate::{
    camera::{PlayerCamera, ThirdPerson},
    combat::CombatFeedback,
    net::{LocalPlayer, LocalSoldier},
};

/// Render layer of everything in the view model (the player camera draws it too; the sun's
/// shadow maps, on layer 0, leave it out).
pub const VIEW_MODEL_LAYER: usize = 1;

/// How far towards the eye the view model is pulled (a scale about the eye): a rifle reaching
/// 0.8 m ahead ends at 0.16 m, closer than any wall the soldier can stand at.
pub const VIEW_MODEL_SHRINK: f32 = 0.2;

/// Field of view BF2's first-person models are drawn with.
pub const VIEW_MODEL_FOV: f32 = 60.0;

/// The near plane the first-person models were made for (0.01 m), shrunk like them: the
/// player camera's near plane.
pub const CAMERA_NEAR: f32 = 0.01 * VIEW_MODEL_SHRINK;

/// On the player camera: what the view model hangs from. Its scale (camera space) is
/// [`ViewModelSpace::scale`].
#[derive(Component)]
pub struct ViewModelAnchor;

/// The view model's scale in camera space (see [`ViewModelAnchor`]): first-person effects
/// are placed with it.
#[derive(Resource, Clone, Copy, Debug)]
pub struct ViewModelSpace {
    pub scale: Vec3,
}

impl Default for ViewModelSpace {
    fn default() -> Self {
        Self {
            scale: Vec3::splat(VIEW_MODEL_SHRINK),
        }
    }
}

impl ViewModelSpace {
    /// `world`, a point of the full-size view model (as if drawn by its own camera at
    /// `camera`), where the shrunk one draws it.
    pub fn shrink(&self, camera: &Transform, world: Vec3) -> Vec3 {
        let local = camera.rotation.inverse() * (world - camera.translation);
        camera.translation + camera.rotation * (local * self.scale)
    }

    /// The inverse of [`Self::shrink`].
    pub fn grow(&self, camera: &Transform, shrunk: Vec3) -> Vec3 {
        let local = camera.rotation.inverse() * (shrunk - camera.translation);
        camera.translation + camera.rotation * (local / self.scale)
    }
}

/// A mesh of the view model (its material is flagged [`Bf2Layers::VIEW_MODEL`]).
#[derive(Component)]
pub struct ViewModelMesh;

pub struct ViewModelPlugin;

impl Plugin for ViewModelPlugin {
    fn build(&self, app: &mut App) {
        app.init_resource::<ViewModelAssets>()
            .init_resource::<ViewModelSpace>()
            .add_observer(add_view_model_anchor)
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
            .add_systems(Update, flag_view_model_materials)
            .add_systems(
                PostUpdate,
                (
                    align_to_camera_bone.after(AnimationSystems),
                    scale_anchor.after(crate::camera::CameraSystems),
                )
                    .before(TransformSystems::Propagate),
            );
    }
}

#[derive(Resource, Default)]
struct ViewModelAssets {
    /// First-person arms by soldier body name (see `render::soldiers::kit_body`): a faction's
    /// heavy and light bodies have different arms, so this follows the local player's actual
    /// kit rather than assuming the team's first kit slot.
    arms: HashMap<String, Handle<Gltf>>,
    /// Weapon first-person animation sets by path.
    sets: HashMap<String, Handle<Gltf>>,
    graphs: HashMap<String, ViewAnimations>,
}

#[derive(Clone)]
struct ViewAnimations {
    graph: Handle<AnimationGraph>,
    clips: HashMap<String, Clip>,
}

/// The arms scene under the view model anchor.
#[derive(Component)]
struct ViewModelRoot {
    /// Soldier body name the arms are for (see `render::soldiers::kit_body`).
    body: String,
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
    /// Loop playing when no one-shot is (stand, run, zoom_stand...).
    base: &'static str,
    /// One-shot playing (fire, reload, deploy) and one queued after it (the bolt after a
    /// bolt-action rifle's shot).
    one_shot: Option<OneShot>,
    next: Option<OneShot>,
    layer: BlendLayer,
    shots_seen: u32,
    was_reloading: bool,
    was_cooking: bool,
}

#[derive(Clone, Copy)]
struct OneShot {
    clip: Clip,
    speed: f32,
    /// Crossfade into what follows it.
    fade_out: f32,
}

impl OneShot {
    fn new(clip: Clip, fade_out: f32) -> Self {
        Self {
            clip,
            speed: 1.0,
            fade_out,
        }
    }
}

fn add_view_model_anchor(add: On<Add, PlayerCamera>, mut commands: Commands) {
    commands.spawn((
        ViewModelAnchor,
        Transform::from_scale(Vec3::splat(VIEW_MODEL_SHRINK)),
        Visibility::default(),
        ChildOf(add.entity),
    ));
}

/// Scales the anchor for the camera's field of view (zooming changes it): x and y by
/// `tan(fov / 2) / tan(60° / 2)` on top of the shrink, so the model covers the screen as it
/// does at BF2's first-person field of view.
fn scale_anchor(
    camera: Query<&Projection, With<PlayerCamera>>,
    mut anchors: Query<&mut Transform, With<ViewModelAnchor>>,
    mut space: ResMut<ViewModelSpace>,
) {
    let Ok(Projection::Perspective(perspective)) = camera.single() else {
        return;
    };
    let ratio = (perspective.fov * 0.5).tan() / (VIEW_MODEL_FOV.to_radians() * 0.5).tan();
    let scale = Vec3::new(ratio, ratio, 1.0) * VIEW_MODEL_SHRINK;
    if space.scale != scale {
        space.scale = scale;
    }
    for mut transform in &mut anchors {
        if transform.scale != scale {
            transform.scale = scale;
        }
    }
}

/// Keeps screen-space ambient occlusion off the view model (its own camera had none): the
/// shrunk model is a tiny object right at the eye to it. Also after a level change rebuilt
/// the materials.
fn flag_view_model_materials(
    meshes: Query<&MeshMaterial3d<Bf2Material>, With<ViewModelMesh>>,
    mut materials: ResMut<Assets<Bf2Material>>,
) {
    for material in &meshes {
        if materials.get(&material.0).is_some_and(|m| m.extension.flags & Bf2Layers::VIEW_MODEL == 0)
            && let Some(mut material) = materials.get_mut(&material.0)
        {
            material.extension.flags |= Bf2Layers::VIEW_MODEL;
        }
    }
}

fn load_arms(level: Res<LoadedLevel>, paths: Res<GamePaths>, asset_server: Res<AssetServer>, mut assets: ResMut<ViewModelAssets>) {
    assets.arms.clear();
    let mut names: Vec<&str> = level
        .desc
        .teams
        .iter()
        .flat_map(|team| &team.kits)
        .map(|kit| kit.soldier.as_str())
        .filter(|s| !s.is_empty())
        .collect();
    names.sort_unstable();
    names.dedup();
    for name in names {
        let Some(path) = paths
            .read_ron::<SoldierDesc>(format!("soldiers/{name}.ron"))
            .ok()
            .and_then(|desc| desc.mesh_1p)
        else {
            continue;
        };
        assets.arms.insert(name.to_string(), asset_server.load(format!("imported://{path}")));
    }
}

/// Spawns the local team's arms under the view model anchor.
#[allow(clippy::type_complexity)]
#[allow(clippy::type_complexity)]
fn attach_arms(
    mut commands: Commands,
    assets: Res<ViewModelAssets>,
    gltfs: Res<Assets<Gltf>>,
    level: Option<Res<LoadedLevel>>,
    camera: Query<Entity, With<ViewModelAnchor>>,
    local_team: Query<&Team, With<LocalPlayer>>,
    local_soldier: Query<Option<&Loadout>, With<LocalSoldier>>,
    roots: Query<(Entity, &ViewModelRoot)>,
) {
    let (Ok(camera), Ok(team), Some(level)) = (camera.single(), local_team.single(), level.as_deref()) else {
        return;
    };
    let team_index = match team {
        Team::One => 0,
        Team::Two => 1,
        Team::Spectator => return,
    };
    let kit = local_soldier.single().ok().flatten().map(|l| l.kit.as_str());
    let Some(body) = super::soldiers::kit_body(level, team_index, kit) else {
        return;
    };
    if let Ok((entity, root)) = roots.single() {
        if root.body == body {
            return;
        }
        commands.entity(entity).despawn();
    }
    let Some(scene) = assets.arms.get(&body).and_then(|h| gltfs.get(h)).and_then(|g| g.default_scene.clone()) else {
        return;
    };
    commands
        .spawn((
            ViewModelRoot { body },
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
                            .insert((RenderLayers::layer(VIEW_MODEL_LAYER), NotShadowCaster, ViewModelMesh));
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

/// What the hands hold: the weapon, or the C4 detonator while it is out. Returns a name to
/// tell them apart, the first-person model and the animation set.
fn held<'a>(weapon: &'a game_data::WeaponDesc, detonator: bool) -> (String, Option<&'a String>, Option<&'a String>) {
    match weapon.detonator.as_ref().filter(|_| detonator) {
        Some(d) => (format!("{}/detonator", weapon.name), d.mesh_1p.as_ref(), d.animations_1p.as_ref()),
        None => (weapon.name.clone(), weapon.mesh_1p.as_ref(), weapon.animations_1p.as_ref()),
    }
}

/// Puts the active weapon's first-person parts on the arms' weapon bones.
#[allow(clippy::type_complexity)]
fn attach_weapon(
    mut commands: Commands,
    armory: Res<Armory>,
    asset_server: Res<AssetServer>,
    gltfs: Res<Assets<Gltf>>,
    gltf_meshes: Res<Assets<GltfMesh>>,
    mut materials: Bf2Materials,
    feedback: Res<CombatFeedback>,
    soldier: Query<(&Loadout, &Inventory), With<LocalSoldier>>,
    mut roots: Query<(&ViewRig, &mut ViewState)>,
) {
    let (Ok((loadout, inventory)), Ok((rig, mut state))) = (soldier.single(), roots.single_mut()) else {
        return;
    };
    let Some(weapon) = local_weapon(&armory, loadout, inventory) else {
        return;
    };
    let (held, mesh_1p, _) = held(weapon, feedback.detonator);
    if state.weapon != held {
        for part in state.parts.drain(..) {
            commands.entity(part).try_despawn();
        }
        state.weapon = held;
        state.parts_ready = false;
        state.weapon_gltf = mesh_1p.map(|path| asset_server.load(format!("imported://{path}")));
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
    let meshes: Vec<_> = gltf
        .meshes
        .iter()
        .enumerate()
        .filter_map(|(index, mesh)| Some((index, rig.bones.get(index).copied().flatten()?, gltf_meshes.get(mesh)?)))
        .collect();
    // Wait until the glTF's materials are ready.
    let Some(part_materials) = meshes
        .iter()
        .flat_map(|(_, _, mesh)| &mesh.primitives)
        .map(|primitive| materials.for_primitive(primitive))
        .collect::<Option<Vec<_>>>()
    else {
        return;
    };
    let mut part_materials = part_materials.into_iter();
    for (index, bone, mesh) in meshes {
        for (primitive, material) in mesh.primitives.iter().zip(part_materials.by_ref()) {
            let part = commands
                .spawn((
                    super::scope::WeaponPart(index),
                    Mesh3d(primitive.mesh.clone()),
                    MeshMaterial3d(material),
                    RenderLayers::layer(VIEW_MODEL_LAYER),
                    NotShadowCaster,
                    ViewModelMesh,
                    ChildOf(bone),
                ))
                .id();
            state.parts.push(part);
        }
    }
    state.parts_ready = true;
}

fn view_animations<'a>(
    assets: &'a mut ViewModelAssets,
    set: &str,
    asset_server: &AssetServer,
    gltfs: &Assets<Gltf>,
    clip_assets: &Assets<AnimationClip>,
    graphs: &mut Assets<AnimationGraph>,
) -> Option<&'a ViewAnimations> {
    if !assets.graphs.contains_key(set) {
        if !assets.sets.contains_key(set) {
            assets.sets.insert(set.to_string(), asset_server.load(format!("imported://{set}")));
        }
        let gltf = gltfs.get(&assets.sets[set])?;
        let mut graph = AnimationGraph::new();
        let root = graph.root;
        let clips = gltf
            .named_animations
            .iter()
            .map(|(name, clip)| {
                let node = graph.add_clip(clip.clone(), 1.0, root);
                let duration = clip_assets.get(clip).map_or(1.0, |c| c.duration());
                (name.to_string(), Clip { node, duration })
            })
            .collect();
        let animations = ViewAnimations {
            graph: graphs.add(graph),
            clips,
        };
        assets.graphs.insert(set.to_string(), animations);
    }
    assets.graphs.get(set)
}

/// Crossfade times (seconds), after BF2's first-person bundles: fire cuts in and blends back
/// quickly, loops and zooming crossfade about as fast as the camera zooms.
const FADE_LOOP: f32 = 0.2;
const FADE_FIRE_OUT: f32 = 0.08;
const FADE_BOLT_IN: f32 = 0.1;
const FADE_ONE_SHOT_IN: f32 = 0.12;
const FADE_ONE_SHOT_OUT: f32 = 0.25;

#[allow(clippy::too_many_arguments, clippy::type_complexity)]
fn animate_view_model(
    mut commands: Commands,
    time: Res<Time>,
    mut assets: ResMut<ViewModelAssets>,
    armory: Res<Armory>,
    asset_server: Res<AssetServer>,
    gltfs: Res<Assets<Gltf>>,
    clip_assets: Res<Assets<AnimationClip>>,
    mut graphs: ResMut<Assets<AnimationGraph>>,
    feedback: Res<CombatFeedback>,
    mut zoom: ResMut<Zoom>,
    third_person: Res<ThirdPerson>,
    // Critically wounded soldiers hold nothing.
    soldier: Query<
        (&Loadout, &Inventory, &SoldierMotion),
        (With<LocalSoldier>, Without<game_shared::vehicle::Seated>, Without<game_shared::revive::Downed>),
    >,
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
    // Both hands on a ladder (or a parachute's risers): the weapon is slung, as in BF2's
    // ladder and parachute seats. Swimming, it's put away too (BF2 can't fire in the water).
    if motion.climbing || motion.riding || motion.parachute || motion.swimming {
        visibility.set_if_neq(Visibility::Hidden);
        state.animated_weapon.clear();
        return;
    }
    let weapon = local_weapon(&armory, loadout, inventory);
    let held = weapon.map(|w| held(w, feedback.detonator));
    let set = held.as_ref().and_then(|(_, _, set)| set.cloned());
    let (Some(weapon), Some((held, ..)), Some(set)) = (weapon, held, set) else {
        // Nothing to hold (or no first-person animations for it).
        visibility.set_if_neq(Visibility::Hidden);
        return;
    };
    visibility.set_if_neq(if third_person.0 { Visibility::Hidden } else { Visibility::Inherited });
    let Some(animations) = view_animations(&mut assets, &set, &asset_server, &gltfs, &clip_assets, &mut graphs) else {
        return;
    };
    let Ok(mut player) = players.get_mut(rig.player) else {
        return;
    };
    let clip = |name: &str| animations.clips.get(name).copied();
    let state = &mut *state;

    if state.set != set {
        // Each set has its own graph: start over (the deploy clip follows).
        commands.entity(rig.player).insert(AnimationGraphHandle(animations.graph.clone()));
        player.stop_all();
        state.layer = BlendLayer::default();
        state.one_shot = None;
        state.next = None;
        state.base = "";
        state.set = set;
    }
    // One-shot starting now, and its fade in.
    let mut started = None;
    if state.animated_weapon != held {
        state.animated_weapon = held;
        state.shots_seen = feedback.shots_fired;
        state.was_reloading = false;
        started = clip("deploy").map(|c| (OneShot::new(c, FADE_ONE_SHOT_OUT), 0.0));
    }
    let zoomed = zoom.held > 0.0;
    if feedback.shots_fired != state.shots_seen {
        state.shots_seen = feedback.shots_fired;
        // Bolt-action rifles leave the zoom to work the bolt, so their shot plays unzoomed
        // and the bolt ("load", BF2's shift animation) follows within the shift delay.
        let bolt_action = weapon.zoom.out_after_fire;
        // Grenades and charges have only a throw.
        let fire = match zoomed && !bolt_action {
            true => clip("zoom_fire").or(clip("fire")),
            false => clip("fire").or(clip("fire_throw")),
        };
        let fire_time = fire.map_or(0.0, |c| c.duration);
        // Seconds from the shot until the rifle is ready again (BF2 `animation.shiftDelay`).
        let shift_delay = weapon.shift_delay.max(fire_time);
        state.next = clip("load").filter(|_| bolt_action).map(|load| OneShot {
            speed: (load.duration / (shift_delay - fire_time).max(0.1)).max(1.0),
            ..OneShot::new(load, FADE_ONE_SHOT_OUT)
        });
        if bolt_action {
            zoom.bolt = if state.next.is_some() { shift_delay } else { fire_time };
        }
        // Cuts in so every shot shows at once.
        started = fire.map(|c| (OneShot::new(c, FADE_FIRE_OUT), 0.0));
    }
    // Winding up a throw pulls the pin; the grenade is then held ready.
    if feedback.cooking && !state.was_cooking {
        started = clip("fire_pinremove").map(|c| (OneShot::new(c, FADE_ONE_SHOT_OUT), FADE_ONE_SHOT_IN));
        state.next = None;
    }
    state.was_cooking = feedback.cooking;
    if feedback.reloading && !state.was_reloading {
        started = clip("reload").map(|c| (OneShot::new(c, FADE_ONE_SHOT_OUT), FADE_ONE_SHOT_IN));
        state.next = None;
    }
    state.was_reloading = feedback.reloading;

    let mut restart = started.is_some();
    if let Some((one_shot, fade_in)) = started {
        state.one_shot = Some(one_shot);
        state.layer.set_fade(fade_in);
    } else if let Some(one_shot) = state.one_shot
        && one_shot.clip.finished(&player)
    {
        // On to the queued one-shot, or blend back into whichever loop fits now.
        state.one_shot = state.next.take();
        restart = state.one_shot.is_some();
        state.layer.set_fade(if restart { FADE_BOLT_IN } else { one_shot.fade_out });
        state.base = "";
    }

    state.layer.begin();
    if let Some(one_shot) = state.one_shot {
        state.layer.play(&mut player, one_shot.clip, Play::once(restart).speed(one_shot.speed));
    } else {
        // Run and sprint play at the ground speed (BF2's 1p_move and 1p_sprint speeds).
        let speed = Vec2::new(motion.velocity.x, motion.velocity.z).length();
        let (base, rate) = match (zoomed, speed > 5.0, speed > 0.5) {
            (true, _, true) => ("zoom_run", 1.0),
            (true, _, false) => ("zoom_stand", 1.0),
            (false, true, _) => ("sprint", (speed / 6.3).clamp(0.5, 1.5)),
            (false, false, true) => ("run", (speed / 4.2).clamp(0.5, 1.5)),
            _ => ("stand", 1.0),
        };
        let base = match (feedback.cooking, clip("stand_loaded_ready").is_some()) {
            (true, true) => "stand_loaded_ready",
            (true, false) => "stand_loaded",
            _ => base,
        };
        if state.base != base {
            if !state.base.is_empty() {
                state.layer.set_fade(FADE_LOOP);
            }
            state.base = base;
        }
        if let Some(loop_clip) = clip(base).or_else(|| clip("stand")) {
            state.layer.play(&mut player, loop_clip, Play::looping(1.0).speed(rate));
        }
    }
    state.layer.update(&mut player, time.delta_secs());
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
