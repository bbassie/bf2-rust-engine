//! The top of the screen in the tactical map style (`Settings::map_style`), after
//! Battlefield 6: both teams' tickets in boxes of their colour (ours left) with thin ticket
//! bars growing towards the middle, under them the objectives as the maps' shapes
//! (`map_shapes`: ours a circle, theirs a diamond, neutral a square, each with its letter, the
//! capture progress as a pie and a pulse while it changes hands; Rush's charges the same way),
//! and a short line saying what to do (the stage in the staged modes). The classic style keeps
//! `conquest_hud`'s bar.

use bevy::prelude::*;
use game_shared::{
    conquest::{ControlPoint, FlagState, Tickets},
    level::LoadedLevel,
    modes::{Charge, ChargeState, ChargeTimes, Locked, ModeState},
    protocol::Team,
};

use game_data::modes::ModeKind;

use crate::{
    conquest_hud::{ClassicBar, FlagFeedRoot, team_name},
    map_shapes::{
        ObjectiveLetters, ShapeMaterial, ShapeParams, ShapeText, Side, charge_look, objective_look, spawn_shape, update_shape,
    },
    net::LocalPlayer,
    settings::{MapStyle, Settings},
    ui_theme::font,
};

pub struct ObjectiveBarPlugin;

impl Plugin for ObjectiveBarPlugin {
    fn build(&self, app: &mut App) {
        app.add_systems(Startup, spawn_bar)
            .add_systems(Update, (apply_style, rebuild_objectives, update_tickets, update_objectives, update_line).chain());
    }
}

/// Size of the objective shapes, logical pixels.
const SHAPE: f32 = 24.0;
/// Width of each team's ticket bar.
const BAR_WIDTH: f32 = 190.0;
const LINE: Color = Color::srgba(0.88, 0.91, 0.95, 0.85);

#[derive(Component)]
struct TacticalBar;
/// A team's ticket box (0 ours, left; 1 theirs, right) and its text.
#[derive(Component)]
struct TicketBox(usize);
#[derive(Component)]
struct TicketNumber(usize);
/// A team's ticket bar: the track, and its fill.
#[derive(Component)]
struct TicketTrack(usize);
#[derive(Component)]
struct TicketFill(usize);
#[derive(Component)]
struct ObjectiveRow;
/// What an objective shape stands for.
#[derive(Component, Clone, Copy, PartialEq)]
enum Objective {
    Flag(Entity),
    Charge(Entity),
}
#[derive(Component)]
struct ObjectiveLine;
/// An armed charge's fuse, under its shape.
#[derive(Component)]
struct FuseText(Entity);

fn spawn_bar(mut commands: Commands) {
    let root = commands
        .spawn((
            TacticalBar,
            Node {
                position_type: PositionType::Absolute,
                width: percent(100),
                top: px(10),
                flex_direction: FlexDirection::Column,
                align_items: AlignItems::Center,
                row_gap: px(4),
                ..default()
            },
            Visibility::Hidden,
            Pickable::IGNORE,
        ))
        .id();
    let tickets = commands
        .spawn((
            Node {
                align_items: AlignItems::Center,
                column_gap: px(6),
                ..default()
            },
            ChildOf(root),
        ))
        .id();
    for side in [0, 1] {
        let ticket_box = commands
            .spawn((
                TicketBox(side),
                Node {
                    min_width: px(66),
                    height: px(30),
                    padding: UiRect::axes(px(10), px(0)),
                    justify_content: JustifyContent::Center,
                    align_items: AlignItems::Center,
                    border_radius: BorderRadius::all(px(2)),
                    ..default()
                },
                BackgroundColor(Side::Friendly.fill()),
            ))
            .with_child((
                TicketNumber(side),
                Text::new(""),
                font(22.0),
                TextColor(Side::Friendly.color()),
            ))
            .id();
        let track = commands
            .spawn((
                TicketTrack(side),
                Node {
                    width: px(BAR_WIDTH),
                    height: px(7),
                    // Ours grows from our box, theirs from theirs: both towards the middle.
                    justify_content: if side == 0 { JustifyContent::FlexStart } else { JustifyContent::FlexEnd },
                    ..default()
                },
                BackgroundColor(Color::srgba(1.0, 1.0, 1.0, 0.1)),
            ))
            .with_child((
                TicketFill(side),
                Node {
                    width: percent(100),
                    height: percent(100),
                    ..default()
                },
                BackgroundColor(Side::Friendly.color()),
            ))
            .id();
        let (first, second) = if side == 0 { (ticket_box, track) } else { (track, ticket_box) };
        commands.entity(tickets).add_children(&[first, second]);
        if side == 0 {
            // A gap in the middle.
            commands.spawn((
                Node {
                    width: px(10),
                    ..default()
                },
                ChildOf(tickets),
            ));
        }
    }
    commands.spawn((
        ObjectiveRow,
        Node {
            column_gap: px(5),
            align_items: AlignItems::Center,
            margin: UiRect::top(px(2)),
            ..default()
        },
        ChildOf(root),
    ));
    commands.spawn((
        ObjectiveLine,
        Text::new(""),
        font(12.0),
        TextColor(LINE),
        TextShadow {
            offset: Vec2::splat(1.0),
            color: Color::srgba(0.0, 0.0, 0.0, 0.8),
        },
        Node {
            padding: UiRect::axes(px(8), px(1)),
            border_radius: BorderRadius::all(px(2)),
            ..default()
        },
        BackgroundColor(Color::srgba(0.02, 0.03, 0.05, 0.4)),
        ChildOf(root),
    ));
}

fn local_team(players: &Query<&Team, With<LocalPlayer>>) -> Team {
    players.single().copied().unwrap_or_default()
}

/// Our team (left) and theirs; a spectator sees team 1 left.
fn sides(local: Team) -> [Team; 2] {
    let ours = if local == Team::Two { Team::Two } else { Team::One };
    [ours, ours.opponent()]
}

/// Shows this bar or the classic one for the style (once there is a match), and moves the
/// capture notifications under the one showing.
#[allow(clippy::type_complexity)]
fn apply_style(
    settings: Res<Settings>,
    tickets: Query<(), With<Tickets>>,
    mut tactical: Query<(&mut Visibility, &ComputedNode), (With<TacticalBar>, Without<ClassicBar>)>,
    mut classic: Query<&mut Visibility, (With<ClassicBar>, Without<TacticalBar>)>,
    mut feed: Query<&mut Node, With<FlagFeedRoot>>,
    big_map: Query<&Visibility, (With<crate::bigmap::BigMapRoot>, Without<TacticalBar>, Without<ClassicBar>)>,
    mut below_big_map: Query<
        &mut Visibility,
        (
            Or<(With<ObjectiveRow>, With<ObjectiveLine>)>,
            Without<TacticalBar>,
            Without<ClassicBar>,
            Without<crate::bigmap::BigMapRoot>,
        ),
    >,
) {
    let on = settings.map_style == MapStyle::Tactical;
    let shown = |show: bool| if show { Visibility::Inherited } else { Visibility::Hidden };
    let mut height = 0.0;
    for (mut visibility, node) in &mut tactical {
        visibility.set_if_neq(shown(on && !tickets.is_empty()));
        height = node.size.y * node.inverse_scale_factor;
    }
    for mut visibility in &mut classic {
        visibility.set_if_neq(shown(!on));
    }
    // The objectives would peek out over the big map's top edge.
    let big_map_open = big_map.iter().any(|v| *v != Visibility::Hidden);
    for mut visibility in &mut below_big_map {
        visibility.set_if_neq(shown(!big_map_open));
    }
    // The notifications go under whichever bar shows (its whole height: with a fuse under a
    // charge it grows).
    let top = px(if on { (10.0 + height + 8.0).round().max(72.0) } else { 72.0 });
    for mut node in &mut feed {
        if node.top != top {
            node.top = top;
        }
    }
}

/// The objectives the bar shows: the current stage's charges in Rush, else the flags that can
/// be captured now (not main bases, not locked), in layout order.
#[allow(clippy::type_complexity)]
fn rebuild_objectives(
    mut commands: Commands,
    settings: Res<Settings>,
    modes: Query<&ModeState>,
    points: Query<(Entity, &ControlPoint, Has<Locked>)>,
    charges: Query<(Entity, &Charge)>,
    letters: Res<ObjectiveLetters>,
    row: Single<(Entity, Option<&Children>), With<ObjectiveRow>>,
    mut shapes: ResMut<Assets<ShapeMaterial>>,
    mut built: Local<Vec<Objective>>,
) {
    if settings.map_style != MapStyle::Tactical {
        return;
    }
    let stage = modes.single().ok().filter(|m| m.kind == ModeKind::Rush).map(|m| m.stage);
    let mut wanted: Vec<(u8, Objective)> = match stage {
        Some(stage) => charges
            .iter()
            .filter(|(_, c)| c.stage == stage)
            .map(|(e, c)| (c.index, Objective::Charge(e)))
            .collect(),
        None => points
            .iter()
            .filter(|(_, cp, locked)| !cp.uncapturable && !locked)
            .map(|(e, cp, _)| (cp.index, Objective::Flag(e)))
            .collect(),
    };
    wanted.sort_by_key(|(index, _)| *index);
    let wanted: Vec<Objective> = wanted.into_iter().map(|(_, o)| o).collect();
    if *built == wanted && !letters.is_changed() {
        return;
    }
    let (row, children) = *row;
    for child in children.into_iter().flatten() {
        commands.entity(*child).despawn();
    }
    for objective in &wanted {
        // The shape, and under it (in the flow, so the bar grows) an armed charge's fuse.
        let cell = commands
            .spawn((
                Node {
                    flex_direction: FlexDirection::Column,
                    align_items: AlignItems::Center,
                    ..default()
                },
                ChildOf(row),
            ))
            .id();
        let shape_box = commands
            .spawn((
                Node {
                    width: px(SHAPE + 4.0),
                    height: px(SHAPE + 4.0),
                    ..default()
                },
                ChildOf(cell),
            ))
            .id();
        let center = commands
            .spawn((
                Node {
                    position_type: PositionType::Absolute,
                    left: percent(50),
                    top: percent(50),
                    ..default()
                },
                ChildOf(shape_box),
            ))
            .id();
        let letter = match objective {
            Objective::Flag(e) => letters.get(*e).to_string(),
            Objective::Charge(e) => charges.get(*e).map_or(String::new(), |(_, c)| c.name.clone()),
        };
        let look = objective_look(&FlagState::default(), false, Team::Spectator, &letter);
        spawn_shape(&mut commands, &mut shapes, center, &look, SHAPE, 0.0, *objective);
        if let Objective::Charge(charge) = objective {
            commands.spawn((
                FuseText(*charge),
                Text::new(""),
                font(11.0),
                TextColor(Side::Friendly.color()),
                TextShadow {
                    offset: Vec2::splat(1.0),
                    color: Color::srgba(0.0, 0.0, 0.0, 0.9),
                },
                TextLayout::new(Justify::Center, LineBreak::NoWrap),
                Node {
                    display: Display::None,
                    ..default()
                },
                ChildOf(cell),
            ));
        }
    }
    *built = wanted;
}

/// The tickets: numbers (the team's name for the staged modes' defenders, who have none),
/// pulsing while they bleed or run low, and the bars.
#[allow(clippy::type_complexity)]
fn update_tickets(
    time: Res<Time>,
    settings: Res<Settings>,
    level: Option<Res<LoadedLevel>>,
    players: Query<&Team, With<LocalPlayer>>,
    tickets: Query<&Tickets>,
    modes: Query<&ModeState>,
    mut boxes: Query<(&TicketBox, &mut BackgroundColor), (Without<TicketFill>, Without<TicketTrack>)>,
    mut numbers: Query<(&TicketNumber, &mut Text, &mut TextColor)>,
    mut tracks: Query<(&TicketTrack, &mut Node), Without<TicketFill>>,
    mut fills: Query<(&TicketFill, &mut Node, &mut BackgroundColor), (Without<TicketTrack>, Without<TicketBox>)>,
) {
    if settings.map_style != MapStyle::Tactical {
        return;
    }
    let Ok(tickets) = tickets.single() else {
        return;
    };
    let local = local_team(&players);
    let teams = sides(local);
    let mode = modes.single().ok();
    let index = |team: Team| if team == Team::Two { 1 } else { 0 };
    let has_tickets = |team: Team| mode.is_none_or(|m| m.has_tickets(team));
    for (ticket_box, mut background) in &mut boxes {
        background.set_if_neq(BackgroundColor(Side::of(teams[ticket_box.0], local).fill()));
    }
    for (number, mut text, mut color) in &mut numbers {
        let team = teams[number.0];
        let i = index(team);
        let line = if has_tickets(team) {
            format!("{}", tickets.remaining[i].ceil().max(0.0) as i32)
        } else {
            team_name(level.as_deref(), team)
        };
        if text.0 != line {
            text.0 = line;
        }
        let low = mode.is_some_and(|m| m.staged() && m.has_tickets(team)) && tickets.remaining[i] < 0.2 * tickets.start[i];
        let pulse = if tickets.bleed[i] > 0.0 || low { 0.65 + 0.35 * (time.elapsed_secs() * 5.0).sin() } else { 1.0 };
        let wanted = Side::of(team, local).color().with_alpha(pulse);
        if color.0 != wanted {
            color.0 = wanted;
        }
    }
    for (track, mut node) in &mut tracks {
        let display = if has_tickets(teams[track.0]) { Display::Flex } else { Display::None };
        if node.display != display {
            node.display = display;
        }
    }
    for (fill, mut node, mut background) in &mut fills {
        let team = teams[fill.0];
        let i = index(team);
        let share = if tickets.start[i] > 0.0 { (tickets.remaining[i] / tickets.start[i]).clamp(0.0, 1.0) } else { 0.0 };
        // Whole percent steps: a changed node lays the HUD out again.
        crate::hud::set_width(&mut node, percent((share * 100.0).round()));
        background.set_if_neq(BackgroundColor(Side::of(team, local).color()));
    }
}

/// Each objective shape for its flag or charge.
#[allow(clippy::type_complexity)]
fn update_objectives(
    settings: Res<Settings>,
    players: Query<&Team, With<LocalPlayer>>,
    modes: Query<(&ModeState, Option<&ChargeTimes>)>,
    points: Query<(&ControlPoint, &FlagState)>,
    charges: Query<(&Charge, &ChargeState)>,
    letters: Res<ObjectiveLetters>,
    mut materials: ResMut<Assets<ShapeMaterial>>,
    mut shapes: Query<(&Objective, &MaterialNode<ShapeMaterial>, &Children, &mut Visibility)>,
    mut texts: Query<&mut TextColor, (With<ShapeText>, Without<FuseText>)>,
    mut fuses: Query<(&FuseText, &mut Text, &mut TextColor, &mut Node), Without<ShapeText>>,
) {
    if settings.map_style != MapStyle::Tactical {
        return;
    }
    let local = local_team(&players);
    let (mode, times) = modes.single().ok().map_or((None, None), |(m, t)| (Some(m), t));
    let fuse = times.map(|t| t.fuse);
    // An armed charge's fuse, in whole seconds (the text changes once a second).
    for (fuse_text, mut text, mut color, mut node) in &mut fuses {
        let left = match charges.get(fuse_text.0) {
            Ok((_, ChargeState::Armed { fuse, .. })) => Some(*fuse),
            _ => None,
        };
        let display = if left.is_some() { Display::Flex } else { Display::None };
        if node.display != display {
            node.display = display;
        }
        if let (Some(left), Some(mode)) = (left, mode) {
            let wanted = format!("{:.0}", left.ceil().max(0.0));
            if text.0 != wanted {
                text.0 = wanted;
            }
            let side = Side::of(mode.attacker, local).color();
            if color.0 != side {
                color.0 = side;
            }
        }
    }
    for (objective, material, children, mut visibility) in &mut shapes {
        let look = match *objective {
            Objective::Flag(e) => points
                .get(e)
                .ok()
                .map(|(cp, state)| objective_look(state, cp.uncapturable, local, letters.get(e))),
            Objective::Charge(e) => {
                mode.zip(charges.get(e).ok()).and_then(|(mode, (charge, state))| charge_look(state, mode, local, &charge.name, fuse))
            }
        };
        visibility.set_if_neq(if look.is_some() { Visibility::Inherited } else { Visibility::Hidden });
        let Some(look) = look else { continue };
        update_shape(&mut materials, material, ShapeParams::new(&look, SHAPE, 0.0));
        for child in children {
            if let Ok(mut color) = texts.get_mut(*child)
                && color.0 != look.text_color
            {
                color.0 = look.text_color;
            }
        }
    }
}

/// What to do: in Conquest to hold more objectives, in the staged modes the stage and our
/// role.
fn update_line(
    settings: Res<Settings>,
    level: Option<Res<LoadedLevel>>,
    matches: Query<&game_shared::protocol::MatchInfo>,
    players: Query<&Team, With<LocalPlayer>>,
    modes: Query<&ModeState>,
    line: Single<&mut Text, With<ObjectiveLine>>,
) {
    if settings.map_style != MapStyle::Tactical {
        return;
    }
    let local = local_team(&players);
    let mode = modes.single().ok();
    let wanted = match mode.filter(|m| m.staged()) {
        None if mode.is_some_and(|m| m.kind == ModeKind::TeamDeathmatch) => "DEFEAT THE ENEMY TEAM".to_string(),
        None => "CAPTURE AND HOLD MORE OBJECTIVES THAN THE ENEMY".to_string(),
        Some(mode) => {
            let rush = mode.kind == ModeKind::Rush;
            let role = match local {
                Team::Spectator => "",
                team if team == mode.attacker && rush => "  |  ARM THE CHARGES",
                team if team == mode.attacker => "  |  CAPTURE THE OBJECTIVES",
                _ if rush => "  |  DEFEND THE CHARGES",
                _ => "  |  HOLD THE OBJECTIVES",
            };
            let overtime = if mode.overtime { "  |  OVERTIME" } else { "" };
            let name = level
                .as_deref()
                .zip(matches.single().ok())
                .and_then(|(level, info)| level.game_mode(&info.mode, info.size))
                .and_then(|layout| layout.staged.as_ref())
                .and_then(|staged| staged.stages.get(mode.stage as usize))
                .map(|stage| stage.name.clone())
                .filter(|name| !name.is_empty() && !name.eq_ignore_ascii_case(&mode.stage_label()))
                .map_or(String::new(), |name| format!(": {}", name.to_uppercase()));
            format!("{}/{}{name}{role}{overtime}", mode.stage_label().to_uppercase(), mode.stages)
        }
    };
    let mut text = line.into_inner();
    if text.0 != wanted {
        text.0 = wanted;
    }
}
