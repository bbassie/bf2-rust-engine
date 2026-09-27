//! Our own menus: the main menu (play, host, join, settings), the loading screen, and the
//! in-game menu on Esc. Same style as the HUD: dark translucent rounded panels, white text,
//! blue accent.
//!
//! Every button has a `Name` (`menu:play`, `level:strike_at_karkand`, `layout:gpm_cq:32`,
//! `start`, `pause:leave`, `tab:graphics`, `toggle:shadows`, ...) so scenarios can press it
//! with `Click`. A scenario with `menu: true` starts here instead of in a match.

mod browser;
mod input;
mod levels;
mod loading;
mod pages;
mod preview;
mod widgets;

use std::{
    net::{SocketAddr, ToSocketAddrs},
    sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    },
};

use bevy::{
    input::{InputSystems, mouse::AccumulatedMouseScroll},
    input_focus::InputFocus,
    prelude::*,
    render::{Render, RenderApp, RenderSystems, render_resource::PipelineCache},
    tasks::{AsyncComputeTaskPool, Task, futures::check_ready},
    text::{EditableText, EditableTextFilter},
    ui::RelativeCursorPosition,
    ui_widgets::ScrollArea,
    window::{CursorGrabMode, CursorOptions, Monitor, PrimaryMonitor, PrimaryWindow},
};
use bevy_replicon::prelude::ClientState;
use game_server::ServerSettings;
use game_shared::{
    config::GamePaths,
    level::{LoadedLevel, TEST_RANGE},
    protocol::MatchInfo,
};
use serde::Deserialize;

use crate::{
    Cli,
    conquest_hud::{ENEMY, FRIENDLY},
    deploy::DeployScreen,
    net::{self, ActiveMatch, LocalPlayer, LocalSoldier, MatchNotice, MatchSetup},
    scenario::{ScenarioInput, ScenarioSystems},
    settings::{Action, Binding, DisplayMode, Settings, ViewDistance},
};

use self::{browser::*, input::*, levels::*, loading::*, pages::*, preview::*, widgets::*};

pub struct MenuPlugin {
    /// Where the client starts: the main menu, or loading a match from the command line.
    pub start: Screen,
}

impl Plugin for MenuPlugin {
    fn build(&self, app: &mut App) {
        let compiling = CompilingPipelines::default();
        if let Some(render_app) = app.get_sub_app_mut(RenderApp) {
            render_app.insert_resource(compiling.clone()).add_systems(
                Render,
                count_compiling_pipelines.in_set(RenderSystems::Cleanup),
            );
        }
        app.insert_resource(compiling)
            .insert_state(self.start)
            .init_resource::<Menu>()
            .init_resource::<LevelCatalog>()
            .init_resource::<LoadingProgress>()
            .init_resource::<ServerBrowser>()
            .add_observer(level_changed)
            .add_systems(Startup, scan_levels)
            .add_systems(PreUpdate, menu_keys.in_set(MenuKeys).after(InputSystems))
            .add_systems(OnEnter(Screen::Menu), spawn_main_menu)
            .add_systems(OnEnter(Screen::Loading), spawn_loading_screen)
            .add_systems(OnExit(Screen::InGame), |mut menu: ResMut<Menu>| {
                menu.paused = false
            })
            .add_systems(
                Update,
                (
                    (collect_levels, open_browser, poll_browser),
                    (press_buttons, drag_sliders, sync_text_fields),
                    (sync_pause_overlay, build_pages, build_level_details, build_server_list),
                    (
                        paint_buttons,
                        paint_switches,
                        paint_sliders,
                        paint_text_fields,
                        update_values,
                    ),
                )
                    .chain()
                    .after(ScenarioSystems),
            )
            .add_systems(
                Update,
                (start_pending, track_loading, update_loading_screen)
                    .chain()
                    .run_if(in_state(Screen::Loading)),
            )
            .add_systems(Update, pause_time.run_if(in_state(Screen::InGame)));
    }
}

/// Where the menus take the keyboard (Esc, Enter); the chat box goes first.
#[derive(SystemSet, Debug, Clone, PartialEq, Eq, Hash)]
pub struct MenuKeys;

/// What the client is doing.
#[derive(States, Default, Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Screen {
    /// The main menu; no match.
    #[default]
    Menu,
    /// A match is starting or we're connecting; the level loads behind the loading screen.
    Loading,
    /// Playing or spectating.
    InGame,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
pub enum Page {
    /// Main menu start page; in game, the Esc menu's buttons.
    #[default]
    Home,
    Play,
    Host,
    Join,
    Settings,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
pub enum SettingsTab {
    #[default]
    Game,
    Graphics,
    Audio,
    Controls,
}

impl SettingsTab {
    const ALL: [SettingsTab; 4] = [
        SettingsTab::Game,
        SettingsTab::Graphics,
        SettingsTab::Audio,
        SettingsTab::Controls,
    ];

    fn label(self) -> &'static str {
        match self {
            SettingsTab::Game => "Game",
            SettingsTab::Graphics => "Graphics",
            SettingsTab::Audio => "Audio",
            SettingsTab::Controls => "Controls",
        }
    }
}

/// Menu navigation and state.
#[derive(Resource, Default)]
pub struct Menu {
    pub page: Page,
    tab: SettingsTab,
    /// In game: the Esc menu is open.
    pub paused: bool,
    /// Waiting for a key or mouse button to bind to this action.
    rebinding: Option<Action>,
    /// The press that finished rebinding shouldn't also press a button.
    swallow_click: bool,
    /// Match to start once the loading screen has been drawn, and frames to wait for that.
    pending: Option<(MatchSetup, u32)>,
    /// The match was started here: take the mouse when it begins.
    grab_on_start: bool,
    /// Enter was pressed: the page's main button.
    submit: bool,
}

const TEXT: Color = Color::srgb(0.95, 0.96, 0.98);

const DIM: Color = Color::srgba(0.85, 0.87, 0.9, 0.6);

const ACCENT: Color = FRIENDLY;

const PANEL: Color = Color::srgba(0.05, 0.06, 0.08, 0.92);
// UI colors blend in linear space, where a little white goes a long way: greys instead.

const CARD: Color = Color::srgba(0.1, 0.115, 0.15, 0.85);

const BUTTON: Color = Color::srgb(0.15, 0.17, 0.21);

const HOVER: Color = Color::srgb(0.21, 0.235, 0.285);

const FIELD: Color = Color::srgb(0.12, 0.135, 0.17);

const TRACK: Color = Color::srgb(0.25, 0.27, 0.31);

const MAP_BACKGROUND: Color = Color::srgb(0.12, 0.13, 0.15);

/// What a button does. Also names it for scenarios.
#[derive(Component, Clone, Debug, PartialEq)]
enum MenuButton {
    Page(Page),
    Back,
    Quit,
    /// Home page: play the last match again.
    QuickPlay,
    Level(String),
    Layout(String, u32),
    Team(u8),
    Start,
    Connect,
    Resume,
    Leave,
    CancelLoading,
    Tab(SettingsTab),
    Toggle(Toggle),
    Step(Slider, i8),
    Display(DisplayMode),
    WindowSize(u32, u32),
    ViewDistance(ViewDistance),
    Rebind(Action),
    ResetBindings,
    /// Join page: ask for servers again.
    Refresh,
    /// Join page: pick the listed server at this address and game port.
    Server(String, u16),
    /// Join page: star or unstar the listed server at this address and game port.
    Favourite(String, u16),
    /// Join page: star the address typed in.
    AddFavourite,
}

impl MenuButton {
    fn element_name(&self) -> String {
        match self {
            MenuButton::Page(page) => format!("menu:{}", format!("{page:?}").to_lowercase()),
            MenuButton::Back => "back".into(),
            MenuButton::Quit => "menu:quit".into(),
            MenuButton::QuickPlay => "home:play".into(),
            MenuButton::Level(name) => format!("level:{name}"),
            MenuButton::Layout(mode, size) => format!("layout:{mode}:{size}"),
            MenuButton::Team(team) => format!("team:{team}"),
            MenuButton::Start => "start".into(),
            MenuButton::Connect => "connect".into(),
            MenuButton::Resume => "pause:resume".into(),
            MenuButton::Leave => "pause:leave".into(),
            MenuButton::CancelLoading => "loading:cancel".into(),
            MenuButton::Tab(tab) => format!("tab:{}", tab.label().to_lowercase()),
            MenuButton::Toggle(toggle) => format!("toggle:{}", toggle.id()),
            MenuButton::Step(slider, dir) => {
                format!("step:{}:{}", slider.id(), if *dir > 0 { "+" } else { "-" })
            }
            MenuButton::Display(mode) => format!("display:{}", mode.label().to_lowercase()),
            MenuButton::WindowSize(w, h) => format!("size:{w}x{h}"),
            MenuButton::ViewDistance(distance) => format!("view:{}", distance.label().to_lowercase()),
            MenuButton::Rebind(action) => format!("bind:{}", action.id()),
            MenuButton::ResetBindings => "bind:reset".into(),
            MenuButton::Refresh => "browser:refresh".into(),
            MenuButton::Server(address, port) => format!("server:{address}:{port}"),
            MenuButton::Favourite(address, port) => format!("favourite:{address}:{port}"),
            MenuButton::AddFavourite => "favourite:add".into(),
        }
    }
}

/// How a button is drawn.
#[derive(Component, Clone, Copy, PartialEq)]
enum Look {
    /// Left column of the main menu.
    Nav,
    Plain,
    /// The main action of a page.
    Primary,
    /// Leaves or quits.
    Danger,
    /// An entry in a list.
    Item,
    /// Painted by its own system (switches, slider bars).
    Custom,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Toggle {
    Spectate,
    Public,
    InvertY,
    VSync,
    Shadows,
    Ssao,
}

impl Toggle {
    fn id(self) -> &'static str {
        match self {
            Toggle::Spectate => "spectate",
            Toggle::Public => "public",
            Toggle::InvertY => "invert_y",
            Toggle::VSync => "vsync",
            Toggle::Shadows => "shadows",
            Toggle::Ssao => "ssao",
        }
    }

    fn get(self, settings: &Settings) -> bool {
        match self {
            Toggle::Spectate => settings.last_match.spectate,
            Toggle::Public => settings.last_match.public,
            Toggle::InvertY => settings.invert_mouse_y,
            Toggle::VSync => settings.vsync,
            Toggle::Shadows => settings.shadows,
            Toggle::Ssao => settings.ambient_occlusion,
        }
    }

    fn flip(self, settings: &mut Settings) {
        match self {
            Toggle::Spectate => settings.last_match.spectate ^= true,
            Toggle::Public => settings.last_match.public ^= true,
            Toggle::InvertY => settings.invert_mouse_y ^= true,
            Toggle::VSync => settings.vsync ^= true,
            Toggle::Shadows => settings.shadows ^= true,
            Toggle::Ssao => settings.ambient_occlusion ^= true,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Slider {
    Sensitivity,
    FieldOfView,
    Volume,
    EffectsVolume,
    AmbienceVolume,
    Bots,
}

impl Slider {
    fn id(self) -> &'static str {
        match self {
            Slider::Sensitivity => "sensitivity",
            Slider::FieldOfView => "fov",
            Slider::Volume => "volume",
            Slider::EffectsVolume => "effects_volume",
            Slider::AmbienceVolume => "ambience_volume",
            Slider::Bots => "bots",
        }
    }

    /// Minimum, maximum, step.
    fn range(self) -> (f32, f32, f32) {
        match self {
            Slider::Sensitivity => (0.1, 4.0, 0.05),
            Slider::FieldOfView => (60.0, 100.0, 1.0),
            Slider::Volume | Slider::EffectsVolume | Slider::AmbienceVolume => (0.0, 1.0, 0.05),
            Slider::Bots => (0.0, 63.0, 1.0),
        }
    }

    fn get(self, settings: &Settings) -> f32 {
        match self {
            Slider::Sensitivity => settings.mouse_sensitivity,
            Slider::FieldOfView => settings.field_of_view,
            Slider::Volume => settings.master_volume,
            Slider::EffectsVolume => settings.effects_volume,
            Slider::AmbienceVolume => settings.ambience_volume,
            Slider::Bots => settings.last_match.bots as f32,
        }
    }

    /// Sets the value, snapped to the step, if it differs (so dragging without moving
    /// doesn't count as a change).
    fn set(self, settings: &mut impl std::ops::DerefMut<Target = Settings>, value: f32) {
        let (min, max, step) = self.range();
        let value = ((value.clamp(min, max) - min) / step).round() * step + min;
        // 0.9, not 0.90000004, in the settings file.
        let value = (value * 1000.0).round() / 1000.0;
        if (self.get(settings) - value).abs() < step * 0.25 {
            return;
        }
        match self {
            Slider::Sensitivity => settings.mouse_sensitivity = value,
            Slider::FieldOfView => settings.field_of_view = value,
            Slider::Volume => settings.master_volume = value,
            Slider::EffectsVolume => settings.effects_volume = value,
            Slider::AmbienceVolume => settings.ambience_volume = value,
            Slider::Bots => settings.last_match.bots = value as u32,
        }
    }

    fn fraction(self, settings: &Settings) -> f32 {
        let (min, max, _) = self.range();
        ((self.get(settings) - min) / (max - min)).clamp(0.0, 1.0)
    }

    fn display(self, settings: &Settings) -> String {
        let value = self.get(settings);
        match self {
            Slider::Sensitivity => format!("{value:.2}"),
            Slider::FieldOfView => format!("{value:.0} deg"),
            Slider::Volume | Slider::EffectsVolume | Slider::AmbienceVolume => format!("{:.0}%", value * 100.0),
            Slider::Bots => format!("{value:.0}"),
        }
    }
}

/// Text kept up to date by [`update_values`].
#[derive(Component, Clone, Copy, PartialEq)]
enum Value {
    Slider(Slider),
    Binding(Action),
}

/// The draggable part of a slider.
#[derive(Component)]
struct SliderBar(Slider);

#[derive(Component)]
struct SliderFill(Slider);

#[derive(Component)]
struct SliderKnob(Slider);

#[derive(Component)]
struct SwitchKnob(Toggle);

/// An editable text field and the setting it edits.
#[derive(Component, Clone, Copy, PartialEq, Debug)]
enum TextField {
    PlayerName,
    Address,
    Port,
}

/// The box around a text field's text, highlighted while it has focus.
#[derive(Component)]
struct TextFieldBox(Entity);

/// Main menu page area, or the Esc menu's panel.
#[derive(Component)]
struct PageRoot;

#[derive(Component)]
struct PauseRoot;

/// The level-dependent half of the play/host page.
#[derive(Component)]
struct LevelDetails {
    host: bool,
}
