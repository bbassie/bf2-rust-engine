//! The minimap, top right: the level's map around us, flags in their owners' colors,
//! teammates as dots, and us with our heading. It turns with us (N switches to north up).

use bevy::{
    asset::embedded_asset,
    platform::collections::HashMap,
    prelude::*,
    render::render_resource::AsBindGroup,
    shader::ShaderRef,
    ui_render::prelude::{MaterialNode, UiMaterial, UiMaterialPlugin},
};
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
        embedded_asset!(app, "minimap.wgsl");
        app.add_plugins(UiMaterialPlugin::<MinimapMaterial>::default())
            .init_resource::<MinimapSettings>()
            .add_systems(Startup, spawn_minimap)
            .add_systems(
                Update,
                (
                    toggle_rotation,
                    set_map_image.run_if(resource_exists_and_changed::<LoadedLevel>),
                    update_minimap,
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

/// World position to map coordinates (0..1 across the terrain, north up) and the map's
/// width in meters.
fn to_map(level: &LoadedLevel, position: Vec3) -> (Vec2, f32) {
    let Some(heightmap) = &level.heightmap else {
        return (Vec2::new(position.x, position.z) / 1000.0 + 0.5, 1000.0);
    };
    let size = heightmap.world_size().max(1.0);
    let corner = heightmap.center() - Vec3::new(size, 0.0, size) * 0.5;
    (Vec2::new(position.x - corner.x, position.z - corner.z) / size, size)
}

#[allow(clippy::too_many_arguments, clippy::type_complexity)]
fn update_minimap(
    mut commands: Commands,
    settings: Res<MinimapSettings>,
    level: Option<Res<LoadedLevel>>,
    camera: Single<&GlobalTransform, With<PlayerCamera>>,
    players: Query<&Team, With<LocalPlayer>>,
    teams: Query<&Team>,
    control_points: Query<(Entity, &ControlPoint, &FlagState)>,
    soldiers: Query<(Entity, &SoldierRender, &ControlledBy), (With<Soldier>, Without<LocalSoldier>)>,
    map: Single<Option<&MaterialNode<MinimapMaterial>>, With<MinimapMap>>,
    mut materials: ResMut<Assets<MinimapMaterial>>,
    icons_root: Single<Entity, With<MinimapIcons>>,
    mut icons: Query<(Entity, &Icon, &mut Node, &mut BackgroundColor, &mut Visibility)>,
    mut heading: Single<&mut UiTransform, With<PlayerHeading>>,
) {
    let Some(level) = level else {
        return;
    };
    let (center, map_meters) = to_map(&level, camera.translation());
    // Heading clockwise from north (-Z, up on the map).
    let forward = camera.forward();
    let heading_angle = forward.x.atan2(-forward.z);
    let map_angle = if settings.rotating { heading_angle } else { 0.0 };
    heading.rotation = Rot2::radians(heading_angle - map_angle);
    if let Some(mut material) = map.and_then(|m| materials.get_mut(&m.0)) {
        material.params.view = Vec4::new(center.x, center.y, map_angle, RANGE / map_meters);
    }

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

    // Map offsets to minimap pixels, turned the opposite way to the map.
    let pixels_per_unit = INNER * map_meters / RANGE;
    let turn = Rot2::radians(-map_angle);
    for (icon_entity, icon, mut node, mut background, mut visibility) in &mut icons {
        let Some((position, color, size)) = wanted.remove(&icon.0) else {
            commands.entity(icon_entity).despawn();
            continue;
        };
        let offset = turn * ((to_map(&level, position).0 - center) * pixels_per_unit);
        let at = offset + Vec2::splat(INNER / 2.0);
        let inside = offset.x.abs() < INNER / 2.0 + size && offset.y.abs() < INNER / 2.0 + size;
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
