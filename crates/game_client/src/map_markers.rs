//! What the maps show besides soldiers: flags, vehicles, spotted enemies, squad orders, the
//! commander's assets and what they are doing. Systems in [`MarkerSystems`] fill
//! [`MapMarkers`] every frame (`map_icons`, `radio`, `commander`); the minimap, the big map
//! and the commander screen draw them with [`MarkerIcons`]: a dot, an image framed by a dot of
//! the marker's colour (flags), or a white silhouette in the marker's colour with a dark
//! outline, turned with its heading and with an outlined line for a turret (vehicles, drawn
//! by [`IconMaterial`]).
//!
//! On the big maps markers are labelled. [`place_labels`] keeps labels from covering each
//! other and the labelled icons: each tries a few spots around its icon (below first), then a
//! smaller font, then a line further away, and is left out when nothing fits. Labels of
//! higher priority (control points) go first, and a label pushed away gets a spot next to its
//! icon back when the one label in the way can move to another spot next to its own.
//!
//! [`MapView`] is the pan and zoom shared by every map that can be zoomed (the deploy screen,
//! the big map, the commander screen): each keeps its own `MapView` and resizes/repositions a
//! single content node (the map image; markers and clicks are its children, positioned by
//! `left`/`top` percent or `RelativeCursorPosition`) to show it, so nothing else has to know
//! about zoom at all: percent-positioned markers keep a constant on-screen size (their own
//! size is in pixels, not percent) and stay lined up with the image because both scale off the
//! same resized node, `RelativeCursorPosition::normalized` on that node already is the map's
//! UV (it's relative to the node's whole, unclipped box), and a marker's label offsets stay
//! valid at every zoom because zooming is a uniform scale: distances between markers only ever
//! grow, so a `place_labels` layout that didn't overlap at 1x can't start overlapping zoomed in.
//! [`drive_map_view`] reads the shared mouse/keyboard/gamepad input.

use bevy::{
    ecs::system::SystemParam,
    input::{
        gamepad::{Gamepad, GamepadButton},
        mouse::{AccumulatedMouseMotion, AccumulatedMouseScroll},
    },
    platform::collections::HashMap,
    prelude::*,
    render::render_resource::{AsBindGroup, ShaderType},
    shader::ShaderRef,
    ui::{FocusPolicy, RelativeCursorPosition},
    ui_render::prelude::{MaterialNode, UiMaterial, UiMaterialPlugin},
};

use crate::{camera::CameraSystems, prediction::RenderStateSystems, vehicles::VehicleViewSystems};

/// Pan and zoom for a map, in map UV space (0..1) so callers never need to know a frame's
/// pixel size except to resize/reposition the one content node that shows it
/// ([`MapView::content_rect`]): `center` is the UV shown at the frame's middle, `zoom` from 1
/// (the whole map) to [`MAX_ZOOM`].
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct MapView {
    pub zoom: f32,
    pub center: Vec2,
}

impl Default for MapView {
    fn default() -> Self {
        Self {
            zoom: 1.0,
            center: Vec2::splat(0.5),
        }
    }
}

/// Enough to place a spawn precisely on a big map (Gulf of Oman, the AIX maps).
pub const MAX_ZOOM: f32 = 4.0;

/// Wheel zoom rate per line of scroll.
const WHEEL_ZOOM_RATE: f32 = 0.18;
/// Zoom multiplier per second while a zoom key (or a trigger, fully pressed) is held.
const KEY_ZOOM_RATE: f32 = 1.8;
/// Keyboard/gamepad-stick pan speed, logical pixels/second (of the content at its current
/// zoom, like dragging: covers less of the map per second zoomed in).
const KEY_PAN_SPEED: f32 = 420.0;
const DOUBLE_CLICK_SECS: f32 = 0.35;
const DOUBLE_CLICK_UV: f32 = 0.05;

impl MapView {
    fn half_extent(&self) -> f32 {
        0.5 / self.zoom
    }

    pub fn reset(&mut self) {
        *self = Self::default();
    }

    /// Keeps the zoom in range and the view from panning past the map's edge.
    pub fn clamp(&mut self) {
        self.zoom = self.zoom.clamp(1.0, MAX_ZOOM);
        let half = self.half_extent();
        self.center = self.center.clamp(Vec2::splat(half), Vec2::splat(1.0 - half));
    }

    /// Left/top and width/height, logical pixels, to show this view in a `frame`-sized node:
    /// what callers set a content node's [`Node`] to every frame it (or the frame) changes.
    pub fn content_rect(&self, frame: Vec2) -> (Vec2, Vec2) {
        let size = frame * self.zoom;
        (frame * 0.5 - self.center * size, size)
    }

    /// Zooms by this factor (>1 in, <1 out), keeping `anchor_uv` (the cursor, or the current
    /// center without one) under the same screen point.
    pub fn zoom_at(&mut self, factor: f32, anchor_uv: Vec2) {
        let old_zoom = self.zoom;
        self.zoom = (old_zoom * factor).clamp(1.0, MAX_ZOOM);
        self.center = anchor_uv - (anchor_uv - self.center) * (old_zoom / self.zoom);
        self.clamp();
    }

    /// Pans so the map moves by this share of the map (like dragging it with the mouse:
    /// positive x/y drags the map right/down, revealing more of its left/top).
    pub fn pan_uv(&mut self, delta: Vec2) {
        self.center -= delta;
        self.clamp();
    }
}

/// Resizes/repositions a zoomable map's content node to show `view` in a `frame`-sized
/// viewport, touching `node` only if something actually moved: an unconditionally-changed
/// `Node` lays out the whole UI tree again every frame, even while the view never changes
/// (see `docs/ARCHITECTURE.md`'s performance notes).
pub fn apply_map_view(node: &mut Node, view: MapView, frame: Vec2) {
    let (left, size) = view.content_rect(frame);
    let (left, top, width, height) = (px(left.x), px(left.y), px(size.x), px(size.y));
    if node.left != left || node.top != top || node.width != width || node.height != height {
        node.left = left;
        node.top = top;
        node.width = width;
        node.height = height;
    }
}

/// Continuous zoom (>0 in, <0 out) and pan direction (screen right/down positive) from `+`/`-`
/// (or a gamepad's triggers) and the arrow keys (or its left stick), for [`drive_map_view`].
/// Only the most active gamepad is read (see `settings::gamepad_activity`), the same one
/// `menu::input::gamepad_menu_nav` would.
fn map_view_keys(keys: &ButtonInput<KeyCode>, gamepads: &Query<&Gamepad>) -> (f32, Vec2) {
    let mut zoom = 0.0;
    if keys.pressed(KeyCode::Equal) || keys.pressed(KeyCode::NumpadAdd) {
        zoom += 1.0;
    }
    if keys.pressed(KeyCode::Minus) || keys.pressed(KeyCode::NumpadSubtract) {
        zoom -= 1.0;
    }
    let mut pan = Vec2::ZERO;
    if keys.pressed(KeyCode::ArrowLeft) {
        pan.x -= 1.0;
    }
    if keys.pressed(KeyCode::ArrowRight) {
        pan.x += 1.0;
    }
    if keys.pressed(KeyCode::ArrowUp) {
        pan.y -= 1.0;
    }
    if keys.pressed(KeyCode::ArrowDown) {
        pan.y += 1.0;
    }
    if let Some(gamepad) = gamepads
        .iter()
        .max_by(|a, b| crate::settings::gamepad_activity(a).total_cmp(&crate::settings::gamepad_activity(b)))
    {
        zoom += gamepad.get(GamepadButton::RightTrigger2).unwrap_or(0.0) - gamepad.get(GamepadButton::LeftTrigger2).unwrap_or(0.0);
        let stick = gamepad.left_stick();
        if stick.length() > 0.2 {
            pan += Vec2::new(stick.x, -stick.y);
        }
    }
    (zoom.clamp(-1.0, 1.0), pan.clamp_length_max(1.0))
}

/// Whether a left click at `uv` (the map UV under the cursor) is a double click on the last one
/// recorded in `last`, which this then updates: shared so the deploy screen, the big map and
/// the commander screen all reset the same way. Call only on an actual left click.
pub fn is_double_click(now: f32, uv: Vec2, last: &mut Option<(f32, Vec2)>) -> bool {
    let double = last.is_some_and(|(t, at)| now - t < DOUBLE_CLICK_SECS && at.distance(uv) < DOUBLE_CLICK_UV);
    *last = Some((now, uv));
    double
}

/// Applies the mouse wheel (zooms around the cursor), a right or middle drag (panning stays
/// while held even if the cursor slips off the map), `+`/`-`/the triggers, the arrows/the
/// stick, and a double left click (resets), to `view`. `cursor` must be the
/// [`RelativeCursorPosition`] of the content node itself (not the clipped frame around it),
/// so its `normalized` is already the map UV; `content_size` is that node's current size
/// (`frame_size * view.zoom`).
#[allow(clippy::too_many_arguments)]
pub fn drive_map_view(
    view: &mut MapView,
    cursor: &RelativeCursorPosition,
    content_size: Vec2,
    keys: &ButtonInput<KeyCode>,
    mouse: &ButtonInput<MouseButton>,
    scroll: &AccumulatedMouseScroll,
    motion: &AccumulatedMouseMotion,
    gamepads: &Query<&Gamepad>,
    dt: f32,
    now: f32,
    dragging: &mut bool,
    last_click: &mut Option<(f32, Vec2)>,
) {
    let hovered = cursor.cursor_over;
    let uv = cursor.normalized.map(|n| n + Vec2::splat(0.5));

    if hovered && scroll.delta.y != 0.0 {
        view.zoom_at((1.0 + WHEEL_ZOOM_RATE).powf(scroll.delta.y), uv.unwrap_or(view.center));
    }

    let pan_button = mouse.pressed(MouseButton::Right) || mouse.pressed(MouseButton::Middle);
    if pan_button && (hovered || *dragging) {
        *dragging = true;
        if motion.delta != Vec2::ZERO && content_size.min_element() > 0.0 {
            view.pan_uv(motion.delta / content_size);
        }
    } else {
        *dragging = false;
    }

    let (zoom_rate, pan_dir) = map_view_keys(keys, gamepads);
    if zoom_rate != 0.0 {
        view.zoom_at(KEY_ZOOM_RATE.powf(zoom_rate * dt), view.center);
    }
    if pan_dir != Vec2::ZERO && content_size.min_element() > 0.0 {
        view.pan_uv(-pan_dir * KEY_PAN_SPEED * dt / content_size);
    }

    if hovered
        && mouse.just_pressed(MouseButton::Left)
        && let Some(uv) = uv
        && is_double_click(now, uv, last_click)
    {
        view.reset();
    }
}

pub struct MapMarkersPlugin;

impl Plugin for MapMarkersPlugin {
    fn build(&self, app: &mut App) {
        embedded_shader!(app, "map_icon.wgsl");
        app.add_plugins(UiMaterialPlugin::<IconMaterial>::default())
            .init_resource::<MapMarkers>()
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

/// Stacking of markers: higher layers are drawn above lower ones. Flags go above vehicles
/// (the ones parked at a main base would hide its flag), soldiers' dots above both.
pub const VEHICLE_LAYER: i32 = 1;
pub const FLAG_LAYER: i32 = 2;
pub const SOLDIER_LAYER: i32 = 3;
pub const ALERT_LAYER: i32 = 4;
/// The vehicle we sit in, above everything else.
pub const OWN_LAYER: i32 = 5;
/// Labels, above every icon.
const LABEL_LAYER: i32 = 6;

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
    /// Drawn as this image instead of a dot: in `color` with a dark outline when `tint`
    /// (white silhouettes), else as it is on a dot of `color`, or on a box of `color` covering
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

    /// Drawn by an [`IconMaterial`]: silhouettes, and anything turned.
    fn turned(&self) -> bool {
        self.tint || self.heading.is_some() || self.pointer.is_some()
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

    /// How an icon's node goes to this point: pixel points move by the node's `UiTransform`
    /// (a moved transform needs no new layout, while a moved `left`/`top` lays the whole map
    /// out again; minimap markers move every frame), shares by `left`/`top`.
    fn icon_placement(self) -> (Val, Val, Val2) {
        match self {
            MapPoint::Pixels(at) => (px(0), px(0), Val2::px(at.x, at.y)),
            MapPoint::Share(_) => {
                let (left, top) = self.vals();
                (left, top, Val2::ZERO)
            }
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

/// Draws a turned map icon in a node that isn't turned itself, since Bevy doesn't clip turned
/// nodes (vehicles at the minimap's edge spilled out of it): a white silhouette (or a rounded
/// box) in the marker's colour with a dark outline, turned with its heading, and a white line
/// with a dark edge for a turret (see `map_icon.wgsl`).
#[derive(AsBindGroup, Asset, TypePath, Debug, Clone)]
pub struct IconMaterial {
    #[uniform(0)]
    params: IconParams,
    #[texture(1)]
    #[sampler(2)]
    image: Handle<Image>,
}

#[derive(ShaderType, Debug, Clone, Copy, PartialEq)]
struct IconParams {
    color: LinearRgba,
    halo: LinearRgba,
    /// x, y: the icon's size; z: the node's side; w: the outline's width (logical pixels).
    size: Vec4,
    /// x: the icon's heading, y: the turret's (clockwise radians, on the map); z, w: the
    /// turret line's length and width (0 without one).
    turn: Vec4,
    /// x: 1 to draw the image, 2 its shape in the colour (images without white), 0 a
    /// rounded box.
    kind: Vec4,
}

impl UiMaterial for IconMaterial {
    fn fragment_shader() -> ShaderRef {
        "embedded://client/map_icon.wgsl".into()
    }
}

/// Sizes of a turned icon, logical pixels.
struct Turned {
    /// Of the square node it is drawn in (room for it turned and for the turret line).
    side: f32,
    outline: f32,
    /// The turret line's length and width (0 without one).
    length: f32,
    thickness: f32,
}

impl Turned {
    fn new(width: f32, height: f32, pointer: bool) -> Self {
        let big = width.max(height);
        // About a pixel of BF2's 16 px silhouettes.
        let outline = (big / 16.0).clamp(1.2, 2.5);
        // Well past the hull's front, and thick enough to read on the minimap.
        let (length, thickness) = if pointer { ((big * 0.66).round(), (big * 0.17).clamp(4.0, 6.5)) } else { (0.0, 0.0) };
        let reach = (0.5 * width.hypot(height) + outline + 1.0).max(length + thickness / 2.0 + 1.0);
        Self {
            side: (reach * 2.0).ceil(),
            outline,
            length,
            thickness,
        }
    }
}

/// Angles are rounded to this (radians), so that icons at rest don't touch their material.
const ANGLE_STEP: f32 = 0.004;

/// `mask`: the image has no white to colour (see [`all_dark`]).
fn icon_params(marker: &MapMarker, style: IconStyle, mask: bool) -> IconParams {
    let size = shape(marker, style).size();
    let turned = Turned::new(size.x, size.y, marker.pointer.is_some());
    let angle = |a: Option<f32>| a.map_or(0.0, |a| ((a - style.turn) / ANGLE_STEP).round() * ANGLE_STEP);
    IconParams {
        color: marker.color.to_linear(),
        halo: HALO.to_linear(),
        size: Vec4::new(size.x, size.y, turned.side, turned.outline),
        turn: Vec4::new(angle(marker.heading), angle(marker.pointer), turned.length, turned.thickness),
        kind: Vec4::new(
            match (&marker.image, mask) {
                (None, _) => 0.0,
                (Some(_), false) => 1.0,
                (Some(_), true) => 2.0,
            },
            0.0,
            0.0,
            0.0,
        ),
    }
}

/// Whether a silhouette has no white in it to colour (BF2's gun icons are all black): those
/// are drawn as a shape of the marker's colour. Compressed images are taken to have some.
fn all_dark(image: &Image) -> bool {
    use bevy::render::render_resource::TextureFormat;
    let Some(data) = &image.data else {
        return false;
    };
    match image.texture_descriptor.format {
        TextureFormat::Rgba8Unorm | TextureFormat::Rgba8UnormSrgb | TextureFormat::Bgra8Unorm | TextureFormat::Bgra8UnormSrgb => {
            !data.chunks_exact(4).any(|p| p[3] > 128 && p[0].max(p[1]).max(p[2]) > 128)
        }
        _ => false,
    }
}

/// Whether `marker`'s silhouette is drawn as a shape (see [`all_dark`]), remembered per image
/// once it is loaded.
fn mask(images: &Assets<Image>, known: &mut HashMap<AssetId<Image>, bool>, marker: &MapMarker) -> bool {
    let Some(image) = marker.image.as_ref().filter(|_| marker.tint) else {
        return false;
    };
    if let Some(&dark) = known.get(&image.id()) {
        return dark;
    }
    let Some(loaded) = images.get(image) else {
        return false;
    };
    let dark = all_dark(loaded);
    known.insert(image.id(), dark);
    dark
}

/// A map's icon for a marker, a child of the map.
#[derive(Component)]
pub struct MarkerIcon {
    key: Entity,
    shape: IconShape,
    body: Entity,
    /// A sibling of the icon (so that it draws above every icon).
    label: Option<Entity>,
}

#[derive(Component)]
pub struct MarkerBody;

#[derive(Component)]
pub struct MarkerLabel {
    /// The spot it took last (see [`LabelSpot::index`]).
    spot: Option<u8>,
}

/// Filter for queries in systems with a [`MarkerIcons`] that touch `Node`, `Visibility`,
/// `BackgroundColor` or `TextFont`: keeps them apart from the icons'.
pub type NotMarker = (Without<MarkerIcon>, Without<MarkerBody>, Without<MarkerLabel>);

/// What can't change without rebuilding the icon.
#[derive(Clone, PartialEq, Debug)]
struct IconShape {
    image: Option<AssetId<Image>>,
    tint: bool,
    turned: bool,
    /// Hundredths of the image.
    frame: Option<[i32; 4]>,
    /// Tenths of a pixel.
    width: i32,
    height: i32,
    label: Option<String>,
    pointer: bool,
    layer: i32,
}

impl IconShape {
    /// Width and height, logical pixels.
    fn size(&self) -> Vec2 {
        Vec2::new(self.width as f32, self.height as f32) / 10.0
    }
}

/// Draws [`MapMarker`]s on a map.
#[derive(SystemParam)]
pub struct MarkerIcons<'w, 's> {
    commands: Commands<'w, 's>,
    materials: ResMut<'w, Assets<IconMaterial>>,
    images: Res<'w, Assets<Image>>,
    /// Which images [`mask`] found without white.
    dark: Local<'s, HashMap<AssetId<Image>, bool>>,
    icons: Query<
        'w,
        's,
        (
            Entity,
            &'static MarkerIcon,
            &'static ChildOf,
            &'static mut Node,
            &'static mut UiTransform,
            &'static mut Visibility,
        ),
        (Without<MarkerBody>, Without<MarkerLabel>),
    >,
    bodies: Query<
        'w,
        's,
        (Option<&'static mut BackgroundColor>, Option<&'static MaterialNode<IconMaterial>>),
        (With<MarkerBody>, Without<MarkerIcon>, Without<MarkerLabel>),
    >,
    labels: Query<
        'w,
        's,
        (&'static mut MarkerLabel, &'static mut Node, &'static mut TextFont, &'static mut Visibility),
        (Without<MarkerIcon>, Without<MarkerBody>),
    >,
}

/// Moves an icon's node to `point`, touching it only if it moved.
fn place_icon(node: &mut Mut<Node>, transform: &mut Mut<UiTransform>, point: MapPoint) {
    let (left, top, translation) = point.icon_placement();
    if node.left != left || node.top != top {
        node.left = left;
        node.top = top;
    }
    if transform.translation != translation {
        transform.translation = translation;
    }
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
            for (_, icon, child_of, ..) in &self.icons {
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
        for (entity, icon, child_of, mut node, mut transform, mut visibility) in &mut self.icons {
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
            place_icon(&mut node, &mut transform, point);
            visibility.set_if_neq(if shown { Visibility::Inherited } else { Visibility::Hidden });
            if let Ok((background, material)) = self.bodies.get_mut(icon.body) {
                match material {
                    // Turned icons: heading, turret and colour live in the material.
                    Some(material) => {
                        let params = icon_params(marker, style, mask(&self.images, &mut self.dark, marker));
                        if self.materials.get(&material.0).is_some_and(|m| m.params != params)
                            && let Some(mut asset) = self.materials.get_mut(&material.0)
                        {
                            asset.params = params;
                        }
                    }
                    None => {
                        if let Some(mut background) = background
                            && background.0 != marker.color
                        {
                            background.0 = marker.color;
                        }
                    }
                }
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
            let mask = mask(&self.images, &mut self.dark, marker);
            spawn(&mut self.commands, &mut self.materials, parent, marker, point, shown, style, mask, spots.get(&marker.key));
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
            obstacles.push(Obstacle {
                rect: Rect::from_corners(at + icon.min, at + icon.max),
                hard: label.is_some(),
            });
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
    let size = shape(marker, style).size();
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
    IconShape {
        image: marker.image.as_ref().map(|i| i.id()),
        tint: marker.tint,
        turned: marker.turned(),
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
/// The outline of silhouettes and turret lines.
const HALO: Color = Color::srgba(0.02, 0.02, 0.03, 0.85);
const LABEL_TEXT: Color = Color::srgb(0.96, 0.97, 0.99);
const LABEL_PLATE: Color = Color::srgba(0.02, 0.03, 0.04, 0.5);
/// Around a label's text, logical pixels.
const LABEL_PADDING: Vec2 = Vec2::new(3.0, 0.0);

/// A white silhouette `image` in `color`, `width`×`height` pixels, with a dark outline that
/// keeps it readable on bright maps: copies of it in black, shifted all around it, then the
/// image. Children of a node of that size (turn that one to turn it). For pictures that
/// don't change (the maps use [`IconMaterial`]).
pub fn silhouette(image: &Handle<Image>, color: Color, width: f32, height: f32) -> Vec<(ImageNode, Node, FocusPolicy)> {
    let reach = (width.max(height) / 16.0).clamp(1.0, 2.0);
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
    let mut parts: Vec<_> = (0..8)
        .map(|i| part(Vec2::from_angle(i as f32 * std::f32::consts::FRAC_PI_4) * reach, HALO))
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

#[allow(clippy::too_many_arguments)]
fn spawn(
    commands: &mut Commands,
    materials: &mut Assets<IconMaterial>,
    parent: Entity,
    marker: &MapMarker,
    point: MapPoint,
    shown: bool,
    style: IconStyle,
    mask: bool,
    spot: Option<&LabelSpot>,
) {
    let shape = shape(marker, style);
    let Vec2 { x: width, y: height } = shape.size();
    let (left, top) = point.vals();
    let (icon_left, icon_top, translation) = point.icon_placement();
    let root = commands
        .spawn((
            Node {
                position_type: PositionType::Absolute,
                left: icon_left,
                top: icon_top,
                width: px(0),
                height: px(0),
                ..default()
            },
            UiTransform {
                translation,
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
    let body = match &marker.image {
        _ if marker.turned() => {
            let side = Turned::new(width, height, marker.pointer.is_some()).side;
            let material = materials.add(IconMaterial {
                params: icon_params(marker, style, mask),
                image: marker.image.clone().unwrap_or_default(),
            });
            commands
                .spawn((
                    MarkerBody,
                    Node {
                        position_type: PositionType::Absolute,
                        left: px(-side / 2.0),
                        top: px(-side / 2.0),
                        width: px(side),
                        height: px(side),
                        ..default()
                    },
                    MaterialNode(material),
                    FocusPolicy::Pass,
                    ChildOf(root),
                ))
                .id()
        }
        Some(image) if marker.frame.is_some() => {
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
        Some(image) => {
            let body = commands
                .spawn((
                    MarkerBody,
                    Node {
                        border: UiRect::all(px(1)),
                        border_radius: BorderRadius::MAX,
                        ..body_node
                    },
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
        None => commands
            .spawn((
                MarkerBody,
                Node {
                    border: UiRect::all(px(1)),
                    border_radius: if marker.aspect == 1.0 { BorderRadius::MAX } else { BorderRadius::all(px(2)) },
                    ..body_node
                },
                BackgroundColor(marker.color),
                BorderColor::all(OUTLINE),
                FocusPolicy::Pass,
                ChildOf(root),
            ))
            .id(),
    };
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

impl LabelSpot {
    /// Next to its icon at full size.
    fn good(&self) -> bool {
        self.index < NEAR
    }
}

/// Size of a label with this many characters (plate included).
pub fn label_size(chars: usize, font: f32) -> Vec2 {
    // The default font is monospaced: 0.6 em wide, lines 1.2 em high.
    Vec2::new((chars as f32 * font * 0.6).ceil(), (font * 1.2).ceil()) + LABEL_PADDING * 2.0
}

/// Font scale of a label that doesn't fit at full size.
const SHRUNK: f32 = 0.84;
/// Spots next to the icon (the rest are a line further away).
const NEAR: u8 = 8;
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

/// An icon labels keep clear of.
#[derive(Clone, Copy, Debug)]
pub struct Obstacle {
    pub rect: Rect,
    /// Never covered (labelled icons: flags, assets); others (vehicles) only when nothing
    /// else fits.
    pub hard: bool,
}

/// Places labels so that none covers another or a labelled icon, inside `area`: by
/// priority, each takes the first spot next to its icon that is clear, then the same with a
/// smaller font, then a line further away, then covering unlabelled icons; `None` when
/// nothing fits. The spot a label had before is tried first, so labels don't jump around.
/// Then a label that didn't get a spot next to its icon at full size takes one if the one
/// label in the way can move to another such spot next to its own icon.
pub fn place_labels(requests: &[LabelRequest], obstacles: &[Obstacle], area: Rect) -> Vec<Option<LabelSpot>> {
    let candidates: Vec<(u8, f32)> = [(0..NEAR, 1.0), (0..NEAR, SHRUNK), (NEAR..SPOTS, 1.0), (NEAR..SPOTS, SHRUNK)]
        .into_iter()
        .flat_map(|(spots, scale)| spots.map(move |spot| (spot, scale)))
        .collect();
    // A label's spot and the box it covers.
    let spot_at = |request: &LabelRequest, spot: u8, scale: f32| {
        let size = label_size(request.chars, request.font * scale);
        let offset = spot_offset(spot, request.icon, size);
        let label = LabelSpot {
            offset,
            size,
            scale,
            index: spot | if scale < 1.0 { 16 } else { 0 },
        };
        (label, Rect::from_corners(request.at + offset, request.at + offset + size))
    };
    // Inside the map and clear of the icons (all of them when `strict`, else the hard ones).
    let clear = |request: &LabelRequest, rect: Rect, strict: bool| {
        rect.min.x >= area.min.x
            && rect.min.y >= area.min.y
            && rect.max.x <= area.max.x
            && rect.max.y <= area.max.y
            && !obstacles
                .iter()
                .enumerate()
                .any(|(j, o)| (strict || o.hard) && Some(j) != request.own && overlaps(o.rect, rect))
    };
    let mut order: Vec<usize> = (0..requests.len()).collect();
    order.sort_by_key(|&i| (std::cmp::Reverse(requests[i].priority), i));
    let mut placed: Vec<Option<Rect>> = vec![None; requests.len()];
    let mut spots: Vec<Option<LabelSpot>> = vec![None; requests.len()];
    for &i in &order {
        let request = &requests[i];
        let previous = request.previous.map(|p| (p & 15, if p & 16 != 0 { SHRUNK } else { 1.0 }));
        let found = [true, false].into_iter().find_map(|strict| {
            previous.into_iter().chain(candidates.iter().copied()).find_map(|(spot, scale)| {
                let (label, rect) = spot_at(request, spot, scale);
                let fits = clear(request, rect, strict) && !placed.iter().flatten().any(|other| overlaps(*other, rect));
                fits.then_some((label, rect))
            })
        });
        if let Some((label, rect)) = found {
            placed[i] = Some(rect);
            spots[i] = Some(label);
        }
    }
    // Labels left out, shrunk or a line away: move the one label in the way of a good spot.
    for &i in &order {
        if spots[i].is_some_and(|s| s.good()) {
            continue;
        }
        let request = &requests[i];
        'spots: for spot in 0..NEAR {
            let (label, rect) = spot_at(request, spot, 1.0);
            if !clear(request, rect, true) {
                continue;
            }
            let mut blockers = (0..requests.len()).filter(|&j| j != i && placed[j].is_some_and(|r| overlaps(r, rect)));
            let (Some(j), None) = (blockers.next(), blockers.next()) else {
                continue;
            };
            let Some(current) = spots[j].filter(|s| s.good()) else {
                continue;
            };
            let other = &requests[j];
            for alternative in (0..NEAR).filter(|&s| s != current.index) {
                let (moved, moved_rect) = spot_at(other, alternative, 1.0);
                let fits = !overlaps(moved_rect, rect)
                    && clear(other, moved_rect, true)
                    && !(0..requests.len()).any(|k| k != i && k != j && placed[k].is_some_and(|r| overlaps(r, moved_rect)));
                if fits {
                    (placed[i], spots[i]) = (Some(rect), Some(label));
                    (placed[j], spots[j]) = (Some(moved_rect), Some(moved));
                    break 'spots;
                }
            }
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

    #[test]
    fn labels_stay_clear_of_labelled_icons() {
        let area = Rect::new(0.0, 0.0, 400.0, 400.0);
        let mut requests = [request(Vec2::new(200.0, 200.0), 8, 0)];
        requests[0].own = Some(0);
        // Its icon, a flag right below it, a vehicle right above it.
        let obstacles = [
            Obstacle {
                rect: Rect::from_center_half_size(Vec2::new(200.0, 200.0), Vec2::splat(6.0)),
                hard: true,
            },
            Obstacle {
                rect: Rect::from_center_half_size(Vec2::new(200.0, 214.0), Vec2::splat(6.0)),
                hard: true,
            },
            Obstacle {
                rect: Rect::from_center_half_size(Vec2::new(200.0, 186.0), Vec2::splat(6.0)),
                hard: false,
            },
        ];
        let spot = place_labels(&requests, &obstacles, area)[0].unwrap();
        let r = rect(&requests[0], spot);
        assert!(!overlaps(r, obstacles[1].rect) && !overlaps(r, obstacles[2].rect));
    }

    /// Like Karkand's "Factory" and "Cement Factory": the first label takes the spot below
    /// its flag that the second one needs, and moves aside for it.
    #[test]
    fn labels_make_room() {
        let area = Rect::new(0.0, 0.0, 400.0, 400.0);
        let mut requests = [request(Vec2::new(147.0, 100.0), 7, 0), request(Vec2::new(100.0, 100.0), 14, 0)];
        requests[0].own = Some(0);
        requests[1].own = Some(1);
        let icon = |at: Vec2| Obstacle {
            rect: Rect::from_center_half_size(at, Vec2::splat(6.0)),
            hard: true,
        };
        // Something above both.
        let obstacles = [
            icon(requests[0].at),
            icon(requests[1].at),
            Obstacle {
                rect: Rect::new(60.0, 70.0, 140.0, 90.0),
                hard: true,
            },
        ];
        let spots = place_labels(&requests, &obstacles, area);
        let (a, b) = (spots[0].unwrap(), spots[1].unwrap());
        assert!(a.good() && b.good(), "{a:?} {b:?}");
        assert!(!overlaps(rect(&requests[0], a), rect(&requests[1], b)));
    }
}
