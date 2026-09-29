//! The full-screen map, shown while M is held: the whole level with flags (named), our team,
//! our squad, vehicles, spotted enemies, orders (see `map_markers`) and us (in a vehicle, its
//! white icon). North is up; the mouse stays with the game.
//!
//! Zooms and pans like the deploy screen (`map_markers::MapView`/`drive_map_view`): the wheel,
//! a right/middle drag, `+`/`-`/the triggers, the arrows/the stick, a double click. [`BigMapView`]
//! resets on a new level; there's no reset button (M is usually held with the same hand as the
//! mouse), just the double click.

use bevy::{
    input::{
        gamepad::Gamepad,
        mouse::{AccumulatedMouseMotion, AccumulatedMouseScroll},
    },
    prelude::*,
    ui::RelativeCursorPosition,
};
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
    map_icons::map_uv,
    map_markers::{IconStyle, MapMarker, MapMarkers, MapPoint, MapView, MarkerIcons, NotMarker, SOLDIER_LAYER, apply_map_view, drive_map_view},
    net::{LocalPlayer, LocalSoldier},
    prediction::SoldierRender,
};

pub struct BigMapPlugin;

impl Plugin for BigMapPlugin {
    fn build(&self, app: &mut App) {
        app.init_resource::<BigMapView>()
            .add_systems(Startup, spawn_big_map)
            .add_systems(
                Update,
                (
                    set_map_image.run_if(resource_exists_and_changed::<LoadedLevel>),
                    show_big_map,
                    drive_big_map_view,
                    apply_big_map_view,
                    update_big_map,
                )
                    .chain(),
            );
    }
}

/// Pan and zoom of the big map: reset on a new level (`set_map_image`).
#[derive(Resource, Default)]
struct BigMapView(MapView);

#[derive(Component)]
struct BigMapRoot;
/// The fixed-size, clipped viewport the map shows through.
#[derive(Component)]
struct BigMapFrame;
/// The map image itself: resized/repositioned by [`apply_big_map_view`] to show the current
/// [`BigMapView`]; markers (see `map_markers`) are its children.
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
                BigMapFrame,
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
            .with_children(|frame| {
                frame
                    .spawn((
                        BigMapImage,
                        RelativeCursorPosition::default(),
                        Node {
                            position_type: PositionType::Absolute,
                            left: px(0),
                            top: px(0),
                            width: percent(100),
                            height: percent(100),
                            ..default()
                        },
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
        });
}

fn set_map_image(
    mut commands: Commands,
    level: Res<LoadedLevel>,
    asset_server: Res<AssetServer>,
    image: Single<Entity, With<BigMapImage>>,
    mut view: ResMut<BigMapView>,
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
    view.0.reset();
}

fn show_big_map(
    actions: crate::settings::Actions,
    deploy: Res<DeployScreen>,
    mut root: Single<&mut Visibility, With<BigMapRoot>>,
) {
    let show = actions.pressed(crate::settings::Action::Map) && !deploy.open;
    root.set_if_neq(if show { Visibility::Inherited } else { Visibility::Hidden });
}

/// Reads the mouse wheel, a right/middle drag, `+`/`-`/the triggers, the arrows/the stick and
/// a double click (see `map_markers::drive_map_view`) into the view, while the map shows.
#[allow(clippy::too_many_arguments)]
fn drive_big_map_view(
    mut view: ResMut<BigMapView>,
    root: Single<&Visibility, With<BigMapRoot>>,
    image: Single<(&RelativeCursorPosition, &ComputedNode), (With<BigMapImage>, Without<BigMapFrame>)>,
    keys: Res<ButtonInput<KeyCode>>,
    mouse: Res<ButtonInput<MouseButton>>,
    scroll: Res<AccumulatedMouseScroll>,
    motion: Res<AccumulatedMouseMotion>,
    gamepads: Query<&Gamepad>,
    time: Res<Time>,
    mut dragging: Local<bool>,
    mut last_click: Local<Option<(f32, Vec2)>>,
) {
    if **root == Visibility::Hidden {
        *dragging = false;
        return;
    }
    let (cursor, node) = *image;
    drive_map_view(
        &mut view.0,
        cursor,
        node.size,
        &keys,
        &mouse,
        &scroll,
        &motion,
        &gamepads,
        time.delta_secs(),
        time.elapsed_secs(),
        &mut *dragging,
        &mut *last_click,
    );
}

/// Resizes/repositions [`BigMapImage`] to show [`BigMapView`].
fn apply_big_map_view(
    view: Res<BigMapView>,
    frame: Single<&ComputedNode, (With<BigMapFrame>, Without<BigMapImage>)>,
    image: Single<&mut Node, (With<BigMapImage>, Without<BigMapFrame>)>,
) {
    let mut node = image.into_inner();
    apply_map_view(&mut node, view.0, frame.size);
}

#[allow(clippy::too_many_arguments, clippy::type_complexity)]
fn update_big_map(
    level: Option<Res<LoadedLevel>>,
    root: Single<&Visibility, (With<BigMapRoot>, NotMarker)>,
    camera: Single<&GlobalTransform, With<PlayerCamera>>,
    players: Query<(&Team, Option<&SquadMember>), With<LocalPlayer>>,
    teams: Query<(&Team, Option<&SquadMember>)>,
    soldiers: Query<(Entity, &SoldierRender, &ControlledBy), (With<Soldier>, Without<LocalSoldier>, Without<Seated>)>,
    image: Single<Entity, With<BigMapImage>>,
    frame: Single<&ComputedNode, With<BigMapFrame>>,
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
    // Sized for the 605 px map of a 720p window, a little larger on larger ones; the frame
    // (not the zoomed image) so markers keep a constant size regardless of zoom.
    icons.sync(*image, placed, IconStyle::big_map(*frame, 1.25, 605.0));
}
