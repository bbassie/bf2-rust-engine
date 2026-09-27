//! The minimap, top right: the level's map around us (north up), flags in their owners'
//! colors, teammates as dots, and us with our heading.

use bevy::{platform::collections::HashMap, prelude::*};
use game_shared::{
    conquest::{ControlPoint, FlagState},
    level::LoadedLevel,
    protocol::{ControlledBy, Team},
    soldier::Soldier,
};

use crate::{
    camera::PlayerCamera,
    conquest_hud::{FRIENDLY, team_color},
    net::{LocalPlayer, LocalSoldier},
    prediction::SoldierRender,
};

pub struct MinimapPlugin;

impl Plugin for MinimapPlugin {
    fn build(&self, app: &mut App) {
        app.add_systems(Startup, spawn_minimap).add_systems(
            Update,
            (set_map_image.run_if(resource_exists_and_changed::<LoadedLevel>), update_minimap).chain(),
        );
    }
}

/// Side of the minimap in pixels.
pub const SIZE: f32 = 190.0;
/// Meters shown across the minimap.
const RANGE: f32 = 260.0;

#[derive(Component)]
struct MinimapImage;
#[derive(Component)]
struct MinimapIcons;
#[derive(Component)]
struct PlayerHeading;
/// A dot for an entity (flag or soldier) on the minimap.
#[derive(Component)]
struct Icon(Entity);

fn spawn_minimap(mut commands: Commands) {
    commands
        .spawn((
            Node {
                position_type: PositionType::Absolute,
                top: px(12),
                right: px(16),
                width: px(SIZE),
                height: px(SIZE),
                border: UiRect::all(px(2)),
                border_radius: BorderRadius::all(px(10)),
                overflow: Overflow::clip(),
                ..default()
            },
            BackgroundColor(Color::srgb(0.14, 0.15, 0.16)),
            BorderColor::all(Color::srgba(0.0, 0.0, 0.0, 0.5)),
        ))
        .with_children(|frame| {
            frame.spawn((
                MinimapImage,
                Node {
                    position_type: PositionType::Absolute,
                    ..default()
                },
                Visibility::Hidden,
            ));
            frame.spawn((
                MinimapIcons,
                Node {
                    position_type: PositionType::Absolute,
                    width: percent(100),
                    height: percent(100),
                    ..default()
                },
            ));
            // Us, in the middle: a dot and a line pointing where we look.
            frame
                .spawn((
                    PlayerHeading,
                    UiTransform::default(),
                    Node {
                        position_type: PositionType::Absolute,
                        left: px(SIZE / 2.0 - 2.0),
                        top: px(SIZE / 2.0 - 2.0),
                        width: px(0),
                        height: px(0),
                        ..default()
                    },
                ))
                .with_children(|heading| {
                    heading.spawn((
                        Node {
                            position_type: PositionType::Absolute,
                            left: px(-1),
                            top: px(-14),
                            width: px(2),
                            height: px(14),
                            ..default()
                        },
                        BackgroundColor(Color::srgba(1.0, 1.0, 1.0, 0.8)),
                    ));
                    heading.spawn((
                        Node {
                            position_type: PositionType::Absolute,
                            left: px(-4),
                            top: px(-4),
                            width: px(8),
                            height: px(8),
                            border_radius: BorderRadius::all(px(4)),
                            ..default()
                        },
                        BackgroundColor(Color::WHITE),
                    ));
                });
        });
}

fn set_map_image(
    mut commands: Commands,
    level: Res<LoadedLevel>,
    asset_server: Res<AssetServer>,
    image: Single<Entity, With<MinimapImage>>,
) {
    let mut image = commands.entity(*image);
    match &level.desc.minimap {
        Some(path) => {
            image.insert((
                ImageNode::new(asset_server.load(format!("imported://{path}"))),
                Visibility::Inherited,
            ));
        }
        None => {
            image.remove::<ImageNode>().insert(Visibility::Hidden);
        }
    }
}

/// World position to map pixels, for a map image of `map_px` pixels covering the terrain.
fn to_map(level: &LoadedLevel, position: Vec3, map_px: f32) -> Vec2 {
    let Some(heightmap) = &level.heightmap else {
        return Vec2::new(position.x, position.z) * (map_px / 1000.0);
    };
    let size = heightmap.world_size().max(1.0);
    let corner = heightmap.center() - Vec3::new(size, 0.0, size) * 0.5;
    Vec2::new(position.x - corner.x, position.z - corner.z) * (map_px / size)
}

#[allow(clippy::too_many_arguments, clippy::type_complexity)]
fn update_minimap(
    mut commands: Commands,
    level: Option<Res<LoadedLevel>>,
    camera: Single<&GlobalTransform, With<PlayerCamera>>,
    players: Query<&Team, With<LocalPlayer>>,
    teams: Query<&Team>,
    control_points: Query<(Entity, &ControlPoint, &FlagState)>,
    soldiers: Query<(Entity, &SoldierRender, &ControlledBy), (With<Soldier>, Without<LocalSoldier>)>,
    mut image: Single<&mut Node, (With<MinimapImage>, Without<Icon>, Without<PlayerHeading>)>,
    icons_root: Single<Entity, With<MinimapIcons>>,
    mut icons: Query<(Entity, &Icon, &mut Node, &mut BackgroundColor, &mut Visibility)>,
    mut heading: Single<&mut UiTransform, With<PlayerHeading>>,
) {
    let Some(level) = level else {
        return;
    };
    let map_px = level.heightmap.as_ref().map_or(1000.0, |h| h.world_size()) * SIZE / RANGE;
    let center = to_map(&level, camera.translation(), map_px);
    image.width = px(map_px);
    image.height = px(map_px);
    image.left = px(SIZE / 2.0 - center.x);
    image.top = px(SIZE / 2.0 - center.y);
    // Heading: yaw 0 looks north (-Z, up on the map), positive yaw turns left.
    let forward = camera.forward();
    heading.rotation = Rot2::radians((forward.x).atan2(-forward.z));

    let local = players.single().copied().unwrap_or_default();
    // What to show: flags always, teammates' soldiers.
    let mut wanted: HashMap<Entity, (Vec3, Color, f32)> = HashMap::default();
    for (entity, cp, state) in &control_points {
        wanted.insert(entity, (cp.position, team_color(state.owner, local), 12.0));
    }
    for (entity, render, controlled_by) in &soldiers {
        let team = teams.get(controlled_by.0).copied().unwrap_or_default();
        if team == local && local != Team::Spectator {
            wanted.insert(entity, (render.position, FRIENDLY, 6.0));
        }
    }

    for (icon_entity, icon, mut node, mut background, mut visibility) in &mut icons {
        let Some((position, color, size)) = wanted.remove(&icon.0) else {
            commands.entity(icon_entity).despawn();
            continue;
        };
        let at = to_map(&level, position, map_px) - center + Vec2::splat(SIZE / 2.0);
        let inside = at.x > -size && at.y > -size && at.x < SIZE + size && at.y < SIZE + size;
        visibility.set_if_neq(if inside { Visibility::Inherited } else { Visibility::Hidden });
        node.left = px(at.x - size / 2.0);
        node.top = px(at.y - size / 2.0);
        background.0 = color;
    }
    for (entity, (_, color, size)) in wanted {
        commands.entity(*icons_root).with_child((
            Icon(entity),
            Node {
                position_type: PositionType::Absolute,
                width: px(size),
                height: px(size),
                border: UiRect::all(px(1)),
                border_radius: BorderRadius::all(px(size / 2.0)),
                ..default()
            },
            BackgroundColor(color),
            BorderColor::all(Color::srgba(0.0, 0.0, 0.0, 0.7)),
            Visibility::Hidden,
        ));
    }
}
