//! The HUD: crosshair with spread, hit marker, health, stamina, ammo, kill feed, death notice,
//! scoreboard (Tab) and a small status line. A clean modern style rather than BF2's.

use bevy::{
    diagnostic::{DiagnosticsStore, FrameTimeDiagnosticsPlugin},
    prelude::*,
    window::PrimaryWindow,
};
use bevy_replicon::prelude::*;
use game_data::FireMode;
use game_shared::{
    level::LoadedLevel,
    protocol::{Player, Score, Team},
    soldier::{Health, SoldierMotion},
    squad::{SquadMember, squad_name},
    weapons::{Armory, Inventory, Loadout},
};

use crate::{
    combat::{CombatFeedback, weapon_display_name},
    net::{LocalPlayer, LocalSoldier},
    prediction::{Predicted, PredictionStats},
    settings::{Action, Actions, CrosshairStyle, Settings},
    ui_theme::{font, shadow},
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
                update_stamina,
                update_kill_feed,
                update_death_notice,
                update_scoreboard,
            ),
        );
    }
}

const PANEL: Color = Color::srgba(0.05, 0.06, 0.08, 0.55);
const ACCENT: Color = Color::srgb(0.95, 0.75, 0.3);
const TEXT: Color = Color::srgb(0.92, 0.93, 0.95);
const DIM: Color = Color::srgba(0.85, 0.87, 0.9, 0.65);
const STAMINA: Color = Color::srgb(0.6, 0.78, 0.95);

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
struct StaminaFill;
#[derive(Component)]
struct WeaponText;
#[derive(Component)]
struct AmmoText;
#[derive(Component)]
struct VitalsPanel;
/// The weapon and ammo panel (hidden in vehicles, which show their own).
#[derive(Component)]
struct AmmoPanel;
#[derive(Component)]
struct KillFeedText;
#[derive(Component)]
struct DeathNotice;
/// The scoreboard's full-screen root: the team columns, and under them what other modules
/// add (voice chat's mute chips, `voice::ui`).
#[derive(Component)]
pub(crate) struct Scoreboard;
#[derive(Component)]
struct ScoreboardColumn(usize);
/// A team's name and flag over its scoreboard column.
#[derive(Component)]
struct ScoreboardTeam(usize);
#[derive(Component)]
struct ScoreboardFlag(usize);


fn spawn_hud(mut commands: Commands) {
    // Status line.
    commands.spawn((
        StatusText,
        Text::new(""),
        font(13.0),
        TextColor(DIM),
        shadow(),
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
            // Sprint stamina, thin, under the health bar.
            panel
                .spawn((
                    Node {
                        width: px(180),
                        height: px(3),
                        border_radius: BorderRadius::all(px(2)),
                        ..default()
                    },
                    BackgroundColor(Color::srgba(1.0, 1.0, 1.0, 0.1)),
                ))
                .with_child((
                    StaminaFill,
                    Node {
                        width: percent(100),
                        height: percent(100),
                        border_radius: BorderRadius::all(px(2)),
                        ..default()
                    },
                    BackgroundColor(STAMINA),
                ));
        });
    commands
        .spawn((
            VitalsPanel,
            AmmoPanel,
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
        shadow(),
        TextLayout::justify(Justify::Right),
        Node {
            position_type: PositionType::Absolute,
            // Below the minimap.
            top: px(crate::minimap::SIZE + 24.0),
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
            shadow(),
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
                flex_direction: FlexDirection::Column,
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
                    board
                        .spawn(Node {
                            flex_direction: FlexDirection::Column,
                            row_gap: px(8),
                            ..default()
                        })
                        .with_children(|column_node| {
                            column_node
                                .spawn(Node {
                                    column_gap: px(10),
                                    align_items: AlignItems::Center,
                                    ..default()
                                })
                                .with_children(|header| {
                                    header.spawn((
                                        ScoreboardFlag(column),
                                        Node {
                                            width: px(56),
                                            height: px(28),
                                            border_radius: BorderRadius::all(px(3)),
                                            display: Display::None,
                                            ..default()
                                        },
                                        ImageNode::default(),
                                    ));
                                    header.spawn((ScoreboardTeam(column), Text::new(""), font(20.0), TextColor(TEXT)));
                                });
                            column_node.spawn((
                                ScoreboardColumn(column),
                                Text::new(""),
                                font(16.0),
                                TextColor(TEXT),
                                TextLayout::no_wrap(),
                                Node {
                                    min_width: px(300),
                                    ..default()
                                },
                            ));
                        });
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
    actions: Actions,
    mut text: Single<&mut Text, With<StatusText>>,
    time: Res<Time<Real>>,
    mut shown_at: Local<f32>,
) {
    // A changed text is laid out again, and the frame rate changes every frame: four times a
    // second is plenty to read it.
    let now = time.elapsed_secs();
    if now - *shown_at < 0.25 && !text.0.is_empty() {
        return;
    }
    *shown_at = now;
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
        // Explicit short lines: auto-wrapped text and its shadow get laid out differently,
        // and long lines run into the ticket bar.
        let key = |action| actions.label(action);
        let movement: String = [Action::MoveForward, Action::MoveLeft, Action::MoveBack, Action::MoveRight]
            .into_iter()
            .map(key)
            .collect();
        format!(
            "\nclick to play  |  {} deploy  |  {} scores\n{movement} move  {} sprint  {} jump\n{} crouch  {} prone  {} third person\n{} fire  {} zoom  {} reload  {} mode\nEsc menu",
            key(Action::Deploy),
            key(Action::Scoreboard),
            key(Action::Sprint),
            key(Action::Jump),
            key(Action::Crouch),
            key(Action::Prone),
            key(Action::ThirdPerson),
            key(Action::Fire),
            key(Action::Zoom),
            key(Action::Reload),
            key(Action::FireMode),
        )
    } else {
        String::new()
    };
    set_text(&mut text, format!("{level}  |  {mode}  |  {fps:.0} fps{net}{help}"));
}

/// Sets a text only when it changes: a changed `Text` (or `Node`) lays the UI out again.
pub(crate) fn set_text(text: &mut Mut<Text>, value: impl Into<String> + AsRef<str>) {
    if text.0 != value.as_ref() {
        text.0 = value.into();
    }
}

/// Sets a node's width only when it changes (see [`set_text`]).
pub(crate) fn set_width(node: &mut Mut<Node>, width: Val) {
    if node.width != width {
        node.width = width;
    }
}

/// Sets a node's size and offset only when they change (see [`set_text`]).
pub(crate) fn set_box(node: &mut Mut<Node>, width: Val, height: Val, left: Val, top: Val) {
    if node.width != width || node.height != height || node.left != left || node.top != top {
        node.width = width;
        node.height = height;
        node.left = left;
        node.top = top;
    }
}

fn update_crosshair(
    feedback: Res<CombatFeedback>,
    settings: Res<Settings>,
    window: Single<&Window, With<PrimaryWindow>>,
    soldier: Query<(), With<LocalSoldier>>,
    vehicle_sight: Res<crate::vehicles::VehicleSight>,
    mut lines: Query<(&CrosshairLine, &mut Node, &mut Visibility, &mut BackgroundColor), Without<HitMarker>>,
    mut marker: Query<&mut BackgroundColor, With<HitMarker>>,
) {
    // Zoomed in, the iron sights (or scope) do the aiming; in vehicles, their sights.
    let alive = !soldier.is_empty() && feedback.zoom > 0.95 && !vehicle_sight.active;
    let fov_degrees = settings.field_of_view * feedback.zoom;
    let px_per_degree = window.height() / fov_degrees;
    let gap = (feedback.spread * 0.5 * px_per_degree).clamp(3.0, 90.0);
    let [r, g, b, a] = settings.crosshair_color;
    let crosshair_color = Color::srgba(r, g, b, a);
    let dot = settings.crosshair_style == CrosshairStyle::Dot;
    for (line, mut node, mut visibility, mut background) in &mut lines {
        let shown = alive && (!dot || line.0 == 0);
        visibility.set_if_neq(if shown { Visibility::Inherited } else { Visibility::Hidden });
        background.set_if_neq(BackgroundColor(crosshair_color));
        if dot {
            set_box(&mut node, px(4), px(4), px(-2.0), px(-2.0));
            continue;
        }
        let horizontal = matches!(line.0, 0 | 1);
        let (left, top) = match line.0 {
            0 => (-gap - 10.0, -1.0),
            1 => (gap, -1.0),
            2 => (-1.0, -gap - 10.0),
            _ => (-1.0, gap),
        };
        let (width, height) = if horizontal { (px(10), px(2)) } else { (px(2), px(10)) };
        set_box(&mut node, width, height, px(left), px(top));
    }
    let alpha = (feedback.hit_marker / 0.2).clamp(0.0, 1.0);
    let color = if feedback.hit_killed {
        Color::srgba(1.0, 0.3, 0.25, alpha)
    } else {
        Color::srgba(1.0, 1.0, 1.0, alpha)
    };
    for mut background in &mut marker {
        background.set_if_neq(BackgroundColor(color));
    }
}

/// Our predicted stamina when connected (the replicated one is a round trip behind).
fn update_stamina(
    soldier: Query<(&SoldierMotion, Option<&Predicted>), With<LocalSoldier>>,
    mut fill: Single<(&mut Node, &mut BackgroundColor), With<StaminaFill>>,
) {
    let Ok((motion, predicted)) = soldier.single() else {
        return;
    };
    let stamina = predicted.map_or(motion, |p| p.motion()).stamina.clamp(0.0, 1.0);
    let (node, color) = &mut *fill;
    let width = percent(stamina * 100.0);
    if node.width != width {
        node.width = width;
    }
    // Amber when too low to start sprinting again soon.
    color.set_if_neq(BackgroundColor(if stamina < 0.2 { ACCENT } else { STAMINA }));
}

#[allow(clippy::type_complexity)]
fn update_vitals(
    armory: Res<Armory>,
    feedback: Res<CombatFeedback>,
    soldier: Query<(&Health, &Loadout, &Inventory, Has<game_shared::vehicle::Seated>), With<LocalSoldier>>,
    mut panels: Query<(&mut Visibility, Has<AmmoPanel>), With<VitalsPanel>>,
    mut health_text: Single<&mut Text, (With<HealthText>, Without<WeaponText>, Without<AmmoText>)>,
    mut health_fill: Single<(&mut Node, &mut BackgroundColor), With<HealthFill>>,
    mut weapon_text: Single<&mut Text, (With<WeaponText>, Without<HealthText>, Without<AmmoText>)>,
    mut ammo_text: Single<&mut Text, (With<AmmoText>, Without<HealthText>, Without<WeaponText>)>,
) {
    let Ok((health, loadout, inventory, seated)) = soldier.single() else {
        for (mut visibility, _) in &mut panels {
            visibility.set_if_neq(Visibility::Hidden);
        }
        return;
    };
    for (mut visibility, ammo) in &mut panels {
        visibility.set_if_neq(if ammo && seated { Visibility::Hidden } else { Visibility::Inherited });
    }
    let fraction = (health.current / health.max.max(1.0)).clamp(0.0, 1.0);
    set_text(&mut health_text, format!("{:.0}", health.current.max(0.0)));
    let (node, color) = &mut *health_fill;
    let width = percent(fraction * 100.0);
    if node.width != width {
        node.width = width;
    }
    color.set_if_neq(BackgroundColor(Color::srgb(0.95 - 0.5 * fraction, 0.35 + 0.5 * fraction, 0.35)));

    let active = inventory.active as usize;
    let weapon = loadout.weapons.get(active).and_then(|w| armory.weapon(w));
    let name = weapon.map_or(String::new(), |w| weapon_display_name(&w.display_name));
    let mode = weapon
        .filter(|w| w.fire.kind == game_data::FireKind::Gun)
        .and_then(|w| w.fire_modes.get(inventory.fire_mode as usize))
        .map_or(String::new(), |m| match m {
            FireMode::Single => "  SINGLE".into(),
            FireMode::Burst => "  BURST".into(),
            FireMode::Auto => "  AUTO".into(),
        });
    // Grenades being cooked count down their fuse; C4 shows when the detonator is out.
    let mode = match (feedback.fuse, feedback.detonator) {
        (Some(fuse), _) => format!("  COOKING {fuse:.1}"),
        (None, true) => "  DETONATOR".into(),
        _ => mode,
    };
    set_text(&mut weapon_text, format!("{name}{mode}"));
    let [in_mag, spare] = inventory.ammo.get(active).copied().unwrap_or([0, 0]);
    let ammo = if inventory.reloading {
        "RELOADING".into()
    } else if weapon.is_some_and(|w| w.magazine_size > 0) {
        format!("{in_mag}  |  {spare}")
    } else {
        String::new()
    };
    set_text(&mut ammo_text, ammo);
}

fn update_kill_feed(time: Res<Time<Real>>, feedback: Res<CombatFeedback>, mut text: Single<&mut Text, With<KillFeedText>>) {
    let now = time.elapsed_secs_f64();
    let feed = feedback
        .kills
        .iter()
        .filter(|(_, at)| now - at < 8.0)
        .map(|(line, _)| line.as_str())
        .collect::<Vec<_>>()
        .join("\n");
    set_text(&mut text, feed);
}

fn update_death_notice(
    feedback: Res<CombatFeedback>,
    player: Query<(), With<LocalPlayer>>,
    soldier: Query<(), With<LocalSoldier>>,
    mut text: Single<&mut Text, With<DeathNotice>>,
) {
    let notice = match (&feedback.killed_by, player.is_empty(), soldier.is_empty()) {
        (Some(killer), false, true) => format!("Killed by {killer}\nRespawning shortly"),
        _ => String::new(),
    };
    set_text(&mut text, notice);
}

#[allow(clippy::type_complexity, clippy::too_many_arguments)]
fn update_scoreboard(
    actions: Actions,
    players: Query<(&Player, &Team, &Score, Option<&SquadMember>, Has<LocalPlayer>, Option<&game_shared::join::AccountBadge>)>,
    mut board: Single<&mut Visibility, With<Scoreboard>>,
    mut columns: Query<(&ScoreboardColumn, &mut Text), Without<ScoreboardTeam>>,
    mut headers: Query<(&ScoreboardTeam, &mut Text, &mut TextColor), Without<ScoreboardColumn>>,
    mut flags: Query<(&ScoreboardFlag, &mut ImageNode, &mut Node)>,
    icons: Res<crate::map_icons::UiIcons>,
    level: Option<Res<LoadedLevel>>,
) {
    let show = actions.pressed(Action::Scoreboard);
    board.set_if_neq(if show { Visibility::Inherited } else { Visibility::Hidden });
    if !show {
        return;
    }
    let local = players.iter().find(|p| p.4).map_or(Team::Spectator, |p| *p.1);
    for (header, mut text, mut color) in &mut headers {
        let team = if header.0 == 0 { Team::One } else { Team::Two };
        let name = crate::conquest_hud::team_name(level.as_deref(), team);
        if text.0 != name {
            text.0 = name;
        }
        color.0 = crate::conquest_hud::team_color(team, local);
    }
    for (flag, mut image, mut node) in &mut flags {
        let team = if flag.0 == 0 { Team::One } else { Team::Two };
        let side = icons.side(team);
        let handle = side.large.clone().or_else(|| side.flag.clone());
        let display = if handle.is_some() { Display::Flex } else { Display::None };
        if node.display != display {
            node.display = display;
        }
        let handle = handle.unwrap_or_default();
        if image.image != handle {
            image.image = handle;
        }
    }
    for (column, mut text) in &mut columns {
        let team = if column.0 == 0 { Team::One } else { Team::Two };
        let mut rows: Vec<_> = players.iter().filter(|(_, t, ..)| **t == team).collect();
        rows.sort_by(|a, b| b.2.score.cmp(&a.2.score));
        let mut out = format!("{:<22}{:<9}{:>5}{:>5}{:>7}\n", "", "SQUAD", "K", "D", "SCORE");
        for (player, _, score, squad, local, badge) in rows {
            let marker = if local { "> " } else { "  " };
            // A verified account's rank before its name (`join::AccountBadge`).
            let name = badge.map_or_else(|| player.name.clone(), |b| format!("{} {}", b.rank_short, player.name));
            let name: String = name.chars().take(19).collect();
            // `*` marks the squad leader.
            let squad = squad.map_or(String::new(), |s| {
                format!("{}{}", squad_name(s.squad), if s.leader { "*" } else { "" })
            });
            out += &format!(
                "{marker}{name:<20}{squad:<9}{:>5}{:>5}{:>7}\n",
                score.kills, score.deaths, score.score
            );
        }
        text.0 = out;
    }
}
