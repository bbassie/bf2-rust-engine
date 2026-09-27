//! The HUD: crosshair with spread, hit marker, health, ammo, kill feed, death notice,
//! scoreboard (Tab) and a small status line. A clean modern style rather than BF2's.

use bevy::{
    diagnostic::{DiagnosticsStore, FrameTimeDiagnosticsPlugin},
    prelude::*,
    render::view::screenshot::{Screenshot, save_to_disk},
    window::PrimaryWindow,
};
use bevy_replicon::prelude::*;
use game_data::FireMode;
use game_shared::{
    level::LoadedLevel,
    protocol::{Player, Score, Team},
    soldier::Health,
    weapons::{Armory, Inventory, Loadout},
};

use crate::{
    Cli,
    combat::{CombatFeedback, weapon_display_name},
    net::{LocalPlayer, LocalSoldier},
    prediction::PredictionStats,
};

pub struct HudPlugin;

impl Plugin for HudPlugin {
    fn build(&self, app: &mut App) {
        app.add_systems(Startup, spawn_hud).add_systems(
            Update,
            (
                update_status,
                update_crosshair,
                update_vitals,
                update_kill_feed,
                update_death_notice,
                update_scoreboard,
                auto_screenshot,
            ),
        );
    }
}

const PANEL: Color = Color::srgba(0.05, 0.06, 0.08, 0.55);
const ACCENT: Color = Color::srgb(0.95, 0.75, 0.3);
const TEXT: Color = Color::srgb(0.92, 0.93, 0.95);
const DIM: Color = Color::srgba(0.85, 0.87, 0.9, 0.65);

#[derive(Component)]
struct StatusText;
#[derive(Component)]
struct CrosshairLine(usize);
#[derive(Component)]
struct HitMarker;
#[derive(Component)]
struct HealthFill;
#[derive(Component)]
struct HealthText;
#[derive(Component)]
struct WeaponText;
#[derive(Component)]
struct AmmoText;
#[derive(Component)]
struct VitalsPanel;
#[derive(Component)]
struct KillFeedText;
#[derive(Component)]
struct DeathNotice;
#[derive(Component)]
struct Scoreboard;
#[derive(Component)]
struct ScoreboardColumn(usize);

fn font(size: f32) -> TextFont {
    TextFont {
        font_size: FontSize::Px(size),
        ..default()
    }
}

fn spawn_hud(mut commands: Commands) {
    // Status line.
    commands.spawn((
        StatusText,
        Text::new(""),
        font(13.0),
        TextColor(DIM),
        TextShadow::default(),
        Node {
            position_type: PositionType::Absolute,
            top: px(8),
            left: px(10),
            ..default()
        },
    ));

    // Crosshair: four lines around the screen center, spread with weapon deviation.
    commands
        .spawn(Node {
            position_type: PositionType::Absolute,
            left: percent(50),
            top: percent(50),
            ..default()
        })
        .with_children(|center| {
            for i in 0..4 {
                let horizontal = i < 2;
                center.spawn((
                    CrosshairLine(i),
                    Node {
                        position_type: PositionType::Absolute,
                        width: px(if horizontal { 10 } else { 2 }),
                        height: px(if horizontal { 2 } else { 10 }),
                        ..default()
                    },
                    BackgroundColor(Color::srgba(1.0, 1.0, 1.0, 0.85)),
                ));
            }
            // Hit marker: an X of two rotated bars.
            for angle in [45f32, -45f32] {
                center.spawn((
                    HitMarker,
                    Node {
                        position_type: PositionType::Absolute,
                        left: px(-11),
                        top: px(-1),
                        width: px(22),
                        height: px(2),
                        ..default()
                    },
                    UiTransform {
                        rotation: Rot2::degrees(angle),
                        ..default()
                    },
                    BackgroundColor(Color::NONE),
                ));
            }
        });

    // Vitals: health bottom-left, weapon and ammo bottom-right.
    commands
        .spawn((
            VitalsPanel,
            Node {
                position_type: PositionType::Absolute,
                left: px(24),
                bottom: px(24),
                padding: UiRect::axes(px(14), px(10)),
                flex_direction: FlexDirection::Column,
                row_gap: px(6),
                border_radius: BorderRadius::all(px(8)),
                ..default()
            },
            BackgroundColor(PANEL),
        ))
        .with_children(|panel| {
            panel.spawn((HealthText, Text::new("100"), font(26.0), TextColor(TEXT)));
            panel
                .spawn((
                    Node {
                        width: px(180),
                        height: px(6),
                        border_radius: BorderRadius::all(px(3)),
                        ..default()
                    },
                    BackgroundColor(Color::srgba(1.0, 1.0, 1.0, 0.15)),
                ))
                .with_child((
                    HealthFill,
                    Node {
                        width: percent(100),
                        height: percent(100),
                        border_radius: BorderRadius::all(px(3)),
                        ..default()
                    },
                    BackgroundColor(Color::srgb(0.45, 0.85, 0.5)),
                ));
        });
    commands
        .spawn((
            VitalsPanel,
            Node {
                position_type: PositionType::Absolute,
                right: px(24),
                bottom: px(24),
                padding: UiRect::axes(px(14), px(10)),
                flex_direction: FlexDirection::Column,
                align_items: AlignItems::End,
                row_gap: px(2),
                border_radius: BorderRadius::all(px(8)),
                ..default()
            },
            BackgroundColor(PANEL),
        ))
        .with_children(|panel| {
            panel.spawn((WeaponText, Text::new(""), font(14.0), TextColor(ACCENT)));
            panel.spawn((AmmoText, Text::new(""), font(30.0), TextColor(TEXT)));
        });

    // Kill feed, top-right.
    commands.spawn((
        KillFeedText,
        Text::new(""),
        font(15.0),
        TextColor(TEXT),
        TextShadow::default(),
        TextLayout::justify(Justify::Right),
        Node {
            position_type: PositionType::Absolute,
            top: px(12),
            right: px(16),
            ..default()
        },
    ));

    // Death notice, lower center.
    commands
        .spawn(Node {
            position_type: PositionType::Absolute,
            width: percent(100),
            bottom: percent(28),
            justify_content: JustifyContent::Center,
            ..default()
        })
        .with_child((
            DeathNotice,
            Text::new(""),
            font(24.0),
            TextColor(TEXT),
            TextShadow::default(),
            TextLayout::justify(Justify::Center),
        ));

    // Scoreboard, shown while Tab is held.
    commands
        .spawn((
            Scoreboard,
            Node {
                position_type: PositionType::Absolute,
                width: percent(100),
                height: percent(100),
                justify_content: JustifyContent::Center,
                align_items: AlignItems::Center,
                ..default()
            },
            Visibility::Hidden,
        ))
        .with_children(|root| {
            root.spawn((
                Node {
                    padding: UiRect::all(px(20)),
                    column_gap: px(40),
                    border_radius: BorderRadius::all(px(10)),
                    ..default()
                },
                BackgroundColor(Color::srgba(0.04, 0.05, 0.07, 0.85)),
            ))
            .with_children(|board| {
                for column in 0..2 {
                    board.spawn((
                        ScoreboardColumn(column),
                        Text::new(""),
                        font(16.0),
                        TextColor(TEXT),
                        Node {
                            min_width: px(300),
                            ..default()
                        },
                    ));
                }
            });
        });
}

fn update_status(
    diagnostics: Res<DiagnosticsStore>,
    client: Res<State<ClientState>>,
    server: Res<State<ServerState>>,
    level: Option<Res<LoadedLevel>>,
    prediction: Res<PredictionStats>,
    client_stats: Res<ClientStats>,
    cursor: Single<&bevy::window::CursorOptions>,
    mut text: Single<&mut Text, With<StatusText>>,
) {
    let fps = diagnostics
        .get(&FrameTimeDiagnosticsPlugin::FPS)
        .and_then(|d| d.smoothed())
        .unwrap_or_default();
    let mode = match (client.get(), server.get()) {
        (ClientState::Connected, _) => "online",
        (ClientState::Connecting, _) => "connecting...",
        (_, ServerState::Running) => "hosting",
        _ => "offline",
    };
    let level = level
        .map(|l| l.desc.display_name.clone())
        .unwrap_or_else(|| "loading...".into());
    let net = if *client.get() == ClientState::Connected {
        format!(
            "  |  {:.0} ms  replay {}  err {:.2} m",
            client_stats.rtt * 1000.0,
            prediction.replayed,
            prediction.last_correction
        )
    } else {
        String::new()
    };
    let help = if cursor.grab_mode == bevy::window::CursorGrabMode::None {
        "\nclick to play  |  WASD move  shift sprint  space jump  ctrl crouch  Z prone  |  LMB fire  RMB zoom  R reload  B fire mode  1-6 weapons  |  V third person  Tab scores"
    } else {
        ""
    };
    text.0 = format!("{level}  |  {mode}  |  {fps:.0} fps{net}{help}");
}

fn update_crosshair(
    feedback: Res<CombatFeedback>,
    window: Single<&Window, With<PrimaryWindow>>,
    soldier: Query<(), With<LocalSoldier>>,
    mut lines: Query<(&CrosshairLine, &mut Node, &mut Visibility)>,
    mut marker: Query<&mut BackgroundColor, With<HitMarker>>,
) {
    let alive = !soldier.is_empty();
    let fov_degrees = 75.0 * feedback.zoom;
    let px_per_degree = window.height() / fov_degrees;
    let gap = (feedback.spread * 0.5 * px_per_degree).clamp(3.0, 90.0);
    for (line, mut node, mut visibility) in &mut lines {
        visibility.set_if_neq(if alive { Visibility::Inherited } else { Visibility::Hidden });
        let (left, top) = match line.0 {
            0 => (-gap - 10.0, -1.0),
            1 => (gap, -1.0),
            2 => (-1.0, -gap - 10.0),
            _ => (-1.0, gap),
        };
        node.left = px(left);
        node.top = px(top);
    }
    let alpha = (feedback.hit_marker / 0.2).clamp(0.0, 1.0);
    let color = if feedback.hit_killed {
        Color::srgba(1.0, 0.3, 0.25, alpha)
    } else {
        Color::srgba(1.0, 1.0, 1.0, alpha)
    };
    for mut background in &mut marker {
        background.0 = color;
    }
}

#[allow(clippy::type_complexity)]
fn update_vitals(
    armory: Res<Armory>,
    soldier: Query<(&Health, &Loadout, &Inventory), With<LocalSoldier>>,
    mut panels: Query<&mut Visibility, With<VitalsPanel>>,
    mut health_text: Single<&mut Text, (With<HealthText>, Without<WeaponText>, Without<AmmoText>)>,
    mut health_fill: Single<(&mut Node, &mut BackgroundColor), With<HealthFill>>,
    mut weapon_text: Single<&mut Text, (With<WeaponText>, Without<HealthText>, Without<AmmoText>)>,
    mut ammo_text: Single<&mut Text, (With<AmmoText>, Without<HealthText>, Without<WeaponText>)>,
) {
    let Ok((health, loadout, inventory)) = soldier.single() else {
        for mut visibility in &mut panels {
            visibility.set_if_neq(Visibility::Hidden);
        }
        return;
    };
    for mut visibility in &mut panels {
        visibility.set_if_neq(Visibility::Inherited);
    }
    let fraction = (health.current / health.max.max(1.0)).clamp(0.0, 1.0);
    health_text.0 = format!("{:.0}", health.current.max(0.0));
    let (node, color) = &mut *health_fill;
    node.width = percent(fraction * 100.0);
    color.0 = Color::srgb(0.95 - 0.5 * fraction, 0.35 + 0.5 * fraction, 0.35);

    let active = inventory.active as usize;
    let weapon = loadout.weapons.get(active).and_then(|w| armory.weapon(w));
    let name = weapon.map_or(String::new(), |w| weapon_display_name(&w.display_name));
    let mode = weapon
        .and_then(|w| w.fire_modes.get(inventory.fire_mode as usize))
        .map_or("", |m| match m {
            FireMode::Single => "  SINGLE",
            FireMode::Burst => "  BURST",
            FireMode::Auto => "  AUTO",
        });
    weapon_text.0 = format!("{name}{mode}");
    let [in_mag, spare] = inventory.ammo.get(active).copied().unwrap_or([0, 0]);
    ammo_text.0 = if inventory.reloading {
        "RELOADING".into()
    } else if weapon.is_some_and(|w| w.magazine_size > 0) {
        format!("{in_mag}  |  {spare}")
    } else {
        String::new()
    };
}

fn update_kill_feed(time: Res<Time<Real>>, feedback: Res<CombatFeedback>, mut text: Single<&mut Text, With<KillFeedText>>) {
    let now = time.elapsed_secs_f64();
    text.0 = feedback
        .kills
        .iter()
        .filter(|(_, at)| now - at < 8.0)
        .map(|(line, _)| line.as_str())
        .collect::<Vec<_>>()
        .join("\n");
}

fn update_death_notice(
    feedback: Res<CombatFeedback>,
    player: Query<(), With<LocalPlayer>>,
    soldier: Query<(), With<LocalSoldier>>,
    mut text: Single<&mut Text, With<DeathNotice>>,
) {
    text.0 = match (&feedback.killed_by, player.is_empty(), soldier.is_empty()) {
        (Some(killer), false, true) => format!("Killed by {killer}\nRespawning shortly"),
        _ => String::new(),
    };
}

fn update_scoreboard(
    keys: Res<ButtonInput<KeyCode>>,
    players: Query<(&Player, &Team, &Score, Has<LocalPlayer>)>,
    mut board: Single<&mut Visibility, With<Scoreboard>>,
    mut columns: Query<(&ScoreboardColumn, &mut Text)>,
    level: Option<Res<LoadedLevel>>,
) {
    let show = keys.pressed(KeyCode::Tab);
    board.set_if_neq(if show { Visibility::Inherited } else { Visibility::Hidden });
    if !show {
        return;
    }
    for (column, mut text) in &mut columns {
        let team = if column.0 == 0 { Team::One } else { Team::Two };
        let name = level
            .as_ref()
            .and_then(|l| l.desc.teams.get(column.0))
            .map(|t| t.name.clone())
            .filter(|n| !n.is_empty())
            .unwrap_or_else(|| format!("Team {}", column.0 + 1));
        let mut rows: Vec<_> = players.iter().filter(|(_, t, _, _)| **t == team).collect();
        rows.sort_by(|a, b| b.2.score.cmp(&a.2.score));
        let mut out = format!("{name}\n{:<24}{:>6}{:>6}{:>7}\n", "", "K", "D", "SCORE");
        for (player, _, score, local) in rows {
            let marker = if local { "> " } else { "  " };
            let name: String = player.name.chars().take(20).collect();
            out += &format!(
                "{marker}{name:<22}{:>6}{:>6}{:>7}\n",
                score.kills, score.deaths, score.score
            );
        }
        text.0 = out;
    }
}

/// `--screenshot <path>`: save a screenshot after a few seconds, then quit. Handy for
/// checking rendering without a human at the keyboard.
fn auto_screenshot(
    mut commands: Commands,
    time: Res<Time<Real>>,
    cli: Res<Cli>,
    mut state: Local<u8>,
    mut exit: MessageWriter<AppExit>,
) {
    let Some(path) = &cli.screenshot else {
        return;
    };
    let t = time.elapsed_secs();
    if *state == 0 && t > cli.screenshot_delay {
        commands
            .spawn(Screenshot::primary_window())
            .observe(save_to_disk(path.clone()));
        *state = 1;
    } else if *state == 1 && t > cli.screenshot_delay + 1.5 {
        exit.write(AppExit::Success);
        *state = 2;
    }
}
