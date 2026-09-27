//! The chat box, bottom left: chat and server messages, fading after a while. T talks to
//! everyone, Y to the team and U to the squad (see the settings); Enter sends, Esc cancels.
//! While typing, the keyboard belongs to the chat. Lines starting with `/` are admin
//! commands (`/help`), for the host or after `/login <password>`.

use std::collections::VecDeque;

use bevy::{
    input::{InputSystems, keyboard::KeyboardInput},
    prelude::*,
};
use game_shared::{
    chat::{ChatChannel, ChatLine, ChatRequest, MAX_CHAT_LENGTH},
    protocol::Team,
};

use crate::{
    conquest_hud::{SQUAD, team_color},
    menu::{Menu, MenuKeys, Screen},
    net::{ActiveMatch, LocalPlayer},
    settings::{Action, Binding, Settings},
};

pub struct ChatPlugin;

impl Plugin for ChatPlugin {
    fn build(&self, app: &mut App) {
        app.init_resource::<ChatBox>()
            .add_systems(Startup, spawn_chat_box)
            .add_systems(PreUpdate, chat_keys.after(InputSystems).before(MenuKeys))
            .add_systems(Update, (receive_chat, rebuild_lines, fade_lines, update_input).chain());
    }
}

/// Lines kept.
const HISTORY: usize = 40;
/// Lines shown while not typing, and for how long.
const SHOWN: usize = 6;
const SHOW_SECONDS: f64 = 12.0;
const FADE_SECONDS: f64 = 2.0;
/// Lines shown while typing.
const SHOWN_TYPING: usize = 12;

const SERVER: Color = Color::srgb(0.95, 0.75, 0.3);
const PRIVATE: Color = Color::srgb(0.6, 0.85, 1.0);
const TEXT: Color = Color::srgb(0.95, 0.96, 0.98);

/// Chat history and what is being typed.
#[derive(Resource, Default)]
pub struct ChatBox {
    /// Received lines, oldest first, with the (real) time they arrived.
    lines: VecDeque<(ChatLine, f64)>,
    /// Typing a message on this channel.
    pub typing: Option<ChatChannel>,
    draft: String,
    /// Bumped when the lines change.
    version: u32,
}

#[derive(Component)]
struct ChatLines;

#[derive(Component)]
struct ChatInput;

#[derive(Component)]
struct ChatInputText;

/// A shown line, and when it arrived (for fading).
#[derive(Component)]
struct LineAge(f64);

/// A text span's own color, before fading.
#[derive(Component)]
struct BaseColor(Color);

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

fn spawn_chat_box(mut commands: Commands) {
    commands
        .spawn(Node {
            position_type: PositionType::Absolute,
            left: px(24),
            // Above the health panel.
            bottom: px(112),
            width: px(560),
            flex_direction: FlexDirection::Column,
            row_gap: px(6),
            ..default()
        })
        .with_children(|root| {
            root.spawn((
                ChatLines,
                Node {
                    flex_direction: FlexDirection::Column,
                    row_gap: px(2),
                    padding: UiRect::axes(px(10), px(6)),
                    border_radius: BorderRadius::all(px(8)),
                    ..default()
                },
                BackgroundColor(Color::NONE),
            ));
            root.spawn((
                ChatInput,
                Node {
                    padding: UiRect::axes(px(10), px(7)),
                    border_radius: BorderRadius::all(px(8)),
                    ..default()
                },
                BackgroundColor(Color::srgba(0.05, 0.06, 0.08, 0.8)),
                Visibility::Hidden,
            ))
            .with_child((ChatInputText, Text::new(""), font(16.0), TextColor(TEXT)));
        });
}

/// Opens the chat on its keys, and while typing takes every key press.
#[allow(clippy::too_many_arguments)]
fn chat_keys(
    mut keys: ResMut<ButtonInput<KeyCode>>,
    mut mouse: ResMut<ButtonInput<MouseButton>>,
    mut events: MessageReader<KeyboardInput>,
    settings: Res<Settings>,
    screen: Res<State<Screen>>,
    menu: Res<Menu>,
    active: Res<ActiveMatch>,
    mut chat: ResMut<ChatBox>,
    mut requests: MessageWriter<ChatRequest>,
) {
    let presses: Vec<KeyboardInput> = events.read().filter(|e| e.state.is_pressed()).cloned().collect();
    let playing = *screen.get() == Screen::InGame && !menu.paused && active.setup.is_some();
    let Some(channel) = chat.typing else {
        if !playing {
            return;
        }
        let opened = [
            (Action::ChatAll, ChatChannel::All),
            (Action::ChatTeam, ChatChannel::Team),
            (Action::ChatSquad, ChatChannel::Squad),
        ]
        .into_iter()
        .find(|(action, _)| match settings.binding(*action) {
            Binding::Key(key) => keys.just_pressed(key),
            Binding::Mouse(button) => mouse.just_pressed(button),
        });
        if let Some((_, channel)) = opened {
            chat.typing = Some(channel);
            chat.draft.clear();
            // The opening key does nothing else (and isn't typed).
            keys.reset_all();
            mouse.reset_all();
        }
        return;
    };
    if !playing {
        chat.typing = None;
        return;
    }
    for press in presses {
        match press.key_code {
            KeyCode::Enter | KeyCode::NumpadEnter => {
                let text = chat.draft.trim().to_string();
                if !text.is_empty() {
                    requests.write(ChatRequest { channel, text });
                }
                chat.typing = None;
                break;
            }
            KeyCode::Escape => {
                chat.typing = None;
                break;
            }
            KeyCode::Backspace => {
                chat.draft.pop();
            }
            _ => {
                for c in press.text.iter().flat_map(|t| t.chars()) {
                    if (c.is_ascii_graphic() || c == ' ') && chat.draft.len() < MAX_CHAT_LENGTH {
                        chat.draft.push(c);
                    }
                }
            }
        }
    }
    // The game and the menus see nothing of it.
    keys.reset_all();
    mouse.reset_all();
}

fn receive_chat(time: Res<Time<Real>>, mut lines: MessageReader<ChatLine>, mut chat: ResMut<ChatBox>) {
    for line in lines.read() {
        match &line.sender {
            Some(sender) => info!("chat [{:?}] {sender}: {}", line.channel, line.text),
            None => info!("chat [{:?}] {}", line.channel, line.text),
        }
        chat.lines.push_back((line.clone(), time.elapsed_secs_f64()));
        while chat.lines.len() > HISTORY {
            chat.lines.pop_front();
        }
        chat.version += 1;
    }
}

/// The spans of a line: channel tag, sender, text.
fn spans(line: &ChatLine, local: Team) -> Vec<(String, Color)> {
    let mut spans = Vec::new();
    match line.channel {
        ChatChannel::Server => spans.push((line.text.clone(), SERVER)),
        ChatChannel::Private => spans.push((line.text.clone(), PRIVATE)),
        channel => {
            match channel {
                ChatChannel::Team => spans.push(("[TEAM] ".into(), team_color(line.team, local))),
                ChatChannel::Squad => spans.push(("[SQUAD] ".into(), SQUAD)),
                _ => {}
            }
            let sender = line.sender.clone().unwrap_or_default();
            spans.push((format!("{sender}: "), team_color(line.team, local)));
            spans.push((line.text.clone(), TEXT));
        }
    }
    spans
}

/// Rebuilds the shown lines when new ones arrive, old ones expire or typing starts or ends.
fn rebuild_lines(
    mut commands: Commands,
    time: Res<Time<Real>>,
    chat: Res<ChatBox>,
    local: Query<&Team, With<LocalPlayer>>,
    container: Single<(Entity, &mut BackgroundColor), With<ChatLines>>,
    mut built: Local<Option<(u32, bool, usize, Team)>>,
) {
    let now = time.elapsed_secs_f64();
    let typing = chat.typing.is_some();
    let (count, max_age) = if typing { (SHOWN_TYPING, f64::MAX) } else { (SHOWN, SHOW_SECONDS + FADE_SECONDS) };
    let shown: Vec<&(ChatLine, f64)> = chat
        .lines
        .iter()
        .rev()
        .take(count)
        .filter(|(_, at)| now - at < max_age)
        .collect();
    let local = local.single().copied().unwrap_or_default();
    let key = (chat.version, typing, shown.len(), local);
    if *built == Some(key) {
        return;
    }
    *built = Some(key);
    let (entity, mut background) = container.into_inner();
    background.0 = if typing && !shown.is_empty() {
        Color::srgba(0.05, 0.06, 0.08, 0.6)
    } else {
        Color::NONE
    };
    commands.entity(entity).despawn_related::<Children>();
    commands.entity(entity).with_children(|lines| {
        for (line, at) in shown.into_iter().rev() {
            lines
                .spawn((
                    LineAge(if typing { f64::MAX } else { *at }),
                    Text::new(""),
                    font(15.0),
                    TextColor(TEXT),
                    shadow(),
                ))
                .with_children(|text| {
                    for (span, color) in spans(line, local) {
                        text.spawn((TextSpan::new(span), font(15.0), TextColor(color), BaseColor(color)));
                    }
                });
        }
    });
}

/// Old lines fade out when not typing.
fn fade_lines(
    time: Res<Time<Real>>,
    lines: Query<(&LineAge, &Children)>,
    mut spans: Query<(&BaseColor, &mut TextColor)>,
) {
    let now = time.elapsed_secs_f64();
    for (age, children) in &lines {
        let alpha = if age.0 == f64::MAX {
            1.0
        } else {
            (1.0 - (now - age.0 - SHOW_SECONDS) / FADE_SECONDS).clamp(0.0, 1.0) as f32
        };
        for child in children {
            if let Ok((base, mut color)) = spans.get_mut(*child) {
                let faded = base.0.with_alpha(base.0.alpha() * alpha);
                if color.0 != faded {
                    color.0 = faded;
                }
            }
        }
    }
}

fn update_input(
    time: Res<Time<Real>>,
    chat: Res<ChatBox>,
    mut input: Single<&mut Visibility, With<ChatInput>>,
    mut text: Single<&mut Text, With<ChatInputText>>,
) {
    let Some(channel) = chat.typing else {
        input.set_if_neq(Visibility::Hidden);
        return;
    };
    input.set_if_neq(Visibility::Inherited);
    let tag = match channel {
        ChatChannel::Team => "Team",
        ChatChannel::Squad => "Squad",
        _ => "All",
    };
    let caret = if time.elapsed_secs().fract() < 0.5 { "_" } else { " " };
    let line = format!("{tag}: {}{caret}", chat.draft);
    if text.0 != line {
        text.0 = line;
    }
}
