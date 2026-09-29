//! Voice chat on screen: who is talking (left side, BF4-style rows: a level dot, the name and
//! a channel badge, ours first while we transmit), and muting players from the scoreboard:
//! while the scoreboard is held, right-click frees the mouse and each teammate's chip under
//! it mutes or unmutes him for the session. Same look as the rest of the HUD (`ui_theme`).

use bevy::{
    prelude::*,
    window::{CursorGrabMode, CursorOptions},
};
use game_shared::{
    protocol::{Player, PlayerNetId, Team},
    voice::VoiceChannel,
};

use super::{Talkers, VoiceMutes, VoiceState};
use crate::{
    conquest_hud::{ENEMY, SQUAD},
    hud::Scoreboard,
    menu::Screen,
    net::LocalPlayer,
    settings::{Action, Actions},
    ui_theme::{font, shadow},
};

pub(super) struct VoiceUiPlugin;

impl Plugin for VoiceUiPlugin {
    fn build(&self, app: &mut App) {
        app.add_systems(Startup, spawn_talker_list).add_systems(
            Update,
            (
                update_talker_list,
                attach_scoreboard_panel,
                scoreboard_cursor,
                build_mute_chips,
                press_mute_chips,
                paint_mute_chips,
            )
                .chain(),
        );
    }
}

const PANEL: Color = Color::srgba(0.05, 0.06, 0.08, 0.6);
const TEXT: Color = Color::srgb(0.92, 0.93, 0.95);
const DIM: Color = Color::srgba(0.85, 0.87, 0.9, 0.65);
const COMMAND: Color = Color::srgb(0.95, 0.75, 0.3);
const HOVER: Color = Color::srgba(0.25, 0.3, 0.38, 0.8);
/// Rows in the talker list (ours and seven others).
const ROWS: usize = 8;

/// While the scoreboard is held: the mouse is free for the mute chips (right-click).
#[derive(Resource, Default)]
pub struct ScoreboardCursor(pub bool);

#[derive(Component)]
struct TalkerList;

/// A row of the talker list and its parts.
#[derive(Component)]
struct TalkerRow {
    dot: Entity,
    name: Entity,
    badge: Entity,
    badge_text: Entity,
}

fn channel_badge(channel: VoiceChannel) -> (&'static str, Color) {
    match channel {
        VoiceChannel::Squad => ("SQUAD", SQUAD),
        VoiceChannel::Command => ("CMD", COMMAND),
    }
}

fn spawn_talker_list(mut commands: Commands) {
    commands
        .spawn((
            TalkerList,
            Node {
                position_type: PositionType::Absolute,
                left: px(24),
                top: percent(34),
                flex_direction: FlexDirection::Column,
                row_gap: px(4),
                ..default()
            },
            Visibility::Hidden,
        ))
        .with_children(|list| {
            for _ in 0..ROWS {
                let mut row = list.spawn((
                    Node {
                        align_items: AlignItems::Center,
                        column_gap: px(8),
                        padding: UiRect::axes(px(10), px(5)),
                        border_radius: BorderRadius::all(px(7)),
                        display: Display::None,
                        ..default()
                    },
                    BackgroundColor(PANEL),
                ));
                let dot = row
                    .commands()
                    .spawn((
                        Node {
                            width: px(9),
                            height: px(9),
                            border_radius: BorderRadius::all(px(5)),
                            ..default()
                        },
                        BackgroundColor(SQUAD),
                    ))
                    .id();
                let name = row.commands().spawn((Text::new(""), font(15.0), TextColor(TEXT), shadow())).id();
                let badge_text = row.commands().spawn((Text::new(""), font(11.0), TextColor(Color::BLACK))).id();
                let badge = row
                    .commands()
                    .spawn((
                        Node {
                            padding: UiRect::axes(px(5), px(1)),
                            border_radius: BorderRadius::all(px(4)),
                            ..default()
                        },
                        BackgroundColor(SQUAD),
                    ))
                    .add_child(badge_text)
                    .id();
                row.add_children(&[dot, name, badge]);
                row.insert(TalkerRow { dot, name, badge, badge_text });
            }
        });
}

/// One line of the list.
struct Line {
    name: String,
    channel: VoiceChannel,
    /// Ours, held but refused: why.
    note: Option<&'static str>,
}

#[allow(clippy::too_many_arguments, clippy::type_complexity)]
fn update_talker_list(
    screen: Res<State<Screen>>,
    state: Res<VoiceState>,
    talkers: Res<Talkers>,
    time: Res<Time<Real>>,
    mut list: Single<&mut Visibility, With<TalkerList>>,
    mut rows: Query<(&TalkerRow, &mut Node)>,
    mut texts: Query<&mut Text>,
    mut colors: Query<&mut TextColor>,
    mut backgrounds: Query<&mut BackgroundColor>,
) {
    let in_game = *screen.get() == Screen::InGame;
    list.set_if_neq(if in_game { Visibility::Inherited } else { Visibility::Hidden });
    if !in_game {
        return;
    }
    let mut lines = Vec::new();
    if let Some(channel) = state.sending.or(state.key) {
        lines.push(Line { name: "You".into(), channel, note: state.refused.filter(|_| state.sending.is_none()) });
    }
    for talker in &talkers.0 {
        lines.push(Line { name: talker.name.clone(), channel: talker.channel, note: None });
    }
    // A slow pulse on the dots while talking.
    let pulse = 0.75 + 0.25 * (time.elapsed_secs() * 9.0).sin();
    for (i, (row, mut node)) in rows.iter_mut().enumerate() {
        let Some(line) = lines.get(i) else {
            if node.display != Display::None {
                node.display = Display::None;
            }
            continue;
        };
        if node.display != Display::Flex {
            node.display = Display::Flex;
        }
        let (badge, badge_color) = channel_badge(line.channel);
        let label = match line.note {
            Some(note) => format!("{}: {note}", line.name),
            None => line.name.clone(),
        };
        if let Ok(mut text) = texts.get_mut(row.name)
            && text.0 != label
        {
            text.0 = label;
        }
        if let Ok(mut text) = texts.get_mut(row.badge_text)
            && text.0 != badge
        {
            text.0 = badge.into();
        }
        if let Ok(mut color) = colors.get_mut(row.name) {
            color.set_if_neq(TextColor(if line.note.is_some() { DIM } else { TEXT }));
        }
        let dot = if line.note.is_some() { ENEMY } else { badge_color.with_alpha(pulse) };
        if let Ok(mut background) = backgrounds.get_mut(row.dot) {
            background.set_if_neq(BackgroundColor(dot));
        }
        if let Ok(mut background) = backgrounds.get_mut(row.badge) {
            background.set_if_neq(BackgroundColor(badge_color));
        }
    }
}

/// The voice part of the scoreboard, under the team columns.
#[derive(Component)]
struct MutePanel;

/// Where the chips go.
#[derive(Component)]
struct MuteChips;

#[derive(Component)]
struct MuteHint;

/// A teammate's chip: click to mute or unmute.
#[derive(Component, Clone, Debug, PartialEq)]
struct MuteChip(String);

fn attach_scoreboard_panel(mut commands: Commands, boards: Query<Entity, Added<Scoreboard>>) {
    for board in &boards {
        commands.entity(board).with_children(|root| {
            root.spawn((
                MutePanel,
                Node {
                    flex_direction: FlexDirection::Column,
                    row_gap: px(8),
                    padding: UiRect::axes(px(20), px(12)),
                    margin: UiRect::top(px(10)),
                    max_width: px(900),
                    border_radius: BorderRadius::all(px(10)),
                    ..default()
                },
                BackgroundColor(Color::srgba(0.04, 0.05, 0.07, 0.85)),
            ))
            .with_children(|panel| {
                panel.spawn((MuteHint, Text::new(""), font(14.0), TextColor(DIM)));
                panel.spawn((
                    MuteChips,
                    Node {
                        flex_wrap: FlexWrap::Wrap,
                        column_gap: px(6),
                        row_gap: px(6),
                        ..default()
                    },
                ));
            });
        });
    }
}

/// Right-click while the scoreboard is held frees the mouse; letting go of the scoreboard
/// takes it back.
fn scoreboard_cursor(
    actions: Actions,
    mouse: Res<ButtonInput<MouseButton>>,
    screen: Res<State<Screen>>,
    mut free: ResMut<ScoreboardCursor>,
    mut cursor: Single<&mut CursorOptions>,
) {
    let held = actions.pressed(Action::Scoreboard) && *screen.get() == Screen::InGame;
    if held && !free.0 && mouse.just_pressed(MouseButton::Right) && cursor.grab_mode != CursorGrabMode::None {
        free.0 = true;
    } else if !held && free.0 {
        free.0 = false;
        if *screen.get() == Screen::InGame {
            cursor.visible = false;
            cursor.grab_mode = CursorGrabMode::Locked;
        }
    }
}

/// The chips: human teammates other than us, rebuilt when they (or their mutes) change.
#[allow(clippy::type_complexity)]
fn build_mute_chips(
    mut commands: Commands,
    actions: Actions,
    mutes: Res<VoiceMutes>,
    free: Res<ScoreboardCursor>,
    players: Query<(&Player, &Team, Has<LocalPlayer>), With<PlayerNetId>>,
    chips: Query<(Entity, Option<&Children>), With<MuteChips>>,
    mut hints: Query<&mut Text, With<MuteHint>>,
    mut built: Local<Option<(Entity, Vec<(String, bool)>)>>,
) {
    if !actions.pressed(Action::Scoreboard) {
        return;
    }
    let Ok((container, children)) = chips.single() else {
        return;
    };
    let team = players.iter().find(|p| p.2).map(|p| *p.1);
    let mut mates: Vec<(String, bool)> = players
        .iter()
        .filter(|(player, t, local)| !local && !player.is_bot && Some(**t) == team)
        .map(|(player, ..)| (player.name.clone(), mutes.is_muted(&player.name)))
        .collect();
    mates.sort();
    let hint = if mates.is_empty() {
        "Voice: no other players on your team".to_string()
    } else if free.0 {
        format!("Voice: click a teammate to mute or unmute ({} muted)", mates.iter().filter(|m| m.1).count())
    } else {
        "Voice: right-click to use the mouse, then click a teammate to mute".to_string()
    };
    for mut text in &mut hints {
        if text.0 != hint {
            text.0 = hint.clone();
        }
    }
    if built.as_ref().is_some_and(|(e, list)| *e == container && *list == mates) {
        return;
    }
    *built = Some((container, mates.clone()));
    for child in children.into_iter().flatten() {
        commands.entity(*child).despawn();
    }
    commands.entity(container).with_children(|row| {
        for (name, muted) in mates {
            row.spawn((
                Name::new(format!("voice:mute:{name}")),
                MuteChip(name.clone()),
                Button,
                Node {
                    padding: UiRect::axes(px(10), px(4)),
                    border_radius: BorderRadius::all(px(6)),
                    ..default()
                },
                BackgroundColor(PANEL),
                children![(
                    Text::new(if muted { format!("{name} (muted)") } else { name }),
                    font(14.0),
                    TextColor(if muted { ENEMY } else { TEXT }),
                )],
            ));
        }
    });
}

fn press_mute_chips(
    chips: Query<(&Interaction, &MuteChip), Changed<Interaction>>,
    mut mutes: ResMut<VoiceMutes>,
) {
    for (interaction, chip) in &chips {
        if *interaction == Interaction::Pressed {
            mutes.toggle(&chip.0);
            let state = if mutes.is_muted(&chip.0) { "muted" } else { "unmuted" };
            info!("voice: {} {state}", chip.0);
        }
    }
}

fn paint_mute_chips(
    talkers: Res<Talkers>,
    mut chips: Query<(&MuteChip, &Interaction, &mut BackgroundColor)>,
) {
    for (chip, interaction, mut background) in &mut chips {
        let talking = talkers.0.iter().any(|t| t.name == chip.0);
        let color = if *interaction != Interaction::None {
            HOVER
        } else if talking {
            SQUAD.with_alpha(0.35)
        } else {
            PANEL
        };
        background.set_if_neq(BackgroundColor(color));
    }
}
