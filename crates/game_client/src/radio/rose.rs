//! The commo rose: hold the key (Q) and a ring of radio messages opens around the crosshair;
//! moving the mouse picks one (the view holds still meanwhile), letting go sends it. "Spotted"
//! reports whatever the crosshair is on.

use bevy::{input::mouse::AccumulatedMouseMotion, prelude::*};
use game_shared::{
    radio::{RadioCommand, RadioRequest},
    revive::Downed,
};

use crate::{
    camera::CameraSystems,
    chat::ChatBox,
    deploy::DeployScreen,
    local_input::LookState,
    menu::{Menu, Screen},
    net::LocalSoldier,
    settings::{Action, Actions},
    ui_theme::font,
};

pub struct RosePlugin;

impl Plugin for RosePlugin {
    fn build(&self, app: &mut App) {
        app.init_resource::<Rose>()
            .add_systems(Startup, spawn_rose)
            .add_systems(Update, (rose_input, show_rose).chain())
            .add_systems(PostUpdate, hold_view.before(CameraSystems));
    }
}

/// Ring radii and item size (logical pixels): wide enough that neighbours never overlap.
const RADIUS: Vec2 = Vec2::new(230.0, 165.0);
const ITEM: Vec2 = Vec2::new(132.0, 34.0);
/// The pointer moves within this radius; closer to the center than `DEAD_ZONE` picks nothing.
const POINTER_RANGE: f32 = 90.0;
const DEAD_ZONE: f32 = 25.0;

const PANEL: Color = Color::srgba(0.05, 0.06, 0.08, 0.72);
const PICKED: Color = Color::srgb(0.95, 0.75, 0.3);
const TEXT: Color = Color::srgb(0.92, 0.93, 0.95);
const PICKED_TEXT: Color = Color::srgb(0.08, 0.08, 0.1);
const DIM: Color = Color::srgba(0.85, 0.87, 0.9, 0.65);

#[derive(Resource, Default)]
struct Rose {
    open: bool,
    /// Mouse movement since opening, screen space.
    pointer: Vec2,
    picked: Option<usize>,
    /// The view when it opened, held while choosing.
    view: (f32, f32),
}

#[derive(Component)]
struct RoseRoot;

#[derive(Component)]
struct RoseItem(usize);

#[derive(Component)]
struct RoseCenterText;

#[derive(Component)]
struct RosePointer;

fn spawn_rose(mut commands: Commands) {
    commands
        .spawn((
            RoseRoot,
            Node {
                position_type: PositionType::Absolute,
                left: percent(50),
                top: percent(50),
                ..default()
            },
            Visibility::Hidden,
        ))
        .with_children(|center| {
            let count = RadioCommand::ROSE.len();
            for (index, command) in RadioCommand::ROSE.iter().enumerate() {
                let angle = index as f32 / count as f32 * std::f32::consts::TAU;
                let at = Vec2::new(angle.sin(), -angle.cos()) * RADIUS - ITEM / 2.0;
                center.spawn((
                    RoseItem(index),
                    Name::new(format!("rose:{}", command.message_id())),
                    Node {
                        position_type: PositionType::Absolute,
                        left: px(at.x),
                        top: px(at.y),
                        width: px(ITEM.x),
                        height: px(ITEM.y),
                        justify_content: JustifyContent::Center,
                        align_items: AlignItems::Center,
                        border_radius: BorderRadius::all(px(6)),
                        ..default()
                    },
                    BackgroundColor(PANEL),
                    children![(Text::new(command.label()), font(15.0), TextColor(TEXT))],
                ));
            }
            // The hub: what's picked, and where the mouse points.
            center.spawn((
                Node {
                    position_type: PositionType::Absolute,
                    left: px(-60),
                    top: px(-60),
                    width: px(120),
                    height: px(120),
                    border: UiRect::all(px(2)),
                    border_radius: BorderRadius::all(px(60)),
                    justify_content: JustifyContent::Center,
                    align_items: AlignItems::Center,
                    ..default()
                },
                BackgroundColor(Color::srgba(0.05, 0.06, 0.08, 0.45)),
                BorderColor::all(Color::srgba(1.0, 1.0, 1.0, 0.18)),
                children![(RoseCenterText, Text::new("RADIO"), font(13.0), TextColor(DIM), TextLayout::justify(Justify::Center))],
            ));
            center.spawn((
                RosePointer,
                Node {
                    position_type: PositionType::Absolute,
                    left: px(-4),
                    top: px(-4),
                    width: px(8),
                    height: px(8),
                    border_radius: BorderRadius::all(px(4)),
                    ..default()
                },
                BackgroundColor(PICKED),
            ));
        });
}

/// The item a pointer offset picks: clockwise from the top.
fn pick(pointer: Vec2) -> Option<usize> {
    if pointer.length() < DEAD_ZONE {
        return None;
    }
    let count = RadioCommand::ROSE.len() as f32;
    let angle = pointer.x.atan2(-pointer.y).rem_euclid(std::f32::consts::TAU);
    Some((angle / std::f32::consts::TAU * count).round() as usize % RadioCommand::ROSE.len())
}

#[allow(clippy::too_many_arguments)]
fn rose_input(
    actions: Actions,
    motion: Res<AccumulatedMouseMotion>,
    screen: Res<State<Screen>>,
    menu: Res<Menu>,
    deploy: Res<DeployScreen>,
    chat: Res<ChatBox>,
    soldier: Query<Has<Downed>, With<LocalSoldier>>,
    look: Res<LookState>,
    mut rose: ResMut<Rose>,
    mut requests: MessageWriter<RadioRequest>,
) {
    let usable = *screen.get() == Screen::InGame
        && !menu.paused
        && !deploy.open
        && chat.typing.is_none()
        && soldier.single().is_ok_and(|downed| !downed);
    if !usable {
        *rose = Rose::default();
        return;
    }
    if !rose.open {
        if actions.just_pressed(Action::CommoRose) {
            *rose = Rose {
                open: true,
                view: (look.yaw, look.pitch),
                ..default()
            };
        }
        return;
    }
    rose.pointer = (rose.pointer + motion.delta).clamp_length_max(POINTER_RANGE);
    rose.picked = pick(rose.pointer);
    if !actions.pressed(Action::CommoRose) {
        if let Some(index) = rose.picked {
            let command = RadioCommand::ROSE[index];
            debug!(target: "audio", "commo rose: {command:?}");
            requests.write(RadioRequest { command });
        }
        *rose = Rose::default();
    }
}

/// The view holds still while the mouse picks an item.
fn hold_view(rose: Res<Rose>, mut look: ResMut<LookState>) {
    if rose.open {
        (look.yaw, look.pitch) = rose.view;
    }
}

fn show_rose(
    rose: Res<Rose>,
    mut root: Single<&mut Visibility, With<RoseRoot>>,
    mut items: Query<(&RoseItem, &mut BackgroundColor, &Children)>,
    mut colors: Query<&mut TextColor, Without<RoseCenterText>>,
    mut center: Single<&mut Text, With<RoseCenterText>>,
    mut pointer: Single<&mut Node, With<RosePointer>>,
) {
    root.set_if_neq(if rose.open { Visibility::Inherited } else { Visibility::Hidden });
    if !rose.open {
        return;
    }
    for (item, mut background, children) in &mut items {
        let picked = rose.picked == Some(item.0);
        background.0 = if picked { PICKED } else { PANEL };
        for child in children.iter() {
            if let Ok(mut color) = colors.get_mut(child) {
                color.0 = if picked { PICKED_TEXT } else { TEXT };
            }
        }
    }
    let label = rose.picked.map_or("RADIO", |i| RadioCommand::ROSE[i].label());
    if center.0 != label {
        center.0 = label.to_string();
    }
    let at = rose.pointer * (50.0 / POINTER_RANGE);
    pointer.left = px(at.x - 4.0);
    pointer.top = px(at.y - 4.0);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn picks_clockwise_from_the_top() {
        assert_eq!(pick(Vec2::ZERO), None);
        assert_eq!(pick(Vec2::new(0.0, -80.0)), Some(0));
        assert_eq!(pick(Vec2::new(80.0, -30.0)), Some(2));
        assert_eq!(pick(Vec2::new(10.0, -10.0)), None);
        assert_eq!(pick(Vec2::new(0.0, 80.0)), Some(5));
        assert_eq!(pick(Vec2::new(-80.0, -1.0)), Some(8));
    }
}
