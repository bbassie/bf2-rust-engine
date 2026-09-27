//! Soldier visuals: the team's imported BF2 soldier model with movement animations and the
//! weapon in hand, or a team-colored capsule when no model is available (test range).
//!
//! Animation is layered like BF2: the legs play the soldier's movement clips, the upper
//! body plays the matching clip of the current weapon's animation set.

use bevy::{
    gltf::{Gltf, GltfMesh},
    platform::collections::HashMap,
    prelude::*,
    world_serialization::WorldInstanceReady,
};
use game_data::SoldierDesc;
use game_shared::{
    config::GamePaths,
    level::LoadedLevel,
    protocol::{ControlledBy, Team},
    soldier::{SOLDIER_CENTER, SOLDIER_HEIGHT, SOLDIER_RADIUS, Soldier, Stance},
    weapons::{Armory, Inventory, Loadout},
};

use crate::{
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
                    .before(TransformSystems::Propagate),
            );
    }
}

/// Movement clips, by lowercase BF2 file name.
mod clips {
    pub const STAND: &str = "3p_stand";
    pub const RUN_FORWARD: &str = "3p_runforward";
    pub const RUN_BACKWARD: &str = "3p_runbackward";
    pub const STRAFE_LEFT: &str = "3p_walkleft";
    pub const STRAFE_RIGHT: &str = "3p_walkright";
    pub const SPRINT: &str = "3p_sprint";
    pub const CROUCH: &str = "3p_crouchstill";
    pub const CROUCH_FORWARD: &str = "3p_crouchforward";
    pub const CROUCH_BACKWARD: &str = "3p_crouchbackward";
    pub const CROUCH_LEFT: &str = "3p_crouchstrafeleft";
    pub const CROUCH_RIGHT: &str = "3p_crouchstraferight";
    pub const PRONE: &str = "3p_pronestill";
    pub const PRONE_FORWARD: &str = "3p_proneforward";
    pub const PRONE_BACKWARD: &str = "3p_pronebackward";
    pub const PRONE_LEFT: &str = "3p_pronestrafeleft";
    pub const PRONE_RIGHT: &str = "3p_pronestraferight";
    pub const AIRBORNE: &str = "3p_runforwardjumploop";

    pub const ALL: &[&str] = &[
        STAND, RUN_FORWARD, RUN_BACKWARD, STRAFE_LEFT, STRAFE_RIGHT, SPRINT, CROUCH,
        CROUCH_FORWARD, CROUCH_BACKWARD, CROUCH_LEFT, CROUCH_RIGHT, PRONE, PRONE_FORWARD,
        PRONE_BACKWARD, PRONE_LEFT, PRONE_RIGHT, AIRBORNE,
    ];
}

/// Upper-body set for weapons without their own.
const DEFAULT_WEAPON_ANIMATIONS: &str = "objects/weapons/handheld/rurif_ak47/animations/3p.glb";

/// Loaded soldier models and animation graphs.
#[derive(Resource, Default)]
struct SoldierModels {
    /// The model each team wears (its first kit's body for now).
    teams: [Option<Handle<Gltf>>; 2],
    /// Weapon upper-body animation sets by path.
    weapon_sets: HashMap<String, Handle<Gltf>>,
    /// One graph per (team model, weapon set).
    graphs: HashMap<(usize, String), ModelAnimations>,
}

#[derive(Clone)]
struct ModelAnimations {
    graph: Handle<AnimationGraph>,
    /// Per movement clip: the legs clip and the matching upper-body clip.
    nodes: HashMap<&'static str, (AnimationNodeIndex, Option<AnimationNodeIndex>)>,
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
    graph: Option<(usize, String)>,
    playing: &'static str,
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
    models.graphs.clear();
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

/// The graph for a team model holding a weapon with this animation set, built on first use
/// once both files are loaded.
fn graph_for(
    models: &mut SoldierModels,
    team: usize,
    set: &str,
    asset_server: &AssetServer,
    gltfs: &Assets<Gltf>,
    graphs: &mut Assets<AnimationGraph>,
) -> Option<ModelAnimations> {
    let key = (team, set.to_string());
    if let Some(found) = models.graphs.get(&key) {
        return Some(found.clone());
    }
    let body = gltfs.get(models.teams[team].as_ref()?)?;
    let set_handle = models
        .weapon_sets
        .entry(set.to_string())
        .or_insert_with(|| asset_server.load(format!("imported://{set}")))
        .clone();
    let weapon = gltfs.get(&set_handle)?;

    let mut graph = AnimationGraph::new();
    let mut nodes = HashMap::new();
    for &name in clips::ALL {
        let Some(legs) = body.named_animations.get(name) else {
            continue;
        };
        let legs = graph.add_clip(legs.clone(), 1.0, graph.root);
        // Weapon clips are named by state: `3p_crouchstill` pairs with `crouchstill`.
        let state = match name.trim_start_matches("3p_") {
            "runforwardjumploop" => "runforward",
            "walkleft" => "strafeleft",
            "walkright" => "straferight",
            other => other,
        };
        let upper = [state, name.trim_start_matches("3p_"), "stand"]
            .iter()
            .find_map(|s| weapon.named_animations.get(*s))
            .map(|clip| graph.add_clip(clip.clone(), 1.0, graph.root));
        nodes.insert(name, (legs, upper));
    }
    let animations = ModelAnimations {
        graph: graphs.add(graph),
        nodes,
    };
    models.graphs.insert(key, animations.clone());
    Some(animations)
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
) -> Option<&'a game_data::WeaponDesc> {
    let (loadout, inventory) = (loadout?, inventory?);
    armory.weapon(loadout.weapons.get(inventory.active as usize)?).map(|w| w.as_ref())
}

/// Puts the soldier's current weapon model on the weapon bones (part `n` on `mesh{n+1}`).
#[allow(clippy::type_complexity)]
fn attach_weapons(
    mut commands: Commands,
    armory: Res<Armory>,
    asset_server: Res<AssetServer>,
    gltfs: Res<Assets<Gltf>>,
    gltf_meshes: Res<Assets<GltfMesh>>,
    soldiers: Query<(Option<&Loadout>, Option<&Inventory>)>,
    mut visuals: Query<(Entity, &SoldierVisual, &ModelRig, Option<&mut HeldWeapon>)>,
) {
    for (entity, visual, rig, held) in &mut visuals {
        let Ok((loadout, inventory)) = soldiers.get(visual.soldier) else {
            continue;
        };
        let weapon = active_weapon(&armory, loadout, inventory);
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
        let mut parts = Vec::new();
        for (index, mesh) in gltf.meshes.iter().enumerate() {
            let (Some(bone), Some(mesh)) = (rig.weapon_bones.get(index).copied().flatten(), gltf_meshes.get(mesh)) else {
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
    soldiers: Query<(&SoldierRender, Has<LocalSoldier>)>,
    mut visuals: Query<(&SoldierVisual, &mut Transform, &mut Visibility)>,
) {
    for (visual, mut transform, mut visibility) in &mut visuals {
        let Ok((render, local)) = soldiers.get(visual.soldier) else {
            continue;
        };
        // First person: don't draw our own body inside the camera.
        visibility.set_if_neq(if local && !third_person.0 {
            Visibility::Hidden
        } else {
            Visibility::Inherited
        });
        *transform = Transform::from_translation(render.position)
            .with_rotation(Quat::from_rotation_y(render.yaw));
    }
}

/// Picks the movement clip from speed, direction and stance.
fn movement_clip(render: &SoldierRender) -> &'static str {
    let local = Quat::from_rotation_y(-render.yaw) * render.velocity;
    let (right, forward) = (local.x, -local.z);
    let speed = Vec2::new(right, forward).length();
    if !render.grounded && render.velocity.y.abs() > 1.0 {
        return clips::AIRBORNE;
    }
    let moving = speed > 0.4;
    let sideways = right.abs() > forward.abs();
    use clips::*;
    match render.stance {
        Stance::Standing if !moving => STAND,
        Stance::Standing if speed > 5.0 && forward > 0.0 => SPRINT,
        Stance::Standing if sideways => if right > 0.0 { STRAFE_RIGHT } else { STRAFE_LEFT },
        Stance::Standing => if forward >= 0.0 { RUN_FORWARD } else { RUN_BACKWARD },
        Stance::Crouching if !moving => CROUCH,
        Stance::Crouching if sideways => if right > 0.0 { CROUCH_RIGHT } else { CROUCH_LEFT },
        Stance::Crouching => if forward >= 0.0 { CROUCH_FORWARD } else { CROUCH_BACKWARD },
        Stance::Prone if !moving => PRONE,
        Stance::Prone if sideways => if right > 0.0 { PRONE_RIGHT } else { PRONE_LEFT },
        Stance::Prone => if forward >= 0.0 { PRONE_FORWARD } else { PRONE_BACKWARD },
    }
}

#[allow(clippy::too_many_arguments, clippy::type_complexity)]
fn animate(
    mut commands: Commands,
    mut models: ResMut<SoldierModels>,
    armory: Res<Armory>,
    asset_server: Res<AssetServer>,
    gltfs: Res<Assets<Gltf>>,
    mut graphs: ResMut<Assets<AnimationGraph>>,
    soldiers: Query<(&SoldierRender, Option<&Loadout>, Option<&Inventory>)>,
    mut visuals: Query<(&SoldierVisual, &AttachedBody, &ModelRig, &mut SoldierAnimator)>,
    mut players: Query<&mut AnimationPlayer>,
) {
    for (visual, body, rig, mut animator) in &mut visuals {
        let (Ok((render, loadout, inventory)), Some(team)) = (soldiers.get(visual.soldier), body.0) else {
            continue;
        };
        let set = active_weapon(&armory, loadout, inventory)
            .and_then(|w| w.animations_3p.clone())
            .unwrap_or_else(|| DEFAULT_WEAPON_ANIMATIONS.to_string());
        let Some(animations) = graph_for(&mut models, team, &set, &asset_server, &gltfs, &mut graphs) else {
            continue;
        };
        let key = (team, set);
        if animator.graph.as_ref() != Some(&key) {
            // New weapon set: swap graphs and restart the clips.
            commands.entity(rig.player).insert(AnimationGraphHandle(animations.graph.clone()));
            animator.graph = Some(key);
            animator.playing = "";
            continue;
        }
        let wanted = movement_clip(render);
        if animator.playing == wanted {
            continue;
        }
        let (Some(&(legs, upper)), Ok(mut player)) = (animations.nodes.get(wanted), players.get_mut(rig.player)) else {
            continue;
        };
        // Legs and upper body animate disjoint bones, so both play at full weight.
        player.stop_all();
        player.play(legs).repeat();
        if let Some(upper) = upper {
            player.play(upper).repeat();
        }
        animator.playing = wanted;
    }
}
