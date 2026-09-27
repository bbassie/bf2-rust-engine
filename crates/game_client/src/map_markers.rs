//! What the maps show besides soldiers: flags, vehicles, spotted enemies, squad orders, the
//! commander's assets and what they are doing. Systems in [`MarkerSystems`] fill
//! [`MapMarkers`] every frame (`map_icons`, `radio`, `commander`); the minimap, the big map
//! and the commander screen draw them with [`MarkerIcons`]: a dot, an image framed by a dot of
//! the marker's colour (flags), or an image in the marker's colour with a dark halo, turned
//! with its heading (vehicles), with an outlined line for a turret.
//!
//! On the big maps markers are labelled. [`place_labels`] keeps labels from covering each
//! other: each tries a few spots around its icon (below first), then a smaller font, and is
//! left out when nothing fits; labels of higher priority (control points) go first.

use bevy::{ecs::system::SystemParam, platform::collections::HashMap, prelude::*, ui::FocusPolicy};

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
/// Labels, above every icon.
const LABEL_LAYER: i32 = 3;

/// Label priorities (see [`MapMarker::priority`]).
pub const CONTROL_POINT_LABEL: i32 = 3;
pub const SQUAD_LABEL: i32 = 2;

#[derive(Clone, Debug)]
pub struct MapMarker {
    /// What it marks: an icon follows it.
    pub key: Entity,
    pub position: Vec3,
    pub color: Color,
    /// Diameter on the minimap, logical pixels (the big maps draw them larger).
    pub size: f32,
    /// Shown next to it on the big maps.
    pub label: Option<String>,
    /// Labels of higher priority are placed first and win crowded spots (control point
    /// names before asset names); equal ones go by layer, higher first.
    pub priority: i32,
    /// Drawn as this image instead of a dot: in `color` with a dark halo when `tint` (white
    /// silhouettes), else as it is on a dot of `color`, or on a box of `color` covering
    /// `frame` (a part of the image, in shares of its size).
    pub image: Option<Handle<Image>>,
    pub tint: bool,
    pub frame: Option<Rect>,
    /// The part of the image labels keep clear of, in shares of its size (all of it if none).
    pub bounds: Option<Rect>,
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
            priority: 0,
            image: None,
            tint: false,
            frame: None,
            bounds: None,
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

    pub fn priority(mut self, priority: i32) -> Self {
        self.priority = priority;
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

impl MapPoint {
    fn vals(self) -> (Val, Val) {
        match self {
            MapPoint::Pixels(at) => (px(at.x), px(at.y)),
            MapPoint::Share(at) => (percent(at.x * 100.0), percent(at.y * 100.0)),
        }
    }

    fn pixels(self, map: Vec2) -> Vec2 {
        match self {
            MapPoint::Pixels(at) => at,
            MapPoint::Share(at) => at * map,
        }
    }
}

/// How a map draws its markers.
#[derive(Clone, Copy, Debug)]
pub struct IconStyle {
    /// Sizes (and label fonts) are multiplied by this.
    pub scale: f32,
    pub labels: bool,
    /// The map's rotation, clockwise radians (headings are turned back by it).
    pub turn: f32,
    /// The map's inner size, logical pixels: what [`MapPoint::Share`]s are shares of, and
    /// where labels have to stay.
    pub size: Vec2,
}

impl IconStyle {
    /// A labelled, north-up map drawn in `node`: sizes times `scale`, growing a little on
    /// maps larger than `reference` logical pixels (bigger windows).
    pub fn big_map(node: &ComputedNode, scale: f32, reference: f32) -> Self {
        let inner = (node.size - node.border.min_inset - node.border.max_inset) * node.inverse_scale_factor;
        let grow = if inner.x > 0.0 { (inner.x / reference).sqrt().clamp(1.0, 1.35) } else { 1.0 };
        Self {
            scale: scale * grow,
            labels: true,
            turn: 0.0,
            size: inner,
        }
    }
}

/// A map's icon for a marker, a child of the map.
#[derive(Component)]
pub struct MarkerIcon {
    key: Entity,
    shape: IconShape,
    body: Entity,
    pointer: Option<Entity>,
    /// A sibling of the icon (so that it draws above every icon).
    label: Option<Entity>,
}

#[derive(Component)]
pub struct MarkerBody;

#[derive(Component)]
pub struct MarkerPointer;

#[derive(Component)]
pub struct MarkerLabel {
    /// The spot it took last (see [`LabelSpot::index`]).
    spot: Option<u8>,
}

/// Filter for queries in systems with a [`MarkerIcons`] that touch `Node`, `Visibility`,
/// `UiTransform`, `BackgroundColor` or `TextFont`: keeps them apart from the icons'.
pub type NotMarker = (Without<MarkerIcon>, Without<MarkerBody>, Without<MarkerPointer>, Without<MarkerLabel>);

/// What can't change without rebuilding the icon.
#[derive(Clone, PartialEq, Debug)]
struct IconShape {
    image: Option<AssetId<Image>>,
    tint: bool,
    /// The colour of a tinted image (its halo is made of copies).
    tint_color: Option<[u8; 4]>,
    /// Hundredths of the image.
    frame: Option<[i32; 4]>,
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
        (Without<MarkerBody>, Without<MarkerPointer>, Without<MarkerLabel>),
    >,
    bodies: Query<
        'w,
        's,
        (&'static mut UiTransform, Option<&'static mut BackgroundColor>),
        (With<MarkerBody>, Without<MarkerIcon>, Without<MarkerPointer>, Without<MarkerLabel>),
    >,
    pointers: Query<
        'w,
        's,
        &'static mut UiTransform,
        (With<MarkerPointer>, Without<MarkerBody>, Without<MarkerIcon>, Without<MarkerLabel>),
    >,
    labels: Query<
        'w,
        's,
        (&'static mut MarkerLabel, &'static mut Node, &'static mut TextFont, &'static mut Visibility),
        (Without<MarkerIcon>, Without<MarkerBody>, Without<MarkerPointer>),
    >,
}

/// Moves a node to `point`, touching it only if it moved.
fn place(node: &mut Mut<Node>, point: MapPoint) {
    let (left, top) = point.vals();
    if node.left != left || node.top != top {
        node.left = left;
        node.top = top;
    }
}

impl MarkerIcons<'_, '_> {
    /// Makes `parent`'s icons show `markers` (with where each goes and whether it shows).
    pub fn sync<'a>(&mut self, parent: Entity, markers: impl IntoIterator<Item = (&'a MapMarker, MapPoint, bool)>, style: IconStyle) {
        let markers: Vec<(&MapMarker, MapPoint, bool)> = markers.into_iter().collect();
        // Where the labels are, so that they stay there while it still fits.
        let mut previous: HashMap<Entity, u8> = HashMap::default();
        if style.labels {
            for (_, icon, child_of, _, _) in &self.icons {
                if child_of.parent() == parent
                    && let Some(label) = icon.label
                    && let Ok((state, ..)) = self.labels.get(label)
                    && let Some(spot) = state.spot
                {
                    previous.insert(icon.key, spot);
                }
            }
        }
        let spots = if style.labels { label_spots(&markers, style, &previous) } else { HashMap::default() };

        let mut wanted: HashMap<Entity, (&MapMarker, MapPoint, bool)> =
            markers.iter().map(|&(marker, point, shown)| (marker.key, (marker, point, shown))).collect();
        for (entity, icon, child_of, mut node, mut visibility) in &mut self.icons {
            if child_of.parent() != parent {
                continue;
            }
            let current = wanted.get(&icon.key).copied().filter(|(marker, ..)| shape(marker, style) == icon.shape);
            let Some((marker, point, shown)) = current else {
                self.commands.entity(entity).despawn();
                if let Some(label) = icon.label {
                    self.commands.entity(label).despawn();
                }
                continue;
            };
            wanted.remove(&icon.key);
            place(&mut node, point);
            visibility.set_if_neq(if shown { Visibility::Inherited } else { Visibility::Hidden });
            if let Ok((mut transform, background)) = self.bodies.get_mut(icon.body) {
                let rotation = Rot2::radians(marker.heading.map_or(0.0, |h| h - style.turn));
                if transform.rotation != rotation {
                    transform.rotation = rotation;
                }
                // Every node has a background: a silhouette's (see `silhouette`) stays clear.
                let painted = !(marker.tint && marker.image.is_some());
                if let Some(mut background) = background
                    && painted
                    && background.0 != marker.color
                {
                    background.0 = marker.color;
                }
            }
            if let (Some(pointer), Some(angle)) = (icon.pointer, marker.pointer)
                && let Ok(mut transform) = self.pointers.get_mut(pointer)
            {
                transform.rotation = Rot2::radians(angle - style.turn);
            }
            if let Some(label) = icon.label
                && let Ok((mut state, mut node, mut font, mut visibility)) = self.labels.get_mut(label)
            {
                match spots.get(&icon.key).filter(|_| shown) {
                    Some(spot) => {
                        place(&mut node, point);
                        let margin = UiRect {
                            left: px(spot.offset.x),
                            top: px(spot.offset.y),
                            ..default()
                        };
                        if node.margin != margin || node.width != px(spot.size.x) {
                            node.margin = margin;
                            node.width = px(spot.size.x);
                            node.height = px(spot.size.y);
                        }
                        let size = FontSize::Px(label_font(style) * spot.scale);
                        if font.font_size != size {
                            font.font_size = size;
                        }
                        state.spot = Some(spot.index);
                        visibility.set_if_neq(Visibility::Inherited);
                    }
                    None => {
                        state.spot = None;
                        visibility.set_if_neq(Visibility::Hidden);
                    }
                }
            }
        }
        for (marker, point, shown) in wanted.into_values() {
            spawn(&mut self.commands, parent, marker, point, shown, style, spots.get(&marker.key));
        }
    }
}

/// Label font size on a map.
fn label_font(style: IconStyle) -> f32 {
    10.5 * style.scale
}

/// Where the labels of `markers` go on a map (only those that fit).
fn label_spots(
    markers: &[(&MapMarker, MapPoint, bool)],
    style: IconStyle,
    previous: &HashMap<Entity, u8>,
) -> HashMap<Entity, LabelSpot> {
    let mut obstacles = Vec::new();
    let mut requests = Vec::new();
    let mut keys = Vec::new();
    for &(marker, point, shown) in markers {
        if !shown {
            continue;
        }
        let at = point.pixels(style.size);
        let icon = icon_box(marker, style);
        let label = marker.label.as_deref().filter(|l| !l.is_empty());
        // Soldiers move too much to steer labels around, unless they are labelled.
        if marker.layer != SOLDIER_LAYER || label.is_some() {
            obstacles.push(Rect::from_corners(at + icon.min, at + icon.max));
        }
        if let Some(label) = label {
            requests.push(LabelRequest {
                at,
                icon,
                chars: label.chars().count(),
                font: label_font(style),
                priority: marker.priority * 8 + marker.layer,
                previous: previous.get(&marker.key).copied(),
                own: Some(obstacles.len() - 1),
            });
            keys.push(marker.key);
        }
    }
    let area = if style.size.min_element() > 0.0 {
        Rect::from_corners(Vec2::ZERO, style.size)
    } else {
        Rect::from_center_half_size(Vec2::ZERO, Vec2::splat(f32::MAX / 4.0))
    };
    keys.into_iter()
        .zip(place_labels(&requests, &obstacles, area))
        .filter_map(|(key, spot)| Some((key, spot?)))
        .collect()
}

/// The box an icon covers around its point, logical pixels.
fn icon_box(marker: &MapMarker, style: IconStyle) -> Rect {
    let shape = shape(marker, style);
    let size = Vec2::new(shape.width as f32, shape.height as f32) / 10.0;
    match marker.bounds {
        Some(bounds) if marker.image.is_some() => Rect::from_corners(bounds.min * size - size / 2.0, bounds.max * size - size / 2.0),
        // Silhouettes don't fill their image.
        _ if marker.tint && marker.image.is_some() => Rect::from_center_half_size(Vec2::ZERO, size * 0.36),
        _ => Rect::from_center_half_size(Vec2::ZERO, size / 2.0),
    }
}

fn shape(marker: &MapMarker, style: IconStyle) -> IconShape {
    let size = marker.size * style.scale;
    let (width, height) = if marker.image.is_none() && marker.aspect != 1.0 {
        (size * marker.aspect, size)
    } else {
        (size, size)
    };
    let tinted = marker.image.is_some() && marker.tint;
    IconShape {
        image: marker.image.as_ref().map(|i| i.id()),
        tint: marker.tint,
        tint_color: tinted.then(|| marker.color.to_srgba().to_u8_array()),
        frame: marker
            .frame
            .filter(|_| marker.image.is_some() && !marker.tint)
            .map(|r| [r.min.x, r.min.y, r.max.x, r.max.y].map(|v| (v * 100.0).round() as i32)),
        width: (width * 10.0) as i32,
        height: (height * 10.0) as i32,
        label: marker.label.clone().filter(|_| style.labels),
        pointer: marker.pointer.is_some(),
        layer: marker.layer,
    }
}

const OUTLINE: Color = Color::srgba(0.0, 0.0, 0.0, 0.7);
/// The halo around silhouettes and turret lines.
const HALO: Color = Color::srgba(0.02, 0.02, 0.03, 0.8);
const LABEL_TEXT: Color = Color::srgb(0.96, 0.97, 0.99);
const LABEL_PLATE: Color = Color::srgba(0.02, 0.03, 0.04, 0.5);
/// Around a label's text, logical pixels.
const LABEL_PADDING: Vec2 = Vec2::new(3.0, 0.0);

/// A white silhouette `image` in `color`, `width`×`height` pixels, with a dark halo that
/// keeps it readable on bright maps: copies of it in black, shifted around it, then the
/// image. Children of a node of that size (turn that one to turn it).
pub fn silhouette(image: &Handle<Image>, color: Color, width: f32, height: f32) -> Vec<(ImageNode, Node, FocusPolicy)> {
    let reach = (width.max(height) / 20.0).clamp(1.0, 2.0);
    let part = |offset: Vec2, color: Color| {
        (
            ImageNode::new(image.clone()).with_color(color),
            Node {
                position_type: PositionType::Absolute,
                left: px(offset.x),
                top: px(offset.y),
                width: px(width),
                height: px(height),
                ..default()
            },
            FocusPolicy::Pass,
        )
    };
    let mut parts: Vec<_> = [Vec2::new(1.0, 1.0), Vec2::new(-1.0, 1.0), Vec2::new(1.0, -1.0), Vec2::new(-1.0, -1.0)]
        .into_iter()
        .map(|d| part(d * reach, HALO))
        .collect();
    parts.push(part(Vec2::ZERO, color));
    parts
}

/// A map label's text, font and plate; its `Node` places it.
pub fn label_bundle(text: &str, font: f32) -> impl Bundle {
    (
        Text::new(text),
        TextFont {
            font_size: FontSize::Px(font),
            ..default()
        },
        TextColor(LABEL_TEXT),
        TextShadow {
            offset: Vec2::splat(1.0),
            color: Color::srgba(0.0, 0.0, 0.0, 0.9),
        },
        TextLayout::new(Justify::Center, LineBreak::NoWrap),
        BackgroundColor(LABEL_PLATE),
        FocusPolicy::Pass,
    )
}

/// A label's node: at `offset` from the node's `left`/`top`, `size` big.
pub fn label_node(left: Val, top: Val, offset: Vec2, size: Vec2) -> Node {
    Node {
        position_type: PositionType::Absolute,
        left,
        top,
        margin: UiRect {
            left: px(offset.x),
            top: px(offset.y),
            ..default()
        },
        width: px(size.x),
        height: px(size.y),
        padding: UiRect::axes(px(LABEL_PADDING.x), px(LABEL_PADDING.y)),
        border_radius: BorderRadius::all(px(3)),
        justify_content: JustifyContent::Center,
        align_items: AlignItems::Center,
        ..default()
    }
}

fn spawn(
    commands: &mut Commands,
    parent: Entity,
    marker: &MapMarker,
    point: MapPoint,
    shown: bool,
    style: IconStyle,
    spot: Option<&LabelSpot>,
) {
    let shape = shape(marker, style);
    let (width, height) = (shape.width as f32 / 10.0, shape.height as f32 / 10.0);
    let (left, top) = point.vals();
    let root = commands
        .spawn((
            Node {
                position_type: PositionType::Absolute,
                left,
                top,
                width: px(0),
                height: px(0),
                ..default()
            },
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
        (Some(image), true) => {
            let body = commands.spawn((MarkerBody, body_node, rotation, FocusPolicy::Pass, ChildOf(root))).id();
            for part in silhouette(image, marker.color, width, height) {
                commands.spawn((part, ChildOf(body)));
            }
            body
        }
        (Some(image), false) if marker.frame.is_some() => {
            // A box of the marker's colour behind part of the image (a flag's cloth), the
            // image around it.
            let frame = marker.frame.unwrap_or(Rect::new(0.0, 0.0, 1.0, 1.0));
            let body = commands
                .spawn((
                    MarkerBody,
                    Node {
                        position_type: PositionType::Absolute,
                        left: px(-width / 2.0 + frame.min.x * width),
                        top: px(-height / 2.0 + frame.min.y * height),
                        width: px(frame.width() * width),
                        height: px(frame.height() * height),
                        border_radius: BorderRadius::all(px(2)),
                        ..default()
                    },
                    rotation,
                    BackgroundColor(marker.color),
                    FocusPolicy::Pass,
                    ChildOf(root),
                ))
                .id();
            commands.spawn((
                ImageNode::new(image.clone()),
                Node {
                    position_type: PositionType::Absolute,
                    left: px(-frame.min.x * width),
                    top: px(-frame.min.y * height),
                    width: px(width),
                    height: px(height),
                    ..default()
                },
                FocusPolicy::Pass,
                ChildOf(body),
            ));
            body
        }
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
    // The turret: a white line with a dark edge from the middle to past the hull's front.
    let pointer = marker.pointer.map(|angle| {
        let length = (width.max(height) * 0.62).round();
        let thickness = if width >= 26.0 { 5.0 } else { 4.0 };
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
                        left: px(-thickness / 2.0),
                        top: px(-length),
                        width: px(thickness),
                        height: px(length + thickness / 2.0),
                        border: UiRect::all(px(1)),
                        border_radius: BorderRadius::all(px(thickness / 2.0)),
                        ..default()
                    },
                    BackgroundColor(Color::WHITE),
                    BorderColor::all(HALO),
                    FocusPolicy::Pass,
                )],
            ))
            .id()
    });
    let label = shape.label.as_ref().map(|text| {
        let font = label_font(style);
        let (offset, size, scale) = spot.map_or((Vec2::ZERO, label_size(text.chars().count(), font), 1.0), |s| {
            (s.offset, s.size, s.scale)
        });
        commands
            .spawn((
                MarkerLabel {
                    spot: spot.map(|s| s.index),
                },
                label_bundle(text, font * scale),
                label_node(left, top, offset, size),
                ZIndex(LABEL_LAYER),
                if shown && spot.is_some() { Visibility::Inherited } else { Visibility::Hidden },
                ChildOf(parent),
            ))
            .id()
    });
    commands.entity(root).insert(MarkerIcon {
        key: marker.key,
        shape,
        body,
        pointer,
        label,
    });
}

/// A label to place next to its marker's icon.
#[derive(Clone, Debug)]
pub struct LabelRequest {
    /// The marker's point on the map, pixels.
    pub at: Vec2,
    /// The box its icon covers, from the point.
    pub icon: Rect,
    /// Characters and font size of the label (the font is monospaced).
    pub chars: usize,
    pub font: f32,
    /// Higher ones are placed first.
    pub priority: i32,
    /// The spot it had before ([`LabelSpot::index`]), tried first.
    pub previous: Option<u8>,
    /// Its own icon among the obstacles.
    pub own: Option<usize>,
}

/// Where a label goes.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct LabelSpot {
    /// Top left corner, from the marker's point.
    pub offset: Vec2,
    pub size: Vec2,
    /// Of the font size: 1, or smaller in a crowd.
    pub scale: f32,
    /// Which spot around the icon, and whether shrunk (16).
    pub index: u8,
}

/// Size of a label with this many characters (plate included).
pub fn label_size(chars: usize, font: f32) -> Vec2 {
    // The default font is monospaced: 0.6 em wide, lines 1.2 em high.
    Vec2::new((chars as f32 * font * 0.6).ceil(), (font * 1.2).ceil()) + LABEL_PADDING * 2.0
}

/// Font scale of a label that doesn't fit at full size.
const SHRUNK: f32 = 0.84;
const SPOTS: u8 = 10;

/// Top left corner of a `size` label in spot `spot` around an icon covering `icon`.
fn spot_offset(spot: u8, icon: Rect, size: Vec2) -> Vec2 {
    let gap = 1.0;
    let center = icon.center();
    let inset = icon.size() * 0.25;
    let (w, h) = (size.x, size.y);
    let offset = match spot {
        // Below, above, right, left.
        0 => Vec2::new(center.x - w / 2.0, icon.max.y + gap),
        1 => Vec2::new(center.x - w / 2.0, icon.min.y - gap - h),
        2 => Vec2::new(icon.max.x + gap, center.y - h / 2.0),
        3 => Vec2::new(icon.min.x - gap - w, center.y - h / 2.0),
        // The corners.
        4 => Vec2::new(icon.max.x - inset.x, icon.max.y - inset.y + gap),
        5 => Vec2::new(icon.min.x + inset.x - w, icon.max.y - inset.y + gap),
        6 => Vec2::new(icon.max.x - inset.x, icon.min.y + inset.y - gap - h),
        7 => Vec2::new(icon.min.x + inset.x - w, icon.min.y + inset.y - gap - h),
        // A line further below or above.
        8 => Vec2::new(center.x - w / 2.0, icon.max.y + gap * 2.0 + h),
        _ => Vec2::new(center.x - w / 2.0, icon.min.y - gap * 2.0 - h * 2.0),
    };
    offset.round()
}

fn overlaps(a: Rect, b: Rect) -> bool {
    a.min.x < b.max.x - 0.5 && b.min.x < a.max.x - 0.5 && a.min.y < b.max.y - 0.5 && b.min.y < a.max.y - 0.5
}

/// Places labels so that none covers another, inside `area`: by priority, each takes the
/// first spot around its icon that is clear of the labels placed so far and of the
/// `obstacles` (icons), then with a smaller font, then covering icons; `None` when nothing
/// fits. The spot a label had before is tried first, so labels don't jump around.
pub fn place_labels(requests: &[LabelRequest], obstacles: &[Rect], area: Rect) -> Vec<Option<LabelSpot>> {
    let mut order: Vec<usize> = (0..requests.len()).collect();
    order.sort_by_key(|&i| (std::cmp::Reverse(requests[i].priority), i));
    let mut placed: Vec<Rect> = Vec::with_capacity(requests.len());
    let mut spots = vec![None; requests.len()];
    for i in order {
        let request = &requests[i];
        let found = [(1.0, true), (SHRUNK, true), (1.0, false), (SHRUNK, false)]
            .into_iter()
            .find_map(|(scale, clear_of_icons)| {
                let shrunk = if scale < 1.0 { 16 } else { 0 };
                let size = label_size(request.chars, request.font * scale);
                let previous = request.previous.filter(|p| p & 16 == shrunk).map(|p| p & 15);
                previous.into_iter().chain(0..SPOTS).find_map(|spot| {
                    let offset = spot_offset(spot, request.icon, size);
                    let rect = Rect::from_corners(request.at + offset, request.at + offset + size);
                    let inside = rect.min.x >= area.min.x
                        && rect.min.y >= area.min.y
                        && rect.max.x <= area.max.x
                        && rect.max.y <= area.max.y;
                    let fits = inside
                        && !placed.iter().any(|other| overlaps(*other, rect))
                        && (!clear_of_icons
                            || !obstacles
                                .iter()
                                .enumerate()
                                .any(|(j, icon)| Some(j) != request.own && overlaps(*icon, rect)));
                    fits.then_some(LabelSpot {
                        offset,
                        size,
                        scale,
                        index: spot | shrunk,
                    })
                })
            });
        if let Some(spot) = found {
            placed.push(Rect::from_corners(request.at + spot.offset, request.at + spot.offset + spot.size));
            spots[i] = Some(spot);
        }
    }
    spots
}

#[cfg(test)]
mod tests {
    use super::*;

    fn request(at: Vec2, chars: usize, priority: i32) -> LabelRequest {
        LabelRequest {
            at,
            icon: Rect::from_center_half_size(Vec2::ZERO, Vec2::splat(6.0)),
            chars,
            font: 12.0,
            priority,
            previous: None,
            own: None,
        }
    }

    fn rect(request: &LabelRequest, spot: LabelSpot) -> Rect {
        Rect::from_corners(request.at + spot.offset, request.at + spot.offset + spot.size)
    }

    #[test]
    fn crowded_labels_dont_overlap() {
        let area = Rect::new(0.0, 0.0, 400.0, 400.0);
        let requests: Vec<_> = (0..6).map(|i| request(Vec2::new(200.0 + i as f32 * 8.0, 200.0), 10, 0)).collect();
        let spots = place_labels(&requests, &[], area);
        let placed: Vec<Rect> = requests.iter().zip(&spots).filter_map(|(r, s)| Some(rect(r, (*s)?))).collect();
        assert!(placed.len() >= 4);
        for (i, a) in placed.iter().enumerate() {
            for b in &placed[i + 1..] {
                assert!(!overlaps(*a, *b));
            }
        }
    }

    #[test]
    fn priority_takes_the_first_spot() {
        let area = Rect::new(0.0, 0.0, 400.0, 400.0);
        let requests = [request(Vec2::new(200.0, 200.0), 8, 0), request(Vec2::new(202.0, 200.0), 8, 3)];
        let spots = place_labels(&requests, &[], area);
        assert_eq!(spots[1].unwrap().index, 0);
        assert_ne!(spots[0].unwrap().index, 0);
    }

    #[test]
    fn labels_stay_inside() {
        let area = Rect::new(0.0, 0.0, 100.0, 100.0);
        let requests = [request(Vec2::new(50.0, 97.0), 6, 0)];
        let spot = place_labels(&requests, &[], area)[0].unwrap();
        let r = rect(&requests[0], spot);
        assert!(r.max.y <= 100.0 && r.min.y >= 0.0);
    }
}
