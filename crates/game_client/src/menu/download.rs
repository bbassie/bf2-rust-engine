//! The join flow's content step (see `crate::content` and `crate::join`): what the server
//! shares, "Download N files, X MB?" with Download / Always / Cancel, then the download's
//! progress, and while connected "The server is checking your content". Shown over the
//! loading screen while a content job runs.
//!
//! A server whose key isn't trusted yet gets a "New server" question instead, with its name,
//! address and key fingerprint; downloading trusts it. Settings > Game has the download
//! setting, the new-server question and the trusted servers (each can be forgotten).
//!
//! Buttons are named for scenarios: `content:download`, `content:always`, `content:cancel`,
//! `content-setting:ask` (`always`, `never`), `content:confirm-new`, `trusted:forget-all`,
//! `trusted:forget:<first 8 hex digits of the key>`.

use game_shared::content::format_bytes;

use super::*;
use crate::content::{ContentDownloads, ContentJob, Offer, Phase, TrustedServer};

pub(super) struct DownloadUiPlugin;

impl Plugin for DownloadUiPlugin {
    fn build(&self, app: &mut App) {
        app.add_systems(
            Update,
            (sync_panel, press_download_buttons, update_panel, paint_download_buttons, build_trusted_list)
                .chain()
                .after(ScenarioSystems),
        );
    }
}

#[derive(Component, Clone, PartialEq, Debug)]
pub(super) enum DownloadButton {
    Download,
    /// Download, and from now on without asking.
    Always,
    Cancel,
    Setting(ContentDownloads),
    /// Settings: ask before downloading from a new server (on/off).
    ConfirmNew,
    /// Settings: forget a trusted server (its public key).
    Forget(String),
    ForgetAll,
}

impl DownloadButton {
    fn element_name(&self) -> String {
        match self {
            DownloadButton::Download => "content:download".into(),
            DownloadButton::Always => "content:always".into(),
            DownloadButton::Cancel => "content:cancel".into(),
            DownloadButton::Setting(mode) => format!("content-setting:{}", mode.label().to_lowercase()),
            DownloadButton::ConfirmNew => "content:confirm-new".into(),
            DownloadButton::Forget(key) => format!("trusted:forget:{}", key.get(..8).unwrap_or(key)),
            DownloadButton::ForgetAll => "trusted:forget-all".into(),
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

/// The new-server box: name, address, fingerprint.
#[derive(Component)]
pub(super) struct IdentityBox;

#[derive(Component)]
pub(super) struct IdentityText;

/// The Download button's label (it says "Trust and download" for a new server).
#[derive(Component)]
pub(super) struct DownloadLabel;

/// Download and Always: only while asking.
#[derive(Component)]
pub(super) struct AskOnly;

/// Settings: the trusted servers, rebuilt when they change.
#[derive(Component)]
pub(super) struct TrustedList;

fn download_button(p: &mut ChildSpawnerCommands, action: DownloadButton, look: Look, label: &str) {
    let (padding, size) = match look {
        Look::Primary => (UiRect::axes(px(28), px(11)), 18.0),
        _ => (UiRect::axes(px(14), px(8)), 15.0),
    };
    let ask_only = matches!(action, DownloadButton::Download | DownloadButton::Always);
    let is_download = action == DownloadButton::Download;
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
    if is_download {
        button.with_child((DownloadLabel, text(label, size, TEXT)));
    } else {
        button.with_child(text(label, size, TEXT));
    }
    if ask_only {
        button.insert(AskOnly);
    }
}

/// The settings on the Game tab.
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
    row(p, "New servers", |c| {
        download_button(c, DownloadButton::ConfirmNew, Look::Plain, "");
        c.spawn(text("Ask before downloading from a server I haven't trusted yet", 14.0, DIM));
    });
    p.spawn((
        TrustedList,
        Node {
            flex_direction: FlexDirection::Column,
            ..default()
        },
    ));
}

/// Rebuilds the trusted servers list when they change.
fn build_trusted_list(
    mut commands: Commands,
    settings: Res<Settings>,
    lists: Query<(Entity, Option<&Children>), With<TrustedList>>,
    mut built: Local<Option<(Entity, Vec<TrustedServer>)>>,
) {
    let Ok((list, children)) = lists.single() else {
        return;
    };
    if built.as_ref().is_some_and(|(e, servers)| *e == list && *servers == settings.trusted_servers) {
        return;
    }
    *built = Some((list, settings.trusted_servers.clone()));
    for child in children.into_iter().flatten() {
        commands.entity(*child).despawn();
    }
    commands.entity(list).with_children(|p| {
        row(p, "Trusted servers", |c| {
            if settings.trusted_servers.is_empty() {
                c.spawn(text("None yet: you are asked before the first download from a server.", 14.0, DIM));
            } else {
                c.spawn(text(format!("{} servers", settings.trusted_servers.len()), 14.0, TEXT));
                download_button(c, DownloadButton::ForgetAll, Look::Plain, "Forget all");
            }
        });
        for server in &settings.trusted_servers {
            p.spawn(Node {
                margin: UiRect::left(px(216)),
                column_gap: px(12),
                align_items: AlignItems::Center,
                min_height: px(34),
                ..default()
            })
            .with_children(|r| {
                let name = if server.name.is_empty() { "(no name)" } else { server.name.as_str() };
                r.spawn((
                    text(name, 14.0, TEXT),
                    Node {
                        width: px(170),
                        overflow: Overflow::clip(),
                        ..default()
                    },
                ));
                r.spawn((
                    text(server.address.clone(), 13.0, DIM),
                    Node {
                        width: px(150),
                        overflow: Overflow::clip(),
                        ..default()
                    },
                ));
                r.spawn(text(server.fingerprint.clone(), 13.0, DIM));
                download_button(r, DownloadButton::Forget(server.public_key.clone()), Look::Plain, "Forget");
            });
        }
    });
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
            root.spawn((
                IdentityBox,
                Node {
                    display: Display::None,
                    flex_direction: FlexDirection::Column,
                    row_gap: px(6),
                    padding: UiRect::axes(px(18), px(14)),
                    max_width: px(600),
                    border: UiRect::left(px(3)),
                    border_radius: BorderRadius::all(px(8)),
                    ..default()
                },
                BackgroundColor(CARD),
                BorderColor::all(ACCENT),
            ))
            .with_child((IdentityText, text("", 15.0, TEXT)));
            root.spawn((PanelStatus, text("", 17.0, TEXT)));
            root.spawn((
                PanelDetail,
                text("", 14.0, DIM),
                Node {
                    max_width: px(600),
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

/// The new-server box's text.
fn identity_text(offer: &Offer) -> String {
    let Some(identity) = &offer.identity else {
        return String::new();
    };
    let mut text = format!(
        "{}\n{}\nKey fingerprint: {}\n\nYou haven't downloaded from this server before. Only download from servers you trust: what comes down is game data (maps, models, textures, sounds), never programs, and every file is checked against the server's list.",
        if identity.name.is_empty() { "(no name)" } else { &identity.name },
        identity.address,
        identity.fingerprint
    );
    if offer.key_changed {
        text += "\n\nYou trusted this address with another key before: this may not be the same server.";
    }
    text
}

#[allow(clippy::type_complexity)]
fn update_panel(
    time: Res<Time<Real>>,
    job: Res<ContentJob>,
    mut texts: ParamSet<(
        Query<&mut Text, With<PanelTitle>>,
        Query<&mut Text, With<PanelStatus>>,
        Query<&mut Text, With<PanelDetail>>,
        Query<&mut Text, With<IdentityText>>,
        Query<&mut Text, With<DownloadLabel>>,
    )>,
    mut bar: Query<&mut Node, (With<PanelBar>, Without<AskOnly>, Without<IdentityBox>)>,
    mut ask_only: Query<&mut Node, (With<AskOnly>, Without<PanelBar>, Without<IdentityBox>)>,
    mut identity_box: Query<&mut Node, (With<IdentityBox>, Without<PanelBar>, Without<AskOnly>)>,
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
    let new_server = matches!(&phase, Phase::Ask(offer) if offer.new_server);
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
        Phase::Verifying => (
            "The server is checking your content...".into(),
            "You join once your files match the server's.".into(),
            None,
        ),
        Phase::Ask(offer) if offer.repair => (
            format!(
                "The server found {} of your files different from its own. Download them again ({})?",
                offer.files,
                format_bytes(offer.bytes)
            ),
            "Outdated or changed files would make your game disagree with the server.".into(),
            Some(0.0),
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
    let heading = if new_server { "New server".to_string() } else { format!("Joining {server}") };
    let identity = match &phase {
        Phase::Ask(offer) if offer.new_server => identity_text(offer),
        _ => String::new(),
    };
    let label = if new_server { "Trust and download" } else { "Download" };
    let set = |text: &mut Text, value: &str| {
        if text.0 != value {
            text.0 = value.to_string();
        }
    };
    for mut t in &mut texts.p0() {
        set(&mut t, &heading);
    }
    for mut t in &mut texts.p1() {
        set(&mut t, &line);
    }
    for mut t in &mut texts.p2() {
        set(&mut t, &more);
    }
    for mut t in &mut texts.p3() {
        set(&mut t, &identity);
    }
    for mut t in &mut texts.p4() {
        set(&mut t, label);
    }
    for mut node in &mut identity_box {
        let display = if new_server { Display::Flex } else { Display::None };
        if node.display != display {
            node.display = display;
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
                    // Downloading from a new server trusts its key.
                    if let Phase::Ask(offer) = job.shared.phase()
                        && offer.new_server
                        && let Some(identity) = &offer.identity
                    {
                        let entry = identity.trusted_entry();
                        info!("content: trusting {} ({}, key {})", entry.name, entry.address, entry.fingerprint);
                        settings.trusted_servers.retain(|t| t.public_key != entry.public_key);
                        settings.trusted_servers.push(entry);
                    }
                    job.shared.decide(true);
                }
                if *button == DownloadButton::Always {
                    settings.content_downloads = ContentDownloads::Always;
                }
            }
            // Leaving stops the download.
            DownloadButton::Cancel => menu.leave = true,
            DownloadButton::Setting(mode) => settings.content_downloads = *mode,
            DownloadButton::ConfirmNew => settings.confirm_new_servers ^= true,
            DownloadButton::Forget(key) => {
                if let Some(server) = settings.trusted_servers.iter().find(|t| &t.public_key == key) {
                    info!("content: forgot trusted server {} ({}, key {})", server.name, server.address, server.fingerprint);
                }
                settings.trusted_servers.retain(|t| &t.public_key != key);
            }
            DownloadButton::ForgetAll => {
                info!("content: forgot all {} trusted servers", settings.trusted_servers.len());
                settings.trusted_servers.clear();
            }
        }
    }
}

fn paint_download_buttons(
    settings: Res<Settings>,
    mut buttons: Query<(&DownloadButton, &Look, &Interaction, &mut BackgroundColor, &Children)>,
    mut labels: Query<&mut Text, Without<DownloadLabel>>,
) {
    for (button, look, interaction, mut background, children) in &mut buttons {
        let hovered = *interaction != Interaction::None;
        let selected = match button {
            DownloadButton::Setting(mode) => *mode == settings.content_downloads,
            DownloadButton::ConfirmNew => settings.confirm_new_servers,
            _ => false,
        };
        let color = match look {
            Look::Primary if hovered => ACCENT.lighter(0.08),
            Look::Primary => ACCENT,
            _ if selected => ACCENT.with_alpha(0.45),
            _ if hovered => HOVER,
            _ => BUTTON,
        };
        background.set_if_neq(BackgroundColor(color));
        if *button == DownloadButton::ConfirmNew {
            let label = if settings.confirm_new_servers { "On" } else { "Off" };
            for child in children {
                if let Ok(mut text) = labels.get_mut(*child)
                    && text.0 != label
                {
                    text.0 = label.to_string();
                }
            }
        }
    }
}
