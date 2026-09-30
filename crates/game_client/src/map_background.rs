//! The background of every map (the minimap, the big map, the commander screen, the deploy
//! map): [`MapBackground`] (`map_background.wgsl`) draws the level's map for a [`MapSurface`],
//! in the style of `Settings::map_style`. Classic: BF2's map image. Tactical: the generated
//! tactical map (`tactical_map`; BF2's image toned down until it is ready, a flat colour
//! without either), hatched outside the combat area when there is a mask, main bases tinted in
//! their team's colour, a grid on the big maps, and our squad's order lines.
//!
//! Each map sets the dynamic part ([`MapSurface`]: view, zones, lines) only when it changed;
//! [`sync_surfaces`] turns it into the material then, or when the style or the images change.

use bevy::{
    prelude::*,
    render::render_resource::{AsBindGroup, ShaderType},
    shader::ShaderRef,
    ui_render::prelude::{MaterialNode, UiMaterial, UiMaterialPlugin},
};
use game_shared::level::LoadedLevel;

use crate::{
    settings::{MapStyle, Settings},
    tactical_map::TacticalMap,
};

pub struct MapBackgroundPlugin;

impl Plugin for MapBackgroundPlugin {
    fn build(&self, app: &mut App) {
        embedded_shader!(app, "map_background.wgsl");
        app.add_plugins(UiMaterialPlugin::<MapBackground>::default())
            .init_resource::<MapImages>()
            .add_systems(Update, (update_images, apply_style))
            .add_systems(PostUpdate, sync_surfaces);
    }
}

/// Order lines a map draws at most.
pub const MAX_LINES: usize = 12;
/// Team zones a map draws at most.
pub const MAX_ZONES: usize = 4;

/// The loaded level's map images.
#[derive(Resource, Default, PartialEq)]
pub struct MapImages {
    /// BF2's minimap image.
    pub bf2: Option<Handle<Image>>,
    /// The generated tactical map and the combat area mask (see `tactical_map`).
    pub tactical: Option<Handle<Image>>,
    pub bounds: Option<Handle<Image>>,
}

fn update_images(
    level: Option<Res<LoadedLevel>>,
    tactical: Option<Res<TacticalMap>>,
    asset_server: Res<AssetServer>,
    mut images: ResMut<MapImages>,
) {
    let level_changed = level.as_ref().is_some_and(|l| l.is_changed());
    let tactical_changed = tactical.as_ref().is_some_and(|t| t.is_changed());
    if !level_changed && !tactical_changed {
        return;
    }
    let bf2 = level
        .as_ref()
        .and_then(|l| l.desc.minimap.as_ref())
        .map(|path| asset_server.load(format!("imported://{path}")));
    let next = MapImages {
        bf2,
        tactical: tactical.as_ref().and_then(|t| t.image.clone()),
        bounds: tactical.as_ref().and_then(|t| t.bounds.clone()),
    };
    if *images != next {
        *images = next;
    }
}

/// What a map shows: set by each map, only when it changed.
#[derive(Component, Clone, Debug, PartialEq)]
pub struct MapSurface {
    /// The map position (0..1) at the node's centre, the rotation (clockwise from north) and
    /// the share of the map across the node.
    pub center: Vec2,
    pub rotation: f32,
    pub span: f32,
    /// Logical pixels per map share across the node (for line widths and hatching).
    pub pixels_per_uv: f32,
    /// Outside the map (and the whole node without an image).
    pub background: Color,
    /// Opacity of the tactical map (the minimap lets the world show through).
    pub opacity: f32,
    /// Grid cells across the map in the tactical style (0: no grid).
    pub grid: f32,
    /// Order lines, map positions (tactical style).
    pub lines: Vec<(Vec2, Vec2)>,
    /// Team zones (tactical style).
    pub zones: Vec<Zone>,
}

impl Default for MapSurface {
    fn default() -> Self {
        Self {
            center: Vec2::splat(0.5),
            rotation: 0.0,
            span: 1.0,
            pixels_per_uv: 600.0,
            background: Color::srgb(0.14, 0.15, 0.16),
            opacity: 1.0,
            grid: 0.0,
            lines: Vec::new(),
            zones: Vec::new(),
        }
    }
}

impl MapSurface {
    /// Sets it to `next` only if it differs (a changed surface updates its material).
    pub fn set(this: &mut Mut<MapSurface>, next: MapSurface) {
        if **this != next {
            **this = next;
        }
    }
}

/// A team zone (a main base): an octagon, the intersection of the half planes
/// `dot(uv, ZONE_NORMALS[i]) <= planes[i]` in map shares, tinted in `color`.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Zone {
    pub planes: [f32; 8],
    pub color: Color,
}

/// The zones' plane normals: every 45 degrees from +x.
pub fn zone_normals() -> [Vec2; 8] {
    std::array::from_fn(|i| Vec2::from_angle(i as f32 * std::f32::consts::FRAC_PI_4))
}

impl Zone {
    /// The octagon around `points` (map shares), `margin` further out.
    pub fn around(points: &[Vec2], margin: f32, color: Color) -> Self {
        let normals = zone_normals();
        let planes = std::array::from_fn(|i| {
            points.iter().map(|p| p.dot(normals[i])).fold(f32::MIN, f32::max) + margin
        });
        Self { planes, color }
    }
}

/// Draws a map for a [`MapSurface`] (see `map_background.wgsl`).
#[derive(AsBindGroup, Asset, TypePath, Debug, Clone)]
pub struct MapBackground {
    #[uniform(0)]
    params: MapParams,
    #[texture(1)]
    #[sampler(2)]
    map: Handle<Image>,
    #[texture(3)]
    #[sampler(4)]
    bounds: Handle<Image>,
}

#[derive(ShaderType, Debug, Clone, Copy, PartialEq)]
struct MapParams {
    view: Vec4,
    background: LinearRgba,
    style: Vec4,
    extra: Vec4,
    line: Vec4,
    line_color: LinearRgba,
    segments: [Vec4; MAX_LINES],
    zones: [Vec4; MAX_ZONES * 2],
    zone_colors: [LinearRgba; MAX_ZONES],
}

impl UiMaterial for MapBackground {
    fn fragment_shader() -> ShaderRef {
        "embedded://client/map_background.wgsl".into()
    }
}

/// Line width and dash length of order lines, and the hatching's spacing, logical pixels.
const LINE_WIDTH: f32 = 2.0;
const DASH: f32 = 9.0;
const HATCH: f32 = 7.0;
const ZONE_OUTLINE: f32 = 1.5;

fn build_material(surface: &MapSurface, style: MapStyle, images: &MapImages) -> MapBackground {
    let tactical = style == MapStyle::Tactical;
    let image = if tactical { images.tactical.clone().or_else(|| images.bf2.clone()) } else { images.bf2.clone() };
    let toned = tactical && images.tactical.is_none() && images.bf2.is_some();
    let bounds = images.bounds.clone().filter(|_| tactical);
    let per_px = 1.0 / surface.pixels_per_uv.max(1.0);
    let mut segments = [Vec4::ZERO; MAX_LINES];
    let lines = if tactical { surface.lines.len().min(MAX_LINES) } else { 0 };
    for (slot, (a, b)) in segments.iter_mut().zip(&surface.lines) {
        *slot = Vec4::new(a.x, a.y, b.x, b.y);
    }
    let mut zones = [Vec4::ZERO; MAX_ZONES * 2];
    let mut zone_colors = [LinearRgba::NONE; MAX_ZONES];
    if tactical {
        for (i, zone) in surface.zones.iter().take(MAX_ZONES).enumerate() {
            let p = zone.planes;
            zones[i * 2] = Vec4::new(p[0], p[1], p[2], p[3]);
            zones[i * 2 + 1] = Vec4::new(p[4], p[5], p[6], p[7]);
            zone_colors[i] = zone.color.to_linear();
        }
    }
    MapBackground {
        params: MapParams {
            view: Vec4::new(surface.center.x, surface.center.y, surface.rotation, surface.span),
            background: surface.background.to_linear(),
            style: Vec4::new(
                if tactical { 1.0 } else { 0.0 },
                if tactical { surface.opacity } else { 1.0 },
                // The tactical map has its hatching and border baked in: only BF2's image
                // gets them drawn here.
                if bounds.is_some() && toned { 1.0 } else { 0.0 },
                if tactical { surface.grid } else { 0.0 },
            ),
            extra: Vec4::new(
                if image.is_some() { 1.0 } else { 0.0 },
                if toned { 1.0 } else { 0.0 },
                HATCH * per_px,
                lines as f32,
            ),
            line: Vec4::new(LINE_WIDTH * per_px, DASH * per_px, ZONE_OUTLINE * per_px, 0.0),
            line_color: crate::map_shapes::palette::ORDER.to_linear(),
            segments,
            zones,
            zone_colors,
        },
        map: image.unwrap_or_default(),
        bounds: bounds.unwrap_or_default(),
    }
}

/// Turns changed [`MapSurface`]s (or a changed style or images) into their material.
#[allow(clippy::type_complexity)]
fn sync_surfaces(
    mut commands: Commands,
    settings: Res<Settings>,
    images: Res<MapImages>,
    mut materials: ResMut<Assets<MapBackground>>,
    surfaces: Query<(Entity, Ref<MapSurface>, Option<&MaterialNode<MapBackground>>)>,
    mut style: Local<Option<MapStyle>>,
) {
    let restyled = *style != Some(settings.map_style) || images.is_changed();
    *style = Some(settings.map_style);
    for (entity, surface, material) in &surfaces {
        if !restyled && !surface.is_changed() && material.is_some() {
            continue;
        }
        let next = build_material(&surface, settings.map_style, &images);
        match material.and_then(|m| materials.get(&m.0).map(|current| (m, current))) {
            Some((handle, current)) => {
                if current.params != next.params || current.map != next.map || current.bounds != next.bounds {
                    if let Some(mut asset) = materials.get_mut(&handle.0) {
                        *asset = next;
                    }
                }
            }
            None => {
                commands.entity(entity).insert(MaterialNode(materials.add(next)));
            }
        }
    }
}

/// Shown only in the tactical style (grid labels), or only in the classic one.
#[derive(Component)]
pub struct TacticalOnly;
#[derive(Component)]
pub struct ClassicOnly;

/// A big map's frame, restyled with the map style: classic a dark rounded box with a dark
/// border, tactical a dark slate square with a thin light border.
#[derive(Component)]
pub struct MapFrameLook;

const TACTICAL_FRAME: Color = Color::srgb(0.06, 0.08, 0.10);
const TACTICAL_FRAME_BORDER: Color = Color::srgba(0.8, 0.86, 0.95, 0.35);
const CLASSIC_FRAME: Color = Color::srgb(0.14, 0.15, 0.16);

/// Applies the map style to [`MapFrameLook`] frames and [`TacticalOnly`]/[`ClassicOnly`]
/// nodes when it changes (and to new ones).
#[allow(clippy::type_complexity)]
fn apply_style(
    settings: Res<Settings>,
    mut applied: Local<Option<MapStyle>>,
    mut frames: Query<(Ref<MapFrameLook>, &mut Node, &mut BackgroundColor, &mut BorderColor)>,
    mut tactical_only: Query<(Ref<TacticalOnly>, &mut Visibility), Without<ClassicOnly>>,
    mut classic_only: Query<(Ref<ClassicOnly>, &mut Visibility), Without<TacticalOnly>>,
) {
    let changed = *applied != Some(settings.map_style);
    *applied = Some(settings.map_style);
    let tactical = settings.map_style == MapStyle::Tactical;
    for (look, mut node, mut background, mut border) in &mut frames {
        if !changed && !look.is_added() {
            continue;
        }
        node.border_radius = BorderRadius::all(px(if tactical { 3.0 } else { 10.0 }));
        node.border = UiRect::all(px(if tactical { 1.0 } else { 2.0 }));
        background.0 = if tactical { TACTICAL_FRAME } else { CLASSIC_FRAME };
        *border = BorderColor::all(if tactical { TACTICAL_FRAME_BORDER } else { Color::srgba(0.0, 0.0, 0.0, 0.6) });
    }
    let shown = |on: bool| if on { Visibility::Inherited } else { Visibility::Hidden };
    for (marker, mut visibility) in &mut tactical_only {
        if changed || marker.is_added() {
            visibility.set_if_neq(shown(tactical));
        }
    }
    for (marker, mut visibility) in &mut classic_only {
        if changed || marker.is_added() {
            visibility.set_if_neq(shown(!tactical));
        }
    }
}

/// Grid cells across the big maps in the tactical style.
pub const GRID: usize = 8;

/// The grid's column letters along the top and row numbers down the left of a zoomable map's
/// content node (they move with it), tactical style only.
pub fn spawn_grid_labels(commands: &mut Commands, content: Entity) {
    let label = |text: String, left: Val, top: Val| {
        (
            TacticalOnly,
            Text::new(text),
            TextFont {
                font_size: FontSize::Px(10.0),
                ..default()
            },
            TextColor(Color::srgba(0.85, 0.9, 1.0, 0.55)),
            TextLayout::new(Justify::Center, LineBreak::NoWrap),
            Node {
                position_type: PositionType::Absolute,
                left,
                top,
                width: px(14),
                margin: UiRect::left(px(-7)),
                ..default()
            },
            bevy::ui::FocusPolicy::Pass,
            Pickable::IGNORE,
            Visibility::Hidden,
            ChildOf(content),
        )
    };
    for i in 0..GRID {
        let share = (i as f32 + 0.5) / GRID as f32 * 100.0;
        commands.spawn(label(((b'A' + i as u8) as char).to_string(), percent(share), px(3)));
        commands.spawn(label(format!("{}", i + 1), px(10), percent(share)));
    }
}

/// A line of controls in a map's lower left corner.
pub fn legend(text: &str) -> impl Bundle {
    (
        Text::new(text),
        TextFont {
            font_size: FontSize::Px(11.0),
            ..default()
        },
        TextColor(Color::srgba(0.88, 0.92, 0.98, 0.8)),
        TextShadow {
            offset: Vec2::splat(1.0),
            color: Color::srgba(0.0, 0.0, 0.0, 0.9),
        },
        Node {
            position_type: PositionType::Absolute,
            left: px(8),
            bottom: px(6),
            padding: UiRect::axes(px(6), px(2)),
            border_radius: BorderRadius::all(px(3)),
            ..default()
        },
        BackgroundColor(Color::srgba(0.02, 0.03, 0.04, 0.55)),
        bevy::ui::FocusPolicy::Pass,
        Pickable::IGNORE,
        ZIndex(20),
    )
}
