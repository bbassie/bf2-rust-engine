//! BF2's out-of-bounds warning: while the server says we're outside the combat area that
//! applies to us (on foot, or in our vehicle), a red edge vignette and a card with the
//! seconds left before we die (`game_server::out_of_bounds`), the countdown itself the big,
//! readable element with a draining bar under it. Coming back inside in time cancels it, same
//! as BF2.
//!
//! The card hides the instant we have no soldier of our own to warn about: dead, not yet
//! respawned, or we've left the match. It doesn't wait on the server's own cancellation
//! message for that (dying or a vehicle being wrecked by the countdown itself despawns our
//! soldier in the same beat that ends the warning, so the two races; losing that race once
//! left a stale "1 s" card showing over the death screen). A watchdog also hides it if the
//! server simply stops sending anything for a while (a connection hiccup): the message is
//! unreliable and resent every tick while active, so a gap that long means something dropped
//! it, not that the countdown is still running.

use bevy::prelude::*;
use game_shared::protocol::OutOfBoundsWarning;

use crate::{
    net::LocalSoldier,
    ui_theme::{font, shadow},
};

pub struct OutOfBoundsPlugin;

impl Plugin for OutOfBoundsPlugin {
    fn build(&self, app: &mut App) {
        app.init_resource::<OutOfBoundsState>()
            .add_systems(Startup, spawn_ui)
            .add_systems(Update, (receive_warning, update_overlay));
    }
}

/// The warning's colour: BF2's out-of-bounds red.
const WARNING: Color = Color::srgb(0.95, 0.3, 0.25);
const TEXT: Color = Color::srgb(0.95, 0.96, 0.98);
const DIM: Color = Color::srgba(0.85, 0.87, 0.9, 0.75);
const PANEL: Color = Color::srgba(0.05, 0.06, 0.08, 0.6);
/// Width of the countdown's draining bar, pixels.
const BAR_WIDTH: f32 = 220.0;
/// No word from the server for this long while a countdown is supposedly running hides the
/// card anyway (it's resent every server tick, far more often than this).
const WATCHDOG_SECONDS: f32 = 1.5;

/// The latest word from the server, and enough to draw the bar and run the watchdog.
#[derive(Resource, Default)]
struct OutOfBoundsState {
    seconds_left: Option<f32>,
    /// The countdown's length when this run of it started (`seconds_left`'s first value), so
    /// the bar can show a share of it. BF2's default (10 s) until then.
    started_at: f32,
    /// Real seconds (`Time<Real>`) since the last message of either kind, for the watchdog.
    since_message: f32,
}

#[derive(Component)]
struct Overlay;
#[derive(Component)]
struct OverlayTime;
#[derive(Component)]
struct OverlayBar;

fn spawn_ui(mut commands: Commands) {
    commands
        .spawn((
            Overlay,
            Node {
                position_type: PositionType::Absolute,
                width: percent(100),
                height: percent(100),
                flex_direction: FlexDirection::Column,
                justify_content: JustifyContent::FlexStart,
                align_items: AlignItems::Center,
                padding: UiRect::top(percent(10)),
                ..default()
            },
            // A red vignette at the edges, like being critically wounded but out of bounds
            // instead: a clear, desaturating warning without blocking the view.
            BackgroundGradient::from(RadialGradient::new(
                UiPosition::CENTER,
                RadialGradientShape::FarthestCorner,
                vec![
                    ColorStop::percent(Color::srgba(0.5, 0.0, 0.0, 0.0), 50),
                    ColorStop::percent(Color::srgba(0.45, 0.0, 0.0, 0.6), 100),
                ],
            )),
            Visibility::Hidden,
        ))
        .with_children(|overlay| {
            overlay
                .spawn((
                    Node {
                        flex_direction: FlexDirection::Column,
                        align_items: AlignItems::Center,
                        row_gap: px(10),
                        padding: UiRect::axes(px(30), px(18)),
                        border_radius: BorderRadius::all(px(10)),
                        ..default()
                    },
                    BackgroundColor(PANEL),
                ))
                .with_children(|card| {
                    card.spawn((Text::new("Return to the battlefield"), font(16.0), TextColor(DIM), shadow()));
                    // The countdown itself: the card's one big, unmissable element.
                    card.spawn((OverlayTime, Text::new(""), font(52.0), TextColor(TEXT), shadow()));
                    card.spawn((
                        Node {
                            width: px(BAR_WIDTH),
                            height: px(6),
                            border_radius: BorderRadius::all(px(3)),
                            ..default()
                        },
                        BackgroundColor(Color::srgba(1.0, 1.0, 1.0, 0.15)),
                    ))
                    .with_child((
                        OverlayBar,
                        Node {
                            width: px(BAR_WIDTH),
                            height: percent(100),
                            border_radius: BorderRadius::all(px(3)),
                            ..default()
                        },
                        BackgroundColor(WARNING),
                    ));
                });
        });
}

fn receive_warning(mut messages: MessageReader<OutOfBoundsWarning>, mut state: ResMut<OutOfBoundsState>) {
    for warning in messages.read() {
        state.since_message = 0.0;
        if let Some(seconds) = warning.seconds_left
            && state.seconds_left.is_none()
        {
            state.started_at = seconds;
        }
        state.seconds_left = warning.seconds_left;
    }
}

fn update_overlay(
    time: Res<Time<Real>>,
    mut state: ResMut<OutOfBoundsState>,
    soldier: Query<(), With<LocalSoldier>>,
    mut overlay: Single<&mut Visibility, With<Overlay>>,
    mut time_text: Single<&mut Text, With<OverlayTime>>,
    mut bar: Single<&mut Node, With<OverlayBar>>,
) {
    state.since_message += time.delta_secs();
    // No soldier of our own to warn about any more (dead, not yet respawned, or we've left
    // the match): hide at once rather than wait on the server's own cancellation, which can
    // lose the race with the despawn that ends it (dying, or the vehicle we were in being
    // wrecked, both happen in the same beat as the final warning).
    let watchdog_tripped = state.seconds_left.is_some() && state.since_message > WATCHDOG_SECONDS;
    if soldier.is_empty() || watchdog_tripped {
        state.seconds_left = None;
    }
    let Some(seconds) = state.seconds_left else {
        overlay.set_if_neq(Visibility::Hidden);
        return;
    };
    overlay.set_if_neq(Visibility::Inherited);
    let line = format!("{:.0}", seconds.max(0.0).ceil());
    if time_text.0 != line {
        time_text.0 = line;
    }
    bar.width = px(BAR_WIDTH * (seconds / state.started_at.max(1.0)).clamp(0.0, 1.0));
}
