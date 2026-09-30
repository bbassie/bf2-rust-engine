//! The minimap, top right: the level's map around us, flags in their owners' colors,
//! teammates as dots, vehicles, spotted enemies and orders (see `map_markers`), and us with
//! our heading (in a vehicle, its white icon is us). It turns with us (N switches to north
//! up).
//!
//! Its size is `Settings::minimap_size` times [`SIZE`]: the map, the icons (by the square
//! root, so they stay readable small and don't get clumsy large), our marker and the pinned
//! objectives all follow it. How far it shows eases between the on-foot, vehicle and air
//! ranges of the settings with our speed and height (never a jump: [`MinimapZoom`]).
//!
//! In the tactical style (`Settings::map_style`) it is square and translucent with a thin
//! border, draws the tactical map (`map_background`) with our squad's order lines and the
//! main bases' zones, objectives as lettered shapes pinned to its edge while out of range,
//! squad mates as numbered circles and teammates as dots with their heading (`map_shapes`).

use bevy::prelude::*;
use game_shared::{
    level::LoadedLevel,
    protocol::{ControlledBy, Team},
    soldier::Soldier,
    squad::SquadMember,
    vehicle::{Seated, Vehicle},
};

use crate::{
    camera::PlayerCamera,
    conquest_hud::{FRIENDLY, SQUAD},
    map_background::MapSurface,
    map_icons::{UiIcons, to_map},
    map_markers::{IconStyle, MapMarker, MapMarkers, MapPoint, MarkerIcons, NotMarker, SOLDIER_LAYER},
    map_shapes::{
        OrderLines, ShapeKind, ShapeLook, ShapeMaterial, SquadSlots, is_order, map_lines, order_rings, soldier_look, spawn_shape,
        BaseZones,
    },
    net::{LocalPlayer, LocalSoldier},
    prediction::SoldierRender,
    settings::{MapStyle, Settings},
};

pub struct MinimapPlugin;

impl Plugin for MinimapPlugin {
    fn build(&self, app: &mut App) {
        app.init_resource::<MinimapSettings>()
            .init_resource::<MinimapZoom>()
            .add_systems(Startup, spawn_minimap)
            .add_systems(
                Update,
                (
                    toggle_rotation,
                    apply_minimap_look.run_if(resource_changed::<Settings>),
                    update_zoom,
                    update_minimap,
                )
                    .chain(),
            );
    }
}

/// Side of the minimap in logical pixels at `minimap_size` 1.
pub const SIZE: f32 = 190.0;
const BACKGROUND: Color = Color::srgb(0.14, 0.15, 0.16);
/// The tactical minimap's own background, under the translucent map.
const TACTICAL_BACKGROUND: Color = Color::srgba(0.04, 0.06, 0.08, 0.5);
const TACTICAL_BORDER: Color = Color::srgba(0.85, 0.9, 1.0, 0.4);
/// How much of the world shows through the tactical map.
const TACTICAL_OPACITY: f32 = 0.84;
/// Pinned objectives are drawn this much larger than the others.
const PIN_SCALE: f32 = 1.3;

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

/// How far the minimap shows (meters across), eased towards what our speed and height call
/// for (see [`update_zoom`]).
#[derive(Resource, Default)]
pub struct MinimapZoom {
    pub range: f32,
    /// Smoothed ground speed, m/s.
    speed: f32,
    last: Option<Vec3>,
    /// On foot, in a land vehicle or flying (for the log).
    mode: Option<ZoomMode>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum ZoomMode {
    Foot,
    Vehicle,
    Air,
}

/// The minimap's size and look for the current settings.
#[derive(Clone, Copy, PartialEq)]
struct Look {
    /// Outer side and border, logical pixels.
    side: f32,
    border: f32,
    /// Multiplies icon sizes.
    icons: f32,
    tactical: bool,
}

impl Look {
    fn of(settings: &Settings) -> Self {
        let scale = settings.minimap_size.clamp(0.5, 1.75);
        let tactical = settings.map_style == MapStyle::Tactical;
        Self {
            side: (SIZE * scale).round(),
            border: if tactical { 1.0 } else { 2.0 },
            icons: scale.sqrt(),
            tactical,
        }
    }

    /// Inside the border.
    fn inner(&self) -> f32 {
        self.side - 2.0 * self.border
    }
}

#[derive(Component)]
struct MinimapMap;
#[derive(Component)]
struct MinimapIcons;
/// Pinned objectives, over the frame's edge (not clipped by it).
#[derive(Component)]
struct MinimapEdge;
#[derive(Component)]
struct PlayerHeading;
/// Our classic marker (a dot and a line) and the tactical arrow.
#[derive(Component)]
struct ClassicHeading;
#[derive(Component)]
struct TacticalHeading;
/// North, at the tactical minimap's edge.
#[derive(Component)]
struct NorthMark;

/// The minimap's outer node (size) and its frame (border, background).
#[derive(Component)]
struct MinimapRoot;
#[derive(Component)]
struct MinimapFrame;

fn spawn_minimap(mut commands: Commands, mut shapes: ResMut<Assets<ShapeMaterial>>) {
    let root = commands
        .spawn((
            MinimapRoot,
            Node {
                position_type: PositionType::Absolute,
                top: px(12),
                right: px(16),
                width: px(SIZE),
                height: px(SIZE),
                ..default()
            },
        ))
        .id();
    let frame = commands
        .spawn((
            MinimapFrame,
            Node {
                position_type: PositionType::Absolute,
                width: percent(100),
                height: percent(100),
                border: UiRect::all(px(2)),
                border_radius: BorderRadius::all(px(10)),
                overflow: Overflow::clip(),
                ..default()
            },
            BackgroundColor(BACKGROUND),
            BorderColor::all(Color::srgba(0.0, 0.0, 0.0, 0.5)),
            ChildOf(root),
        ))
        .id();
    commands.spawn((
        MinimapMap,
        MapSurface::default(),
        Node {
            position_type: PositionType::Absolute,
            width: percent(100),
            height: percent(100),
            ..default()
        },
        ChildOf(frame),
    ));
    commands.spawn((
        MinimapIcons,
        Node {
            position_type: PositionType::Absolute,
            width: percent(100),
            height: percent(100),
            ..default()
        },
        ChildOf(frame),
    ));
    // Us, in the middle: scaled and turned by its transform.
    let heading = commands
        .spawn((
            PlayerHeading,
            UiTransform::default(),
            Node {
                position_type: PositionType::Absolute,
                left: percent(50),
                top: percent(50),
                width: px(0),
                height: px(0),
                ..default()
            },
            ZIndex(10),
            ChildOf(frame),
        ))
        .id();
    // Classic: a dot and a line pointing where we look.
    commands.spawn((
        ClassicHeading,
        Node {
            position_type: PositionType::Absolute,
            width: px(0),
            height: px(0),
            ..default()
        },
        ChildOf(heading),
        children![
            (
                Node {
                    position_type: PositionType::Absolute,
                    left: px(-1),
                    top: px(-14),
                    width: px(2),
                    height: px(14),
                    ..default()
                },
                BackgroundColor(Color::srgba(1.0, 1.0, 1.0, 0.8)),
            ),
            (
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
            ),
        ],
    ));
    // Tactical: a white arrow.
    let arrow = commands
        .spawn((
            TacticalHeading,
            Node {
                position_type: PositionType::Absolute,
                width: px(0),
                height: px(0),
                ..default()
            },
            Visibility::Hidden,
            ChildOf(heading),
        ))
        .id();
    let look = ShapeLook {
        kind: ShapeKind::Arrow,
        fill: Color::WHITE,
        outline: Color::srgb(0.1, 0.12, 0.14),
        progress: None,
        pulse: false,
        text: None,
        text_color: Color::WHITE,
        heading: Some(0.0),
    };
    spawn_shape(&mut commands, &mut shapes, arrow, &look, 17.0, 0.0, ());
    let edge = commands
        .spawn((
            MinimapEdge,
            Node {
                position_type: PositionType::Absolute,
                left: px(2),
                top: px(2),
                width: px(SIZE - 4.0),
                height: px(SIZE - 4.0),
                ..default()
            },
            ChildOf(root),
        ))
        .id();
    commands.spawn((
        NorthMark,
        Text::new("N"),
        TextFont {
            font_size: FontSize::Px(11.0),
            ..default()
        },
        TextColor(Color::srgba(0.92, 0.95, 1.0, 0.9)),
        TextShadow {
            offset: Vec2::splat(1.0),
            color: Color::srgba(0.0, 0.0, 0.0, 0.9),
        },
        TextLayout::new(Justify::Center, LineBreak::NoWrap),
        Node {
            position_type: PositionType::Absolute,
            left: px(-6),
            top: px(-8),
            width: px(12),
            height: px(16),
            ..default()
        },
        UiTransform::default(),
        Visibility::Hidden,
        ChildOf(edge),
    ));
}

/// Sizes and styles the minimap for the settings (`minimap_size`, `map_style`), and moves the
/// kill feed under it.
#[allow(clippy::type_complexity)]
fn apply_minimap_look(
    settings: Res<Settings>,
    mut applied: Local<Option<Look>>,
    mut root: Query<&mut Node, (With<MinimapRoot>, Without<MinimapFrame>, Without<MinimapEdge>, NotMarker)>,
    mut frame: Query<
        (&mut Node, &mut BackgroundColor, &mut BorderColor),
        (With<MinimapFrame>, Without<MinimapRoot>, Without<MinimapEdge>, NotMarker),
    >,
    mut edge: Query<&mut Node, (With<MinimapEdge>, Without<MinimapRoot>, Without<MinimapFrame>, NotMarker)>,
    mut classic: Query<&mut Visibility, (With<ClassicHeading>, Without<TacticalHeading>, NotMarker)>,
    mut tactical: Query<&mut Visibility, (With<TacticalHeading>, Without<ClassicHeading>, NotMarker)>,
    mut kill_feed: Query<
        &mut Node,
        (With<crate::hud::KillFeedText>, Without<MinimapRoot>, Without<MinimapFrame>, Without<MinimapEdge>, NotMarker),
    >,
) {
    let look = Look::of(&settings);
    if *applied == Some(look) {
        return;
    }
    *applied = Some(look);
    if let Ok(mut node) = root.single_mut() {
        node.width = px(look.side);
        node.height = px(look.side);
    }
    if let Ok((mut node, mut background, mut border)) = frame.single_mut() {
        node.border = UiRect::all(px(look.border));
        node.border_radius = BorderRadius::all(px(if look.tactical { 2.0 } else { 10.0 }));
        background.0 = if look.tactical { TACTICAL_BACKGROUND } else { BACKGROUND };
        *border = BorderColor::all(if look.tactical { TACTICAL_BORDER } else { Color::srgba(0.0, 0.0, 0.0, 0.5) });
    }
    if let Ok(mut node) = edge.single_mut() {
        node.left = px(look.border);
        node.top = px(look.border);
        node.width = px(look.inner());
        node.height = px(look.inner());
    }
    for mut visibility in &mut classic {
        visibility.set_if_neq(if look.tactical { Visibility::Hidden } else { Visibility::Inherited });
    }
    for mut visibility in &mut tactical {
        visibility.set_if_neq(if look.tactical { Visibility::Inherited } else { Visibility::Hidden });
    }
    // --- The kill feed sits under the minimap (hud.rs) ---
    if let Ok(mut node) = kill_feed.single_mut() {
        let top = px(12.0 + look.side + 12.0);
        if node.top != top {
            node.top = top;
        }
    }
}

/// With `BF2_MINIMAP_TIMING` set, logs what the minimap's systems ([`update_zoom`],
/// [`update_minimap`]) cost the main thread: the mean over 5 s (see `docs/ARCHITECTURE.md`'s
/// "Frame time"). Timed inside the systems: other systems may run between them.
#[derive(Default)]
struct Timing {
    total: std::time::Duration,
    max_frame: std::time::Duration,
    frame: std::time::Duration,
    frames: u32,
    since: Option<std::time::Instant>,
}

static TIMING_ON: std::sync::LazyLock<bool> = std::sync::LazyLock::new(|| std::env::var_os("BF2_MINIMAP_TIMING").is_some());
static TIMING: std::sync::Mutex<Option<Timing>> = std::sync::Mutex::new(None);

/// Adds the time since it was made to the frame's minimap time when dropped; `last` closes the
/// frame.
struct TimeGuard {
    start: std::time::Instant,
    last: bool,
}

impl TimeGuard {
    fn new(last: bool) -> Option<Self> {
        TIMING_ON.then(|| Self {
            start: std::time::Instant::now(),
            last,
        })
    }
}

impl Drop for TimeGuard {
    fn drop(&mut self) {
        let now = std::time::Instant::now();
        let Ok(mut guard) = TIMING.lock() else { return };
        let timing = guard.get_or_insert_with(Timing::default);
        timing.frame += now - self.start;
        if !self.last {
            return;
        }
        timing.total += timing.frame;
        timing.max_frame = timing.max_frame.max(timing.frame);
        timing.frame = std::time::Duration::ZERO;
        timing.frames += 1;
        let since = *timing.since.get_or_insert(now);
        if now - since >= std::time::Duration::from_secs(5) {
            info!(
                "minimap timing: {:.3} ms mean, {:.3} ms max over {} frames",
                timing.total.as_secs_f64() * 1000.0 / timing.frames as f64,
                timing.max_frame.as_secs_f64() * 1000.0,
                timing.frames
            );
            *timing = Timing {
                since: Some(now),
                ..default()
            };
        }
    }
}

fn toggle_rotation(actions: crate::settings::Actions, mut settings: ResMut<MinimapSettings>) {
    if actions.just_pressed(crate::settings::Action::MinimapRotation) {
        settings.rotating = !settings.rotating;
    }
}

/// How quickly the range follows its target (seconds), and how fast it may change at most
/// (a factor per second): entering a fast vehicle zooms out over a second or two, never in a
/// jump.
const ZOOM_EASE: f32 = 0.9;
const ZOOM_MAX_RATE: f32 = 1.6;
/// Speed smoothing (seconds), and the speeds and heights at which the far range is reached.
const SPEED_EASE: f32 = 0.5;
const VEHICLE_FULL_SPEED: f32 = 22.0;
const AIR_FULL_SPEED: f32 = 90.0;
const AIR_FULL_HEIGHT: f32 = 300.0;

/// Eases [`MinimapZoom::range`] towards the range our situation calls for: on foot the
/// on-foot range (further out falling from high up), in a land or sea vehicle from a little
/// more than that at rest to the vehicle range at speed, flying from the vehicle range to the
/// air range with speed or height; with `minimap_speed_zoom` off just the three ranges.
/// Easing is in log space (zooming 2x takes as long at any range).
#[allow(clippy::too_many_arguments)]
fn update_zoom(
    time: Res<Time>,
    settings: Res<Settings>,
    level: Option<Res<LoadedLevel>>,
    camera: Single<&GlobalTransform, With<PlayerCamera>>,
    seat: Query<&Seated, With<LocalSoldier>>,
    vehicles: Query<(&Vehicle, Option<&crate::vehicles::VehicleView>)>,
    icons: Res<UiIcons>,
    mut zoom: ResMut<MinimapZoom>,
) {
    let _timing = TimeGuard::new(false);
    let dt = time.delta_secs().max(1e-4);
    let position = camera.translation();
    if level.as_ref().is_some_and(|l| l.is_changed()) {
        zoom.range = 0.0;
        zoom.last = None;
    }
    let seated = seat.single().ok();
    let vehicle = seated.and_then(|s| vehicles.get(s.vehicle).ok());
    // Speed over the ground: the vehicle's, else the camera's (ignoring teleports, respawns
    // and the camera moving into a seat).
    let measured = match vehicle.and_then(|(_, view)| view) {
        Some(view) => Some((view.velocity * Vec3::new(1.0, 0.0, 1.0)).length()),
        None => zoom.last.map(|last| ((position - last) * Vec3::new(1.0, 0.0, 1.0)).length() / dt).filter(|s| *s < 70.0),
    };
    if let Some(speed) = measured {
        zoom.speed += (speed - zoom.speed) * (1.0 - (-dt / SPEED_EASE).exp());
    }
    zoom.last = Some(position);
    let height = level
        .as_ref()
        .and_then(|l| l.heightmap.as_ref())
        .map_or(0.0, |h| (position.y - h.height_at(position.x, position.z) - 2.0).max(0.0));

    let air = vehicle
        .and_then(|(v, _)| icons.vehicle_class(&v.template))
        .is_some_and(|c| matches!(c, game_data::VehicleClass::Jet | game_data::VehicleClass::Helicopter));
    let mode = match seated {
        None => ZoomMode::Foot,
        Some(_) if air => ZoomMode::Air,
        Some(_) => ZoomMode::Vehicle,
    };
    let foot = settings.minimap_range.max(50.0);
    let vehicle = settings.minimap_vehicle_range.max(foot);
    let far = settings.minimap_air_range.max(vehicle);
    // `a` to `b` by `t` in log space.
    let towards = |a: f32, b: f32, t: f32| a * (b / a).powf(t.clamp(0.0, 1.0));
    let speed_zoom = settings.minimap_speed_zoom;
    let target = match mode {
        ZoomMode::Foot if speed_zoom => towards(foot, vehicle, (height - 25.0) / 150.0),
        ZoomMode::Foot => foot,
        ZoomMode::Vehicle if speed_zoom => towards(foot, vehicle, 0.35 + 0.65 * zoom.speed / VEHICLE_FULL_SPEED),
        ZoomMode::Vehicle => vehicle,
        ZoomMode::Air if speed_zoom => {
            towards(vehicle, far, (zoom.speed / AIR_FULL_SPEED).max((height - 10.0) / AIR_FULL_HEIGHT))
        }
        ZoomMode::Air => far,
    };
    if zoom.mode != Some(mode) {
        info!("minimap: {mode:?} range, easing to {target:.0} m");
        zoom.mode = Some(mode);
    }
    if zoom.range <= 0.0 {
        zoom.range = target;
        return;
    }
    let current = zoom.range.ln();
    let step = (target.ln() - current) * (1.0 - (-dt / ZOOM_EASE).exp());
    let limit = ZOOM_MAX_RATE.ln() * dt;
    zoom.range = (current + step.clamp(-limit, limit)).exp();
}

#[allow(clippy::too_many_arguments, clippy::type_complexity)]
fn update_minimap(
    settings: Res<MinimapSettings>,
    game_settings: Res<Settings>,
    zoom: Res<MinimapZoom>,
    level: Option<Res<LoadedLevel>>,
    camera: Single<&GlobalTransform, With<PlayerCamera>>,
    players: Query<(&Team, Option<&SquadMember>), With<LocalPlayer>>,
    teams: Query<(&Team, Option<&SquadMember>)>,
    soldiers: Query<
        (Entity, &SoldierRender, &ControlledBy, Has<game_shared::revive::Downed>),
        (With<Soldier>, Without<LocalSoldier>, Without<Seated>),
    >,
    (slots, order_lines, base_zones, orders): (
        Res<SquadSlots>,
        Res<OrderLines>,
        Res<BaseZones>,
        Query<(), With<game_shared::commander::SquadOrder>>,
    ),
    mut map: Single<&mut MapSurface, With<MinimapMap>>,
    icons_root: Single<Entity, With<MinimapIcons>>,
    edge_root: Single<Entity, With<MinimapEdge>>,
    mut icons: MarkerIcons,
    (mut heading, mut north): (
        Single<(&mut UiTransform, &mut Visibility), (With<PlayerHeading>, NotMarker)>,
        Single<(&mut UiTransform, &mut Visibility), (With<NorthMark>, Without<PlayerHeading>, NotMarker)>,
    ),
    seated: Query<(), (With<LocalSoldier>, With<Seated>)>,
    markers: Res<MapMarkers>,
) {
    let _timing = TimeGuard::new(true);
    let Some(level) = level else {
        return;
    };
    let look = Look::of(&game_settings);
    let inner = look.inner();
    let half = inner / 2.0;
    let range = if zoom.range > 0.0 { zoom.range } else { game_settings.minimap_range };
    let (center, map_meters) = to_map(&level, camera.translation());
    // Heading clockwise from north (-Z, up on the map).
    let forward = camera.forward();
    let heading_angle = forward.x.atan2(-forward.z);
    let map_angle = if settings.rotating { heading_angle } else { 0.0 };
    let (transform, visibility) = &mut *heading;
    let rotation = Rot2::radians(heading_angle - map_angle);
    let scale = Vec2::splat(look.icons);
    if transform.rotation != rotation || transform.scale != scale {
        transform.rotation = rotation;
        transform.scale = scale;
    }
    // In a vehicle its icon shows where we are (the dot would cover its turret).
    visibility.set_if_neq(if seated.is_empty() { Visibility::Inherited } else { Visibility::Hidden });

    let (local, local_squad) = players
        .single()
        .map(|(team, squad)| (*team, squad.copied()))
        .unwrap_or_default();

    // The map: only touched when it moved (a changed material is prepared again).
    let pixels_per_unit = inner * map_meters / range;
    MapSurface::set(
        &mut map,
        MapSurface {
            center,
            rotation: map_angle,
            span: range / map_meters,
            pixels_per_uv: pixels_per_unit,
            background: if look.tactical { Color::NONE } else { BACKGROUND },
            opacity: TACTICAL_OPACITY,
            grid: 0.0,
            lines: if look.tactical { map_lines(&level, &order_lines.ours) } else { Vec::new() },
            zones: if look.tactical { base_zones.0.clone() } else { Vec::new() },
        },
    );

    // Teammates on foot (those in vehicles show with the vehicle).
    let mut teammates = Vec::new();
    for (entity, render, controlled_by, downed) in &soldiers {
        let (team, squad) = teams.get(controlled_by.0).map(|(t, s)| (*t, s.copied())).unwrap_or_default();
        if team == local && local != Team::Spectator {
            let squad_mate = local_squad.zip(squad).is_some_and(|(a, b)| a.squad == b.squad);
            let marker = match (downed, squad_mate, look.tactical) {
                // Critically wounded teammates stand out, for medics.
                (true, _, _) => MapMarker::dot(entity, render.position, crate::wounded::WOUNDED, 9.0),
                (false, true, true) => {
                    let slot = slots.0.get(&controlled_by.0).copied().unwrap_or(0);
                    MapMarker::shape(entity, render.position, soldier_look(Some(slot), -render.yaw), 11.0)
                }
                (false, false, true) => MapMarker::shape(entity, render.position, soldier_look(None, -render.yaw), 6.5),
                (false, true, false) => MapMarker::dot(entity, render.position, SQUAD, 6.0),
                (false, false, false) => MapMarker::dot(entity, render.position, FRIENDLY, 6.0),
            };
            teammates.push(marker.layer(SOLDIER_LAYER));
        }
    }

    // Map offsets to minimap pixels, turned the opposite way to the map.
    let turn = Rot2::radians(-map_angle);
    let offset_of = |position: Vec3| turn * ((to_map(&level, position).0 - center) * pixels_per_unit);
    // Objectives out of range stay at the edge, larger (tactical style).
    let mut pinned: Vec<(MapMarker, Vec2, bool)> = Vec::new();
    let rings = if look.tactical { order_rings(&markers.0, &orders) } else { Vec::new() };
    let placed: Vec<(&MapMarker, MapPoint, bool)> = teammates
        .iter()
        .chain(markers.0.iter().filter(|m| !look.tactical || !is_order(m, &orders)))
        .chain(&rings)
        .map(|marker| {
            let offset = offset_of(marker.position);
            let reach = marker.size * look.icons * 0.5;
            let inside = offset.x.abs() < half + reach && offset.y.abs() < half + reach;
            let mut shown = inside;
            if look.tactical && marker.pin {
                let out = offset.x.abs().max(offset.y.abs());
                let pin = out > half - 2.0;
                // Mostly inside the frame, over its edge.
                let edge = half - marker.size * PIN_SCALE * look.icons * 0.3;
                let at = if pin { offset * (edge / out) } else { offset };
                pinned.push((
                    MapMarker {
                        size: marker.size * PIN_SCALE,
                        label: None,
                        ..marker.clone()
                    },
                    at,
                    pin,
                ));
                shown = !pin;
            }
            (marker, MapPoint::Pixels(offset + Vec2::splat(half)), shown)
        })
        .collect();
    let style = IconStyle {
        scale: look.icons,
        labels: false,
        turn: map_angle,
        size: Vec2::splat(inner),
    };
    icons.sync(*icons_root, placed, style);
    let pin_size = crate::map_icons::TACTICAL_FLAG_SIZE * PIN_SCALE * look.icons;
    spread_pinned(&mut pinned, half - pin_size * 0.3, pin_size * 1.05);
    icons.sync(
        *edge_root,
        pinned.iter().map(|(m, at, shown)| (m, MapPoint::Pixels(*at + Vec2::splat(half)), *shown)),
        style,
    );

    // North at the edge while the map turns.
    let (transform, visibility) = &mut *north;
    let show_north = look.tactical && settings.rotating;
    visibility.set_if_neq(if show_north { Visibility::Inherited } else { Visibility::Hidden });
    if show_north {
        let up = turn * Vec2::new(0.0, -1.0);
        let at = up * (half / up.x.abs().max(up.y.abs())) + Vec2::splat(half);
        let at = at - up * 8.0;
        let translation = Val2::px(at.x.round(), at.y.round());
        if transform.translation != translation {
            transform.translation = translation;
        }
    }
}

/// Moves pinned objectives (points on the square of half side `edge` around the centre, where
/// the flag is `true`) apart along the square's edge so that none covers another (`gap`
/// apart at least): relaxed in turns, each overlapping pair pushed apart equally.
fn spread_pinned(pinned: &mut [(MapMarker, Vec2, bool)], edge: f32, gap: f32) {
    let perimeter = 8.0 * edge;
    if edge <= 0.0 {
        return;
    }
    // Clockwise from the top left corner.
    let to_t = |p: Vec2| {
        let p = p.clamp(Vec2::splat(-edge), Vec2::splat(edge));
        if (p.y + edge).abs() < 1e-3 {
            p.x + edge
        } else if (p.x - edge).abs() < 1e-3 {
            2.0 * edge + p.y + edge
        } else if (p.y - edge).abs() < 1e-3 {
            4.0 * edge + edge - p.x
        } else {
            6.0 * edge + edge - p.y
        }
    };
    let from_t = |t: f32| {
        let t = t.rem_euclid(perimeter);
        let side = (t / (2.0 * edge)).floor();
        let along = t - side * 2.0 * edge;
        match side as i32 {
            0 => Vec2::new(-edge + along, -edge),
            1 => Vec2::new(edge, -edge + along),
            2 => Vec2::new(edge - along, edge),
            _ => Vec2::new(-edge, edge - along),
        }
    };
    let mut ts: Vec<(f32, usize)> = pinned
        .iter()
        .enumerate()
        .filter(|(_, (_, _, pin))| *pin)
        .map(|(i, (_, at, _))| {
            let out = at.x.abs().max(at.y.abs()).max(1e-3);
            (to_t(*at * (edge / out)), i)
        })
        .collect();
    if ts.len() < 2 {
        return;
    }
    ts.sort_by(|a, b| a.0.total_cmp(&b.0));
    for _ in 0..12 {
        let mut moved = false;
        for k in 0..ts.len() {
            let next = (k + 1) % ts.len();
            let mut d = ts[next].0 - ts[k].0;
            if next == 0 {
                d += perimeter;
            }
            if d < gap {
                let push = (gap - d) * 0.5;
                ts[k].0 -= push;
                ts[next].0 += push;
                moved = true;
            }
        }
        if !moved {
            break;
        }
    }
    for (t, i) in ts {
        pinned[i].1 = from_t(t);
    }
}
