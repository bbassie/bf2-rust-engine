//! Minimal HUD: status text and a crosshair.

use bevy::{
    diagnostic::{DiagnosticsStore, FrameTimeDiagnosticsPlugin},
    prelude::*,
    render::view::screenshot::{Screenshot, save_to_disk},
};
use bevy_replicon::prelude::*;
use game_shared::{
    level::LoadedLevel,
    protocol::{Player, Team},
};

use crate::{
    Cli,
    net::{LocalPlayer, LocalSoldier},
    prediction::{PredictionStats, SoldierRender},
};

pub struct HudPlugin;

impl Plugin for HudPlugin {
    fn build(&self, app: &mut App) {
        app.add_systems(Startup, spawn_hud)
            .add_systems(Update, (update_hud, auto_screenshot));
    }
}

#[derive(Component)]
struct StatusText;

fn spawn_hud(mut commands: Commands) {
    commands.spawn((
        StatusText,
        Text::new(""),
        TextFont {
            font_size: FontSize::Px(15.0),
            ..default()
        },
        TextShadow::default(),
        Node {
            position_type: PositionType::Absolute,
            top: px(8),
            left: px(10),
            ..default()
        },
    ));
    commands.spawn((
        Node {
            width: percent(100),
            height: percent(100),
            justify_content: JustifyContent::Center,
            align_items: AlignItems::Center,
            ..default()
        },
        children![(
            Text::new("+"),
            TextFont {
                font_size: FontSize::Px(22.0),
                ..default()
            },
            TextShadow::default(),
        )],
    ));
}

fn update_hud(
    diagnostics: Res<DiagnosticsStore>,
    client: Res<State<ClientState>>,
    server: Res<State<ServerState>>,
    level: Option<Res<LoadedLevel>>,
    players: Query<(&Player, &Team, Has<LocalPlayer>)>,
    soldier: Query<&SoldierRender, With<LocalSoldier>>,
    prediction: Res<PredictionStats>,
    client_stats: Res<ClientStats>,
    mut text: Single<&mut Text, With<StatusText>>,
) {
    let fps = diagnostics
        .get(&FrameTimeDiagnosticsPlugin::FPS)
        .and_then(|d| d.smoothed())
        .unwrap_or_default();
    let mode = match (client.get(), server.get()) {
        (ClientState::Connected, _) => "connected",
        (ClientState::Connecting, _) => "connecting...",
        (_, ServerState::Running) => "hosting",
        _ => "singleplayer",
    };
    let level = level
        .map(|l| l.desc.display_name.clone())
        .unwrap_or_else(|| "loading...".into());
    let (mut humans, mut bots) = (0, 0);
    let mut me = String::new();
    for (player, team, local) in &players {
        if player.is_bot {
            bots += 1;
        } else {
            humans += 1;
        }
        if local {
            me = format!("{} ({team:?})", player.name);
        }
    }
    let position = soldier
        .single()
        .map(|s| format!("{:.1} {:.1} {:.1}", s.position.x, s.position.y, s.position.z))
        .unwrap_or_else(|_| "spectating".into());

    let net = if *client.get() == ClientState::Connected {
        format!(
            "rtt {:.0} ms  loss {:.1}%  down {:.1} KB/s  replay {} ticks  correction {:.3} m\n",
            client_stats.rtt * 1000.0,
            client_stats.packet_loss * 100.0,
            client_stats.received_bps / 1024.0,
            prediction.replayed,
            prediction.last_correction
        )
    } else {
        String::new()
    };
    text.0 = format!(
        "{mode} | {level} | {fps:.0} fps\n\
         {me}  players: {humans} + {bots} bots\n\
         pos: {position}\n\
         {net}\
         click: capture mouse   esc: release   WASD move   shift sprint   space jump   ctrl crouch   Z prone"
    );
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
