//! Voice chat settings on the Audio tab (see `crate::voice`): on or off, the voice volume,
//! push to talk or voice activation (with its level), microphone and output devices, the
//! microphone gain, and a mic test with a level meter that plays you back through the codec.
//!
//! Buttons are named for scenarios: `voice:enabled`, `voice:mode:push`,
//! `voice:mode:activation`, `voice:input`, `voice:output` (each click picks the next device),
//! `voice:test`.

use super::*;
use crate::voice::{MicTest, VoiceDevices, VoiceMode, VoiceState};

pub(super) struct VoiceUiPlugin;

impl Plugin for VoiceUiPlugin {
    fn build(&self, app: &mut App) {
        app.add_systems(
            Update,
            (list_devices, press_voice_buttons, paint_voice_buttons, update_meter, stop_test_off_page)
                .chain()
                .after(ScenarioSystems),
        );
    }
}

#[derive(Component, Clone, Copy, PartialEq, Eq, Debug)]
pub(super) enum VoiceButton {
    Enabled,
    Mode(VoiceMode),
    /// Picks the next microphone.
    Input,
    /// Picks the next output.
    Output,
    Test,
}

impl VoiceButton {
    fn element_name(self) -> String {
        match self {
            VoiceButton::Enabled => "voice:enabled".into(),
            VoiceButton::Mode(VoiceMode::PushToTalk) => "voice:mode:push".into(),
            VoiceButton::Mode(VoiceMode::VoiceActivation) => "voice:mode:activation".into(),
            VoiceButton::Input => "voice:input".into(),
            VoiceButton::Output => "voice:output".into(),
            VoiceButton::Test => "voice:test".into(),
        }
    }
}

/// The mic test's level bar and the activation level's mark on it.
#[derive(Component)]
pub(super) struct MeterFill;

#[derive(Component)]
pub(super) struct MeterMark;

#[derive(Component)]
pub(super) struct MeterText;

/// Meter range in dBFS.
const METER_MIN: f32 = -70.0;
const METER_MAX: f32 = 0.0;

fn meter_fraction(db: f32) -> f32 {
    ((db - METER_MIN) / (METER_MAX - METER_MIN)).clamp(0.0, 1.0)
}

fn voice_button(p: &mut ChildSpawnerCommands, action: VoiceButton, label: &str) {
    p.spawn((
        Name::new(action.element_name()),
        action,
        Button,
        Node {
            padding: UiRect::axes(px(14), px(8)),
            justify_content: JustifyContent::Center,
            align_items: AlignItems::Center,
            border_radius: BorderRadius::all(px(6)),
            max_width: px(360),
            overflow: Overflow::clip(),
            ..default()
        },
        BackgroundColor(BUTTON),
    ))
    .with_child(text(label, 15.0, TEXT));
}

/// The voice rows of the Audio tab.
pub(super) fn settings_rows(p: &mut ChildSpawnerCommands) {
    section(p, "Voice chat");
    row(p, "Voice chat", |c| {
        voice_button(c, VoiceButton::Enabled, "");
        c.spawn(text("off: the microphone is never opened and nobody is played", 12.0, DIM));
    });
    row(p, "Voice volume", |c| slider(c, Slider::VoiceVolume));
    row(p, "Talk with", |c| {
        for mode in VoiceMode::ALL {
            voice_button(c, VoiceButton::Mode(mode), mode.label());
        }
    });
    row(p, "Activation level", |c| {
        slider(c, Slider::VoiceThreshold);
        c.spawn(text("voice activation opens above this", 12.0, DIM));
    });
    row(p, "Push-to-talk keys", |c| {
        c.spawn(text("squad and commander keys are on the Controls tab (Voice)", 13.0, DIM));
    });
    row(p, "Microphone", |c| voice_button(c, VoiceButton::Input, ""));
    row(p, "Microphone gain", |c| slider(c, Slider::VoiceGain));
    row(p, "Mic test", |c| {
        voice_button(c, VoiceButton::Test, "");
        c.spawn((
            Node {
                width: px(220),
                height: px(10),
                border_radius: BorderRadius::all(px(5)),
                overflow: Overflow::clip(),
                ..default()
            },
            BackgroundColor(TRACK),
        ))
        .with_children(|bar| {
            bar.spawn((
                MeterFill,
                Node {
                    position_type: PositionType::Absolute,
                    width: percent(0),
                    height: percent(100),
                    ..default()
                },
                BackgroundColor(ACCENT),
            ));
            bar.spawn((
                MeterMark,
                Node {
                    position_type: PositionType::Absolute,
                    left: percent(0),
                    width: px(2),
                    height: percent(100),
                    ..default()
                },
                BackgroundColor(TEXT),
            ));
        });
        c.spawn((MeterText, text("", 12.0, DIM)));
    });
    row(p, "Voice output", |c| voice_button(c, VoiceButton::Output, ""));
}

/// Lists the devices when the Audio tab shows (at most every few seconds).
fn list_devices(time: Res<Time<Real>>, shown: Query<(), Added<VoiceButton>>, mut devices: ResMut<VoiceDevices>) {
    if !shown.is_empty() {
        devices.refresh(time.elapsed_secs_f64());
    }
}

/// The next of `names` after `current` (`None` is the system default, first in the cycle).
fn next_device(names: &[String], current: &Option<String>) -> Option<String> {
    match current {
        None => names.first().cloned(),
        Some(name) => match names.iter().position(|n| n == name) {
            Some(i) => names.get(i + 1).cloned(),
            None => None,
        },
    }
}

fn press_voice_buttons(
    buttons: Query<(&Interaction, &VoiceButton), Changed<Interaction>>,
    devices: Res<VoiceDevices>,
    mut settings: ResMut<Settings>,
    mut test: ResMut<MicTest>,
) {
    for (interaction, button) in &buttons {
        if *interaction != Interaction::Pressed {
            continue;
        }
        match button {
            VoiceButton::Enabled => {
                settings.voice.enabled ^= true;
                if !settings.voice.enabled {
                    test.active = false;
                }
            }
            VoiceButton::Mode(mode) => settings.voice.mode = *mode,
            VoiceButton::Input => {
                settings.voice.input_device = next_device(&devices.inputs, &settings.voice.input_device);
            }
            VoiceButton::Output => {
                settings.voice.output_device = next_device(&devices.outputs, &settings.voice.output_device);
            }
            VoiceButton::Test => test.active = !test.active && settings.voice.enabled,
        }
    }
}

fn paint_voice_buttons(
    settings: Res<Settings>,
    test: Res<MicTest>,
    mut buttons: Query<(&VoiceButton, &Interaction, &mut BackgroundColor, &Children)>,
    mut labels: Query<&mut Text>,
) {
    let voice = &settings.voice;
    for (button, interaction, mut background, children) in &mut buttons {
        let selected = match button {
            VoiceButton::Enabled => voice.enabled,
            VoiceButton::Mode(mode) => voice.mode == *mode,
            VoiceButton::Test => test.active,
            _ => false,
        };
        let color = if selected {
            ACCENT.with_alpha(0.45)
        } else if *interaction != Interaction::None {
            HOVER
        } else {
            BUTTON
        };
        background.set_if_neq(BackgroundColor(color));
        let label = match button {
            VoiceButton::Enabled => Some(if voice.enabled { "On".to_string() } else { "Off".to_string() }),
            VoiceButton::Input => Some(voice.input_device.clone().unwrap_or_else(|| "System default".into())),
            VoiceButton::Output => Some(voice.output_device.clone().unwrap_or_else(|| "System default".into())),
            VoiceButton::Test => Some(if test.active { "Stop test".into() } else { "Test microphone".into() }),
            VoiceButton::Mode(_) => None,
        };
        if let Some(label) = label {
            for child in children {
                if let Ok(mut text) = labels.get_mut(*child)
                    && text.0 != label
                {
                    text.0 = label.clone();
                }
            }
        }
    }
}

#[allow(clippy::type_complexity)]
fn update_meter(
    settings: Res<Settings>,
    test: Res<MicTest>,
    state: Res<VoiceState>,
    test_input: Res<crate::voice::VoiceTestInput>,
    mut fills: Query<(&mut Node, &mut BackgroundColor), (With<MeterFill>, Without<MeterMark>)>,
    mut marks: Query<&mut Node, (With<MeterMark>, Without<MeterFill>)>,
    mut texts: Query<&mut Text, With<MeterText>>,
) {
    let level = if test.active { state.level_db } else { game_voice::level::SILENCE_DB };
    let threshold = settings.voice.activation_threshold_db;
    for (mut node, mut color) in &mut fills {
        let width = percent(meter_fraction(level) * 100.0);
        if node.width != width {
            node.width = width;
        }
        let above = level >= threshold;
        color.set_if_neq(BackgroundColor(if above { crate::conquest_hud::SQUAD } else { ACCENT.with_alpha(0.6) }));
    }
    for mut node in &mut marks {
        let left = percent(meter_fraction(threshold) * 100.0);
        if node.left != left {
            node.left = left;
        }
    }
    let line = match (&state.error, test.active, &state.input) {
        (Some(err), _, _) => format!("can't open: {err}"),
        (None, true, Some(_)) => format!("{level:.0} dB: you hear yourself"),
        (None, true, None) => "opening...".into(),
        (None, false, _) if test_input.0.is_some() => "BF2_VOICE_TEST_INPUT replaces the microphone".into(),
        (None, false, _) => String::new(),
    };
    for mut text in &mut texts {
        if text.0 != line {
            text.0 = line.clone();
        }
    }
}

/// The mic test stops when its row is gone (another tab, or the menu closed).
fn stop_test_off_page(meters: Query<(), With<MeterFill>>, mut test: ResMut<MicTest>) {
    if test.active && meters.is_empty() {
        test.active = false;
    }
}
