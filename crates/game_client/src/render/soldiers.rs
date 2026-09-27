//! Soldier visuals: the team's imported BF2 soldier model with movement animations and the
//! weapon in hand, or a team-colored capsule when no model is available (test range).
//!
//! Animation is layered like BF2: the legs play the soldier's movement clips, the upper
//! body plays the matching clip of the current weapon's animation set. Every change
//! crossfades: movement blends the four directional clips by direction and plays them at
//! the ground speed, turning on the spot steps the feet round, and jumps go through
//! take-off, airborne and landing clips. Shots, reloads and weapon switches play the
//! weapon's one-shots on the upper body; a switch lowers the old weapon, swaps the model
//! out of view and raises the new one.

use bevy::{
    app::AnimationSystems,
    gltf::{Gltf, GltfMesh},
    platform::collections::HashMap,
    prelude::*,
    world_serialization::WorldInstanceReady,
};
use std::sync::Arc;

use game_data::{SoldierDesc, WeaponDesc};
use game_shared::{
    config::GamePaths,
    level::LoadedLevel,
    protocol::{ControlledBy, ShotFired, Team},
    soldier::{SOLDIER_CENTER, SOLDIER_HEIGHT, SOLDIER_RADIUS, Soldier, Stance},
    weapons::{Armory, Inventory, Loadout},
};

use super::{
    blend::{BlendLayer, Clip, Play},
    materials::Bf2Materials,
};
use crate::{
    combat::CombatFeedback,
    net::LocalSoldier,
    prediction::{RenderStateSystems, SoldierRender},
};

pub struct SoldierRenderPlugin;

impl Plugin for SoldierRenderPlugin {
    fn build(&self, app: &mut App) {
        app.init_resource::<SoldierModels>()
            .add_systems(Startup, load_placeholder_assets)
            .add_observer(spawn_visual)
            .add_observer(despawn_visual)
            .add_systems(
                Update,
                (
                    load_team_models.run_if(resource_exists_and_changed::<LoadedLevel>),
                    attach_models,
                    attach_weapons,
                ),
            )
            .add_systems(
                PostUpdate,
                (update_visuals, animate)
                    .after(RenderStateSystems)
                    .before(AnimationSystems)
                    .before(TransformSystems::Propagate),
            );
    }
}

/// Clips by lowercase BF2 file name: the body's movement clips (legs) and the weapon sets'
/// upper-body clips.
mod clips {
    pub const STAND: &str = "3p_stand";
    pub const CROUCH: &str = "3p_crouchstill";
    pub const PRONE: &str = "3p_pronestill";
    pub const SPRINT: &str = "3p_sprint";
    /// Getting up from lying on the back after a revive. Its first frame, held, is how a
    /// critically wounded soldier lies.
    pub const REVIVE: &str = "3p_reviveonback";
    /// Directional sets, BF2's movement bundles: forward, backward, left, right.
    pub const WALK: [&str; 4] = ["3p_walkforward", "3p_walkbackward", "3p_walkleft", "3p_walkright"];
    pub const RUN: [&str; 4] = ["3p_runforward", "3p_runbackward", "3p_strafeleft", "3p_straferight"];
    pub const CROUCH_MOVE: [&str; 4] =
        ["3p_crouchforward", "3p_crouchbackward", "3p_crouchstrafeleft", "3p_crouchstraferight"];
    pub const PRONE_MOVE: [&str; 4] =
        ["3p_proneforward", "3p_pronebackward", "3p_pronestrafeleft", "3p_pronestraferight"];
    /// Stepping round on the spot, left and right. Prone, BF2 turns with the strafe clips
    /// (the left one backwards).
    pub const STAND_TURN: [&str; 2] = ["3p_standturnleft", "3p_standturnright"];
    pub const CROUCH_TURN: [&str; 2] = ["3p_crouchturnleft", "3p_crouchturnright"];
    /// Take-off, airborne loop and landing per jump direction (see [`super::jump_direction`]).
    pub const JUMP: [[&str; 3]; 5] = [
        ["3p_stilljumpstart", "3p_stilljumploop", "3p_stilljumpend"],
        ["3p_runforwardjumpstart", "3p_runforwardjumploop", "3p_runforwardjumpend"],
        ["3p_runbackwardjumpstart", "3p_runbackwardjumploop", "3p_runbackwardjumpend"],
        ["3p_strafeleftjumpstart", "3p_strafeleftjumploop", "3p_strafeleftjumpend"],
        ["3p_straferightjumpstart", "3p_straferightjumploop", "3p_straferightjumpend"],
    ];

    /// Walk, run, sprint and crawl cycles: they all start with the left foot forward, so
    /// switching between them keeps the step phase.
    pub fn cycles() -> impl Iterator<Item = &'static str> {
        WALK.into_iter().chain(RUN).chain(CROUCH_MOVE).chain(PRONE_MOVE).chain([SPRINT])
    }

    /// Climbing a ladder, and sliding down one (BF2's `objects/common/ladder` clips).
    pub const CLIMB: &str = "3p_climbup";
    pub const SLIDE: &str = "3p_climbdownfast";

    pub fn legs() -> impl Iterator<Item = &'static str> {
        [STAND, CROUCH, PRONE, CLIMB, SLIDE, REVIVE]
            .into_iter()
            .chain(cycles())
            .chain(STAND_TURN)
            .chain(CROUCH_TURN)
            .chain(JUMP.into_iter().flatten())
    }

    /// Upper-body clips named like the movement clip they pair with (`3p_crouchstill` with
    /// `crouchstill`); anything else (jumps) pairs with `stand`.
    pub const UPPER: &[&str] = &[
        "stand", "crouchstill", "pronestill", "sprint",
        "walkforward", "walkbackward", "walkleft", "walkright",
        "runforward", "runbackward", "strafeleft", "straferight",
        "crouchforward", "crouchbackward", "crouchstrafeleft", "crouchstraferight",
        "proneforward", "pronebackward", "pronestrafeleft", "pronestraferight",
        "standturnleft", "standturnright", "crouchturnleft", "crouchturnright",
    ];
    /// Upper-body one-shots, standing (and crouched) and prone.
    pub const DEPLOY: [&str; 2] = ["standdeploy", "pronedeploy"];
    pub const FIRE: [&str; 2] = ["standfire", "pronefire"];
    pub const RELOAD: [&str; 2] = ["reload", "pronereload"];

    pub fn upper_for(legs: &str) -> &'static str {
        let state = legs.trim_start_matches("3p_");
        UPPER.iter().copied().find(|&u| u == state).unwrap_or("stand")
    }
}

/// Ground speeds (m/s) at which the movement clips play at normal speed, from BF2's
/// animation value holders (they match the foot speed in the clips).
const WALK_SPEED: f32 = 1.5;
const RUN_SPEED: f32 = 3.9;
const SPRINT_SPEED: f32 = 6.3;
const CROUCH_SPEED: f32 = 1.7;
const PRONE_SPEED: f32 = 0.7;
/// Climbing speed the ladder clip is made for (its feet move about 1.25 m/s).
const CLIMB_SPEED: f32 = 1.25;

/// Crossfade times (seconds), roughly BF2's bundle fade times.
const FADE: f32 = 0.2;
const FADE_START_MOVING: f32 = 0.15;
const FADE_STANCE: f32 = 0.3;
const FADE_PRONE: f32 = 0.4;
const FADE_JUMP: f32 = 0.1;
const FADE_TURN_IN: f32 = 0.1;
const FADE_TURN_OUT: f32 = 0.25;
/// Upper-body one-shots: a weapon switch first lowers the old weapon over `FADE_DEPLOY_IN`
/// (into the deploy clip's first, lowest frame), swaps the model and then plays the clip.
const FADE_DEPLOY_IN: f32 = 0.15;
const FADE_FIRE_IN: f32 = 0.05;
const FADE_RELOAD_IN: f32 = 0.15;
const FADE_ACTION_OUT: f32 = 0.2;

/// Turning on the spot faster than this (rad/s) steps the feet round; slower than
/// `TURN_STOP` ends it.
const TURN_START: f32 = 1.0;
const TURN_STOP: f32 = 0.4;
/// Turning speed (rad/s) at which the turn clips play at normal speed (BF2's value holder).
const TURN_SPEED: f32 = 1.0;
/// How quickly the measured turning speed follows the yaw (per second).
const TURN_SMOOTHING: f32 = 6.0;

/// Upward speed that means the soldier jumped rather than stepped off something.
const TAKE_OFF_SPEED: f32 = 1.0;
/// Time off the ground before a fall (not a jump) plays the airborne loop.
const FALL_TIME: f32 = 0.25;
/// Shorter jumps end without a landing.
const MIN_AIR_TIME: f32 = 0.15;
/// How long a landing plays before movement takes over, standing still and moving.
const LAND_TIME: f32 = 0.35;
const LAND_TIME_MOVING: f32 = 0.12;

/// Upper-body set for weapons without their own.
const DEFAULT_WEAPON_ANIMATIONS: &str = "objects/weapons/handheld/rurif_ak47/animations/3p.glb";

/// Loaded soldier models and animation graphs.
#[derive(Resource, Default)]
struct SoldierModels {
    /// The model each team wears (its first kit's body for now).
    teams: [Option<Handle<Gltf>>; 2],
    /// Weapon upper-body animation sets by path.
    weapon_sets: HashMap<String, Handle<Gltf>>,
    /// One graph per team model.
    graphs: [Option<ModelAnimations>; 2],
}

/// A team model's animation graph: its movement clips plus the upper-body clips of every
/// weapon set used so far, so a weapon switch crossfades within one graph.
struct ModelAnimations {
    graph: Handle<AnimationGraph>,
    legs: HashMap<&'static str, Clip>,
    /// Nodes of [`clips::cycles`].
    cycles: Vec<AnimationNodeIndex>,
    /// Per weapon set path, its clips of [`clips::UPPER`] and the one-shots.
    upper: HashMap<String, HashMap<&'static str, Clip>>,
}

#[derive(Resource)]
struct PlaceholderAssets {
    body: Handle<Mesh>,
    visor: Handle<Mesh>,
    visor_material: Handle<StandardMaterial>,
    /// Spectator/unknown, team one, team two.
    team_materials: [Handle<StandardMaterial>; 3],
}

/// The visual entity drawn for a soldier. Kept separate from the simulated entity so
/// smoothing never moves the hitbox.
#[derive(Component)]
struct SoldierVisual {
    soldier: Entity,
}

/// Which body is attached to a visual: `Some(team index)` for a model, `None` for the capsule.
#[derive(Component)]
struct AttachedBody(Option<usize>);

/// Parts of an attached model found when its scene spawned.
#[derive(Component)]
struct ModelRig {
    player: Entity,
    /// Weapon part bones `mesh1..mesh8`.
    weapon_bones: [Option<Entity>; 8],
}

/// Animation state of a visual.
#[derive(Component, Default)]
struct SoldierAnimator {
    /// Graph given to the player.
    graph: Option<AssetId<AnimationGraph>>,
    /// Weapon set the upper body plays.
    set: String,
    legs: BlendLayer,
    upper: BlendLayer,
    state: Option<Legs>,
    /// Seconds in `state`.
    state_time: f32,
    /// Seconds since the soldier last stood on the ground.
    airborne: f32,
    /// Last yaw and the smoothed turning speed (rad/s, positive to the left).
    yaw: Option<f32>,
    yaw_rate: f32,
    /// One-shot playing on the upper body over the movement clips.
    action: Option<Action>,
    /// Weapon model to show, when it isn't the active weapon yet: during a switch the old
    /// one stays in the hands until it has been lowered.
    hand: Option<Arc<WeaponDesc>>,
    /// Hands busy (on a ladder, or down): no weapon shown.
    stowed: bool,
    /// Our own soldier: shots fired so far (others' shots arrive as [`ShotFired`]).
    shots_seen: Option<u32>,
    was_reloading: bool,
}

/// An upper-body one-shot.
#[derive(Clone, Copy, Debug)]
struct Action {
    kind: ActionKind,
    clip: Clip,
    /// Seconds since it started.
    time: f32,
}

#[derive(Clone, Copy, PartialEq, Debug)]
enum ActionKind {
    Deploy,
    Fire,
    Reload,
}

impl Action {
    /// A weapon switch still lowering the old weapon.
    fn lowering(&self) -> bool {
        self.kind == ActionKind::Deploy && self.time < FADE_DEPLOY_IN
    }
}

/// What the legs do.
#[derive(Clone, Copy, PartialEq, Debug)]
enum Legs {
    Still(Stance),
    /// Turning on the spot, to the left if `true`.
    Turn(Stance, bool),
    /// Standing, slower than a run.
    Walk,
    Move(Stance),
    Sprint,
    /// Take-off, then the airborne loop (by [`jump_direction`]).
    Jump(usize),
    /// Stepped off something: straight into the airborne loop.
    Fall(usize),
    Land(usize),
    /// On a ladder: climbing, or sliding down it if `true`.
    Climb(bool),
    /// Critically wounded, lying where he fell.
    Down,
    /// Revived: getting up.
    GetUp,
}

impl Legs {
    fn stance(self) -> Stance {
        match self {
            Legs::Still(stance) | Legs::Turn(stance, _) | Legs::Move(stance) => stance,
            Legs::Down | Legs::GetUp => Stance::Prone,
            _ => Stance::Standing,
        }
    }
}

/// The weapon model currently in the soldier's hands.
#[derive(Component)]
struct HeldWeapon {
    name: String,
    gltf: Option<Handle<Gltf>>,
    parts: Vec<Entity>,
    spawned: bool,
}

#[derive(Component)]
struct VisualOf(Entity);

fn load_placeholder_assets(
    mut commands: Commands,
    mut meshes: ResMut<Assets<Mesh>>,
    mut materials: ResMut<Assets<StandardMaterial>>,
) {
    let mut team = |r, g, b| {
        materials.add(StandardMaterial {
            base_color: Color::srgb(r, g, b),
            perceptual_roughness: 0.8,
            ..default()
        })
    };
    let team_materials = [team(0.6, 0.6, 0.6), team(0.25, 0.35, 0.7), team(0.7, 0.3, 0.2)];
    commands.insert_resource(PlaceholderAssets {
        body: meshes.add(Capsule3d::new(SOLDIER_RADIUS, SOLDIER_HEIGHT - 2.0 * SOLDIER_RADIUS)),
        visor: meshes.add(Cuboid::new(0.36, 0.12, 0.2)),
        visor_material: materials.add(StandardMaterial {
            base_color: Color::srgb(0.08, 0.08, 0.08),
            perceptual_roughness: 0.3,
            ..default()
        }),
        team_materials,
    });
}

fn load_team_models(
    level: Res<LoadedLevel>,
    paths: Res<GamePaths>,
    asset_server: Res<AssetServer>,
    mut models: ResMut<SoldierModels>,
) {
    models.graphs = default();
    for (index, slot) in models.teams.iter_mut().enumerate() {
        *slot = level
            .desc
            .teams
            .get(index)
            .and_then(|team| team.kits.first())
            .and_then(|kit| {
                let path = paths.imported.join("soldiers").join(format!("{}.ron", kit.soldier));
                game_data::read_ron::<SoldierDesc>(&path)
                    .map_err(|err| warn!("soldier model: {err}"))
                    .ok()
            })
            .map(|desc| asset_server.load(format!("imported://{}", desc.mesh)));
    }
}

/// The team model's animations, built once its file has loaded. The weapon set is added to
/// the graph once its file has loaded too (check `upper` for it).
#[allow(clippy::too_many_arguments)]
fn team_animations<'a>(
    models: &'a mut SoldierModels,
    team: usize,
    set: &str,
    asset_server: &AssetServer,
    gltfs: &Assets<Gltf>,
    clip_assets: &Assets<AnimationClip>,
    graphs: &mut Assets<AnimationGraph>,
) -> Option<&'a ModelAnimations> {
    if models.graphs[team].is_none() {
        let body = gltfs.get(models.teams[team].as_ref()?)?;
        let mut graph = AnimationGraph::new();
        let mut legs = HashMap::default();
        for name in clips::legs() {
            if let Some(handle) = body.named_animations.get(name) {
                legs.insert(name, add_clip(&mut graph, handle, clip_assets));
            }
        }
        let cycles = clips::cycles().filter_map(|name| legs.get(name)).map(|c| c.node).collect();
        models.graphs[team] = Some(ModelAnimations {
            graph: graphs.add(graph),
            legs,
            cycles,
            upper: HashMap::default(),
        });
    }
    let animations = models.graphs[team].as_mut()?;
    if !animations.upper.contains_key(set) {
        preload_set(&mut models.weapon_sets, set, asset_server);
        let handle = &models.weapon_sets[set];
        if let (Some(weapon), Some(mut graph)) = (gltfs.get(handle), graphs.get_mut(&animations.graph)) {
            let mut upper = HashMap::default();
            let one_shots = [clips::DEPLOY, clips::FIRE, clips::RELOAD];
            for &name in clips::UPPER.iter().chain(one_shots.iter().flatten()) {
                if let Some(handle) = weapon.named_animations.get(name) {
                    upper.insert(name, add_clip(&mut graph, handle, clip_assets));
                }
            }
            animations.upper.insert(set.to_string(), upper);
        }
    }
    Some(animations)
}

/// Starts loading a weapon's upper-body set, so switching to the weapon animates at once.
fn preload_set(sets: &mut HashMap<String, Handle<Gltf>>, set: &str, asset_server: &AssetServer) {
    if !sets.contains_key(set) {
        sets.insert(set.to_string(), asset_server.load(format!("imported://{set}")));
    }
}

fn add_clip(graph: &mut AnimationGraph, handle: &Handle<AnimationClip>, clips: &Assets<AnimationClip>) -> Clip {
    let root = graph.root;
    Clip {
        node: graph.add_clip(handle.clone(), 1.0, root),
        duration: clips.get(handle).map_or(1.0, |clip| clip.duration()),
    }
}

fn spawn_visual(add: On<Add, Soldier>, mut commands: Commands) {
    let visual = commands
        .spawn((
            SoldierVisual {
                soldier: add.entity,
            },
            Transform::default(),
            Visibility::default(),
        ))
        .id();
    commands.entity(add.entity).insert(VisualOf(visual));
}

fn despawn_visual(remove: On<Remove, Soldier>, mut commands: Commands, visuals: Query<&VisualOf>) {
    if let Ok(VisualOf(visual)) = visuals.get(remove.entity) {
        commands.entity(*visual).try_despawn();
    }
}

fn soldier_team(soldier: Entity, controllers: &Query<&ControlledBy>, teams: &Query<&Team>) -> Team {
    controllers
        .get(soldier)
        .ok()
        .and_then(|c| teams.get(c.0).ok())
        .copied()
        .unwrap_or_default()
}

/// Gives each visual its body once the team (and its model) is known.
fn attach_models(
    mut commands: Commands,
    visuals: Query<(Entity, &SoldierVisual, Option<&AttachedBody>)>,
    controllers: Query<&ControlledBy>,
    teams: Query<&Team>,
    models: Res<SoldierModels>,
    gltfs: Res<Assets<Gltf>>,
    placeholder: Res<PlaceholderAssets>,
) {
    for (entity, visual, attached) in &visuals {
        let team = soldier_team(visual.soldier, &controllers, &teams);
        let team_index = match team {
            Team::One => Some(0),
            Team::Two => Some(1),
            Team::Spectator => None,
        };
        let model = team_index.and_then(|i| models.teams[i].as_ref().map(|m| (i, m)));
        let wanted = model.map(|(i, _)| i);
        if attached.is_some_and(|a| a.0 == wanted) {
            continue;
        }
        // Wait for the model to load before swapping out the capsule.
        let scene = model.and_then(|(_, m)| gltfs.get(m)).and_then(|g| g.default_scene.clone());
        if model.is_some() && scene.is_none() {
            if attached.is_none() {
                attach_capsule(&mut commands, entity, team, &placeholder);
            }
            continue;
        }
        commands
            .entity(entity)
            .despawn_related::<Children>()
            .remove::<(ModelRig, SoldierAnimator, HeldWeapon)>()
            .insert(AttachedBody(wanted));
        let Some(scene) = scene else {
            attach_capsule(&mut commands, entity, team, &placeholder);
            continue;
        };
        commands.spawn((WorldAssetRoot(scene), ChildOf(entity))).observe(
            |ready: On<WorldInstanceReady>,
             mut commands: Commands,
             children: Query<&Children>,
             players: Query<(), With<AnimationPlayer>>,
             names: Query<&Name>,
             parents: Query<&ChildOf>| {
                let mut player = None;
                let mut weapon_bones = [None; 8];
                for descendant in children.iter_descendants(ready.entity) {
                    if players.contains(descendant) {
                        player = Some(descendant);
                    }
                    if let Ok(name) = names.get(descendant)
                        && let Some(n) = name.as_str().strip_prefix("mesh").and_then(|n| n.parse::<usize>().ok())
                        && (1..=8).contains(&n)
                    {
                        weapon_bones[n - 1] = Some(descendant);
                    }
                }
                let (Some(player), Ok(visual)) = (player, parents.get(ready.entity)) else {
                    return;
                };
                commands.entity(visual.parent()).insert((
                    ModelRig {
                        player,
                        weapon_bones,
                    },
                    SoldierAnimator::default(),
                ));
            },
        );
    }
}

fn attach_capsule(commands: &mut Commands, visual: Entity, team: Team, assets: &PlaceholderAssets) {
    let material = match team {
        Team::Spectator => 0,
        Team::One => 1,
        Team::Two => 2,
    };
    commands
        .entity(visual)
        .despawn_related::<Children>()
        .insert(AttachedBody(None))
        .with_children(|parent| {
            parent.spawn((
                Mesh3d(assets.body.clone()),
                MeshMaterial3d(assets.team_materials[material].clone()),
                Transform::from_translation(SOLDIER_CENTER),
            ));
            parent.spawn((
                Mesh3d(assets.visor.clone()),
                MeshMaterial3d(assets.visor_material.clone()),
                Transform::from_xyz(0.0, SOLDIER_HEIGHT - 0.25, -SOLDIER_RADIUS + 0.02),
            ));
        });
}

/// The active weapon of a soldier.
fn active_weapon<'a>(
    armory: &'a Armory,
    loadout: Option<&Loadout>,
    inventory: Option<&Inventory>,
) -> Option<&'a Arc<WeaponDesc>> {
    let (loadout, inventory) = (loadout?, inventory?);
    armory.weapon(loadout.weapons.get(inventory.active as usize)?)
}

/// Puts the soldier's weapon model on the weapon bones (part `n` on `mesh{n+1}`): the
/// active weapon, or the one the animator still has in hand during a switch.
#[allow(clippy::type_complexity)]
fn attach_weapons(
    mut commands: Commands,
    armory: Res<Armory>,
    asset_server: Res<AssetServer>,
    gltfs: Res<Assets<Gltf>>,
    gltf_meshes: Res<Assets<GltfMesh>>,
    mut materials: Bf2Materials,
    soldiers: Query<(Option<&Loadout>, Option<&Inventory>)>,
    mut visuals: Query<(Entity, &SoldierVisual, &ModelRig, Option<&SoldierAnimator>, Option<&mut HeldWeapon>)>,
) {
    for (entity, visual, rig, animator, held) in &mut visuals {
        let Ok((loadout, inventory)) = soldiers.get(visual.soldier) else {
            continue;
        };
        let weapon = if animator.is_some_and(|a| a.stowed) {
            None
        } else {
            animator
                .and_then(|a| a.hand.as_ref())
                .or_else(|| active_weapon(&armory, loadout, inventory))
        };
        let name = weapon.map_or(String::new(), |w| w.name.clone());
        let mut held = match held {
            Some(held) if held.name == name => held,
            Some(mut held) => {
                for part in held.parts.drain(..) {
                    commands.entity(part).try_despawn();
                }
                held.name = name;
                held.gltf = weapon.and_then(|w| w.mesh_3p.as_ref()).map(|p| asset_server.load(format!("imported://{p}")));
                held.spawned = false;
                held
            }
            None => {
                commands.entity(entity).insert(HeldWeapon {
                    name,
                    gltf: weapon.and_then(|w| w.mesh_3p.as_ref()).map(|p| asset_server.load(format!("imported://{p}"))),
                    parts: Vec::new(),
                    spawned: false,
                });
                continue;
            }
        };
        if held.spawned {
            continue;
        }
        let Some(gltf) = held.gltf.as_ref().and_then(|h| gltfs.get(h)) else {
            if held.gltf.is_none() {
                held.spawned = true;
            }
            continue;
        };
        let meshes: Vec<_> = gltf
            .meshes
            .iter()
            .enumerate()
            .filter_map(|(index, mesh)| Some((rig.weapon_bones.get(index).copied().flatten()?, gltf_meshes.get(mesh)?)))
            .collect();
        // Wait until the glTF's materials are ready.
        let Some(part_materials) = meshes
            .iter()
            .flat_map(|(_, mesh)| &mesh.primitives)
            .map(|primitive| materials.for_primitive(primitive))
            .collect::<Option<Vec<_>>>()
        else {
            continue;
        };
        let mut part_materials = part_materials.into_iter();
        let mut parts = Vec::new();
        for (bone, mesh) in meshes {
            for (primitive, material) in mesh.primitives.iter().zip(part_materials.by_ref()) {
                parts.push(
                    commands
                        .spawn((Mesh3d(primitive.mesh.clone()), MeshMaterial3d(material), ChildOf(bone)))
                        .id(),
                );
            }
        }
        held.parts = parts;
        held.spawned = true;
    }
}

fn update_visuals(
    third_person: Res<crate::camera::ThirdPerson>,
    soldiers: Query<(
        &SoldierRender,
        Has<LocalSoldier>,
        Has<game_shared::vehicle::Seated>,
        Has<game_shared::revive::Downed>,
    )>,
    mut visuals: Query<(&SoldierVisual, &mut Transform, &mut Visibility)>,
) {
    for (visual, mut transform, mut visibility) in &mut visuals {
        let Ok((render, local, seated, downed)) = soldiers.get(visual.soldier) else {
            continue;
        };
        // First person: don't draw our own body inside the camera (unless we're down: the
        // camera looks at it then). Seated soldiers have no seated poses yet.
        visibility.set_if_neq(if (local && !third_person.0 && !downed) || seated {
            Visibility::Hidden
        } else {
            Visibility::Inherited
        });
        *transform = Transform::from_translation(render.position)
            .with_rotation(Quat::from_rotation_y(render.yaw));
    }
}


/// Horizontal velocity in the soldier's frame: (right, forward).
fn local_velocity(render: &SoldierRender) -> Vec2 {
    let local = Quat::from_rotation_y(-render.yaw) * render.velocity;
    Vec2::new(local.x, -local.z)
}

/// Weights of the forward, backward, left and right clips for a movement direction.
fn direction_weights(velocity: Vec2) -> [f32; 4] {
    let sum = velocity.x.abs() + velocity.y.abs();
    if sum < 1e-4 {
        return [1.0, 0.0, 0.0, 0.0];
    }
    [velocity.y, -velocity.y, -velocity.x, velocity.x].map(|w| w.max(0.0) / sum)
}

/// Index into [`clips::JUMP`]: still, forward, backward, left, right.
fn jump_direction(velocity: Vec2) -> usize {
    match velocity {
        v if v.length() < 1.0 => 0,
        v if v.y.abs() >= v.x.abs() => {
            if v.y > 0.0 { 1 } else { 2 }
        }
        v => {
            if v.x < 0.0 { 3 } else { 4 }
        }
    }
}

/// The legs state for this frame. Thresholds have some hysteresis so noisy speeds don't
/// flicker between states.
fn next_legs(state: Option<Legs>, time: f32, airborne: f32, yaw_rate: f32, render: &SoldierRender) -> Legs {
    let velocity = local_velocity(render);
    let speed = velocity.length();
    if render.climbing {
        return Legs::Climb(render.velocity.y < -CLIMB_SPEED * 1.5);
    }
    match state {
        // A blip off the ground (a step or slope edge) is no jump worth landing from.
        Some(Legs::Jump(dir) | Legs::Fall(dir)) if render.grounded => {
            if time > MIN_AIR_TIME {
                return Legs::Land(dir);
            }
        }
        Some(jump @ (Legs::Jump(_) | Legs::Fall(_))) => return jump,
        _ if !render.grounded && render.velocity.y > TAKE_OFF_SPEED => {
            return Legs::Jump(jump_direction(velocity));
        }
        _ if airborne > FALL_TIME => return Legs::Fall(jump_direction(velocity)),
        Some(Legs::Land(dir)) if time < if speed > 1.0 { LAND_TIME_MOVING } else { LAND_TIME } => {
            return Legs::Land(dir);
        }
        _ => {}
    }
    let standing = matches!(state, None | Some(Legs::Still(_) | Legs::Turn(..)));
    let moving = speed > if standing { 0.5 } else { 0.3 };
    let turning = yaw_rate.abs() > if matches!(state, Some(Legs::Turn(..))) { TURN_STOP } else { TURN_START };
    let sprint_above = if state == Some(Legs::Sprint) { 4.8 } else { 5.2 };
    let walk_below = if state == Some(Legs::Walk) { 2.4 } else { 2.0 };
    match render.stance {
        stance if !moving && turning => Legs::Turn(stance, yaw_rate > 0.0),
        stance if !moving => Legs::Still(stance),
        Stance::Standing if speed > sprint_above && velocity.y > 0.0 => Legs::Sprint,
        Stance::Standing if speed < walk_below => Legs::Walk,
        stance => Legs::Move(stance),
    }
}

fn fade_time(from: Option<Legs>, to: Legs) -> f32 {
    let Some(from) = from else {
        return 0.0;
    };
    match (from, to) {
        (_, Legs::Jump(_) | Legs::Fall(_) | Legs::Land(_) | Legs::Climb(_) | Legs::Down | Legs::GetUp)
        | (Legs::Climb(_), _) => FADE_JUMP,
        _ if from.stance() != to.stance() => {
            if from.stance() == Stance::Prone || to.stance() == Stance::Prone {
                FADE_PRONE
            } else {
                FADE_STANCE
            }
        }
        (_, Legs::Turn(..)) => FADE_TURN_IN,
        (Legs::Turn(..), Legs::Still(_)) => FADE_TURN_OUT,
        (Legs::Still(_) | Legs::Turn(..), _) => FADE_START_MOVING,
        _ => FADE,
    }
}

/// What a soldier's animator needs to know this frame.
struct Cues<'a> {
    render: &'a SoldierRender,
    weapon: Option<&'a Arc<WeaponDesc>>,
    /// The weapon's upper-body set.
    set: &'a str,
    fired: bool,
    reloading: bool,
    /// Critically wounded.
    downed: bool,
}

impl SoldierAnimator {
    /// Picks this frame's clips and weights and advances the crossfades.
    fn update(&mut self, player: &mut AnimationPlayer, animations: &ModelAnimations, cues: &Cues, dt: f32) {
        let render = cues.render;
        self.airborne = if render.grounded { 0.0 } else { self.airborne + dt };
        if dt > 0.0 {
            let turned = self.yaw.map_or(0.0, |yaw| angle_between(yaw, render.yaw));
            self.yaw_rate += (turned / dt - self.yaw_rate) * (1.0 - (-TURN_SMOOTHING * dt).exp());
        }
        self.yaw = Some(render.yaw);
        let get_up_time = animations.legs.get(clips::REVIVE).map_or(0.0, |c| c.duration);
        let state = match self.state {
            _ if cues.downed => Legs::Down,
            Some(Legs::Down) => Legs::GetUp,
            Some(Legs::GetUp) if self.state_time < get_up_time => Legs::GetUp,
            _ => next_legs(self.state, self.state_time, self.airborne, self.yaw_rate, render),
        };
        let entered = self.state != Some(state);
        if entered {
            let fade = fade_time(self.state, state);
            self.legs.set_fade(fade);
            if self.action.is_none() {
                self.upper.set_fade(fade);
            }
            self.state = Some(state);
            self.state_time = 0.0;
        } else {
            self.state_time += dt;
        }

        // Legs: up to four clips with weights, all at one speed.
        let velocity = local_velocity(render);
        let mut targets = [("", 0.0); 4];
        let mut speed = 1.0;
        let mut once = None;
        match state {
            Legs::Still(stance) => {
                let clip = match stance {
                    Stance::Standing => clips::STAND,
                    Stance::Crouching => clips::CROUCH,
                    Stance::Prone => clips::PRONE,
                };
                targets[0] = (clip, 1.0);
            }
            Legs::Turn(stance, left) => {
                let side = if left { 0 } else { 1 };
                let clip = match stance {
                    Stance::Standing => clips::STAND_TURN[side],
                    Stance::Crouching => clips::CROUCH_TURN[side],
                    Stance::Prone => clips::PRONE_MOVE[2 + side],
                };
                targets[0] = (clip, 1.0);
                speed = (self.yaw_rate.abs() / TURN_SPEED).clamp(0.6, 1.8);
                if stance == Stance::Prone && left {
                    speed = -speed;
                }
            }
            Legs::Walk | Legs::Move(_) => {
                let (set, normal) = match state {
                    Legs::Move(Stance::Standing) => (clips::RUN, RUN_SPEED),
                    Legs::Move(Stance::Crouching) => (clips::CROUCH_MOVE, CROUCH_SPEED),
                    Legs::Move(Stance::Prone) => (clips::PRONE_MOVE, PRONE_SPEED),
                    _ => (clips::WALK, WALK_SPEED),
                };
                for (target, entry) in targets.iter_mut().zip(set.into_iter().zip(direction_weights(velocity))) {
                    *target = entry;
                }
                speed = (velocity.length() / normal).clamp(0.3, 2.0);
            }
            Legs::Sprint => {
                targets[0] = (clips::SPRINT, 1.0);
                speed = (velocity.length() / SPRINT_SPEED).clamp(0.3, 2.0);
            }
            Legs::Jump(dir) => {
                let [take_off, air, _] = clips::JUMP[dir];
                let take_off_time = animations.legs.get(take_off).map_or(0.0, |c| c.duration);
                if self.state_time < take_off_time {
                    targets[0] = (take_off, 1.0);
                    once = Some(entered);
                } else {
                    targets[0] = (air, 1.0);
                }
            }
            Legs::Fall(dir) => targets[0] = (clips::JUMP[dir][1], 1.0),
            Legs::Land(dir) => {
                targets[0] = (clips::JUMP[dir][2], 1.0);
                once = Some(entered);
            }
            // Holding still on the rungs pauses the climb.
            Legs::Climb(false) => {
                targets[0] = (clips::CLIMB, 1.0);
                speed = (render.velocity.y / CLIMB_SPEED).clamp(-2.0, 2.0);
            }
            Legs::Climb(true) => targets[0] = (clips::SLIDE, 1.0),
            Legs::Down | Legs::GetUp if animations.legs.contains_key(clips::REVIVE) => {
                targets[0] = (clips::REVIVE, 1.0);
                once = Some(entered);
                speed = if state == Legs::Down { 0.0 } else { 1.0 };
            }
            Legs::Down | Legs::GetUp => targets[0] = (clips::PRONE, 1.0),
        }
        let targets = targets.iter().filter(|(_, weight)| *weight > 0.02);

        // Cycles starting now pick up the step phase of the cycle playing.
        let cycle_phase = self
            .legs
            .heaviest(|node| animations.cycles.contains(&node))
            .and_then(|clip| clip.phase(player))
            .unwrap_or(0.0);
        self.legs.begin();
        for &(name, weight) in targets.clone() {
            let Some(&clip) = animations.legs.get(name) else {
                continue;
            };
            let play = match once {
                Some(restart) => Play::once(restart).speed(speed),
                None if animations.cycles.contains(&clip.node) => {
                    Play::looping(weight).speed(speed).phase(cycle_phase)
                }
                None => Play::looping(weight).speed(speed),
            };
            self.legs.play(player, clip, play);
        }
        self.legs.update(player, dt);

        // Both hands on the rungs: no weapon, and the ladder clip moves the arms too. Down,
        // the weapon is dropped.
        self.stowed = matches!(state, Legs::Climb(_) | Legs::Down | Legs::GetUp);
        if self.stowed {
            self.action = None;
            self.upper.begin();
            self.upper.update(player, dt);
            self.hand = None;
            return;
        }

        // Upper body: a one-shot (weapon switch, shot, reload) or else the weapon set's
        // clips paired with the legs clips, in step with them.
        let first_set = self.set.is_empty();
        let switched = self.set != cues.set && animations.upper.contains_key(cues.set);
        if switched {
            self.set = cues.set.to_string();
        }
        let Some(upper) = animations.upper.get(&self.set) else {
            self.hand = None;
            return;
        };
        let pose = usize::from(render.stance == Stance::Prone);
        let one_shot = |names: [&str; 2]| upper.get(names[pose]).copied();
        let started = if switched && !first_set {
            one_shot(clips::DEPLOY).map(|clip| (ActionKind::Deploy, clip, FADE_DEPLOY_IN))
        } else if cues.fired {
            one_shot(clips::FIRE).map(|clip| (ActionKind::Fire, clip, FADE_FIRE_IN))
        } else if cues.reloading && !self.was_reloading {
            one_shot(clips::RELOAD).map(|clip| (ActionKind::Reload, clip, FADE_RELOAD_IN))
        } else {
            None
        };
        self.was_reloading = cues.reloading;
        if let Some((kind, clip, fade)) = started {
            self.action = Some(Action { kind, clip, time: 0.0 });
            self.upper.set_fade(fade);
        } else if let Some(action) = &mut self.action {
            action.time += dt;
            let done = action.clip.finished(player) || (action.kind == ActionKind::Reload && !cues.reloading);
            if done {
                self.action = None;
                self.upper.set_fade(FADE_ACTION_OUT);
            }
        }
        self.upper.begin();
        if let Some(action) = self.action {
            // Held on its first frame while the old weapon goes down.
            let speed = if action.lowering() { 0.0 } else { 1.0 };
            self.upper.play(player, action.clip, Play::once(started.is_some()).speed(speed));
        } else {
            for &(name, weight) in targets {
                let Some(&clip) = upper.get(clips::upper_for(name)).or_else(|| upper.get("stand")) else {
                    continue;
                };
                let phase = animations.legs.get(name).and_then(|legs| legs.phase(player)).unwrap_or(0.0);
                let speed = if once.is_some() { 1.0 } else { speed };
                self.upper.play(player, clip, Play::looping(weight).speed(speed).phase(phase));
            }
        }
        self.upper.update(player, dt);

        if !self.action.is_some_and(|a| a.lowering()) {
            self.hand = cues.weapon.cloned();
        }
    }
}

/// Signed angle from `a` to `b`, radians.
fn angle_between(a: f32, b: f32) -> f32 {
    use std::f32::consts::{PI, TAU};
    (b - a + PI).rem_euclid(TAU) - PI
}

#[allow(clippy::too_many_arguments, clippy::type_complexity)]
fn animate(
    mut commands: Commands,
    time: Res<Time>,
    mut models: ResMut<SoldierModels>,
    armory: Res<Armory>,
    asset_server: Res<AssetServer>,
    gltfs: Res<Assets<Gltf>>,
    clip_assets: Res<Assets<AnimationClip>>,
    mut graphs: ResMut<Assets<AnimationGraph>>,
    feedback: Res<CombatFeedback>,
    mut shots: MessageReader<ShotFired>,
    soldiers: Query<(
        &SoldierRender,
        Option<Ref<Loadout>>,
        Option<&Inventory>,
        Has<LocalSoldier>,
        Has<game_shared::revive::Downed>,
    )>,
    mut visuals: Query<(&SoldierVisual, &AttachedBody, &ModelRig, &mut SoldierAnimator)>,
    mut players: Query<&mut AnimationPlayer>,
) {
    let fired: Vec<Entity> = shots.read().map(|shot| shot.soldier).collect();
    for (visual, body, rig, mut animator) in &mut visuals {
        let (Ok((render, loadout, inventory, local, downed)), Some(team)) = (soldiers.get(visual.soldier), body.0) else {
            continue;
        };
        let loadout_changed = loadout.as_ref().is_some_and(|l| l.is_changed());
        let loadout = loadout.as_deref();
        let weapon = active_weapon(&armory, loadout, inventory);
        let set = weapon.and_then(|w| w.animations_3p.as_deref()).unwrap_or(DEFAULT_WEAPON_ANIMATIONS);
        if animator.graph.is_none() || loadout_changed {
            for name in loadout.map_or(&[][..], |l| &l.weapons) {
                if let Some(set) = armory.weapon(name).and_then(|w| w.animations_3p.as_deref()) {
                    preload_set(&mut models.weapon_sets, set, &asset_server);
                }
            }
        }
        let animations = team_animations(&mut models, team, set, &asset_server, &gltfs, &clip_assets, &mut graphs);
        let (Some(animations), Ok(mut player)) = (animations, players.get_mut(rig.player)) else {
            animator.hand = None;
            continue;
        };
        if animator.graph != Some(animations.graph.id()) {
            commands.entity(rig.player).insert(AnimationGraphHandle(animations.graph.clone()));
            player.stop_all();
            *animator = SoldierAnimator {
                graph: Some(animations.graph.id()),
                ..default()
            };
        }
        // Our own shots are predicted locally; everyone else's arrive from the server.
        let (fired, reloading) = if local {
            let fired = animator.shots_seen.is_some_and(|seen| seen != feedback.shots_fired);
            animator.shots_seen = Some(feedback.shots_fired);
            (fired, feedback.reloading)
        } else {
            (fired.contains(&visual.soldier), inventory.is_some_and(|i| i.reloading))
        };
        let cues = Cues {
            render,
            weapon,
            set,
            fired,
            reloading,
            downed,
        };
        animator.update(&mut player, animations, &cues, time.delta_secs());
    }
}
