//! Soldier visuals: the team's imported BF2 soldier model with movement animations, or a
//! team-colored capsule when no model is available (e.g. on the test range).

use bevy::{
    gltf::Gltf,
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
                    build_animation_graphs,
                    attach_models,
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

/// Clips used for movement, by lowercase BF2 file name.
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

/// Upper-body animations while holding a weapon. Until kits exist everyone carries the
/// same rifle.
const DEFAULT_WEAPON_ANIMATIONS: &str = "objects/weapons/handheld/rurif_ak47/animations/3p.glb";

/// The soldier model each team wears (the first kit's body for now).
#[derive(Resource, Default)]
struct SoldierModels {
    teams: [Option<SoldierModel>; 2],
    weapon_animations: Option<Handle<Gltf>>,
}

struct SoldierModel {
    gltf: Handle<Gltf>,
    animations: Option<ModelAnimations>,
}

#[derive(Clone)]
struct ModelAnimations {
    graph: Handle<AnimationGraph>,
    /// Per movement clip: the legs clip and the matching upper-body (weapon) clip.
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

/// The animation player inside an attached model, and the clip it plays.
#[derive(Component)]
struct SoldierAnimator {
    player: Entity,
    playing: &'static str,
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
    models.weapon_animations = Some(asset_server.load(format!("imported://{DEFAULT_WEAPON_ANIMATIONS}")));
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
            .map(|desc| SoldierModel {
                gltf: asset_server.load(format!("imported://{}", desc.mesh)),
                animations: None,
            });
    }
}

fn build_animation_graphs(
    mut models: ResMut<SoldierModels>,
    gltfs: Res<Assets<Gltf>>,
    mut graphs: ResMut<Assets<AnimationGraph>>,
) {
    let models = &mut *models;
    let weapon = models.weapon_animations.as_ref().and_then(|h| gltfs.get(h));
    for model in models.teams.iter_mut().flatten() {
        if model.animations.is_some() {
            continue;
        }
        let Some(gltf) = gltfs.get(&model.gltf) else {
            continue;
        };
        let mut graph = AnimationGraph::new();
        let mut nodes = HashMap::new();
        for &name in clips::ALL {
            let Some(legs) = gltf.named_animations.get(name) else {
                continue;
            };
            let legs = graph.add_clip(legs.clone(), 1.0, graph.root);
            // Weapon clips are named by state: `3p_crouchstill` pairs with `crouchstill`.
            let state = match name.trim_start_matches("3p_") {
                "runforwardjumploop" => "runforward",
                other => other,
            };
            let upper = weapon
                .and_then(|w| w.named_animations.get(state).or_else(|| w.named_animations.get("stand")))
                .map(|clip| graph.add_clip(clip.clone(), 1.0, graph.root));
            nodes.insert(name, (legs, upper));
        }
        model.animations = Some(ModelAnimations {
            graph: graphs.add(graph),
            nodes,
        });
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

fn soldier_team(
    soldier: Entity,
    controllers: &Query<&ControlledBy>,
    teams: &Query<&Team>,
) -> Team {
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
        let scene = model.and_then(|(_, m)| gltfs.get(&m.gltf)).and_then(|g| g.default_scene.clone());
        if model.is_some() && scene.is_none() {
            if attached.is_none() {
                attach_capsule(&mut commands, entity, team, &placeholder);
            }
            continue;
        }
        commands
            .entity(entity)
            .despawn_related::<Children>()
            .remove::<SoldierAnimator>()
            .insert(AttachedBody(wanted));
        match (scene, model) {
            (Some(scene), Some(_)) => {
                commands.spawn((WorldAssetRoot(scene), ChildOf(entity))).observe(
                    |ready: On<WorldInstanceReady>,
                     mut commands: Commands,
                     children: Query<&Children>,
                     players: Query<(), With<AnimationPlayer>>,
                     parents: Query<&ChildOf>| {
                        let Some(player) = children
                            .iter_descendants(ready.entity)
                            .find(|e| players.contains(*e))
                        else {
                            return;
                        };
                        if let Ok(visual) = parents.get(ready.entity) {
                            commands.entity(visual.parent()).insert(SoldierAnimator {
                                player,
                                playing: "",
                            });
                        }
                    },
                );
            }
            _ => attach_capsule(&mut commands, entity, team, &placeholder),
        }
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

fn animate(
    mut commands: Commands,
    models: Res<SoldierModels>,
    soldiers: Query<&SoldierRender>,
    mut visuals: Query<(&SoldierVisual, &AttachedBody, &mut SoldierAnimator)>,
    mut players: Query<(&mut AnimationPlayer, Has<AnimationGraphHandle>)>,
) {
    for (visual, body, mut animator) in &mut visuals {
        let (Ok(render), Some(team)) = (soldiers.get(visual.soldier), body.0) else {
            continue;
        };
        let Some(animations) = models.teams[team].as_ref().and_then(|m| m.animations.as_ref()) else {
            continue;
        };
        let Ok((mut player, has_graph)) = players.get_mut(animator.player) else {
            continue;
        };
        if !has_graph {
            // The graph may finish building after the model spawned.
            commands
                .entity(animator.player)
                .insert(AnimationGraphHandle(animations.graph.clone()));
            continue;
        }
        let wanted = movement_clip(render);
        if animator.playing == wanted {
            continue;
        }
        let Some(&(legs, upper)) = animations.nodes.get(wanted) else {
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
