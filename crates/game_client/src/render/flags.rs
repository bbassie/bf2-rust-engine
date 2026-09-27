//! Flag poles at the control points, with the flag on the pole moving up and down as it is
//! raised and lowered. BF2's waving flag models where the level has them, simple shapes
//! otherwise.

use bevy::{
    gltf::GltfAssetLabel, platform::collections::HashMap, prelude::*,
    world_serialization::WorldInstanceReady,
};
use game_shared::{
    conquest::{ControlPoint, FlagState},
    level::LoadedLevel,
    protocol::Team,
};

pub struct FlagRenderPlugin;

impl Plugin for FlagRenderPlugin {
    fn build(&self, app: &mut App) {
        app.init_resource::<FlagAnimations>()
            .add_systems(Update, (add_poles, update_flags).chain());
    }
}

/// Height of the stand-in pole on levels without flag models.
const DEFAULT_POLE_HEIGHT: f32 = 8.0;
/// A lowered flag still hangs this high.
const FLAG_BOTTOM: f32 = 1.2;

/// On a control point entity once its pole is drawn.
#[derive(Component)]
struct FlagPole {
    height: f32,
}

/// The flag hanging on a pole (a child of the control point).
#[derive(Component)]
struct FlagCloth {
    team: Team,
}

/// Waving animation per flag model.
#[derive(Resource, Default)]
struct FlagAnimations(HashMap<String, (Handle<AnimationGraph>, AnimationNodeIndex)>);

fn flag_slot(team: Team) -> usize {
    match team {
        Team::Spectator => 0,
        Team::One => 1,
        Team::Two => 2,
    }
}

#[allow(clippy::type_complexity)]
fn add_poles(
    mut commands: Commands,
    level: Option<Res<LoadedLevel>>,
    asset_server: Res<AssetServer>,
    mut meshes: ResMut<Assets<Mesh>>,
    mut materials: ResMut<Assets<StandardMaterial>>,
    control_points: Query<(Entity, &ControlPoint), Without<FlagPole>>,
) {
    let Some(level) = level else {
        return;
    };
    let models = &level.desc.flag_models;
    for (entity, cp) in &control_points {
        if let Some(sound) = &models.sound {
            commands.spawn((
                crate::audio::SoundEmitter::new(sound.clone()).channel(crate::audio::Channel::Ambience),
                Transform::from_xyz(0.0, models.pole_height * 0.8, 0.0),
                ChildOf(entity),
            ));
        }
        let height = match &models.pole {
            Some(pole) => {
                commands.spawn((
                    WorldAssetRoot(asset_server.load(GltfAssetLabel::Scene(0).from_asset(format!("imported://{pole}")))),
                    ChildOf(entity),
                ));
                models.pole_height.max(2.0)
            }
            None => {
                commands.spawn((
                    Mesh3d(meshes.add(Cylinder::new(0.06, DEFAULT_POLE_HEIGHT))),
                    MeshMaterial3d(materials.add(Color::srgb(0.55, 0.56, 0.58))),
                    Transform::from_xyz(0.0, DEFAULT_POLE_HEIGHT / 2.0, 0.0),
                    ChildOf(entity),
                ));
                DEFAULT_POLE_HEIGHT
            }
        };
        commands.entity(entity).insert((
            FlagPole { height },
            Transform::from_translation(cp.position),
            Visibility::default(),
        ));
    }
}

/// Swaps the flag when another team's goes up and moves it to the replicated height.
#[allow(clippy::too_many_arguments)]
fn update_flags(
    mut commands: Commands,
    level: Option<Res<LoadedLevel>>,
    asset_server: Res<AssetServer>,
    mut animations: ResMut<FlagAnimations>,
    mut graphs: ResMut<Assets<AnimationGraph>>,
    mut meshes: ResMut<Assets<Mesh>>,
    mut materials: ResMut<Assets<StandardMaterial>>,
    poles: Query<(Entity, &FlagPole, &FlagState, Option<&Children>)>,
    mut cloths: Query<(&FlagCloth, &mut Transform)>,
) {
    let Some(level) = level else {
        return;
    };
    for (entity, pole, state, children) in &poles {
        let y = FLAG_BOTTOM + (pole.height - FLAG_BOTTOM - 0.1) * state.height;
        let mut current = None;
        for child in children.into_iter().flatten() {
            if let Ok((cloth, mut transform)) = cloths.get_mut(*child) {
                if cloth.team == state.flag {
                    transform.translation.y = y;
                    current = Some(*child);
                } else {
                    commands.entity(*child).despawn();
                }
            }
        }
        if current.is_some() {
            continue;
        }
        let transform = Transform::from_xyz(0.0, y, 0.0);
        let cloth = FlagCloth { team: state.flag };
        match &level.desc.flag_models.flags[flag_slot(state.flag)] {
            Some(path) => {
                let (graph, node) = animations
                    .0
                    .entry(path.clone())
                    .or_insert_with(|| {
                        let clip = asset_server
                            .load(GltfAssetLabel::Animation(0).from_asset(format!("imported://{path}")));
                        let (graph, node) = AnimationGraph::from_clip(clip);
                        (graphs.add(graph), node)
                    })
                    .clone();
                commands
                    .spawn((
                        cloth,
                        transform,
                        WorldAssetRoot(asset_server.load(GltfAssetLabel::Scene(0).from_asset(format!("imported://{path}")))),
                        ChildOf(entity),
                    ))
                    .observe(
                        move |ready: On<WorldInstanceReady>,
                              mut commands: Commands,
                              children: Query<&Children>,
                              mut players: Query<(Entity, &mut AnimationPlayer)>| {
                            for descendant in children.iter_descendants(ready.entity) {
                                if let Ok((player_entity, mut player)) = players.get_mut(descendant) {
                                    commands.entity(player_entity).insert(AnimationGraphHandle(graph.clone()));
                                    // Out of step with the other flags.
                                    player.play(node).repeat().seek_to(fastrand::f32() * 2.0);
                                }
                            }
                        },
                    );
            }
            None => {
                let color = match state.flag {
                    Team::Spectator => Color::srgb(0.9, 0.9, 0.9),
                    Team::One => Color::srgb(0.85, 0.35, 0.2),
                    Team::Two => Color::srgb(0.25, 0.45, 0.9),
                };
                commands
                    .spawn((cloth, transform, Visibility::default(), ChildOf(entity)))
                    .with_child((
                        Mesh3d(meshes.add(Cuboid::new(1.2, 0.8, 0.02))),
                        MeshMaterial3d(materials.add(StandardMaterial {
                            base_color: color,
                            cull_mode: None,
                            ..default()
                        })),
                        Transform::from_xyz(0.66, -0.4, 0.0),
                    ));
            }
        }
    }
}
