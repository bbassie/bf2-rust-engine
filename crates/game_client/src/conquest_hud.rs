//! The objectives on the HUD, for every mode: tickets and objectives at the top (with the
//! teams' flags; a pill per control point with its owner's flag and its letter on the owner's
//! colour, or per Rush charge with its state), the stage in the staged modes, progress while
//! standing at a flag or a charge, objective notifications, and the end-of-round banner.
//! Colors are relative to us: blue is our team, red the enemy, grey neutral.
//!
//! - Conquest, co-op: both teams' tickets, every capturable flag.
//! - Breakthrough: the attackers' tickets, the open sector's flags, `SECTOR 2/4 | ATTACK`.
//! - Rush: the attackers' tickets, the stage's charges (arming progress, the fuse of an armed
//!   one), `STAGE 2/4 | DEFEND`; at a charge, what to do there and how far along it is.
//!
//! The charges themselves, their markers and the map icons are in [`crate::mode_hud`].

use std::collections::VecDeque;

use bevy::prelude::*;
use game_shared::{
    conquest::{ControlPoint, FlagEvent, FlagEventKind, FlagState, RoundState, Tickets},
    level::LoadedLevel,
    modes::{Charge, ChargeState, ChargeTimes, Locked, ModeState, ObjectiveEvent, ObjectiveEventKind},
    protocol::Team,
    soldier::SoldierMotion,
};

use crate::{
    map_icons::UiIcons,
    net::{LocalPlayer, LocalSoldier},
};

pub struct ConquestHudPlugin;

impl Plugin for ConquestHudPlugin {
    fn build(&self, app: &mut App) {
        app.init_resource::<FlagFeed>()
            .add_systems(Startup, spawn_conquest_hud)
            .add_systems(
                Update,
                (
                    rebuild_flag_pills,
                    rebuild_charge_pills,
                    update_tickets,
                    update_ticket_flags,
                    update_flag_pills,
                    update_charge_pills,
                    update_stage_line,
                    update_capture_panel,
                    receive_flag_events,
                    receive_objective_events,
                    update_flag_feed,
                    update_round_banner,
                ),
            );
    }
}

pub const FRIENDLY: Color = Color::srgb(0.30, 0.58, 1.0);
pub const ENEMY: Color = Color::srgb(0.95, 0.33, 0.28);
pub const NEUTRAL: Color = Color::srgb(0.62, 0.64, 0.68);
/// Our squad, as in BF2.
pub const SQUAD: Color = Color::srgb(0.45, 0.9, 0.4);
/// A destroyed charge.
pub const DESTROYED: Color = Color::srgb(0.22, 0.23, 0.25);
const TEXT: Color = Color::srgb(0.95, 0.96, 0.98);
const DIM: Color = Color::srgba(0.85, 0.87, 0.9, 0.65);
const PANEL: Color = Color::srgba(0.05, 0.06, 0.08, 0.6);
/// Charges near enough to work on show in the progress panel, meters.
const CHARGE_PANEL_DISTANCE: f32 = 6.0;

/// Color of `team` as seen by a player of `local`.
pub fn team_color(team: Team, local: Team) -> Color {
    match (team, local) {
        (Team::Spectator, _) => NEUTRAL,
        (team, Team::Spectator) => {
            if team == Team::One { Color::srgb(0.95, 0.6, 0.25) } else { FRIENDLY }
        }
        (team, local) if team == local => FRIENDLY,
        _ => ENEMY,
    }
}

/// Color of a charge in `state` for a player of `local`: the defenders' while it is to be
/// armed, the attackers' (pulsing, at `time` seconds) once armed, dark when destroyed.
pub fn charge_color(state: &ChargeState, mode: &ModeState, local: Team, time: f32) -> Color {
    match state {
        ChargeState::Waiting => NEUTRAL.with_alpha(0.6),
        ChargeState::Active { .. } => team_color(mode.defender(), local),
        ChargeState::Armed { .. } => {
            let pulse = 0.6 + 0.4 * (time * 6.0).sin().abs();
            team_color(mode.attacker, local).with_alpha(pulse)
        }
        ChargeState::Destroyed => DESTROYED,
    }
}

/// Short display name of a team from the level (`MEC`, `US`, ...).
pub fn team_name(level: Option<&LoadedLevel>, team: Team) -> String {
    let index = match team {
        Team::One => 0,
        Team::Two => 1,
        Team::Spectator => return "Neutral".into(),
    };
    level
        .and_then(|l| l.desc.teams.get(index))
        .map(|t| t.name.clone())
        .filter(|n| !n.is_empty())
        .unwrap_or_else(|| format!("Team {}", index + 1))
}

fn local_team(players: &Query<&Team, With<LocalPlayer>>) -> Team {
    players.single().copied().unwrap_or_default()
}

#[derive(Component)]
struct TicketText {
    /// 0 = ours (left), 1 = theirs (right).
    side: usize,
}
/// A team's flag next to its tickets.
#[derive(Component)]
struct TicketFlag {
    side: usize,
}
#[derive(Component)]
struct FlagRow;
#[derive(Component)]
struct FlagPill(Entity);
/// The owner's flag inside a pill.
#[derive(Component)]
struct FlagPillIcon(Entity);
#[derive(Component)]
struct FlagPillBar(Entity);
/// Rush: the stage's charges.
#[derive(Component)]
struct ChargeRow;
#[derive(Component)]
struct ChargePill(Entity);
/// The letter, or the fuse of an armed charge.
#[derive(Component)]
struct ChargePillText(Entity);
#[derive(Component)]
struct ChargePillBar(Entity);
/// `STAGE 2/4 | ATTACK` under the tickets (staged modes).
#[derive(Component)]
struct StageLine;
#[derive(Component)]
struct CapturePanel;
#[derive(Component)]
struct CaptureText;
#[derive(Component)]
struct CaptureFill;
#[derive(Component)]
struct FlagFeedText;
#[derive(Component)]
struct RoundBanner;
#[derive(Component)]
struct RoundBannerText;

fn font(size: f32) -> TextFont {
    TextFont {
        font_size: FontSize::Px(size),
        ..default()
    }
}

fn shadow() -> TextShadow {
    TextShadow {
        offset: Vec2::splat(1.0),
        color: Color::srgba(0.0, 0.0, 0.0, 0.8),
    }
}

fn spawn_conquest_hud(mut commands: Commands) {
    // Top center: our tickets, the objectives, their tickets; the stage under them.
    commands
        .spawn(Node {
            position_type: PositionType::Absolute,
            width: percent(100),
            top: px(10),
            justify_content: JustifyContent::Center,
            ..default()
        })
        .with_children(|bar| {
            bar.spawn((
                Node {
                    padding: UiRect::axes(px(12), px(6)),
                    flex_direction: FlexDirection::Column,
                    align_items: AlignItems::Center,
                    row_gap: px(3),
                    border_radius: BorderRadius::all(px(8)),
                    ..default()
                },
                BackgroundColor(PANEL),
            ))
            .with_children(|panel| {
                panel
                    .spawn(Node {
                        column_gap: px(12),
                        align_items: AlignItems::Center,
                        ..default()
                    })
                    .with_children(|row| {
                        row.spawn(ticket_flag(0));
                        row.spawn((TicketText { side: 0 }, Text::new(""), font(20.0), TextColor(FRIENDLY)));
                        row.spawn((
                            FlagRow,
                            Node {
                                column_gap: px(5),
                                align_items: AlignItems::Center,
                                ..default()
                            },
                        ));
                        row.spawn((
                            ChargeRow,
                            Node {
                                column_gap: px(6),
                                align_items: AlignItems::Center,
                                display: Display::None,
                                ..default()
                            },
                        ));
                        row.spawn((TicketText { side: 1 }, Text::new(""), font(20.0), TextColor(ENEMY)));
                        row.spawn(ticket_flag(1));
                    });
                panel.spawn((
                    StageLine,
                    Text::new(""),
                    font(12.0),
                    TextColor(DIM),
                    Node {
                        display: Display::None,
                        ..default()
                    },
                ));
            });
        });

    // Capture progress, above the crosshair's lower half.
    commands
        .spawn(Node {
            position_type: PositionType::Absolute,
            width: percent(100),
            top: percent(62),
            justify_content: JustifyContent::Center,
            ..default()
        })
        .with_children(|root| {
            root.spawn((
                CapturePanel,
                Node {
                    padding: UiRect::axes(px(14), px(8)),
                    flex_direction: FlexDirection::Column,
                    align_items: AlignItems::Center,
                    row_gap: px(6),
                    border_radius: BorderRadius::all(px(8)),
                    ..default()
                },
                BackgroundColor(PANEL),
                Visibility::Hidden,
            ))
            .with_children(|panel| {
                panel.spawn((CaptureText, Text::new(""), font(15.0), TextColor(TEXT)));
                panel
                    .spawn((
                        Node {
                            width: px(220),
                            height: px(6),
                            border_radius: BorderRadius::all(px(3)),
                            ..default()
                        },
                        BackgroundColor(Color::srgba(1.0, 1.0, 1.0, 0.15)),
                    ))
                    .with_child((
                        CaptureFill,
                        Node {
                            width: percent(0),
                            height: percent(100),
                            border_radius: BorderRadius::all(px(3)),
                            ..default()
                        },
                        BackgroundColor(NEUTRAL),
                    ));
            });
        });

    // Capture notifications, under the flag bar.
    commands
        .spawn(Node {
            position_type: PositionType::Absolute,
            width: percent(100),
            top: px(72),
            justify_content: JustifyContent::Center,
            ..default()
        })
        .with_child((
            FlagFeedText,
            Text::new(""),
            font(16.0),
            TextColor(TEXT),
            shadow(),
            TextLayout::justify(Justify::Center),
        ));

    // End of round.
    commands
        .spawn((
            RoundBanner,
            Node {
                position_type: PositionType::Absolute,
                width: percent(100),
                top: percent(22),
                justify_content: JustifyContent::Center,
                ..default()
            },
            Visibility::Hidden,
        ))
        .with_children(|root| {
            root.spawn((
                Node {
                    padding: UiRect::axes(px(36), px(18)),
                    border_radius: BorderRadius::all(px(10)),
                    ..default()
                },
                BackgroundColor(Color::srgba(0.04, 0.05, 0.07, 0.85)),
            ))
            .with_child((
                RoundBannerText,
                Text::new(""),
                font(30.0),
                TextColor(TEXT),
                TextLayout::justify(Justify::Center),
            ));
        });
}

fn ticket_flag(side: usize) -> impl Bundle {
    (
        TicketFlag { side },
        Node {
            width: px(30),
            height: px(20),
            border: UiRect::all(px(1)),
            border_radius: BorderRadius::all(px(3)),
            display: Display::None,
            ..default()
        },
        BorderColor::all(Color::srgba(0.0, 0.0, 0.0, 0.6)),
        ImageNode::default(),
    )
}

/// One pill per control point that can be captured now (not a main base, not locked), in
/// layout order, rebuilt when the set of points (or their locks) changes.
#[allow(clippy::too_many_arguments)]
fn rebuild_flag_pills(
    mut commands: Commands,
    added: Query<(), Added<ControlPoint>>,
    mut removed: RemovedComponents<ControlPoint>,
    locked_now: Query<(), Added<Locked>>,
    mut unlocked: RemovedComponents<Locked>,
    control_points: Query<(Entity, &ControlPoint, Has<Locked>)>,
    row: Single<(Entity, Option<&Children>), With<FlagRow>>,
) {
    let removed = removed.read().count() + unlocked.read().count();
    if added.is_empty() && locked_now.is_empty() && removed == 0 {
        return;
    }
    let (row, children) = *row;
    for child in children.into_iter().flatten() {
        commands.entity(*child).despawn();
    }
    let mut points: Vec<(Entity, &ControlPoint)> = control_points
        .iter()
        .filter(|(_, cp, locked)| !cp.uncapturable && !locked)
        .map(|(entity, cp, _)| (entity, cp))
        .collect();
    points.sort_by_key(|(_, cp)| cp.index);
    for (entity, cp) in points {
        let letter: String = cp.name.chars().find(|c| c.is_alphanumeric()).into_iter().collect();
        commands.entity(row).with_children(|row| {
            row.spawn(Node {
                flex_direction: FlexDirection::Column,
                align_items: AlignItems::Stretch,
                row_gap: px(2),
                ..default()
            })
            .with_children(|column| {
                // The owner's flag, and the point's letter beside it on the owner's colour
                // (on the flag it was hard to read).
                column
                    .spawn((
                        FlagPill(entity),
                        Node {
                            height: px(23),
                            padding: UiRect {
                                left: px(3),
                                right: px(5),
                                ..default()
                            },
                            column_gap: px(3),
                            justify_content: JustifyContent::Center,
                            align_items: AlignItems::Center,
                            border_radius: BorderRadius::all(px(5)),
                            ..default()
                        },
                        BackgroundColor(NEUTRAL),
                    ))
                    .with_children(|pill| {
                        pill.spawn((
                            FlagPillIcon(entity),
                            Node {
                                width: px(25),
                                height: px(17),
                                border: UiRect::all(px(1)),
                                border_radius: BorderRadius::all(px(2)),
                                display: Display::None,
                                ..default()
                            },
                            BorderColor::all(Color::srgba(0.0, 0.0, 0.0, 0.45)),
                            ImageNode::default(),
                        ));
                        pill.spawn((
                            Text::new(letter.to_uppercase()),
                            font(14.0),
                            TextColor(TEXT),
                            TextShadow {
                                offset: Vec2::splat(1.0),
                                color: Color::srgba(0.0, 0.0, 0.0, 0.7),
                            },
                            Node {
                                min_width: px(8),
                                ..default()
                            },
                        ));
                    });
                column
                    .spawn((
                        Node {
                            height: px(3),
                            border_radius: BorderRadius::all(px(1.5)),
                            ..default()
                        },
                        BackgroundColor(Color::srgba(1.0, 1.0, 1.0, 0.12)),
                    ))
                    .with_child((
                        FlagPillBar(entity),
                        Node {
                            width: percent(100),
                            height: percent(100),
                            border_radius: BorderRadius::all(px(1.5)),
                            ..default()
                        },
                        BackgroundColor(NEUTRAL),
                    ));
            });
        });
    }
}

/// Rush: a pill per charge of the current stage, rebuilt when the stage or the charges change.
fn rebuild_charge_pills(
    mut commands: Commands,
    modes: Query<&ModeState>,
    charges: Query<(Entity, &Charge)>,
    row: Single<(Entity, Option<&Children>, &mut Node), With<ChargeRow>>,
    mut built: Local<Option<(u8, Vec<Entity>)>>,
) {
    let stage = modes.single().ok().map(|m| m.stage);
    let mut current: Vec<(Entity, &Charge)> =
        charges.iter().filter(|(_, c)| Some(c.stage) == stage).collect();
    current.sort_by_key(|(_, c)| c.index);
    let key = stage.map(|s| (s, current.iter().map(|(e, _)| *e).collect::<Vec<_>>()));
    if *built == key {
        return;
    }
    *built = key;
    let (row, children, mut node) = row.into_inner();
    for child in children.into_iter().flatten() {
        commands.entity(*child).despawn();
    }
    node.display = if current.is_empty() { Display::None } else { Display::Flex };
    for (entity, charge) in current {
        commands.entity(row).with_children(|row| {
            row.spawn(Node {
                flex_direction: FlexDirection::Column,
                align_items: AlignItems::Stretch,
                row_gap: px(2),
                ..default()
            })
            .with_children(|column| {
                column
                    .spawn((
                        ChargePill(entity),
                        Name::new(format!("charge:{}", charge.name)),
                        Node {
                            min_width: px(34),
                            height: px(23),
                            padding: UiRect::axes(px(6), px(0)),
                            justify_content: JustifyContent::Center,
                            align_items: AlignItems::Center,
                            border: UiRect::all(px(1.5)),
                            border_radius: BorderRadius::all(px(5)),
                            ..default()
                        },
                        BackgroundColor(NEUTRAL),
                        BorderColor::all(Color::srgba(0.0, 0.0, 0.0, 0.5)),
                    ))
                    .with_child((
                        ChargePillText(entity),
                        Text::new(charge.name.clone()),
                        font(14.0),
                        TextColor(TEXT),
                        shadow(),
                    ));
                column
                    .spawn((
                        Node {
                            height: px(3),
                            border_radius: BorderRadius::all(px(1.5)),
                            ..default()
                        },
                        BackgroundColor(Color::srgba(1.0, 1.0, 1.0, 0.12)),
                    ))
                    .with_child((
                        ChargePillBar(entity),
                        Node {
                            width: percent(0),
                            height: percent(100),
                            border_radius: BorderRadius::all(px(1.5)),
                            ..default()
                        },
                        BackgroundColor(TEXT),
                    ));
            });
        });
    }
}

fn update_tickets(
    time: Res<Time>,
    level: Option<Res<LoadedLevel>>,
    players: Query<&Team, With<LocalPlayer>>,
    tickets: Query<&Tickets>,
    modes: Query<&ModeState>,
    mut texts: Query<(&TicketText, &mut Text, &mut TextColor)>,
) {
    let local = local_team(&players);
    let ours = if local == Team::Two { Team::Two } else { Team::One };
    let Ok(tickets) = tickets.single() else {
        return;
    };
    let mode = modes.single().ok();
    for (ticket, mut text, mut color) in &mut texts {
        let team = if ticket.side == 0 { ours } else { ours.opponent() };
        let index = if team == Team::Two { 1 } else { 0 };
        let bleeding = tickets.bleed[index] > 0.0;
        let name = team_name(level.as_deref(), team);
        // The defenders of the staged modes have no tickets: just the name.
        let line = match mode {
            Some(mode) if !mode.has_tickets(team) => name,
            _ => {
                let value = tickets.remaining[index].ceil() as i32;
                if ticket.side == 0 { format!("{name}  {value}") } else { format!("{value}  {name}") }
            }
        };
        if text.0 != line {
            text.0 = line;
        }
        // Bleeding tickets pulse, and running low in the staged modes.
        let low = mode.is_some_and(|m| m.staged() && m.has_tickets(team))
            && tickets.remaining[index] < 0.2 * tickets.start[index];
        let pulse = if bleeding || low { 0.65 + 0.35 * (time.elapsed_secs() * 5.0).sin() } else { 1.0 };
        color.0 = team_color(team, local).with_alpha(pulse);
    }
}

/// The teams' flags next to their tickets.
fn update_ticket_flags(
    icons: Res<UiIcons>,
    players: Query<&Team, With<LocalPlayer>>,
    mut flags: Query<(&TicketFlag, &mut ImageNode, &mut Node)>,
) {
    let local = local_team(&players);
    let ours = if local == Team::Two { Team::Two } else { Team::One };
    for (flag, mut image, mut node) in &mut flags {
        let team = if flag.side == 0 { ours } else { ours.opponent() };
        let handle = icons.side(team).flag.clone();
        let display = if handle.is_some() { Display::Flex } else { Display::None };
        if node.display != display {
            node.display = display;
        }
        let handle = handle.unwrap_or_default();
        if image.image != handle {
            image.image = handle;
        }
    }
}

#[allow(clippy::type_complexity)]
fn update_flag_pills(
    icons: Res<UiIcons>,
    players: Query<&Team, With<LocalPlayer>>,
    flags: Query<&FlagState>,
    mut pills: Query<(&FlagPill, &mut BackgroundColor), Without<FlagPillBar>>,
    mut bars: Query<(&FlagPillBar, &mut Node, &mut BackgroundColor), (Without<FlagPill>, Without<FlagPillIcon>)>,
    mut pill_icons: Query<(&FlagPillIcon, &mut ImageNode, &mut Node), Without<FlagPillBar>>,
) {
    let local = local_team(&players);
    // Without a flag for the owner (often neutral), just the letter.
    for (icon, mut image, mut node) in &mut pill_icons {
        let Ok(state) = flags.get(icon.0) else { continue };
        let handle = icons.side(state.owner).flag.clone();
        let display = if handle.is_some() { Display::Flex } else { Display::None };
        if node.display != display {
            node.display = display;
        }
        if let Some(handle) = handle
            && image.image != handle
        {
            image.image = handle;
        }
    }
    for (pill, mut background) in &mut pills {
        if let Ok(state) = flags.get(pill.0) {
            background.0 = team_color(state.owner, local).with_alpha(0.85);
        }
    }
    for (bar, mut node, mut background) in &mut bars {
        if let Ok(state) = flags.get(bar.0) {
            node.width = percent(state.height * 100.0);
            background.0 = team_color(state.flag, local);
        }
    }
}

/// Rush: each pill in its charge's colour, with the arming or defusing progress under it and
/// the fuse on it once armed.
#[allow(clippy::type_complexity)]
fn update_charge_pills(
    time: Res<Time>,
    players: Query<&Team, With<LocalPlayer>>,
    modes: Query<&ModeState>,
    charges: Query<(&Charge, &ChargeState)>,
    mut pills: Query<(&ChargePill, &mut BackgroundColor, &mut BorderColor), Without<ChargePillBar>>,
    mut texts: Query<(&ChargePillText, &mut Text)>,
    mut bars: Query<(&ChargePillBar, &mut Node, &mut BackgroundColor), Without<ChargePill>>,
) {
    let (Ok(mode), local) = (modes.single(), local_team(&players)) else {
        return;
    };
    let now = time.elapsed_secs();
    for (pill, mut background, mut border) in &mut pills {
        if let Ok((_, state)) = charges.get(pill.0) {
            background.0 = charge_color(state, mode, local, now).with_alpha(0.9);
            let edge = if state.armed() { TEXT } else { Color::srgba(0.0, 0.0, 0.0, 0.5) };
            *border = BorderColor::all(edge);
        }
    }
    for (text, mut line) in &mut texts {
        if let Ok((charge, state)) = charges.get(text.0) {
            let wanted = match state {
                ChargeState::Armed { fuse, .. } => format!("{} {:.0}", charge.name, fuse.ceil()),
                ChargeState::Destroyed => format!("{} X", charge.name),
                _ => charge.name.clone(),
            };
            if line.0 != wanted {
                line.0 = wanted;
            }
        }
    }
    for (bar, mut node, mut background) in &mut bars {
        if let Ok((_, state)) = charges.get(bar.0) {
            let (progress, color) = match state {
                ChargeState::Active { progress } => (*progress, team_color(mode.attacker, local)),
                ChargeState::Armed { progress, .. } => (*progress, team_color(mode.defender(), local)),
                _ => (0.0, TEXT),
            };
            node.width = percent(progress * 100.0);
            background.0 = color;
        }
    }
}

/// `STAGE 2/4 | ATTACK` (or `DEFEND`, `OVERTIME`) under the tickets in the staged modes, with
/// the stage's name if the layout gives it one of its own.
fn update_stage_line(
    level: Option<Res<LoadedLevel>>,
    matches: Query<&game_shared::protocol::MatchInfo>,
    players: Query<&Team, With<LocalPlayer>>,
    modes: Query<&ModeState>,
    line: Single<(&mut Text, &mut Node, &mut TextColor), With<StageLine>>,
) {
    let (mut text, mut node, mut color) = line.into_inner();
    let Some(mode) = modes.single().ok().filter(|m| m.staged()) else {
        if node.display != Display::None {
            node.display = Display::None;
        }
        return;
    };
    if node.display != Display::Flex {
        node.display = Display::Flex;
    }
    let local = local_team(&players);
    let role = match local {
        team if team == mode.attacker => "  |  ATTACK",
        Team::Spectator => "",
        _ => "  |  DEFEND",
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
    let wanted = format!("{}/{}{name}{role}{overtime}", mode.stage_label().to_uppercase(), mode.stages);
    if text.0 != wanted {
        text.0 = wanted;
    }
    color.0 = if mode.overtime { ENEMY } else { DIM };
}

/// What to do at the flag or charge we stand at, and how far along it is.
#[allow(clippy::too_many_arguments, clippy::type_complexity)]
fn update_capture_panel(
    level: Option<Res<LoadedLevel>>,
    players: Query<&Team, With<LocalPlayer>>,
    soldier: Query<&SoldierMotion, With<LocalSoldier>>,
    control_points: Query<(&ControlPoint, &FlagState, Has<Locked>)>,
    charges: Query<(&Charge, &ChargeState)>,
    modes: Query<(&ModeState, Option<&ChargeTimes>)>,
    mut panel: Single<&mut Visibility, With<CapturePanel>>,
    mut text: Single<&mut Text, With<CaptureText>>,
    mut fill: Single<(&mut Node, &mut BackgroundColor), With<CaptureFill>>,
) {
    let local = local_team(&players);
    let Ok(motion) = soldier.single() else {
        panel.set_if_neq(Visibility::Hidden);
        return;
    };
    // A charge of the current stage close by comes first.
    let mode = modes.single().ok();
    let charge = mode.and_then(|(mode, _)| {
        charges
            .iter()
            .filter(|(c, s)| c.stage == mode.stage && s.in_play())
            .map(|(c, s)| (c, s, c.position.distance(motion.position)))
            .filter(|(.., d)| *d < CHARGE_PANEL_DISTANCE)
            .min_by(|a, b| a.2.total_cmp(&b.2))
    });
    if let (Some((charge, state, _)), Some((mode, times))) = (charge, mode) {
        panel.set_if_neq(Visibility::Inherited);
        let reach = charge.in_reach(motion.position);
        let attacking = local == mode.attacker;
        let (line, progress, color) = match *state {
            ChargeState::Active { progress } if attacking => {
                let what = if progress > 0.0 { "ARMING" } else if reach { "HOLD USE TO ARM" } else { "GET CLOSER TO ARM" };
                (what.to_string(), progress, team_color(mode.attacker, local))
            }
            ChargeState::Active { progress } => {
                let what = if progress > 0.0 { "ENEMY ARMING" } else { "DEFEND" };
                (what.to_string(), progress, team_color(mode.attacker, local))
            }
            ChargeState::Armed { fuse, progress } if !attacking && local != Team::Spectator => {
                let what = if progress > 0.0 { "DEFUSING" } else if reach { "HOLD USE TO DEFUSE" } else { "GET CLOSER TO DEFUSE" };
                (format!("{what}   {fuse:.0} s"), progress, team_color(mode.defender(), local))
            }
            ChargeState::Armed { fuse, progress } => {
                let what = if progress > 0.0 { "ENEMY DEFUSING" } else { "ARMED" };
                (format!("{what}   {fuse:.0} s"), progress, team_color(mode.defender(), local))
            }
            _ => (String::new(), 0.0, NEUTRAL),
        };
        let seconds = times.map_or(String::new(), |t| match state {
            ChargeState::Active { .. } => format!("   ({:.0} s)", t.arm),
            ChargeState::Armed { .. } => String::new(),
            _ => String::new(),
        });
        text.0 = format!("CHARGE {}   {line}{}", charge.name, if progress > 0.0 { "" } else { &seconds });
        let (node, background) = &mut *fill;
        node.width = percent(progress * 100.0);
        background.0 = color;
        return;
    }
    let inside = control_points
        .iter()
        .find(|(cp, _, _)| !cp.uncapturable && cp.contains(motion.position));
    let Some((cp, state, locked)) = inside else {
        panel.set_if_neq(Visibility::Hidden);
        return;
    };
    panel.set_if_neq(Visibility::Inherited);
    let action = match (state.owner == local, state.rate) {
        _ if locked => "Locked",
        (_, r) if r == 0.0 && state.height > 0.0 && state.height < 1.0 => "Contested",
        (true, r) if r >= 0.0 => "Holding",
        (true, _) => "Losing",
        (false, r) if r > 0.0 && state.flag == local => "Capturing",
        (false, r) if r < 0.0 => "Neutralizing",
        (false, _) if state.owner == Team::Spectator => "Neutral",
        _ => "Enemy flag",
    };
    let owner = team_name(level.as_deref(), state.owner);
    text.0 = format!("{}   {action}   ({owner})", cp.name.to_uppercase());
    let (node, background) = &mut *fill;
    node.width = percent(state.height * 100.0);
    background.0 = team_color(state.flag, local);
}

/// Recent capture messages with the time they arrived.
#[derive(Resource, Default)]
struct FlagFeed(VecDeque<(String, f64)>);

impl FlagFeed {
    fn push(&mut self, line: String, now: f64) {
        self.0.push_back((line, now));
        while self.0.len() > 3 {
            self.0.pop_front();
        }
    }
}

fn receive_flag_events(
    time: Res<Time>,
    level: Option<Res<LoadedLevel>>,
    mut events: MessageReader<FlagEvent>,
    control_points: Query<&ControlPoint>,
    mut feed: ResMut<FlagFeed>,
) {
    for event in events.read() {
        let name = control_points
            .get(event.control_point)
            .map(|cp| cp.name.clone())
            .unwrap_or_default();
        let verb = match event.kind {
            FlagEventKind::Captured => "captured",
            FlagEventKind::Neutralized => "neutralized",
        };
        let team = team_name(level.as_deref(), event.team);
        feed.push(format!("{team} {verb} {name}"), time.elapsed_secs_f64());
    }
}

/// Charges armed, defused and destroyed, and stages taken.
fn receive_objective_events(
    time: Res<Time>,
    level: Option<Res<LoadedLevel>>,
    mut events: MessageReader<ObjectiveEvent>,
    charges: Query<&Charge>,
    modes: Query<&ModeState>,
    mut feed: ResMut<FlagFeed>,
) {
    for event in events.read() {
        let charge = event.charge.and_then(|c| charges.get(c).ok()).map_or(String::new(), |c| c.name.clone());
        let team = team_name(level.as_deref(), event.team);
        let line = match event.kind {
            ObjectiveEventKind::Armed => format!("{team} armed charge {charge}"),
            ObjectiveEventKind::Defused => format!("{team} defused charge {charge}"),
            ObjectiveEventKind::Destroyed => format!("Charge {charge} destroyed"),
            ObjectiveEventKind::StageTaken => {
                let noun = modes.single().map_or("Stage", |m| m.kind.stage_noun());
                format!("{team} took {noun} {}: the front moves on", event.stage + 1)
            }
        };
        feed.push(line, time.elapsed_secs_f64());
    }
}

fn update_flag_feed(
    time: Res<Time>,
    mut feed: ResMut<FlagFeed>,
    mut text: Single<&mut Text, With<FlagFeedText>>,
) {
    let now = time.elapsed_secs_f64();
    feed.0.retain(|(_, at)| now - at < 5.0);
    let joined = feed.0.iter().map(|(line, _)| line.as_str()).collect::<Vec<_>>().join("\n");
    if text.0 != joined {
        text.0 = joined;
    }
}

fn update_round_banner(
    level: Option<Res<LoadedLevel>>,
    players: Query<&Team, With<LocalPlayer>>,
    rounds: Query<&RoundState>,
    mut banner: Single<&mut Visibility, With<RoundBanner>>,
    mut text: Single<(&mut Text, &mut TextColor), With<RoundBannerText>>,
) {
    let Some(RoundState::Ended { winner, restart_in }) = rounds.single().ok().copied() else {
        banner.set_if_neq(Visibility::Hidden);
        return;
    };
    banner.set_if_neq(Visibility::Inherited);
    let local = local_team(&players);
    let (text, color) = &mut *text;
    let headline = match winner {
        Team::Spectator => "Draw".to_string(),
        team => format!("{} wins", team_name(level.as_deref(), team)),
    };
    text.0 = format!("{headline}\nNext round in {restart_in:.0} s");
    color.0 = if winner == Team::Spectator { TEXT } else { team_color(winner, local) };
}
