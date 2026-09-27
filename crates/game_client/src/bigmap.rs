//! The full-screen map, shown while M is held: the whole level with flags (named), our team,
//! our squad and us. North is up; the mouse stays with the game.

use bevy::{platform::collections::HashMap, prelude::*};
use game_shared::{
    conquest::{ControlPoint, FlagState},
    level::LoadedLevel,
    protocol::{ControlledBy, Team},
    soldier::Soldier,
    squad::SquadMember,
};

use crate::{
    camera::PlayerCamera,
    conquest_hud::{FRIENDLY, SQUAD, team_color},
    deploy::DeployScreen,
    net::{LocalPlayer, LocalSoldier},
    prediction::SoldierRender,
};

pub struct BigMapPlugin;

impl Plugin for BigMapPlugin {
    fn build(&self, app: &mut App) {
        app.add_systems(Startup, spawn_big_map).add_systems(
            Update,
            (
                set_map_image.run_if(resource_exists_and_changed::<LoadedLevel>),
                show_big_map,
                update_big_map,
            )
                .chain(),
        );
    }
}

const TEXT: Color = Color::srgb(0.95, 0.96, 0.98);

#[derive(Component)]
struct BigMapRoot;
#[derive(Component)]
struct BigMapImage;
#[derive(Component)]
struct BigMapHeading;
/// A marker for an entity (flag or soldier).
#[derive(Component)]
struct BigMapIcon(Entity);

fn spawn_big_map(mut commands: Commands) {
    commands
        .spawn((
            BigMapRoot,
            Node {
                position_type: PositionType::Absolute,
                width: percent(100),
                height: percent(100),
                justify_content: JustifyContent::Center,
                align_items: AlignItems::Center,
                ..default()
            },
            GlobalZIndex(5),
            Visibility::Hidden,
        ))
        .with_children(|root| {
            root.spawn((
                BigMapImage,
                Node {
                    width: vh(84),
                    height: vh(84),
                    border: UiRect::all(px(2)),
                    border_radius: BorderRadius::all(px(10)),
                    overflow: Overflow::clip(),
                    ..default()
                },
                BackgroundColor(Color::srgb(0.14, 0.15, 0.16)),
                BorderColor::all(Color::srgba(0.0, 0.0, 0.0, 0.6)),
            ))
            .with_child((
                BigMapHeading,
                UiTransform::default(),
                Node {
                    position_type: PositionType::Absolute,
                    width: px(0),
                    height: px(0),
                    ..default()
                },
                GlobalZIndex(6),
                children![
                    (
                        Node {
                            position_type: PositionType::Absolute,
                            left: px(-1.5),
                            top: px(-20),
                            width: px(3),
                            height: px(20),
                            ..default()
                        },
                        BackgroundColor(Color::srgba(1.0, 1.0, 1.0, 0.85)),
                    ),
                    (
                        Node {
                            position_type: PositionType::Absolute,
                            left: px(-5),
                            top: px(-5),
                            width: px(10),
                            height: px(10),
                            border_radius: BorderRadius::all(px(5)),
                            ..default()
                        },
                        BackgroundColor(Color::WHITE),
                    ),
                ],
            ));
        });
}

fn set_map_image(
    mut commands: Commands,
    level: Res<LoadedLevel>,
    asset_server: Res<AssetServer>,
    image: Single<Entity, With<BigMapImage>>,
) {
    match &level.desc.minimap {
        Some(path) => {
            commands
                .entity(*image)
                .insert(ImageNode::new(asset_server.load(format!("imported://{path}"))));
        }
        None => {
            commands.entity(*image).remove::<ImageNode>();
        }
    }
}

fn show_big_map(
    keys: Res<ButtonInput<KeyCode>>,
    deploy: Res<DeployScreen>,
    mut root: Single<&mut Visibility, With<BigMapRoot>>,
) {
    let show = keys.pressed(KeyCode::KeyM) && !deploy.open;
    root.set_if_neq(if show { Visibility::Inherited } else { Visibility::Hidden });
}

/// Map position (0..1, top-left origin, north up) of a world position.
fn map_uv(level: &LoadedLevel, position: Vec3) -> Vec2 {
    let Some(heightmap) = &level.heightmap else {
        return Vec2::splat(0.5);
    };
    let size = heightmap.world_size().max(1.0);
    let corner = heightmap.center() - Vec3::new(size, 0.0, size) * 0.5;
    Vec2::new((position.x - corner.x) / size, (position.z - corner.z) / size)
}

#[allow(clippy::too_many_arguments, clippy::type_complexity)]
fn update_big_map(
    mut commands: Commands,
    level: Option<Res<LoadedLevel>>,
    root: Single<&Visibility, With<BigMapRoot>>,
    camera: Single<&GlobalTransform, With<PlayerCamera>>,
    players: Query<(&Team, Option<&SquadMember>), With<LocalPlayer>>,
    teams: Query<(&Team, Option<&SquadMember>)>,
    control_points: Query<(Entity, &ControlPoint, &FlagState)>,
    soldiers: Query<(Entity, &SoldierRender, &ControlledBy), (With<Soldier>, Without<LocalSoldier>)>,
    image: Single<Entity, With<BigMapImage>>,
    mut icons: Query<(Entity, &BigMapIcon, &mut Node, &mut BackgroundColor), Without<BigMapHeading>>,
    mut heading: Single<(&mut Node, &mut UiTransform), With<BigMapHeading>>,
) {
    let Some(level) = level else {
        return;
    };
    if **root == Visibility::Hidden {
        return;
    }
    let at = |uv: Vec2| (percent(uv.x.clamp(0.0, 1.0) * 100.0), percent(uv.y.clamp(0.0, 1.0) * 100.0));

    let (node, transform) = &mut *heading;
    (node.left, node.top) = at(map_uv(&level, camera.translation()));
    let forward = camera.forward();
    transform.rotation = Rot2::radians(forward.x.atan2(-forward.z));

    let (local, local_squad) = players
        .single()
        .map(|(team, squad)| (*team, squad.copied()))
        .unwrap_or_default();
    // Flags with names, teammates (squad mates in green).
    let mut wanted: HashMap<Entity, (Vec3, Color, f32, Option<String>)> = HashMap::default();
    for (entity, cp, state) in &control_points {
        wanted.insert(entity, (cp.position, team_color(state.owner, local), 18.0, Some(cp.name.clone())));
    }
    for (entity, render, controlled_by) in &soldiers {
        let (team, squad) = teams.get(controlled_by.0).map(|(t, s)| (*t, s.copied())).unwrap_or_default();
        if team == local && local != Team::Spectator {
            let squad_mate = local_squad.zip(squad).is_some_and(|(a, b)| a.squad == b.squad);
            wanted.insert(entity, (render.position, if squad_mate { SQUAD } else { FRIENDLY }, 8.0, None));
        }
    }

    for (icon_entity, icon, mut node, mut background) in &mut icons {
        let Some((position, color, _, _)) = wanted.remove(&icon.0) else {
            commands.entity(icon_entity).despawn();
            continue;
        };
        (node.left, node.top) = at(map_uv(&level, position));
        background.0 = color;
    }
    for (entity, (position, color, size, label)) in wanted {
        let (left, top) = at(map_uv(&level, position));
        let mut icon = commands.spawn((
            BigMapIcon(entity),
            Node {
                position_type: PositionType::Absolute,
                left,
                top,
                width: px(size),
                height: px(size),
                margin: UiRect {
                    left: px(-size / 2.0),
                    top: px(-size / 2.0),
                    ..default()
                },
                border: UiRect::all(px(1.5)),
                border_radius: BorderRadius::all(px(size / 2.0)),
                justify_content: JustifyContent::Center,
                ..default()
            },
            BackgroundColor(color),
            BorderColor::all(Color::srgba(0.0, 0.0, 0.0, 0.7)),
            ChildOf(*image),
        ));
        if let Some(label) = label {
            icon.with_child((
                Text::new(label),
                TextFont {
                    font_size: FontSize::Px(13.0),
                    ..default()
                },
                TextColor(TEXT),
                TextShadow {
                    offset: Vec2::splat(1.0),
                    color: Color::srgba(0.0, 0.0, 0.0, 0.9),
                },
                TextLayout::justify(Justify::Center),
                Node {
                    position_type: PositionType::Absolute,
                    top: px(size + 2.0),
                    width: px(140),
                    left: px(size / 2.0 - 70.0),
                    justify_content: JustifyContent::Center,
                    ..default()
                },
            ));
        }
    }
}
