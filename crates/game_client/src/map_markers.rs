//! What the maps show besides soldiers: flags, vehicles, spotted enemies, squad orders, the
//! commander's assets and what they are doing. Systems in [`MarkerSystems`] fill
//! [`MapMarkers`] every frame (`map_icons`, `radio`, `commander`); the minimap, the big map
//! and the commander screen draw them with [`MarkerIcons`]: a dot, an image framed by a dot of
//! the marker's colour (flags), or an image in the marker's colour turned with its heading
//! (vehicles), with a line for a turret and a label on the big maps.

use bevy::{ecs::system::SystemParam, prelude::*, ui::FocusPolicy};

use crate::{camera::CameraSystems, prediction::RenderStateSystems, vehicles::VehicleViewSystems};

pub struct MapMarkersPlugin;

impl Plugin for MapMarkersPlugin {
    fn build(&self, app: &mut App) {
        app.init_resource::<MapMarkers>()
            .configure_sets(
                PostUpdate,
                MarkerSystems
                    .after(RenderStateSystems)
                    .after(VehicleViewSystems)
                    .after(CameraSystems),
            )
            .add_systems(PostUpdate, clear.before(MarkerSystems));
    }
}

/// Systems that add [`MapMarkers`], in `PostUpdate` once soldiers and vehicles moved.
#[derive(SystemSet, Debug, Clone, PartialEq, Eq, Hash)]
pub struct MarkerSystems;

/// Stacking of markers: higher layers are drawn above lower ones.
pub const FLAG_LAYER: i32 = -1;
pub const SOLDIER_LAYER: i32 = 0;
pub const VEHICLE_LAYER: i32 = 1;
pub const ALERT_LAYER: i32 = 2;

#[derive(Clone, Debug)]
pub struct MapMarker {
    /// What it marks: an icon follows it.
    pub key: Entity,
    pub position: Vec3,
    pub color: Color,
    /// Diameter on the minimap, logical pixels (the big maps draw them larger).
    pub size: f32,
    /// Shown under it on the big maps.
    pub label: Option<String>,
    /// Drawn as this image instead of a dot: in `color` when `tint` (white silhouettes),
    /// else as it is on a dot of `color`.
    pub image: Option<Handle<Image>>,
    pub tint: bool,
    /// Width over height of a dot drawn as a rounded box (vehicles without an icon).
    pub aspect: f32,
    /// Clockwise from north, radians: the icon turns with it.
    pub heading: Option<f32>,
    /// A line from the middle pointing this way (a turret), clockwise from north.
    pub pointer: Option<f32>,
    pub layer: i32,
}

impl MapMarker {
    pub fn dot(key: Entity, position: Vec3, color: Color, size: f32) -> Self {
        Self {
            key,
            position,
            color,
            size,
            label: None,
            image: None,
            tint: false,
            aspect: 1.0,
            heading: None,
            pointer: None,
            layer: ALERT_LAYER,
        }
    }

    pub fn label(mut self, label: impl Into<String>) -> Self {
        self.label = Some(label.into());
        self
    }

    pub fn layer(mut self, layer: i32) -> Self {
        self.layer = layer;
        self
    }
}

#[derive(Resource, Default)]
pub struct MapMarkers(pub Vec<MapMarker>);

fn clear(mut markers: ResMut<MapMarkers>) {
    markers.0.clear();
}

/// Where a marker goes on a map.
#[derive(Clone, Copy, Debug)]
pub enum MapPoint {
    /// Logical pixels from the map's top left corner.
    Pixels(Vec2),
    /// Shares of the map's size (0..1).
    Share(Vec2),
}

/// How a map draws its markers.
#[derive(Clone, Copy, Debug)]
pub struct IconStyle {
    /// Sizes are multiplied by this.
    pub scale: f32,
    pub labels: bool,
    /// The map's rotation, clockwise radians (headings are turned back by it).
    pub turn: f32,
}

/// A map's icon for a marker, a child of the map.
#[derive(Component)]
pub struct MarkerIcon {
    key: Entity,
    shape: IconShape,
    body: Entity,
    pointer: Option<Entity>,
}

#[derive(Component)]
pub struct MarkerBody;

#[derive(Component)]
pub struct MarkerPointer;

/// What can't change without rebuilding the icon.
#[derive(Clone, PartialEq, Debug)]
struct IconShape {
    image: Option<AssetId<Image>>,
    tint: bool,
    /// Tenths of a pixel.
    width: i32,
    height: i32,
    label: Option<String>,
    pointer: bool,
    layer: i32,
}

/// Draws [`MapMarker`]s on a map.
#[derive(SystemParam)]
pub struct MarkerIcons<'w, 's> {
    commands: Commands<'w, 's>,
    icons: Query<
        'w,
        's,
        (Entity, &'static MarkerIcon, &'static ChildOf, &'static mut Node, &'static mut Visibility),
        (Without<MarkerBody>, Without<MarkerPointer>),
    >,
    bodies: Query<
        'w,
        's,
        (&'static mut UiTransform, Option<&'static mut BackgroundColor>, Option<&'static mut ImageNode>),
        (With<MarkerBody>, Without<MarkerIcon>, Without<MarkerPointer>),
    >,
    pointers: Query<'w, 's, &'static mut UiTransform, (With<MarkerPointer>, Without<MarkerBody>, Without<MarkerIcon>)>,
}

fn place(node: &mut Node, point: MapPoint) {
    match point {
        MapPoint::Pixels(at) => {
            node.left = px(at.x);
            node.top = px(at.y);
        }
        MapPoint::Share(at) => {
            node.left = percent(at.x * 100.0);
            node.top = percent(at.y * 100.0);
        }
    }
}

impl MarkerIcons<'_, '_> {
    /// Makes `parent`'s icons show `markers` (with where each goes and whether it shows).
    pub fn sync<'a>(&mut self, parent: Entity, markers: impl IntoIterator<Item = (&'a MapMarker, MapPoint, bool)>, style: IconStyle) {
        let mut wanted: bevy::platform::collections::HashMap<Entity, (&MapMarker, MapPoint, bool)> = markers
            .into_iter()
            .map(|(marker, point, shown)| (marker.key, (marker, point, shown)))
            .collect();
        for (entity, icon, child_of, mut node, mut visibility) in &mut self.icons {
            if child_of.parent() != parent {
                continue;
            }
            let Some(&(marker, point, shown)) = wanted.get(&icon.key) else {
                self.commands.entity(entity).despawn();
                continue;
            };
            if shape(marker, style) != icon.shape {
                self.commands.entity(entity).despawn();
                continue;
            }
            wanted.remove(&icon.key);
            place(&mut node, point);
            visibility.set_if_neq(if shown { Visibility::Inherited } else { Visibility::Hidden });
            if let Ok((mut transform, background, image)) = self.bodies.get_mut(icon.body) {
                let rotation = Rot2::radians(marker.heading.map_or(0.0, |h| h - style.turn));
                if transform.rotation != rotation {
                    transform.rotation = rotation;
                }
                match (image, background) {
                    (Some(mut image), _) if marker.tint => image.color = marker.color,
                    (_, Some(mut background)) => background.0 = marker.color,
                    _ => {}
                }
            }
            if let (Some(pointer), Some(angle)) = (icon.pointer, marker.pointer)
                && let Ok(mut transform) = self.pointers.get_mut(pointer)
            {
                transform.rotation = Rot2::radians(angle - style.turn);
            }
        }
        for (marker, point, shown) in wanted.into_values() {
            spawn(&mut self.commands, parent, marker, point, shown, style);
        }
    }
}

fn shape(marker: &MapMarker, style: IconStyle) -> IconShape {
    let size = marker.size * style.scale;
    let (width, height) = if marker.image.is_none() && marker.aspect != 1.0 {
        (size * marker.aspect, size)
    } else {
        (size, size)
    };
    IconShape {
        image: marker.image.as_ref().map(|i| i.id()),
        tint: marker.tint,
        width: (width * 10.0) as i32,
        height: (height * 10.0) as i32,
        label: marker.label.clone().filter(|_| style.labels),
        pointer: marker.pointer.is_some(),
        layer: marker.layer,
    }
}

const OUTLINE: Color = Color::srgba(0.0, 0.0, 0.0, 0.7);

fn spawn(commands: &mut Commands, parent: Entity, marker: &MapMarker, point: MapPoint, shown: bool, style: IconStyle) {
    let shape = shape(marker, style);
    let (width, height) = (shape.width as f32 / 10.0, shape.height as f32 / 10.0);
    let mut root_node = Node {
        position_type: PositionType::Absolute,
        width: px(0),
        height: px(0),
        ..default()
    };
    place(&mut root_node, point);
    let root = commands
        .spawn((
            root_node,
            ZIndex(marker.layer),
            FocusPolicy::Pass,
            if shown { Visibility::Inherited } else { Visibility::Hidden },
            ChildOf(parent),
        ))
        .id();
    let body_node = Node {
        position_type: PositionType::Absolute,
        left: px(-width / 2.0),
        top: px(-height / 2.0),
        width: px(width),
        height: px(height),
        ..default()
    };
    let rotation = UiTransform::from_rotation(Rot2::radians(marker.heading.map_or(0.0, |h| h - style.turn)));
    let body = match (&marker.image, marker.tint) {
        (Some(image), true) => commands
            .spawn((
                MarkerBody,
                body_node,
                rotation,
                ImageNode::new(image.clone()).with_color(marker.color),
                FocusPolicy::Pass,
                ChildOf(root),
            ))
            .id(),
        (Some(image), false) => {
            let body = commands
                .spawn((
                    MarkerBody,
                    Node {
                        border: UiRect::all(px(1)),
                        border_radius: BorderRadius::MAX,
                        ..body_node
                    },
                    rotation,
                    BackgroundColor(marker.color),
                    BorderColor::all(OUTLINE),
                    FocusPolicy::Pass,
                    ChildOf(root),
                ))
                .id();
            commands.spawn((
                ImageNode::new(image.clone()),
                Node {
                    position_type: PositionType::Absolute,
                    left: percent(-8),
                    top: percent(-14),
                    width: percent(116),
                    height: percent(116),
                    ..default()
                },
                FocusPolicy::Pass,
                ChildOf(body),
            ));
            body
        }
        (None, _) => commands
            .spawn((
                MarkerBody,
                Node {
                    border: UiRect::all(px(1)),
                    border_radius: if marker.aspect == 1.0 { BorderRadius::MAX } else { BorderRadius::all(px(2)) },
                    ..body_node
                },
                rotation,
                BackgroundColor(marker.color),
                BorderColor::all(OUTLINE),
                FocusPolicy::Pass,
                ChildOf(root),
            ))
            .id(),
    };
    let pointer = marker.pointer.map(|angle| {
        let length = height * 0.9;
        commands
            .spawn((
                MarkerPointer,
                Node {
                    position_type: PositionType::Absolute,
                    width: px(0),
                    height: px(0),
                    ..default()
                },
                UiTransform::from_rotation(Rot2::radians(angle - style.turn)),
                FocusPolicy::Pass,
                ChildOf(root),
                children![(
                    Node {
                        position_type: PositionType::Absolute,
                        left: px(-1),
                        top: px(-length),
                        width: px(2),
                        height: px(length),
                        ..default()
                    },
                    BackgroundColor(Color::srgba(1.0, 1.0, 1.0, 0.9)),
                    FocusPolicy::Pass,
                )],
            ))
            .id()
    });
    if let Some(label) = &shape.label {
        commands.spawn((
            Text::new(label.clone()),
            TextFont {
                font_size: FontSize::Px(13.0),
                ..default()
            },
            TextColor(Color::srgb(0.95, 0.96, 0.98)),
            TextShadow {
                offset: Vec2::splat(1.0),
                color: Color::srgba(0.0, 0.0, 0.0, 0.9),
            },
            TextLayout::justify(Justify::Center),
            Node {
                position_type: PositionType::Absolute,
                top: px(height / 2.0 + 2.0),
                left: px(-80),
                width: px(160),
                justify_content: JustifyContent::Center,
                ..default()
            },
            FocusPolicy::Pass,
            ChildOf(root),
        ));
    }
    commands.entity(root).insert(MarkerIcon {
        key: marker.key,
        shape,
        body,
        pointer,
    });
}
