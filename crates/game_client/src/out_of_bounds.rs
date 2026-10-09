//! BF2's out-of-bounds warning: while the server says we're outside the combat area that
//! applies to us (on foot, or in our vehicle), a red edge vignette and a "Return to the
//! battlefield" card with the seconds left before we die (`game_server::out_of_bounds`).
//! Coming back inside in time cancels it, same as BF2.

use bevy::prelude::*;
use game_shared::protocol::OutOfBoundsWarning;

use crate::ui_theme::{font, shadow};

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
const PANEL: Color = Color::srgba(0.05, 0.06, 0.08, 0.6);

/// The latest word from the server: `None` once we're back inside (or haven't left).
#[derive(Resource, Default)]
struct OutOfBoundsState {
    seconds_left: Option<f32>,
}

#[derive(Component)]
struct Overlay;
#[derive(Component)]
struct OverlayTime;

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
                padding: UiRect::top(percent(12)),
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
                        row_gap: px(6),
                        padding: UiRect::axes(px(26), px(16)),
                        border_radius: BorderRadius::all(px(10)),
                        ..default()
                    },
                    BackgroundColor(PANEL),
                ))
                .with_children(|card| {
                    card.spawn((Text::new("Return to the battlefield"), font(22.0), TextColor(TEXT), shadow()));
                    card.spawn((OverlayTime, Text::new(""), font(17.0), TextColor(WARNING), shadow()));
                });
        });
}

fn receive_warning(mut messages: MessageReader<OutOfBoundsWarning>, mut state: ResMut<OutOfBoundsState>) {
    for warning in messages.read() {
        state.seconds_left = warning.seconds_left;
    }
}

fn update_overlay(
    state: Res<OutOfBoundsState>,
    mut overlay: Single<&mut Visibility, With<Overlay>>,
    mut time: Single<&mut Text, With<OverlayTime>>,
) {
    let Some(seconds) = state.seconds_left else {
        overlay.set_if_neq(Visibility::Hidden);
        return;
    };
    overlay.set_if_neq(Visibility::Inherited);
    let line = format!("{:.0} s", seconds.max(0.0).ceil());
    if time.0 != line {
        time.0 = line;
    }
}
