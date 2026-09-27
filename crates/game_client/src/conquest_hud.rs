//! Conquest on the HUD: tickets and flags at the top, capture progress while standing at a
//! flag, capture notifications, and the end-of-round banner. Colors are relative to us:
//! blue is our team, red the enemy, grey neutral.

use std::collections::VecDeque;

use bevy::prelude::*;
use game_shared::{
    conquest::{ControlPoint, FlagEvent, FlagEventKind, FlagState, RoundState, Tickets},
    level::LoadedLevel,
    protocol::Team,
    soldier::SoldierMotion,
};

use crate::net::{LocalPlayer, LocalSoldier};

pub struct ConquestHudPlugin;

impl Plugin for ConquestHudPlugin {
    fn build(&self, app: &mut App) {
        app.init_resource::<FlagFeed>()
            .add_systems(Startup, spawn_conquest_hud)
            .add_systems(
                Update,
                (
                    rebuild_flag_pills,
                    update_tickets,
                    update_flag_pills,
                    update_capture_panel,
                    receive_flag_events,
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
const TEXT: Color = Color::srgb(0.95, 0.96, 0.98);
const PANEL: Color = Color::srgba(0.05, 0.06, 0.08, 0.6);

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
#[derive(Component)]
struct FlagRow;
#[derive(Component)]
struct FlagPill(Entity);
#[derive(Component)]
struct FlagPillBar(Entity);
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
    // Top center: our tickets, the flags, their tickets.
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
                    column_gap: px(12),
                    align_items: AlignItems::Center,
                    border_radius: BorderRadius::all(px(8)),
                    ..default()
                },
                BackgroundColor(PANEL),
            ))
            .with_children(|panel| {
                panel.spawn((TicketText { side: 0 }, Text::new(""), font(20.0), TextColor(FRIENDLY)));
                panel.spawn((
                    FlagRow,
                    Node {
                        column_gap: px(6),
                        align_items: AlignItems::Center,
                        ..default()
                    },
                ));
                panel.spawn((TicketText { side: 1 }, Text::new(""), font(20.0), TextColor(ENEMY)));
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
            top: px(64),
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

/// One pill per control point, in layout order, rebuilt when the set of points changes.
fn rebuild_flag_pills(
    mut commands: Commands,
    added: Query<(), Added<ControlPoint>>,
    mut removed: RemovedComponents<ControlPoint>,
    control_points: Query<(Entity, &ControlPoint)>,
    row: Single<(Entity, Option<&Children>), With<FlagRow>>,
) {
    if added.is_empty() && removed.read().next().is_none() {
        return;
    }
    let (row, children) = *row;
    for child in children.into_iter().flatten() {
        commands.entity(*child).despawn();
    }
    let mut points: Vec<(Entity, &ControlPoint)> =
        control_points.iter().filter(|(_, cp)| !cp.uncapturable).collect();
    points.sort_by_key(|(_, cp)| cp.index);
    for (entity, cp) in points {
        let letter: String = cp.name.chars().find(|c| c.is_alphanumeric()).into_iter().collect();
        commands.entity(row).with_children(|row| {
            row.spawn(Node {
                flex_direction: FlexDirection::Column,
                align_items: AlignItems::Center,
                row_gap: px(2),
                ..default()
            })
            .with_children(|column| {
                column
                    .spawn((
                        FlagPill(entity),
                        Node {
                            width: px(26),
                            height: px(26),
                            justify_content: JustifyContent::Center,
                            align_items: AlignItems::Center,
                            border_radius: BorderRadius::all(px(6)),
                            ..default()
                        },
                        BackgroundColor(NEUTRAL),
                    ))
                    .with_child((Text::new(letter.to_uppercase()), font(14.0), TextColor(TEXT)));
                column
                    .spawn((
                        Node {
                            width: px(26),
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

fn update_tickets(
    time: Res<Time>,
    level: Option<Res<LoadedLevel>>,
    players: Query<&Team, With<LocalPlayer>>,
    tickets: Query<&Tickets>,
    mut texts: Query<(&TicketText, &mut Text, &mut TextColor)>,
) {
    let local = local_team(&players);
    let ours = if local == Team::Two { Team::Two } else { Team::One };
    let Ok(tickets) = tickets.single() else {
        return;
    };
    for (ticket, mut text, mut color) in &mut texts {
        let team = if ticket.side == 0 { ours } else { ours.opponent() };
        let index = if team == Team::Two { 1 } else { 0 };
        let bleeding = tickets.bleed[index] > 0.0;
        let name = team_name(level.as_deref(), team);
        let value = tickets.remaining[index].ceil() as i32;
        let line = if ticket.side == 0 { format!("{name}  {value}") } else { format!("{value}  {name}") };
        if text.0 != line {
            text.0 = line;
        }
        // Bleeding tickets pulse.
        let pulse = if bleeding { 0.65 + 0.35 * (time.elapsed_secs() * 5.0).sin() } else { 1.0 };
        color.0 = team_color(team, local).with_alpha(pulse);
    }
}

fn update_flag_pills(
    players: Query<&Team, With<LocalPlayer>>,
    flags: Query<&FlagState>,
    mut pills: Query<(&FlagPill, &mut BackgroundColor), Without<FlagPillBar>>,
    mut bars: Query<(&FlagPillBar, &mut Node, &mut BackgroundColor), Without<FlagPill>>,
) {
    let local = local_team(&players);
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

fn update_capture_panel(
    level: Option<Res<LoadedLevel>>,
    players: Query<&Team, With<LocalPlayer>>,
    soldier: Query<&SoldierMotion, With<LocalSoldier>>,
    control_points: Query<(&ControlPoint, &FlagState)>,
    mut panel: Single<&mut Visibility, With<CapturePanel>>,
    mut text: Single<&mut Text, With<CaptureText>>,
    mut fill: Single<(&mut Node, &mut BackgroundColor), With<CaptureFill>>,
) {
    let local = local_team(&players);
    let inside = soldier.single().ok().and_then(|motion| {
        control_points
            .iter()
            .find(|(cp, _)| !cp.uncapturable && cp.contains(motion.position))
    });
    let Some((cp, state)) = inside else {
        panel.set_if_neq(Visibility::Hidden);
        return;
    };
    panel.set_if_neq(Visibility::Inherited);
    let action = match (state.owner == local, state.rate) {
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
        feed.0.push_back((format!("{team} {verb} {name}"), time.elapsed_secs_f64()));
        while feed.0.len() > 3 {
            feed.0.pop_front();
        }
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
