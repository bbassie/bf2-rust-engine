//! The end-of-round summary, under the round banner during the break: the best players of
//! the round, our own round and career stats, and the next map. The server sends each
//! player their own [`RoundSummary`] when the round ends.

use bevy::prelude::*;
use game_shared::{
    conquest::RoundState,
    level::LoadedLevel,
    protocol::Team,
    summary::{RoundSummary, StatLine},
};

use crate::{
    conquest_hud::{team_color, team_name},
    net::LocalPlayer,
};

pub struct SummaryPlugin;

impl Plugin for SummaryPlugin {
    fn build(&self, app: &mut App) {
        app.init_resource::<LastSummary>()
            .add_systems(Startup, spawn_summary)
            .add_systems(Update, (receive_summary, show_summary).chain());
    }
}

const TEXT: Color = Color::srgb(0.95, 0.96, 0.98);
const DIM: Color = Color::srgba(0.85, 0.87, 0.9, 0.6);
const ACCENT: Color = Color::srgb(0.95, 0.75, 0.3);

/// The summary of the round that just ended, until the next one starts.
#[derive(Resource, Default)]
struct LastSummary {
    summary: Option<RoundSummary>,
    version: u32,
}

#[derive(Component)]
struct SummaryRoot;

#[derive(Component)]
struct SummaryPanel;

fn font(size: f32) -> TextFont {
    TextFont {
        font_size: FontSize::Px(size),
        ..default()
    }
}

fn spawn_summary(mut commands: Commands) {
    commands
        .spawn((
            SummaryRoot,
            Node {
                position_type: PositionType::Absolute,
                width: percent(100),
                // Below the round banner.
                top: percent(37),
                justify_content: JustifyContent::Center,
                ..default()
            },
            Visibility::Hidden,
            // Over the chat box.
            GlobalZIndex(5),
        ))
        .with_child((
            SummaryPanel,
            Node {
                padding: UiRect::axes(px(24), px(18)),
                column_gap: px(36),
                border_radius: BorderRadius::all(px(10)),
                ..default()
            },
            BackgroundColor(Color::srgba(0.04, 0.05, 0.07, 0.85)),
        ));
}

fn receive_summary(mut summaries: MessageReader<RoundSummary>, mut last: ResMut<LastSummary>) {
    for summary in summaries.read() {
        info!(
            "round summary: {} top players, you: {:?}",
            summary.top.len(),
            summary.you.as_ref().map(|y| (y.rank, y.round.score, y.round.kills, y.round.deaths))
        );
        last.summary = Some(summary.clone());
        last.version += 1;
    }
}

fn duration(seconds: f32) -> String {
    let minutes = (seconds / 60.0).round() as u32;
    match minutes {
        0 => format!("{:.0} s", seconds),
        1..60 => format!("{minutes} min"),
        _ => format!("{}h {:02}m", minutes / 60, minutes % 60),
    }
}

/// `label, this round, career` rows.
fn stat_rows(round: &StatLine, career: Option<&StatLine>) -> Vec<(&'static str, String, String)> {
    let both = |f: &dyn Fn(&StatLine) -> String| (f(round), career.map_or(String::new(), f));
    let named = |name: &Option<String>| name.clone().unwrap_or_else(|| "-".into());
    let rows: [(&'static str, &dyn Fn(&StatLine) -> String); 7] = [
        ("Score", &|l| l.score.to_string()),
        ("Kills", &|l| l.kills.to_string()),
        ("Deaths", &|l| l.deaths.to_string()),
        ("K/D", &|l| format!("{:.2}", l.kills as f32 / l.deaths.max(1) as f32)),
        ("Flags", &|l| l.captures.to_string()),
        ("Time", &|l| duration(l.seconds_played)),
        ("Kit", &|l| named(&l.favourite_kit)),
    ];
    let mut out: Vec<_> = rows
        .iter()
        .map(|(label, f)| {
            let (a, b) = both(f);
            (*label, a, b)
        })
        .collect();
    out.push(("Weapon", named(&round.favourite_weapon), career.map_or(String::new(), |c| named(&c.favourite_weapon))));
    if let Some(career) = career {
        out.push(("Rounds", String::new(), career.rounds.to_string()));
    }
    out
}

fn section(p: &mut ChildSpawnerCommands, title: &str) {
    p.spawn((
        Text::new(title.to_uppercase()),
        font(12.0),
        TextColor(DIM),
        Node {
            margin: UiRect::new(px(0), px(0), px(8), px(4)),
            ..default()
        },
    ));
}

#[allow(clippy::too_many_arguments)]
fn show_summary(
    mut commands: Commands,
    mut last: ResMut<LastSummary>,
    rounds: Query<&RoundState>,
    level: Option<Res<LoadedLevel>>,
    local: Query<&Team, With<LocalPlayer>>,
    mut root: Single<&mut Visibility, With<SummaryRoot>>,
    panel: Single<Entity, With<SummaryPanel>>,
    mut built: Local<Option<u32>>,
    mut was_ended: Local<bool>,
) {
    let ended = matches!(rounds.single(), Ok(RoundState::Ended { .. }));
    if *was_ended && !ended {
        // A new round (or map): the summary is history.
        last.summary = None;
    }
    *was_ended = ended;
    let Some(summary) = last.summary.as_ref().filter(|_| ended) else {
        root.set_if_neq(Visibility::Hidden);
        return;
    };
    root.set_if_neq(Visibility::Inherited);
    if *built == Some(last.version) {
        return;
    }
    *built = Some(last.version);
    let local = local.single().copied().unwrap_or_default();
    let level = level.as_deref();
    commands.entity(*panel).despawn_related::<Children>().with_children(|p| {
        p.spawn(Node {
            flex_direction: FlexDirection::Column,
            min_width: px(360),
            ..default()
        })
        .with_children(|column| {
            section(column, "Best players");
            column.spawn((
                Text::new(format!("{:<3}{:<20}{:>6}{:>5}{:>5}", "", "", "SCORE", "K", "D")),
                font(14.0),
                TextColor(DIM),
            ));
            for (i, row) in summary.top.iter().enumerate() {
                let name: String = row.name.chars().take(18).collect();
                column.spawn((
                    Text::new(format!("{:<3}{name:<20}{:>6}{:>5}{:>5}", i + 1, row.score, row.kills, row.deaths)),
                    font(15.0),
                    TextColor(team_color(row.team, local)),
                ));
            }
            if let Some((name, mode, size)) = &summary.next_map {
                section(column, "Next map");
                column.spawn((Text::new(format!("{name}  ({mode} {size})")), font(16.0), TextColor(ACCENT)));
            }
        });
        let Some(you) = &summary.you else {
            return;
        };
        p.spawn(Node {
            flex_direction: FlexDirection::Column,
            min_width: px(300),
            ..default()
        })
        .with_children(|column| {
            let team = if local == Team::Spectator { String::new() } else { format!(" for {}", team_name(level, local)) };
            section(column, &format!("You: #{} of the round{team}", you.rank));
            let career = you.career.as_ref();
            column.spawn((
                Text::new(format!("{:<8}{:>12}{:>12}", "", "ROUND", if career.is_some() { "CAREER" } else { "" })),
                font(14.0),
                TextColor(DIM),
            ));
            for (label, round, total) in stat_rows(&you.round, career) {
                column.spawn((Text::new(format!("{label:<8}{round:>12}{total:>12}")), font(15.0), TextColor(TEXT)));
            }
        });
    });
}
