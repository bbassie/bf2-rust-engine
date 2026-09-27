//! The full-screen map, shown while M is held: the whole level with flags (named), our team,
//! our squad, vehicles, spotted enemies, orders (see `map_markers`) and us (in a vehicle, its
//! white icon). North is up; the mouse stays with the game.

use bevy::prelude::*;
use game_shared::{
    level::LoadedLevel,
    protocol::{ControlledBy, Team},
    soldier::Soldier,
    squad::SquadMember,
    vehicle::Seated,
};

use crate::{
    camera::PlayerCamera,
    conquest_hud::{FRIENDLY, SQUAD},
    deploy::DeployScreen,
    map_markers::{IconStyle, MapMarker, MapMarkers, MapPoint, MarkerIcons, NotMarker, SOLDIER_LAYER},
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


#[derive(Component)]
struct BigMapRoot;
#[derive(Component)]
struct BigMapImage;
#[derive(Component)]
struct BigMapHeading;

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
    level: Option<Res<LoadedLevel>>,
    root: Single<&Visibility, (With<BigMapRoot>, NotMarker)>,
    camera: Single<&GlobalTransform, With<PlayerCamera>>,
    players: Query<(&Team, Option<&SquadMember>), With<LocalPlayer>>,
    teams: Query<(&Team, Option<&SquadMember>)>,
    soldiers: Query<(Entity, &SoldierRender, &ControlledBy), (With<Soldier>, Without<LocalSoldier>, Without<Seated>)>,
    image: Single<(Entity, &ComputedNode), With<BigMapImage>>,
    mut icons: MarkerIcons,
    mut heading: Single<(&mut Node, &mut UiTransform, &mut Visibility), (With<BigMapHeading>, Without<BigMapRoot>, NotMarker)>,
    seated: Query<(), (With<LocalSoldier>, With<Seated>)>,
    markers: Res<MapMarkers>,
) {
    let Some(level) = level else {
        return;
    };
    if **root == Visibility::Hidden {
        return;
    }
    let at = |uv: Vec2| (percent(uv.x.clamp(0.0, 1.0) * 100.0), percent(uv.y.clamp(0.0, 1.0) * 100.0));

    let (node, transform, visibility) = &mut *heading;
    (node.left, node.top) = at(map_uv(&level, camera.translation()));
    let forward = camera.forward();
    transform.rotation = Rot2::radians(forward.x.atan2(-forward.z));
    // In a vehicle its icon shows where we are.
    visibility.set_if_neq(if seated.is_empty() { Visibility::Inherited } else { Visibility::Hidden });

    let (local, local_squad) = players
        .single()
        .map(|(team, squad)| (*team, squad.copied()))
        .unwrap_or_default();
    // Teammates on foot (squad mates in green); flags, vehicles and the rest are markers.
    let mut teammates = Vec::new();
    for (entity, render, controlled_by) in &soldiers {
        let (team, squad) = teams.get(controlled_by.0).map(|(t, s)| (*t, s.copied())).unwrap_or_default();
        if team == local && local != Team::Spectator {
            let squad_mate = local_squad.zip(squad).is_some_and(|(a, b)| a.squad == b.squad);
            let color = if squad_mate { SQUAD } else { FRIENDLY };
            teammates.push(MapMarker::dot(entity, render.position, color, 6.5).layer(SOLDIER_LAYER));
        }
    }
    let placed = teammates
        .iter()
        .chain(&markers.0)
        .map(|marker| (marker, MapPoint::Share(map_uv(&level, marker.position).clamp(Vec2::ZERO, Vec2::ONE)), true));
    let (image, node) = *image;
    // Sized for the 605 px map of a 720p window, a little larger on larger ones.
    icons.sync(image, placed, IconStyle::big_map(node, 1.25, 605.0));
}
