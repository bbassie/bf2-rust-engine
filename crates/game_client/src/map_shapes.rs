//! The tactical map style's shapes (`Settings::map_style`), drawn from distance fields by
//! [`ShapeMaterial`] (`map_shape.wgsl`) so they stay crisp at any size: objectives as a letter
//! in a shape by owner (ours a blue circle, theirs a red diamond, neutral a grey square) with
//! the capture progress as a pie in the capturing team's colour and a pulse while it changes
//! hands, squad mates as numbered green circles and teammates as blue dots, both with a heading
//! triangle, and the player's arrow. The minimap, the big map, the commander screen, the
//! deploy map and the objective strip at the top of the HUD all use the same shapes, colours
//! and letters ([`ObjectiveLetters`]: A, B, C... by layout order, HQ for main bases).

use bevy::{
    platform::collections::HashMap,
    prelude::*,
    render::render_resource::{AsBindGroup, ShaderType},
    shader::ShaderRef,
    ui::FocusPolicy,
    ui_render::prelude::{MaterialNode, UiMaterial, UiMaterialPlugin},
};
use game_shared::{
    commander::SquadOrder,
    conquest::{ControlPoint, FlagState},
    level::LoadedLevel,
    protocol::{ControlledBy, Team},
    soldier::Soldier,
    squad::SquadMember,
    vehicle::Seated,
};

use crate::{
    map_icons::{map_size, map_uv},
    map_markers::MarkerSystems,
    net::LocalPlayer,
    prediction::SoldierRender,
    settings::{MapStyle, Settings},
    vehicles::VehicleView,
};

pub struct MapShapesPlugin;

impl Plugin for MapShapesPlugin {
    fn build(&self, app: &mut App) {
        embedded_shader!(app, "map_shape.wgsl");
        app.add_plugins(UiMaterialPlugin::<ShapeMaterial>::default())
            .init_resource::<ObjectiveLetters>()
            .init_resource::<SquadSlots>()
            .init_resource::<OrderLines>()
            .add_systems(PreUpdate, (assign_letters, assign_squad_slots))
            .add_systems(PostUpdate, order_lines.in_set(MarkerSystems));
    }
}

/// The tactical style's colours.
pub mod palette {
    use bevy::prelude::Color;

    pub const FRIENDLY: Color = Color::srgb(0.36, 0.80, 1.0);
    pub const ENEMY: Color = Color::srgb(1.0, 0.42, 0.30);
    pub const NEUTRAL: Color = Color::srgb(0.86, 0.88, 0.91);
    pub const SQUAD: Color = Color::srgb(0.62, 0.95, 0.45);
    /// Squad order lines (the references' dashed orange).
    pub const ORDER: Color = Color::srgb(1.0, 0.68, 0.25);
    pub const FRIENDLY_FILL: Color = Color::srgba(0.04, 0.20, 0.31, 0.78);
    pub const ENEMY_FILL: Color = Color::srgba(0.36, 0.08, 0.05, 0.82);
    pub const NEUTRAL_FILL: Color = Color::srgba(0.09, 0.10, 0.12, 0.70);
    pub const SQUAD_FILL: Color = Color::srgba(0.72, 0.96, 0.60, 0.95);
    /// Text on a squad mate's light green circle.
    pub const SQUAD_TEXT: Color = Color::srgb(0.08, 0.18, 0.05);
}

/// Whose a thing is, as seen by us.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Side {
    Friendly,
    Enemy,
    Neutral,
}

impl Side {
    /// `team` as seen by a player of `local` (a spectator sees team 2 as friendly).
    pub fn of(team: Team, local: Team) -> Self {
        match (team, local) {
            (Team::Spectator, _) => Side::Neutral,
            (Team::Two, Team::Spectator) => Side::Friendly,
            (_, Team::Spectator) => Side::Enemy,
            (team, local) if team == local => Side::Friendly,
            _ => Side::Enemy,
        }
    }

    pub fn color(self) -> Color {
        match self {
            Side::Friendly => palette::FRIENDLY,
            Side::Enemy => palette::ENEMY,
            Side::Neutral => palette::NEUTRAL,
        }
    }

    pub fn fill(self) -> Color {
        match self {
            Side::Friendly => palette::FRIENDLY_FILL,
            Side::Enemy => palette::ENEMY_FILL,
            Side::Neutral => palette::NEUTRAL_FILL,
        }
    }

    /// Objectives: ours a circle, theirs a diamond, neutral a square.
    pub fn shape(self) -> ShapeKind {
        match self {
            Side::Friendly => ShapeKind::Circle,
            Side::Enemy => ShapeKind::Diamond,
            Side::Neutral => ShapeKind::Square,
        }
    }
}

/// A team colour in the current style: the tactical palette, or the HUD's classic colours.
pub fn team_color(style: MapStyle, team: Team, local: Team) -> Color {
    match style {
        MapStyle::Classic => crate::conquest_hud::team_color(team, local),
        MapStyle::Tactical => Side::of(team, local).color(),
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum ShapeKind {
    Circle,
    Diamond,
    Square,
    /// The player: a chevron pointing along the heading.
    Arrow,
}

/// How a shape looks.
#[derive(Clone, Debug, PartialEq)]
pub struct ShapeLook {
    pub kind: ShapeKind,
    pub fill: Color,
    pub outline: Color,
    /// Capture progress (0..1) as a pie in this colour.
    pub progress: Option<(f32, Color)>,
    /// Changing hands: the outline breathes.
    pub pulse: bool,
    /// A letter or number in the middle.
    pub text: Option<String>,
    pub text_color: Color,
    /// A heading triangle (the arrow's direction), clockwise from north on the map, radians.
    pub heading: Option<f32>,
}

impl ShapeLook {
    /// A plain dot (spotted enemies, teammates).
    pub fn dot(color: Color) -> Self {
        Self {
            kind: ShapeKind::Circle,
            fill: color,
            outline: color.darker(0.25),
            progress: None,
            pulse: false,
            text: None,
            text_color: Color::WHITE,
            heading: None,
        }
    }

    pub fn heading(mut self, heading: f32) -> Self {
        self.heading = Some(heading);
        self
    }

    /// Side of the square node drawn for a shape `size` across (room for the heading
    /// triangle, the halo and the pulse).
    pub fn node_side(&self, size: f32) -> f32 {
        let factor = if self.heading.is_some() && self.kind != ShapeKind::Arrow { 2.0 } else { 1.4 };
        (size * factor).ceil()
    }
}

/// Draws a [`ShapeLook`] (see `map_shape.wgsl`).
#[derive(AsBindGroup, Asset, TypePath, Debug, Clone)]
pub struct ShapeMaterial {
    #[uniform(0)]
    pub params: ShapeParams,
}

#[derive(ShaderType, Debug, Clone, Copy, PartialEq)]
pub struct ShapeParams {
    fill: LinearRgba,
    outline: LinearRgba,
    accent: LinearRgba,
    shape: Vec4,
    extra: Vec4,
}

impl UiMaterial for ShapeMaterial {
    fn fragment_shader() -> ShaderRef {
        "embedded://client/map_shape.wgsl".into()
    }
}

/// Headings are rounded to this (radians) so that turning a little doesn't touch the
/// material (a changed material is prepared and uploaded again).
const HEADING_STEP: f32 = 0.06;

impl ShapeParams {
    /// For `look` drawn `size` logical pixels across on a map turned by `turn` (clockwise).
    pub fn new(look: &ShapeLook, size: f32, turn: f32) -> Self {
        let side = look.node_side(size);
        // Half the node's side is 1.
        let unit = 2.0 / side;
        let outline = match look.kind {
            ShapeKind::Arrow => 1.0,
            _ if look.text.is_some() => (size * 0.085).clamp(1.2, 2.6),
            _ => (size * 0.14).clamp(1.0, 2.0),
        };
        let heading = look.heading.map_or(0.0, |h| ((h - turn) / HEADING_STEP).round() * HEADING_STEP);
        let (progress, accent) = look.progress.map_or((-1.0, Color::NONE), |(p, c)| (p.clamp(0.0, 1.0), c));
        Self {
            fill: look.fill.to_linear(),
            outline: look.outline.to_linear(),
            accent: accent.with_alpha(0.75 * accent.alpha()).to_linear(),
            shape: Vec4::new(
                match look.kind {
                    ShapeKind::Circle => 0.0,
                    ShapeKind::Diamond => 1.0,
                    ShapeKind::Square => 2.0,
                    ShapeKind::Arrow => 3.0,
                },
                size / side,
                outline * unit,
                progress,
            ),
            extra: Vec4::new(
                heading,
                if look.heading.is_some() && look.kind != ShapeKind::Arrow { 1.0 } else { 0.0 },
                if look.pulse { 1.0 } else { 0.0 },
                1.2 * unit,
            ),
        }
    }
}

/// Font size of a shape's text.
pub fn text_size(size: f32, text: &str) -> f32 {
    let chars = text.chars().count().max(1) as f32;
    (size * if chars > 1.0 { 0.44 } else { 0.6 }).max(6.0)
}

/// A shape's text: a child filling the shape's node, centred.
#[derive(Component)]
pub struct ShapeText;

/// Spawns `look` as a child of `parent`: a `side`-square node centred on the parent's
/// top-left corner (a zero-sized marker root) with the text in it. Returns the node.
pub fn spawn_shape(
    commands: &mut Commands,
    materials: &mut Assets<ShapeMaterial>,
    parent: Entity,
    look: &ShapeLook,
    size: f32,
    turn: f32,
    extra: impl Bundle,
) -> Entity {
    let side = look.node_side(size);
    let node = commands
        .spawn((
            Node {
                position_type: PositionType::Absolute,
                left: px(-side / 2.0),
                top: px(-side / 2.0),
                width: px(side),
                height: px(side),
                justify_content: JustifyContent::Center,
                align_items: AlignItems::Center,
                ..default()
            },
            MaterialNode(materials.add(ShapeMaterial {
                params: ShapeParams::new(look, size, turn),
            })),
            FocusPolicy::Pass,
            ChildOf(parent),
            extra,
        ))
        .id();
    if let Some(text) = &look.text {
        commands.spawn((
            ShapeText,
            Text::new(text.clone()),
            TextFont {
                font_size: FontSize::Px(text_size(size, text)),
                ..default()
            },
            TextColor(look.text_color),
            TextLayout::new(Justify::Center, LineBreak::NoWrap),
            FocusPolicy::Pass,
            ChildOf(node),
        ));
    }
    node
}

/// Updates a shape material's parameters, only if they changed.
pub fn update_shape(materials: &mut Assets<ShapeMaterial>, material: &MaterialNode<ShapeMaterial>, params: ShapeParams) {
    if materials.get(&material.0).is_some_and(|m| m.params != params)
        && let Some(mut asset) = materials.get_mut(&material.0)
    {
        asset.params = params;
    }
}

/// Each control point's letter: A, B, C... for the capturable ones by layout order, HQ for
/// main bases.
#[derive(Resource, Default)]
pub struct ObjectiveLetters(pub HashMap<Entity, String>);

impl ObjectiveLetters {
    pub fn get(&self, entity: Entity) -> &str {
        self.0.get(&entity).map_or("?", |s| s.as_str())
    }
}

fn assign_letters(
    added: Query<(), Added<ControlPoint>>,
    mut removed: RemovedComponents<ControlPoint>,
    points: Query<(Entity, &ControlPoint)>,
    mut letters: ResMut<ObjectiveLetters>,
) {
    if added.is_empty() && removed.read().count() == 0 {
        return;
    }
    let mut capturable: Vec<(Entity, &ControlPoint)> = points.iter().filter(|(_, cp)| !cp.uncapturable).collect();
    capturable.sort_by_key(|(_, cp)| cp.index);
    letters.0 = capturable
        .iter()
        .enumerate()
        .map(|(i, (entity, _))| {
            let letter = if i < 26 { ((b'A' + i as u8) as char).to_string() } else { format!("{}", i + 1) };
            (*entity, letter)
        })
        .chain(points.iter().filter(|(_, cp)| cp.uncapturable).map(|(e, _)| (e, "HQ".to_string())))
        .collect();
}

/// Each squad member's number in his squad: the leader 1, the others in the order they are
/// known to us.
#[derive(Resource, Default)]
pub struct SquadSlots(pub HashMap<Entity, u8>);

fn assign_squad_slots(players: Query<(Entity, &Team, &SquadMember)>, mut slots: ResMut<SquadSlots>) {
    let mut squads: HashMap<(Team, u8), Vec<(bool, Entity)>> = HashMap::default();
    for (entity, team, member) in &players {
        squads.entry((*team, member.squad)).or_default().push((!member.leader, entity));
    }
    let mut next: HashMap<Entity, u8> = HashMap::default();
    for members in squads.values_mut() {
        members.sort();
        for (i, (_, entity)) in members.iter().enumerate() {
            next.insert(*entity, i as u8 + 1);
        }
    }
    if slots.0 != next {
        slots.0 = next;
    }
}

/// An objective's look: its letter in its owner's shape, the capture progress as a pie in the
/// capturing team's colour, pulsing while its flag moves.
pub fn objective_look(state: &FlagState, uncapturable: bool, local: Team, letter: &str) -> ShapeLook {
    let side = Side::of(state.owner, local);
    let other = |team: Team| match team {
        Team::One => Team::Two,
        Team::Two => Team::One,
        Team::Spectator => Team::Spectator,
    };
    let progress = if uncapturable {
        None
    } else if state.owner != Team::Spectator && state.flag == state.owner {
        // The owner's flag coming down: the attackers' share.
        (state.height < 0.995).then(|| (1.0 - state.height, Side::of(other(state.owner), local).color()))
    } else if state.flag != Team::Spectator && state.height > 0.005 {
        // A flag going up (or the owner's coming back).
        Some((state.height, Side::of(state.flag, local).color()))
    } else {
        None
    };
    ShapeLook {
        kind: side.shape(),
        fill: side.fill(),
        outline: side.color(),
        progress,
        pulse: state.rate.abs() > 1e-4 && !uncapturable,
        text: Some(letter.to_string()),
        text_color: side.color().lighter(0.08),
        heading: None,
    }
}

/// A teammate on the tactical maps: squad mates a numbered light green circle, others a blue
/// dot; both with a heading triangle.
pub fn soldier_look(squad_slot: Option<u8>, heading: f32) -> ShapeLook {
    match squad_slot {
        Some(slot) => ShapeLook {
            kind: ShapeKind::Circle,
            fill: palette::SQUAD_FILL,
            outline: palette::SQUAD,
            progress: None,
            pulse: false,
            text: Some(slot.to_string()),
            text_color: palette::SQUAD_TEXT,
            heading: Some(heading),
        },
        None => ShapeLook::dot(palette::FRIENDLY).heading(heading),
    }
}

/// Whether the tactical style is on.
pub fn tactical(settings: &Settings) -> bool {
    settings.map_style == MapStyle::Tactical
}

/// Squad order lines, world positions: from each member of our squad to its order, and (for
/// the commander screen) from every ordered squad's members of our team to their order.
#[derive(Resource, Default)]
pub struct OrderLines {
    pub ours: Vec<(Vec3, Vec3)>,
    pub all: Vec<(Vec3, Vec3)>,
}

#[allow(clippy::type_complexity)]
fn order_lines(
    local: Query<(&Team, Option<&SquadMember>), With<LocalPlayer>>,
    orders: Query<&SquadOrder>,
    players: Query<(&Team, &SquadMember)>,
    soldiers: Query<(&SoldierRender, &ControlledBy, Option<&Seated>), With<Soldier>>,
    vehicles: Query<&VehicleView>,
    mut lines: ResMut<OrderLines>,
) {
    lines.ours.clear();
    lines.all.clear();
    let Ok((&team, squad)) = local.single() else {
        return;
    };
    if team == Team::Spectator || orders.is_empty() {
        return;
    }
    let targets: HashMap<u8, Vec3> = orders.iter().filter(|o| o.team == team).map(|o| (o.squad, o.position)).collect();
    for (render, controlled_by, seated) in &soldiers {
        let Ok((member_team, member)) = players.get(controlled_by.0) else {
            continue;
        };
        let Some(&target) = targets.get(&member.squad).filter(|_| *member_team == team) else {
            continue;
        };
        let at = seated
            .and_then(|s| vehicles.get(s.vehicle).ok())
            .map_or(render.position, |v| v.transform.translation);
        lines.all.push((at, target));
        if squad.is_some_and(|s| s.squad == member.squad) {
            lines.ours.push((at, target));
        }
    }
}

/// Main bases (uncapturable points) as tinted zones: map position, radius (map shares) and
/// their owner's colour.
pub fn team_zones<'a>(
    level: &LoadedLevel,
    points: impl IntoIterator<Item = (&'a ControlPoint, &'a FlagState)>,
    local: Team,
) -> Vec<(Vec2, f32, Color)> {
    let size = map_size(level);
    points
        .into_iter()
        .filter(|(cp, state)| cp.uncapturable && state.owner != Team::Spectator)
        .map(|(cp, state)| {
            let radius = (cp.radius * 2.5).clamp(45.0, 120.0) / size;
            (map_uv(level, cp.position), radius, Side::of(state.owner, local).color())
        })
        .collect()
}

/// `lines` as map positions.
pub fn map_lines(level: &LoadedLevel, lines: &[(Vec3, Vec3)]) -> Vec<(Vec2, Vec2)> {
    lines.iter().map(|(a, b)| (map_uv(level, *a), map_uv(level, *b))).collect()
}

/// Squad orders in the tactical style: an orange ring around the ordered spot (under the flag
/// there) instead of a dot covering it. `markers` are all the maps' markers; returns the rings
/// for those that are orders, which the maps draw instead of them (see [`is_order`]).
pub fn order_rings(markers: &[crate::map_markers::MapMarker], orders: &Query<(), With<SquadOrder>>) -> Vec<crate::map_markers::MapMarker> {
    markers
        .iter()
        .filter(|m| orders.contains(m.key))
        .map(|m| {
            let look = ShapeLook {
                kind: ShapeKind::Circle,
                fill: palette::ORDER.with_alpha(0.14),
                outline: palette::ORDER,
                progress: None,
                pulse: false,
                text: None,
                text_color: Color::WHITE,
                heading: None,
            };
            crate::map_markers::MapMarker {
                label: m.label.clone(),
                priority: m.priority,
                layer: 0,
                ..crate::map_markers::MapMarker::shape(m.key, m.position, look, 30.0)
            }
        })
        .collect()
}

/// Whether a marker is a squad order (drawn as a ring by [`order_rings`] instead).
pub fn is_order(marker: &crate::map_markers::MapMarker, orders: &Query<(), With<SquadOrder>>) -> bool {
    orders.contains(marker.key)
}

/// A Rush charge's look (tactical style), `None` while its stage hasn't come: the defenders'
/// shape with its letter while it is to be armed (the arming progress as a pie in the
/// attackers' colour), the attackers' shape pulsing once armed (the defusing progress in the
/// defenders' colour), a dim square once destroyed.
pub fn charge_look(
    state: &game_shared::modes::ChargeState,
    mode: &game_shared::modes::ModeState,
    local: Team,
    name: &str,
) -> Option<ShapeLook> {
    use game_shared::modes::ChargeState;
    let attackers = Side::of(mode.attacker, local);
    let defenders = Side::of(mode.defender(), local);
    let shaped = |side: Side, progress: Option<(f32, Color)>, pulse: bool| ShapeLook {
        kind: side.shape(),
        fill: side.fill(),
        outline: side.color(),
        progress,
        pulse,
        text: Some(name.to_string()),
        text_color: side.color().lighter(0.08),
        heading: None,
    };
    match *state {
        ChargeState::Waiting => None,
        ChargeState::Active { progress } => {
            Some(shaped(defenders, (progress > 0.001).then(|| (progress, attackers.color())), progress > 0.001))
        }
        ChargeState::Armed { progress, .. } => {
            Some(shaped(attackers, (progress > 0.001).then(|| (progress, defenders.color())), true))
        }
        ChargeState::Destroyed => Some(ShapeLook {
            kind: ShapeKind::Square,
            fill: Color::srgba(0.08, 0.08, 0.09, 0.6),
            outline: Color::srgba(0.55, 0.56, 0.6, 0.6),
            progress: None,
            pulse: false,
            text: Some(name.to_string()),
            text_color: Color::srgba(0.6, 0.62, 0.66, 0.8),
            heading: None,
        }),
    }
}
