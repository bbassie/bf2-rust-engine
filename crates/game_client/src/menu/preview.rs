//! The map preview of the level pages: the level's map with what the picked layout puts on
//! it, drawn like the deploy screen: control points with their names and first owners'
//! flags (main bases crossed out: they can't be captured), and the vehicles each side's
//! spawners make, turned the way they face.

use game_data::VehicleClass;
use game_shared::protocol::Team;

use super::*;
use crate::{
    conquest_hud::{NEUTRAL, team_color},
    map_icons::FLAG_CLOTH,
};

/// Size of a control point's icon and of a vehicle's, in pixels, on a 300 px preview.
const FLAG: f32 = 28.0;
const VEHICLE: f32 = 13.0;

/// The team a layout's `initial_team` (0 neutral, 1, 2) means.
fn side(team: u8) -> Team {
    match team {
        1 => Team::One,
        2 => Team::Two,
        _ => Team::Spectator,
    }
}

/// The map of `level` with the objectives of its `mode`/`size` layout, `size` pixels
/// across; colours as seen by a player of `local`.
pub(super) fn layout_preview(
    p: &mut ChildSpawnerCommands,
    level: &LevelInfo,
    layout: Option<(&str, u32)>,
    local: Team,
    size: f32,
    asset_server: &AssetServer,
) {
    let scale = (size / 300.0).clamp(0.6, 1.2);
    let labels = size >= 200.0;
    p.spawn((
        Node {
            width: px(size),
            height: px(size),
            flex_shrink: 0.0,
            border_radius: BorderRadius::all(px(8)),
            overflow: Overflow::clip(),
            justify_content: JustifyContent::Center,
            align_items: AlignItems::Center,
            ..default()
        },
        BackgroundColor(MAP_BACKGROUND),
    ))
    .with_children(|frame| {
        match &level.minimap {
            Some(path) => {
                frame.spawn((
                    ImageNode::new(asset_server.load(format!("imported://{path}"))),
                    Node {
                        position_type: PositionType::Absolute,
                        width: percent(100),
                        height: percent(100),
                        ..default()
                    },
                ));
            }
            None => {
                frame.spawn(text("No map preview", 14.0, DIM));
            }
        }
        let (Some(preview), Some((corner, meters))) =
            (layout.and_then(|(mode, size)| level.preview(mode, size)), level.map_area)
        else {
            return;
        };
        let at = |position: [f32; 3]| {
            let uv = ((Vec2::new(position[0], position[2]) - corner) / meters).clamp(Vec2::ZERO, Vec2::ONE);
            (percent(uv.x * 100.0), percent(uv.y * 100.0))
        };
        let load = |path: &Option<String>| path.as_ref().map(|p| asset_server.load::<Image>(format!("imported://{p}")));

        // Vehicles, under the flags: each spawner's vehicle for the side holding its point.
        for spawner in &preview.vehicles {
            let owner = spawner
                .control_point
                .as_ref()
                .and_then(|id| preview.control_points.iter().find(|cp| &cp.id == id))
                .map_or(0, |cp| cp.initial_team);
            let index = if owner == 2 { 1 } else { 0 };
            let Some(template) = spawner.templates[index].as_ref().or(spawner.templates[1 - index].as_ref()) else {
                continue;
            };
            let icon = level.vehicle_icons.get(&template.to_ascii_lowercase());
            if icon.is_some_and(|i| i.class == VehicleClass::Stationary && i.icon.is_none()) || icon.is_none() {
                continue;
            }
            let color = if owner == 0 { NEUTRAL } else { team_color(side(owner), local) };
            let forward = Quat::from_array(spawner.placement.rotation) * Vec3::NEG_Z;
            let heading = forward.x.atan2(-forward.z);
            let (left, top) = at(spawner.placement.position);
            let edge = VEHICLE * scale;
            frame
                .spawn(Node {
                    position_type: PositionType::Absolute,
                    left,
                    top,
                    width: px(0),
                    height: px(0),
                    ..default()
                })
                .with_children(|anchor| {
                    let body = Node {
                        position_type: PositionType::Absolute,
                        left: px(-edge / 2.0),
                        top: px(-edge / 2.0),
                        width: px(edge),
                        height: px(edge),
                        ..default()
                    };
                    let rotation = UiTransform::from_rotation(Rot2::radians(heading));
                    match load(&icon.and_then(|i| i.icon.clone())) {
                        Some(image) => {
                            anchor.spawn((body, rotation, ImageNode::new(image).with_color(color)));
                        }
                        None => {
                            anchor.spawn((
                                Node {
                                    width: px(edge * 0.6),
                                    left: px(-edge * 0.3),
                                    border_radius: BorderRadius::all(px(2)),
                                    ..body
                                },
                                rotation,
                                BackgroundColor(color),
                            ));
                        }
                    }
                });
        }

        // Control points: the first owner's flag (its pole on the point, the cloth on a box of
        // the owner's colour; a dot without an icon), and the name.
        for cp in &preview.control_points {
            let owner = side(cp.initial_team);
            let icons = &level.icons[(cp.initial_team as usize).min(2)];
            let image = if cp.uncapturable {
                load(&icons.base).or_else(|| load(&icons.map))
            } else {
                load(&icons.map)
            };
            let (left, top) = at(cp.position);
            let color = team_color(owner, local);
            let edge = FLAG * scale;
            frame
                .spawn(Node {
                    position_type: PositionType::Absolute,
                    left,
                    top,
                    width: px(0),
                    height: px(0),
                    ..default()
                })
                .with_children(|anchor| {
                    match image {
                        Some(image) => {
                            let cloth = FLAG_CLOTH;
                            anchor
                                .spawn((
                                    Node {
                                        position_type: PositionType::Absolute,
                                        left: px(-edge / 2.0 + cloth.min.x * edge),
                                        top: px(-edge / 2.0 + cloth.min.y * edge),
                                        width: px(cloth.width() * edge),
                                        height: px(cloth.height() * edge),
                                        border_radius: BorderRadius::all(px(2)),
                                        ..default()
                                    },
                                    BackgroundColor(color),
                                ))
                                .with_child((
                                    ImageNode::new(image),
                                    Node {
                                        position_type: PositionType::Absolute,
                                        left: px(-cloth.min.x * edge),
                                        top: px(-cloth.min.y * edge),
                                        width: px(edge),
                                        height: px(edge),
                                        ..default()
                                    },
                                ));
                        }
                        None => {
                            let dot = edge * 0.5;
                            anchor.spawn((
                                Node {
                                    position_type: PositionType::Absolute,
                                    left: px(-dot / 2.0),
                                    top: px(-dot / 2.0),
                                    width: px(dot),
                                    height: px(dot),
                                    border: UiRect::all(px(1.5)),
                                    border_radius: BorderRadius::MAX,
                                    ..default()
                                },
                                BackgroundColor(color),
                                BorderColor::all(Color::srgba(0.0, 0.0, 0.0, 0.6)),
                            ));
                        }
                    }
                    if labels && !cp.name.is_empty() {
                        anchor.spawn((
                            text(cp.name.clone(), 11.0, TEXT),
                            TextShadow {
                                offset: Vec2::splat(1.0),
                                color: Color::srgba(0.0, 0.0, 0.0, 0.9),
                            },
                            TextLayout::justify(Justify::Center),
                            Node {
                                position_type: PositionType::Absolute,
                                top: px(4.0),
                                left: px(-60),
                                width: px(120),
                                justify_content: JustifyContent::Center,
                                ..default()
                            },
                        ));
                    }
                });
        }
    });
}

/// A team's flag for a menu, `size` pixels high (nothing without one).
pub(super) fn team_flag(p: &mut ChildSpawnerCommands, icons: &game_data::TeamIcons, height: f32, asset_server: &AssetServer) {
    let (path, aspect) = match (&icons.large, &icons.menu) {
        (Some(large), _) => (large, 2.0),
        (None, Some(menu)) => (menu, 1.0),
        (None, None) => return,
    };
    p.spawn((
        ImageNode::new(asset_server.load(format!("imported://{path}"))),
        Node {
            width: px(height * aspect),
            height: px(height),
            border_radius: BorderRadius::all(px(3)),
            ..default()
        },
    ));
}
