//! The commander on the client (the rules are in `game_shared::commander` and
//! `game_server::commander`): the commander screen ([`screen`], Caps Lock), the commander
//! post on the deploy screen (apply, resign, mutiny), squad orders and the team's assets on
//! the HUD, the minimap and the big map ([`markers`]), and what the assets look and sound
//! like: the UAV circling, supply crates floating down, shells whistling in ([`visuals`]).

use bevy::{
    input::InputSystems,
    prelude::*,
    window::{CursorGrabMode, CursorOptions},
};
use game_shared::{
    commander::{Asset, Commander, CommanderRequest, OrderKind},
    level::LoadedLevel,
    protocol::{Player, Team},
};

use crate::{
    deploy::DeployScreen,
    menu::{Menu, MenuKeys},
    net::LocalPlayer,
    settings::{Action, Actions},
};

mod markers;
mod screen;
mod visuals;

pub struct ClientCommanderPlugin;

impl Plugin for ClientCommanderPlugin {
    fn build(&self, app: &mut App) {
        app.init_resource::<CommanderScreen>()
            .add_plugins((screen::ScreenPlugin, markers::MarkersPlugin, visuals::VisualsPlugin))
            .add_systems(PreUpdate, close_on_escape.after(InputSystems).before(MenuKeys))
            .add_systems(
                Update,
                (toggle_screen, rebuild_deploy_panel, press_deploy_panel)
                    .chain()
                    .after(crate::scenario::ScenarioSystems),
            );
    }
}

/// The commander screen: whether it shows (the mouse is ours then), the squad picked for
/// orders and what a click on the map does.
#[derive(Resource, Default)]
pub struct CommanderScreen {
    pub open: bool,
    pub squad: Option<u8>,
    pub tool: Option<Tool>,
    /// A click on the map at this world position, from a scenario.
    pub click: Option<Vec3>,
}

/// What a click on the commander map does.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Tool {
    Order(OrderKind),
    Asset(Asset),
}

/// The deploy screen's commander section (a node in its side column).
#[derive(Component)]
pub struct CommanderPanel;

#[derive(Component, Clone, Copy)]
enum PanelButton {
    Request(CommanderRequest),
    OpenScreen,
}

const TEXT: Color = Color::srgb(0.95, 0.96, 0.98);
const DIM: Color = Color::srgba(0.85, 0.87, 0.9, 0.6);
/// The commander's color on the panels.
pub const GOLD: Color = Color::srgb(0.98, 0.8, 0.35);

use crate::ui_theme::font;

/// Caps Lock opens and closes the commander screen, for the commander only.
#[allow(clippy::too_many_arguments)]
fn toggle_screen(
    actions: Actions,
    mut screen: ResMut<CommanderScreen>,
    local: Query<Has<Commander>, With<LocalPlayer>>,
    level: Option<Res<LoadedLevel>>,
    deploy: Res<DeployScreen>,
    menu: Res<Menu>,
    mut cursor: Single<&mut CursorOptions>,
    window: Single<&Window>,
) {
    let commander = local.single().unwrap_or(false) && level.is_some();
    let was_open = screen.open;
    if !commander {
        screen.open = false;
    } else if actions.just_pressed(Action::CommanderScreen) && !menu.paused {
        screen.open = !screen.open;
    }
    if screen.open && !was_open {
        screen.tool = None;
        cursor.visible = true;
        cursor.grab_mode = CursorGrabMode::None;
    } else if !screen.open && was_open {
        screen.tool = None;
        if window.focused && !deploy.open && !menu.paused {
            cursor.visible = false;
            cursor.grab_mode = CursorGrabMode::Locked;
        }
    }
}

/// Esc closes the commander screen instead of opening the menu.
fn close_on_escape(mut keys: ResMut<ButtonInput<KeyCode>>, mut screen: ResMut<CommanderScreen>) {
    if screen.open && keys.just_pressed(KeyCode::Escape) {
        keys.clear_just_pressed(KeyCode::Escape);
        if screen.tool.take().is_none() {
            screen.open = false;
        }
    }
}

fn panel_button(parent: &mut ChildSpawnerCommands, button: PanelButton, name: &str, label: &str) {
    parent
        .spawn((
            button,
            Button,
            Name::new(name.to_string()),
            Node {
                padding: UiRect::axes(px(10), px(4)),
                border_radius: BorderRadius::all(px(5)),
                ..default()
            },
            BackgroundColor(Color::srgba(1.0, 1.0, 1.0, 0.08)),
        ))
        .with_child((Text::new(label), font(13.0), TextColor(TEXT)));
}

/// Who commands our team, and what we can do about it; rebuilt when that changes.
#[allow(clippy::type_complexity)]
fn rebuild_deploy_panel(
    mut commands: Commands,
    local: Query<(Entity, &Team), With<LocalPlayer>>,
    commanders: Query<(Entity, &Player, &Team), With<Commander>>,
    panel: Query<(Entity, Option<&Children>), With<CommanderPanel>>,
    mut voted: Local<Option<Entity>>,
    buttons: Query<(&Interaction, &PanelButton), Changed<Interaction>>,
    mut built: Local<String>,
) {
    let (Ok((me, &team)), Ok((panel, children))) = (local.single(), panel.single()) else {
        return;
    };
    let commander = commanders.iter().find(|(_, _, t)| **t == team).map(|(e, p, _)| (e, p.name.clone()));
    if buttons
        .iter()
        .any(|(i, b)| *i == Interaction::Pressed && matches!(b, PanelButton::Request(CommanderRequest::Mutiny)))
    {
        *voted = commander.as_ref().map(|c| c.0);
    }
    if voted.is_some() && *voted != commander.as_ref().map(|c| c.0) {
        *voted = None;
    }
    let key = format!("{team:?}{commander:?}{:?}", voted.is_some());
    if *built == key {
        return;
    }
    *built = key;
    for child in children.into_iter().flatten() {
        commands.entity(*child).despawn();
    }
    if team == Team::Spectator {
        return;
    }
    commands.entity(panel).with_children(|panel| {
        panel.spawn((Text::new("COMMANDER"), font(15.0), TextColor(GOLD)));
        let (line, button) = match &commander {
            None => (
                "Nobody commands the team.".to_string(),
                Some((PanelButton::Request(CommanderRequest::Apply), "commander:apply", "Apply for commander")),
            ),
            Some((entity, _)) if *entity == me => (
                "You command the team: orders and assets on the commander screen (Caps Lock).".to_string(),
                Some((PanelButton::Request(CommanderRequest::Resign), "commander:resign", "Resign")),
            ),
            Some((_, name)) if voted.is_some() => (format!("{name} commands the team. You voted to remove them."), None),
            Some((_, name)) => (
                format!("{name} commands the team."),
                Some((PanelButton::Request(CommanderRequest::Mutiny), "commander:mutiny", "Vote to remove")),
            ),
        };
        panel.spawn((Text::new(line), font(12.0), TextColor(DIM)));
        panel
            .spawn(Node {
                column_gap: px(6),
                ..default()
            })
            .with_children(|row| {
                if commander.as_ref().is_some_and(|(e, _)| *e == me) {
                    panel_button(row, PanelButton::OpenScreen, "commander:screen", "Commander screen");
                }
                if let Some((button, name, label)) = button {
                    panel_button(row, button, name, label);
                }
            });
    });
}

fn press_deploy_panel(
    buttons: Query<(&Interaction, &PanelButton), Changed<Interaction>>,
    mut requests: MessageWriter<CommanderRequest>,
    mut screen: ResMut<CommanderScreen>,
) {
    for (interaction, button) in &buttons {
        if *interaction != Interaction::Pressed {
            continue;
        }
        match button {
            PanelButton::Request(request) => {
                requests.write(*request);
            }
            PanelButton::OpenScreen => screen.open = true,
        }
    }
}

// `map_uv`/`map_point`/`map_size` moved to `map_icons` (they were duplicated, byte for byte,
// across `deploy`, `bigmap` and here); re-exported so `screen` and `markers` keep using them
// as `super::map_uv` and the like.
pub use crate::map_icons::{map_point, map_size, map_uv};
