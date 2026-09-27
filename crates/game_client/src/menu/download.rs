//! The join flow's content step (see `crate::content`): what the server shares, "Download N
//! files, X MB?" with Download / Always / Cancel, then the download's progress. Shown over
//! the loading screen while the content job runs. Also the setting's row on the Game tab.
//!
//! Buttons are named for scenarios: `content:download`, `content:always`, `content:cancel`,
//! `content-setting:ask` (`always`, `never`).

use game_shared::content::format_bytes;

use super::*;
use crate::content::{ContentDownloads, ContentJob, Offer, Phase};

pub(super) struct DownloadUiPlugin;

impl Plugin for DownloadUiPlugin {
    fn build(&self, app: &mut App) {
        app.add_systems(
            Update,
            (sync_panel, press_download_buttons, update_panel, paint_download_buttons)
                .chain()
                .after(ScenarioSystems),
        );
    }
}

#[derive(Component, Clone, Copy, PartialEq, Debug)]
pub(super) enum DownloadButton {
    Download,
    /// Download, and from now on without asking.
    Always,
    Cancel,
    Setting(ContentDownloads),
}

impl DownloadButton {
    fn element_name(self) -> String {
        match self {
            DownloadButton::Download => "content:download".into(),
            DownloadButton::Always => "content:always".into(),
            DownloadButton::Cancel => "content:cancel".into(),
            DownloadButton::Setting(mode) => format!("content-setting:{}", mode.label().to_lowercase()),
        }
    }
}

#[derive(Component)]
pub(super) struct DownloadPanel;

#[derive(Component)]
pub(super) struct PanelTitle;

#[derive(Component)]
pub(super) struct PanelStatus;

#[derive(Component)]
pub(super) struct PanelDetail;

#[derive(Component)]
pub(super) struct PanelBar;

/// Download and Always: only while asking.
#[derive(Component)]
pub(super) struct AskOnly;

fn download_button(p: &mut ChildSpawnerCommands, action: DownloadButton, look: Look, label: &str) {
    let (padding, size) = match look {
        Look::Primary => (UiRect::axes(px(28), px(11)), 18.0),
        _ => (UiRect::axes(px(14), px(8)), 15.0),
    };
    let mut button = p.spawn((
        Name::new(action.element_name()),
        action,
        look,
        Button,
        Node {
            padding,
            justify_content: JustifyContent::Center,
            align_items: AlignItems::Center,
            border_radius: BorderRadius::all(px(6)),
            ..default()
        },
        BackgroundColor(Color::NONE),
    ));
    button.with_child(text(label, size, TEXT));
    if matches!(action, DownloadButton::Download | DownloadButton::Always) {
        button.insert(AskOnly);
    }
}

/// The setting on the Game tab.
pub(super) fn settings_row(p: &mut ChildSpawnerCommands) {
    row(p, "Server content", |c| {
        for mode in ContentDownloads::ALL {
            download_button(c, DownloadButton::Setting(mode), Look::Plain, mode.label());
        }
    });
    p.spawn((
        text(
            "Joining a server that shares its mods (or all its content) downloads what you lack first.",
            13.0,
            DIM,
        ),
        Node {
            margin: UiRect::left(px(216)),
            ..default()
        },
    ));
}

/// The panel is up while a content job runs.
fn sync_panel(mut commands: Commands, job: Res<ContentJob>, panels: Query<Entity, With<DownloadPanel>>) {
    let active = job
        .0
        .as_ref()
        .is_some_and(|j| !matches!(j.shared.phase(), Phase::Done(_) | Phase::Failed(_)));
    match (active, panels.iter().next()) {
        (true, None) => spawn_panel(&mut commands),
        (false, Some(panel)) => commands.entity(panel).despawn(),
        _ => {}
    }
}

fn spawn_panel(commands: &mut Commands) {
    commands
        .spawn((
            DownloadPanel,
            DespawnOnExit(Screen::Loading),
            Node {
                position_type: PositionType::Absolute,
                width: percent(100),
                height: percent(100),
                flex_direction: FlexDirection::Column,
                justify_content: JustifyContent::Center,
                align_items: AlignItems::Center,
                row_gap: px(14),
                ..default()
            },
            background(),
            // Over the loading screen.
            GlobalZIndex(25),
        ))
        .with_children(|root| {
            root.spawn((PanelTitle, text("Joining", 30.0, TEXT)));
            root.spawn((PanelStatus, text("", 17.0, TEXT)));
            root.spawn((
                PanelDetail,
                text("", 14.0, DIM),
                Node {
                    max_width: px(560),
                    ..default()
                },
                TextLayout::justify(Justify::Center),
            ));
            root.spawn((
                Node {
                    width: px(420),
                    height: px(6),
                    margin: UiRect::vertical(px(6)),
                    border_radius: BorderRadius::all(px(3)),
                    overflow: Overflow::clip(),
                    ..default()
                },
                BackgroundColor(TRACK),
            ))
            .with_child((
                PanelBar,
                Node {
                    position_type: PositionType::Absolute,
                    width: percent(0),
                    height: percent(100),
                    border_radius: BorderRadius::all(px(3)),
                    ..default()
                },
                BackgroundColor(ACCENT),
            ));
            root.spawn(Node {
                column_gap: px(10),
                margin: UiRect::top(px(6)),
                ..default()
            })
            .with_children(|row| {
                download_button(row, DownloadButton::Download, Look::Primary, "Download");
                download_button(row, DownloadButton::Always, Look::Plain, "Always download");
                download_button(row, DownloadButton::Cancel, Look::Plain, "Cancel");
            });
        });
}

/// "Mods: Sample Mod. Includes the server's BF2 assets."
fn shares(offer: &Offer) -> String {
    let mut line = String::new();
    if !offer.mods.is_empty() {
        line += &format!("Mods: {}.", offer.mods.join(", "));
    }
    if offer.imported {
        if !line.is_empty() {
            line += " ";
        }
        line += "Includes the server's imported BF2 assets.";
    }
    line
}

#[allow(clippy::type_complexity)]
fn update_panel(
    time: Res<Time<Real>>,
    job: Res<ContentJob>,
    mut title: Query<&mut Text, (With<PanelTitle>, Without<PanelStatus>, Without<PanelDetail>)>,
    mut status: Query<&mut Text, (With<PanelStatus>, Without<PanelTitle>, Without<PanelDetail>)>,
    mut detail: Query<&mut Text, (With<PanelDetail>, Without<PanelTitle>, Without<PanelStatus>)>,
    mut bar: Query<&mut Node, (With<PanelBar>, Without<AskOnly>)>,
    mut ask_only: Query<&mut Node, (With<AskOnly>, Without<PanelBar>)>,
    // Download speed: last sample (seconds, bytes) and the smoothed rate.
    mut speed: Local<(f32, u64, f64)>,
) {
    let Some(job) = &job.0 else {
        return;
    };
    let phase = job.shared.phase();
    let progress = &job.shared.progress;
    let done = progress.bytes_done.load(std::sync::atomic::Ordering::Relaxed);
    let total = progress.bytes_total.load(std::sync::atomic::Ordering::Relaxed);
    let now = time.elapsed_secs();
    if now - speed.0 >= 0.5 {
        let rate = done.saturating_sub(speed.1) as f64 / (now - speed.0).max(0.001) as f64;
        speed.2 = if speed.2 == 0.0 { rate } else { speed.2 * 0.6 + rate * 0.4 };
        *speed = (now, done, speed.2);
    }
    let server = match &phase {
        Phase::Ask(offer) | Phase::Downloading(offer) if !offer.server_name.is_empty() => offer.server_name.clone(),
        _ => job.server.to_string(),
    };
    let (line, more, fraction): (String, String, Option<f32>) = match &phase {
        Phase::Contacting => ("Contacting the server...".into(), String::new(), None),
        Phase::Preparing(message) => (
            "The server is preparing its content...".into(),
            message.trim_start_matches("preparing").trim().to_string(),
            None,
        ),
        Phase::Checking => (
            "Comparing the server's content with yours...".into(),
            if total > 0 { format!("{} of {} checked", format_bytes(done), format_bytes(total)) } else { String::new() },
            (total > 0).then(|| done as f32 / total as f32),
        ),
        Phase::Ask(offer) => (
            format!(
                "This server shares {}. Download {} files, {}?",
                if offer.mode == game_shared::content::ContentMode::All { "all its content" } else { "its mods" },
                offer.files,
                format_bytes(offer.bytes)
            ),
            {
                let mut more = shares(offer);
                if offer.have_bytes > 0 {
                    more += &format!(" {} of it is here already.", format_bytes(offer.have_bytes));
                }
                more.trim().to_string()
            },
            Some(0.0),
        ),
        Phase::Downloading(offer) => (
            format!("Downloading {} files, {}", offer.files, format_bytes(offer.bytes)),
            if total > 0 {
                format!("{} of {}  |  {}/s", format_bytes(done), format_bytes(total), format_bytes(speed.2 as u64))
            } else {
                "Copying files you have...".into()
            },
            (total > 0).then(|| done as f32 / total as f32),
        ),
        Phase::Mounting => ("Preparing the content...".into(), String::new(), None),
        Phase::Done(_) | Phase::Failed(_) => return,
    };
    let heading = format!("Joining {server}");
    for (mut text, value) in [(title.single_mut().ok(), heading), (status.single_mut().ok(), line), (detail.single_mut().ok(), more)]
        .into_iter()
        .filter_map(|(t, v)| Some((t?, v)))
    {
        if text.0 != value {
            text.0 = value;
        }
    }
    if let Ok(mut node) = bar.single_mut() {
        match fraction {
            Some(f) => {
                node.left = percent(0);
                node.width = percent(f.clamp(0.0, 1.0) * 100.0);
            }
            None => {
                // Indeterminate: a sweep.
                let t = (now * 0.6).fract();
                node.left = percent(t * 130.0 - 30.0);
                node.width = percent(30);
            }
        }
    }
    let asking = matches!(phase, Phase::Ask(_));
    for mut node in &mut ask_only {
        let display = if asking { Display::Flex } else { Display::None };
        if node.display != display {
            node.display = display;
        }
    }
}

fn press_download_buttons(
    buttons: Query<(&Interaction, &DownloadButton), Changed<Interaction>>,
    job: Res<ContentJob>,
    mut settings: ResMut<Settings>,
    mut menu: ResMut<Menu>,
) {
    for (interaction, button) in &buttons {
        if *interaction != Interaction::Pressed {
            continue;
        }
        match button {
            DownloadButton::Download | DownloadButton::Always => {
                if let Some(job) = &job.0 {
                    job.shared.decide(true);
                }
                if *button == DownloadButton::Always {
                    settings.content_downloads = ContentDownloads::Always;
                }
            }
            // Leaving stops the download.
            DownloadButton::Cancel => menu.leave = true,
            DownloadButton::Setting(mode) => settings.content_downloads = *mode,
        }
    }
}

fn paint_download_buttons(
    settings: Res<Settings>,
    mut buttons: Query<(&DownloadButton, &Look, &Interaction, &mut BackgroundColor)>,
) {
    for (button, look, interaction, mut background) in &mut buttons {
        let hovered = *interaction != Interaction::None;
        let selected = matches!(button, DownloadButton::Setting(mode) if *mode == settings.content_downloads);
        let color = match look {
            Look::Primary if hovered => ACCENT.lighter(0.08),
            Look::Primary => ACCENT,
            _ if selected => ACCENT.with_alpha(0.45),
            _ if hovered => HOVER,
            _ => BUTTON,
        };
        background.set_if_neq(BackgroundColor(color));
    }
}
