//! The minimap, top right: the level's map around us, flags in their owners' colors,
//! teammates as dots, vehicles, spotted enemies and orders (see `map_markers`), and us with
//! our heading (in a vehicle, its white icon is us). It turns with us (N switches to north
//! up).

use bevy::{
    prelude::*,
    render::render_resource::AsBindGroup,
    shader::ShaderRef,
    ui_render::prelude::{MaterialNode, UiMaterial, UiMaterialPlugin},
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
    map_icons::to_map,
    map_markers::{IconStyle, MapMarker, MapMarkers, MapPoint, MarkerIcons, NotMarker, SOLDIER_LAYER},
    net::{LocalPlayer, LocalSoldier},
    prediction::SoldierRender,
};

pub struct MinimapPlugin;

impl Plugin for MinimapPlugin {
    fn build(&self, app: &mut App) {
        embedded_shader!(app, "minimap.wgsl");
        app.add_plugins(UiMaterialPlugin::<MinimapMaterial>::default())
            .init_resource::<MinimapSettings>()
            .add_systems(Startup, spawn_minimap)
            .add_systems(
                Update,
                (
                    toggle_rotation,
                    set_map_image.run_if(resource_exists_and_changed::<LoadedLevel>),
                    update_minimap,
                    apply_minimap_size.run_if(resource_changed::<crate::settings::Settings>),
                )
                    .chain(),
            );
    }
}

/// Side of the minimap in pixels.
pub const SIZE: f32 = 190.0;
/// Inside the 2 px border.
const INNER: f32 = SIZE - 4.0;
/// Meters shown across the minimap.
const RANGE: f32 = 260.0;
const BACKGROUND: Color = Color::srgb(0.14, 0.15, 0.16);

#[derive(Resource)]
pub struct MinimapSettings {
    /// Turn the map with the view (our heading points up) instead of keeping north up.
    pub rotating: bool,
}

impl Default for MinimapSettings {
    fn default() -> Self {
        Self { rotating: true }
    }
}

/// Draws the map texture turned and zoomed around us (see `minimap.wgsl`).
#[derive(AsBindGroup, Asset, TypePath, Debug, Clone)]
struct MinimapMaterial {
    #[uniform(0)]
    params: MinimapParams,
    #[texture(1)]
    #[sampler(2)]
    map: Handle<Image>,
}

#[derive(bevy::render::render_resource::ShaderType, Debug, Clone, Copy)]
struct MinimapParams {
    /// xy: our position on the map (0..1), z: rotation (clockwise from north), w: span.
    view: Vec4,
    background: LinearRgba,
}

impl UiMaterial for MinimapMaterial {
    fn fragment_shader() -> ShaderRef {
        "embedded://client/minimap.wgsl".into()
    }
}

#[derive(Component)]
struct MinimapMap;
#[derive(Component)]
struct MinimapIcons;
#[derive(Component)]
struct PlayerHeading;

/// The minimap's outer frame, for `apply_minimap_size` (the `minimap_size` setting).
#[derive(Component)]
struct MinimapRoot;

/// Scales the minimap frame by the `minimap_size` setting (1.0 = the base [`SIZE`]).
fn apply_minimap_size(settings: Res<crate::settings::Settings>, mut root: Query<&mut Node, With<MinimapRoot>>) {
    let Ok(mut node) = root.single_mut() else {
        return;
    };
    let size = px(SIZE * settings.minimap_size.clamp(0.5, 1.75));
    node.width = size;
    node.height = size;
}

fn spawn_minimap(mut commands: Commands) {
    commands
        .spawn((
            MinimapRoot,
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
            BackgroundColor(BACKGROUND),
            BorderColor::all(Color::srgba(0.0, 0.0, 0.0, 0.5)),
        ))
        .with_children(|frame| {
            frame.spawn((
                MinimapMap,
                Node {
                    position_type: PositionType::Absolute,
                    width: percent(100),
                    height: percent(100),
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
                        left: px(INNER / 2.0),
                        top: px(INNER / 2.0),
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

fn toggle_rotation(actions: crate::settings::Actions, mut settings: ResMut<MinimapSettings>) {
    if actions.just_pressed(crate::settings::Action::MinimapRotation) {
        settings.rotating = !settings.rotating;
    }
}

fn set_map_image(
    mut commands: Commands,
    level: Res<LoadedLevel>,
    asset_server: Res<AssetServer>,
    mut materials: ResMut<Assets<MinimapMaterial>>,
    map: Single<Entity, With<MinimapMap>>,
) {
    let mut map = commands.entity(*map);
    match &level.desc.minimap {
        Some(path) => {
            let material = materials.add(MinimapMaterial {
                params: MinimapParams {
                    view: Vec4::new(0.5, 0.5, 0.0, 1.0),
                    background: BACKGROUND.into(),
                },
                map: asset_server.load(format!("imported://{path}")),
            });
            map.insert((MaterialNode(material), Visibility::Inherited));
        }
        None => {
            map.remove::<MaterialNode<MinimapMaterial>>().insert(Visibility::Hidden);
        }
    }
}

#[allow(clippy::too_many_arguments, clippy::type_complexity)]
fn update_minimap(
    settings: Res<MinimapSettings>,
    level: Option<Res<LoadedLevel>>,
    camera: Single<&GlobalTransform, With<PlayerCamera>>,
    players: Query<(&Team, Option<&SquadMember>), With<LocalPlayer>>,
    teams: Query<(&Team, Option<&SquadMember>)>,
    soldiers: Query<
        (Entity, &SoldierRender, &ControlledBy, Has<game_shared::revive::Downed>),
        (With<Soldier>, Without<LocalSoldier>, Without<Seated>),
    >,
    map: Single<Option<&MaterialNode<MinimapMaterial>>, With<MinimapMap>>,
    mut materials: ResMut<Assets<MinimapMaterial>>,
    icons_root: Single<Entity, With<MinimapIcons>>,
    mut icons: MarkerIcons,
    mut heading: Single<(&mut UiTransform, &mut Visibility), (With<PlayerHeading>, NotMarker)>,
    seated: Query<(), (With<LocalSoldier>, With<Seated>)>,
    markers: Res<MapMarkers>,
) {
    let Some(level) = level else {
        return;
    };
    let (center, map_meters) = to_map(&level, camera.translation());
    // Heading clockwise from north (-Z, up on the map).
    let forward = camera.forward();
    let heading_angle = forward.x.atan2(-forward.z);
    let map_angle = if settings.rotating { heading_angle } else { 0.0 };
    let (transform, visibility) = &mut *heading;
    let rotation = Rot2::radians(heading_angle - map_angle);
    if transform.rotation != rotation {
        transform.rotation = rotation;
    }
    // In a vehicle its icon shows where we are (the dot would cover its turret).
    visibility.set_if_neq(if seated.is_empty() { Visibility::Inherited } else { Visibility::Hidden });
    // Only when it moved: a changed material is prepared (and uploaded) again.
    let view = Vec4::new(center.x, center.y, map_angle, RANGE / map_meters);
    if let Some(map) = *map
        && materials.get(&map.0).is_some_and(|m| m.params.view != view)
        && let Some(mut material) = materials.get_mut(&map.0)
    {
        material.params.view = view;
    }

    let (local, local_squad) = players
        .single()
        .map(|(team, squad)| (*team, squad.copied()))
        .unwrap_or_default();
    // Teammates on foot (those in vehicles show with the vehicle).
    let mut teammates = Vec::new();
    for (entity, render, controlled_by, downed) in &soldiers {
        let (team, squad) = teams.get(controlled_by.0).map(|(t, s)| (*t, s.copied())).unwrap_or_default();
        if team == local && local != Team::Spectator {
            let squad_mate = local_squad.zip(squad).is_some_and(|(a, b)| a.squad == b.squad);
            // Critically wounded teammates stand out, for medics.
            let (color, size) = match (downed, squad_mate) {
                (true, _) => (crate::wounded::WOUNDED, 9.0),
                (false, true) => (SQUAD, 6.0),
                (false, false) => (FRIENDLY, 6.0),
            };
            teammates.push(MapMarker::dot(entity, render.position, color, size).layer(SOLDIER_LAYER));
        }
    }

    // Map offsets to minimap pixels, turned the opposite way to the map.
    let pixels_per_unit = INNER * map_meters / RANGE;
    let turn = Rot2::radians(-map_angle);
    let placed = teammates.iter().chain(&markers.0).map(|marker| {
        let offset = turn * ((to_map(&level, marker.position).0 - center) * pixels_per_unit);
        let reach = marker.size;
        let inside = offset.x.abs() < INNER / 2.0 + reach && offset.y.abs() < INNER / 2.0 + reach;
        (marker, MapPoint::Pixels(offset + Vec2::splat(INNER / 2.0)), inside)
    });
    icons.sync(
        *icons_root,
        placed,
        IconStyle {
            scale: 1.0,
            labels: false,
            turn: map_angle,
            size: Vec2::splat(INNER),
        },
    );
}
