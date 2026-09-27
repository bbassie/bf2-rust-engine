//! Our own menus: the main menu (play, host, join, settings), the loading screen, and the
//! in-game menu on Esc. Same style as the HUD: dark translucent rounded panels, white text,
//! blue accent.
//!
//! Every button has a `Name` (`menu:play`, `level:strike_at_karkand`, `layout:gpm_cq:32`,
//! `start`, `pause:leave`, `tab:graphics`, `toggle:shadows`, ...) so scenarios can press it
//! with `Click`. A scenario with `menu: true` starts here instead of in a match.

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
    settings::{Action, Binding, DisplayMode, Settings},
};

pub struct MenuPlugin {
    /// Where the client starts: the main menu, or loading a match from the command line.
    pub start: Screen,
}

impl Plugin for MenuPlugin {
    fn build(&self, app: &mut App) {
        let compiling = CompilingPipelines::default();
        if let Some(render_app) = app.get_sub_app_mut(RenderApp) {
            render_app
                .insert_resource(compiling.clone())
                .add_systems(Render, count_compiling_pipelines.in_set(RenderSystems::Cleanup));
        }
        app.insert_resource(compiling)
            .insert_state(self.start)
            .init_resource::<Menu>()
            .init_resource::<LevelCatalog>()
            .init_resource::<LoadingProgress>()
            .add_systems(Startup, scan_levels)
            .add_systems(PreUpdate, menu_keys.after(InputSystems))
            .add_systems(OnEnter(Screen::Menu), spawn_main_menu)
            .add_systems(OnEnter(Screen::Loading), spawn_loading_screen)
            .add_systems(OnExit(Screen::InGame), |mut menu: ResMut<Menu>| menu.paused = false)
            .add_systems(
                Update,
                (
                    collect_levels,
                    (press_buttons, drag_sliders, sync_text_fields),
                    (sync_pause_overlay, build_pages, build_level_details),
                    (paint_buttons, paint_switches, paint_sliders, paint_text_fields, update_values),
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
    const ALL: [SettingsTab; 4] = [SettingsTab::Game, SettingsTab::Graphics, SettingsTab::Audio, SettingsTab::Controls];

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

/// Levels to offer: the built-in test range, then everything in `imported/levels`.
#[derive(Resource)]
pub struct LevelCatalog {
    pub levels: Vec<LevelInfo>,
    scan: Option<Task<Vec<LevelInfo>>>,
    /// Bumped when the list changes.
    version: u32,
}

impl Default for LevelCatalog {
    fn default() -> Self {
        let test_range = game_shared::level::test_range().desc;
        Self {
            levels: vec![LevelInfo {
                name: TEST_RANGE.into(),
                display_name: test_range.display_name,
                minimap: None,
                layouts: test_range.game_modes.iter().map(|g| (g.mode.clone(), g.size)).collect(),
                teams: ["Team 1".into(), "Team 2".into()],
            }],
            scan: None,
            version: 0,
        }
    }
}

impl LevelCatalog {
    pub fn get(&self, name: &str) -> Option<&LevelInfo> {
        self.levels.iter().find(|l| l.name == name)
    }
}

#[derive(Clone, Debug)]
pub struct LevelInfo {
    /// Folder name.
    pub name: String,
    pub display_name: String,
    /// Map image, relative to the imported root.
    pub minimap: Option<String>,
    /// Game mode layouts: `(mode, size)`.
    pub layouts: Vec<(String, u32)>,
    pub teams: [String; 2],
}

/// The parts of a `level.ron` the menu needs; the rest is skipped.
#[derive(Deserialize)]
struct LevelSummary {
    display_name: String,
    #[serde(default)]
    minimap: Option<String>,
    #[serde(default)]
    game_modes: Vec<ModeSummary>,
    #[serde(default)]
    teams: Vec<TeamSummary>,
}

#[derive(Deserialize)]
struct ModeSummary {
    mode: String,
    size: u32,
}

#[derive(Deserialize)]
struct TeamSummary {
    name: String,
}

/// `gpm_cq` -> `Conquest`.
fn mode_label(mode: &str) -> String {
    match mode {
        "gpm_cq" => "Conquest".into(),
        "gpm_coop" => "Co-op".into(),
        "gpm_ctf" => "Capture the Flag".into(),
        "sp1" | "sp2" | "sp3" => "Singleplayer".into(),
        other => other.trim_start_matches("gpm_").to_uppercase(),
    }
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

fn font(size: f32) -> TextFont {
    TextFont {
        font_size: FontSize::Px(size),
        ..default()
    }
}

fn text(value: impl Into<String>, size: f32, color: Color) -> impl Bundle {
    (Text::new(value), font(size), TextColor(color))
}

fn background() -> BackgroundGradient {
    BackgroundGradient::from(LinearGradient::new(
        LinearGradient::TO_BOTTOM_RIGHT,
        vec![
            ColorStop::percent(Color::srgb(0.085, 0.105, 0.15), 0),
            ColorStop::percent(Color::srgb(0.03, 0.036, 0.05), 55),
            ColorStop::percent(Color::srgb(0.015, 0.018, 0.024), 100),
        ],
    ))
}

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
    Rebind(Action),
    ResetBindings,
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
            MenuButton::Step(slider, dir) => format!("step:{}:{}", slider.id(), if *dir > 0 { "+" } else { "-" }),
            MenuButton::Display(mode) => format!("display:{}", mode.label().to_lowercase()),
            MenuButton::WindowSize(w, h) => format!("size:{w}x{h}"),
            MenuButton::Rebind(action) => format!("bind:{}", action.id()),
            MenuButton::ResetBindings => "bind:reset".into(),
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
    InvertY,
    VSync,
    Shadows,
    Ssao,
}

impl Toggle {
    fn id(self) -> &'static str {
        match self {
            Toggle::Spectate => "spectate",
            Toggle::InvertY => "invert_y",
            Toggle::VSync => "vsync",
            Toggle::Shadows => "shadows",
            Toggle::Ssao => "ssao",
        }
    }

    fn get(self, settings: &Settings) -> bool {
        match self {
            Toggle::Spectate => settings.last_match.spectate,
            Toggle::InvertY => settings.invert_mouse_y,
            Toggle::VSync => settings.vsync,
            Toggle::Shadows => settings.shadows,
            Toggle::Ssao => settings.ambient_occlusion,
        }
    }

    fn flip(self, settings: &mut Settings) {
        match self {
            Toggle::Spectate => settings.last_match.spectate ^= true,
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
    Bots,
}

impl Slider {
    fn id(self) -> &'static str {
        match self {
            Slider::Sensitivity => "sensitivity",
            Slider::FieldOfView => "fov",
            Slider::Volume => "volume",
            Slider::Bots => "bots",
        }
    }

    /// Minimum, maximum, step.
    fn range(self) -> (f32, f32, f32) {
        match self {
            Slider::Sensitivity => (0.1, 4.0, 0.05),
            Slider::FieldOfView => (60.0, 100.0, 1.0),
            Slider::Volume => (0.0, 1.0, 0.05),
            Slider::Bots => (0.0, 63.0, 1.0),
        }
    }

    fn get(self, settings: &Settings) -> f32 {
        match self {
            Slider::Sensitivity => settings.mouse_sensitivity,
            Slider::FieldOfView => settings.field_of_view,
            Slider::Volume => settings.master_volume,
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
            Slider::Volume => format!("{:.0}%", value * 100.0),
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

// ---------------------------------------------------------------------------------------
// Keyboard

/// Esc goes back, opens and closes the in-game menu; rebinding takes the next key. Menus
/// take the keyboard and the wheel from the game.
#[allow(clippy::too_many_arguments)]
fn menu_keys(
    mut commands: Commands,
    mut keys: ResMut<ButtonInput<KeyCode>>,
    mouse: Res<ButtonInput<MouseButton>>,
    mut scroll: ResMut<AccumulatedMouseScroll>,
    screen: Res<State<Screen>>,
    mut menu: ResMut<Menu>,
    mut settings: ResMut<Settings>,
    deploy: Res<DeployScreen>,
    soldier: Query<(), With<LocalSoldier>>,
    window: Single<(&Window, &mut CursorOptions), With<PrimaryWindow>>,
    scripted: Option<Res<ScenarioInput>>,
) {
    if let Some(action) = menu.rebinding {
        let key = keys.get_just_pressed().next().copied();
        let button = mouse.get_just_pressed().next().copied();
        match (key, button) {
            (Some(KeyCode::Escape), _) => menu.rebinding = None,
            (Some(key), _) => {
                settings.rebind(action, Binding::Key(key));
                menu.rebinding = None;
            }
            (None, Some(button)) => {
                settings.rebind(action, Binding::Mouse(button));
                menu.rebinding = None;
                menu.swallow_click = true;
            }
            (None, None) => {}
        }
        keys.reset_all();
        scroll.delta = Vec2::ZERO;
        return;
    }

    if keys.just_pressed(KeyCode::Escape) {
        let consumed = match screen.get() {
            Screen::Menu => {
                menu.page = Page::Home;
                true
            }
            Screen::Loading => {
                commands.queue(net::leave_match);
                true
            }
            Screen::InGame if menu.paused => {
                if menu.page == Page::Settings {
                    menu.page = Page::Home;
                } else {
                    let (window, mut cursor) = window.into_inner();
                    resume(&mut menu, window, &mut cursor, !deploy.open && scripted.is_none());
                }
                true
            }
            // The deploy screen closes itself.
            Screen::InGame if deploy.open && !soldier.is_empty() => false,
            Screen::InGame => {
                menu.paused = true;
                menu.page = Page::Home;
                true
            }
        };
        if consumed {
            keys.clear_just_pressed(KeyCode::Escape);
        }
    }
    if *screen.get() == Screen::Menu && keys.any_just_pressed([KeyCode::Enter, KeyCode::NumpadEnter]) {
        menu.submit = true;
    }
    if *screen.get() != Screen::InGame || menu.paused {
        keys.reset_all();
        scroll.delta = Vec2::ZERO;
    }
}

// ---------------------------------------------------------------------------------------
// Levels

fn scan_levels(mut catalog: ResMut<LevelCatalog>, paths: Res<GamePaths>) {
    let dir = paths.imported.join("levels");
    catalog.scan = Some(AsyncComputeTaskPool::get().spawn(async move {
        let mut levels = Vec::new();
        let Ok(entries) = std::fs::read_dir(&dir) else {
            return levels;
        };
        for entry in entries.flatten() {
            let path = entry.path().join("level.ron");
            let Ok(text) = std::fs::read_to_string(&path) else {
                continue;
            };
            let summary: LevelSummary = match ron::from_str(&text) {
                Ok(summary) => summary,
                Err(err) => {
                    warn!("{}: {err}", path.display());
                    continue;
                }
            };
            let team = |i: usize| {
                summary
                    .teams
                    .get(i)
                    .map(|t| t.name.clone())
                    .filter(|n| !n.is_empty())
                    .unwrap_or_else(|| format!("Team {}", i + 1))
            };
            let mut layouts: Vec<(String, u32)> =
                summary.game_modes.iter().map(|g| (g.mode.clone(), g.size)).collect();
            layouts.sort_by(|a, b| (a.0 != "gpm_cq", &a.0, a.1).cmp(&(b.0 != "gpm_cq", &b.0, b.1)));
            layouts.dedup();
            levels.push(LevelInfo {
                name: entry.file_name().to_string_lossy().into_owned(),
                display_name: summary.display_name.clone(),
                minimap: summary.minimap.clone(),
                layouts,
                teams: [team(0), team(1)],
            });
        }
        levels.sort_by_key(|l| l.display_name.to_lowercase());
        levels
    }));
}

fn collect_levels(mut catalog: ResMut<LevelCatalog>) {
    let Some(task) = catalog.scan.as_mut() else {
        return;
    };
    let Some(levels) = check_ready(task) else {
        return;
    };
    info!("menu: {} imported levels", levels.len());
    catalog.scan = None;
    catalog.levels.truncate(1);
    catalog.levels.extend(levels);
    catalog.version += 1;
}

/// The layout to use on `level`: the current one if it has it, else the same mode at the
/// closest size, else its first.
fn pick_layout(level: &LevelInfo, mode: &str, size: u32) -> Option<(String, u32)> {
    level
        .layouts
        .iter()
        .filter(|(m, _)| m == mode)
        .min_by_key(|(_, s)| s.abs_diff(size))
        .or_else(|| level.layouts.first())
        .cloned()
}

// ---------------------------------------------------------------------------------------
// Widgets

fn button(p: &mut ChildSpawnerCommands, action: MenuButton, look: Look, label: impl Into<String>) {
    let (padding, size, color) = match look {
        Look::Nav => (UiRect::axes(px(16), px(10)), 22.0, TEXT),
        Look::Primary => (UiRect::axes(px(28), px(11)), 18.0, TEXT),
        _ => (UiRect::axes(px(14), px(8)), 15.0, TEXT),
    };
    p.spawn((
        Name::new(action.element_name()),
        action,
        look,
        Button,
        Node {
            padding,
            justify_content: if look == Look::Nav { JustifyContent::Start } else { JustifyContent::Center },
            align_items: AlignItems::Center,
            border_radius: BorderRadius::all(px(if look == Look::Nav { 8 } else { 6 })),
            ..default()
        },
        BackgroundColor(Color::NONE),
    ))
    .with_child(text(label, size, color));
}

/// A settings row: label on the left, control on the right.
fn row(p: &mut ChildSpawnerCommands, label: &str, control: impl FnOnce(&mut ChildSpawnerCommands)) {
    p.spawn(Node {
        min_height: px(40),
        align_items: AlignItems::Center,
        column_gap: px(16),
        ..default()
    })
    .with_children(|row| {
        row.spawn((
            Node {
                width: px(200),
                flex_shrink: 0.0,
                ..default()
            },
            children![text(label, 15.0, DIM)],
        ));
        row.spawn(Node {
            align_items: AlignItems::Center,
            column_gap: px(8),
            flex_wrap: FlexWrap::Wrap,
            row_gap: px(6),
            ..default()
        })
        .with_children(control);
    });
}

fn switch(p: &mut ChildSpawnerCommands, toggle: Toggle) {
    let action = MenuButton::Toggle(toggle);
    p.spawn((
        Name::new(action.element_name()),
        action,
        Look::Custom,
        Button,
        Node {
            width: px(44),
            height: px(24),
            border_radius: BorderRadius::all(px(12)),
            ..default()
        },
        BackgroundColor(TRACK),
    ))
    .with_child((
        SwitchKnob(toggle),
        Node {
            position_type: PositionType::Absolute,
            left: px(3),
            top: px(3),
            width: px(18),
            height: px(18),
            border_radius: BorderRadius::all(px(9)),
            ..default()
        },
        BackgroundColor(TEXT),
    ));
}

fn slider(p: &mut ChildSpawnerCommands, slider: Slider) {
    button(p, MenuButton::Step(slider, -1), Look::Plain, "-");
    p.spawn((
        Name::new(format!("slider:{}", slider.id())),
        SliderBar(slider),
        Look::Custom,
        Button,
        RelativeCursorPosition::default(),
        Node {
            width: px(200),
            height: px(24),
            align_items: AlignItems::Center,
            ..default()
        },
    ))
    .with_children(|bar| {
        bar.spawn((
            Node {
                width: percent(100),
                height: px(6),
                border_radius: BorderRadius::all(px(3)),
                ..default()
            },
            BackgroundColor(TRACK),
        ))
        .with_child((
            SliderFill(slider),
            Node {
                width: percent(50),
                height: percent(100),
                border_radius: BorderRadius::all(px(3)),
                ..default()
            },
            BackgroundColor(ACCENT),
        ));
        bar.spawn((
            SliderKnob(slider),
            Node {
                position_type: PositionType::Absolute,
                left: percent(50),
                top: px(4),
                width: px(16),
                height: px(16),
                margin: UiRect::left(px(-8)),
                border_radius: BorderRadius::all(px(8)),
                ..default()
            },
            BackgroundColor(TEXT),
        ));
    });
    button(p, MenuButton::Step(slider, 1), Look::Plain, "+");
    p.spawn((
        Value::Slider(slider),
        text("", 15.0, TEXT),
        Node {
            min_width: px(64),
            ..default()
        },
    ));
}

fn text_field(p: &mut ChildSpawnerCommands, field: TextField, value: &str, width: f32) {
    let mut editable = EditableText::new(value);
    editable.max_characters = Some(match field {
        TextField::PlayerName => 24,
        TextField::Address => 64,
        TextField::Port => 5,
    });
    let mut text_entity = Entity::PLACEHOLDER;
    p.spawn((
        Node {
            width: px(width),
            padding: UiRect::axes(px(10), px(7)),
            border: UiRect::all(px(1)),
            border_radius: BorderRadius::all(px(6)),
            ..default()
        },
        BackgroundColor(FIELD),
        BorderColor::all(Color::NONE),
    ))
    .with_children(|b| {
        let mut entity = b.spawn((
            Name::new(format!("field:{}", format!("{field:?}").to_lowercase())),
            field,
            editable,
            font(16.0),
            TextColor(TEXT),
            Node {
                width: percent(100),
                ..default()
            },
        ));
        if field == TextField::Port {
            entity.insert(EditableTextFilter::new(|c| c.is_ascii_digit()));
        }
        text_entity = entity.id();
    })
    .insert(TextFieldBox(text_entity));
}

fn heading(p: &mut ChildSpawnerCommands, title: &str, subtitle: &str) {
    p.spawn(Node {
        flex_direction: FlexDirection::Column,
        row_gap: px(4),
        margin: UiRect::bottom(px(18)),
        ..default()
    })
    .with_children(|h| {
        h.spawn(text(title, 30.0, TEXT));
        if !subtitle.is_empty() {
            h.spawn(text(subtitle, 15.0, DIM));
        }
    });
}

fn section(p: &mut ChildSpawnerCommands, title: &str) {
    p.spawn((
        text(title.to_uppercase(), 12.0, DIM),
        Node {
            margin: UiRect::new(px(0), px(0), px(10), px(4)),
            ..default()
        },
    ));
}

fn notice_box(p: &mut ChildSpawnerCommands, notice: &str) {
    p.spawn((
        Node {
            padding: UiRect::axes(px(14), px(10)),
            margin: UiRect::bottom(px(14)),
            max_width: px(640),
            border: UiRect::left(px(3)),
            border_radius: BorderRadius::all(px(6)),
            ..default()
        },
        BackgroundColor(ENEMY.with_alpha(0.14)),
        BorderColor::all(ENEMY),
        children![text(notice, 15.0, TEXT)],
    ));
}

fn map_preview(p: &mut ChildSpawnerCommands, minimap: Option<&str>, size: f32, asset_server: &AssetServer) {
    let mut frame = p.spawn((
        Node {
            width: px(size),
            height: px(size),
            flex_shrink: 0.0,
            border_radius: BorderRadius::all(px(8)),
            overflow: Overflow::clip(),
            justify_content: JustifyContent::Center,
            align_items: AlignItems::Center,
            ..default()
        },
        BackgroundColor(MAP_BACKGROUND),
    ));
    match minimap {
        Some(path) => {
            frame.with_child((
                ImageNode::new(asset_server.load(format!("imported://{path}"))),
                Node {
                    width: percent(100),
                    height: percent(100),
                    ..default()
                },
            ));
        }
        None => {
            frame.with_child(text("No map preview", 14.0, DIM));
        }
    }
}

// ---------------------------------------------------------------------------------------
// Screens and pages

fn spawn_main_menu(mut commands: Commands, mut menu: ResMut<Menu>) {
    menu.page = Page::Home;
    menu.rebinding = None;
    menu.pending = None;
    commands
        .spawn((
            DespawnOnExit(Screen::Menu),
            Node {
                position_type: PositionType::Absolute,
                width: percent(100),
                height: percent(100),
                ..default()
            },
            background(),
            // Above the HUD and the deploy screen.
            GlobalZIndex(20),
        ))
        .with_children(|root| {
            root.spawn(Node {
                width: px(280),
                flex_shrink: 0.0,
                flex_direction: FlexDirection::Column,
                padding: UiRect::new(px(40), px(24), px(48), px(32)),
                row_gap: px(4),
                ..default()
            })
            .with_children(|nav| {
                nav.spawn(Node {
                    flex_direction: FlexDirection::Column,
                    margin: UiRect::new(px(16), px(0), px(0), px(40)),
                    ..default()
                })
                .with_children(|title| {
                    title.spawn(text("BF2", 56.0, TEXT));
                    title.spawn(text("RUST ENGINE", 15.0, ACCENT));
                });
                button(nav, MenuButton::Page(Page::Play), Look::Nav, "Play");
                button(nav, MenuButton::Page(Page::Host), Look::Nav, "Host");
                button(nav, MenuButton::Page(Page::Join), Look::Nav, "Join");
                button(nav, MenuButton::Page(Page::Settings), Look::Nav, "Settings");
                nav.spawn(Node {
                    flex_grow: 1.0,
                    ..default()
                });
                button(nav, MenuButton::Quit, Look::Nav, "Quit");
                nav.spawn((
                    text(format!("v{}", env!("CARGO_PKG_VERSION")), 12.0, DIM),
                    Node {
                        margin: UiRect::new(px(16), px(0), px(12), px(0)),
                        ..default()
                    },
                ));
            });
            root.spawn((
                PageRoot,
                Node {
                    flex_grow: 1.0,
                    flex_direction: FlexDirection::Column,
                    padding: UiRect::new(px(24), px(48), px(48), px(40)),
                    min_width: px(0),
                    ..default()
                },
            ));
        });
}

/// Opens and closes the Esc menu's overlay.
fn sync_pause_overlay(
    mut commands: Commands,
    menu: Res<Menu>,
    screen: Res<State<Screen>>,
    overlay: Query<Entity, With<PauseRoot>>,
) {
    let open = *screen.get() == Screen::InGame && menu.paused;
    match (open, overlay.single()) {
        (true, Err(_)) => {
            commands
                .spawn((
                    PauseRoot,
                    DespawnOnExit(Screen::InGame),
                    Node {
                        position_type: PositionType::Absolute,
                        width: percent(100),
                        height: percent(100),
                        justify_content: JustifyContent::Center,
                        align_items: AlignItems::Center,
                        ..default()
                    },
                    BackgroundColor(Color::srgba(0.01, 0.015, 0.02, 0.6)),
                    GlobalZIndex(30),
                ))
                .with_child((
                    PageRoot,
                    Node {
                        flex_direction: FlexDirection::Column,
                        padding: UiRect::all(px(28)),
                        border_radius: BorderRadius::all(px(12)),
                        ..default()
                    },
                    BackgroundColor(PANEL),
                ));
        }
        (false, Ok(entity)) => commands.entity(entity).despawn(),
        _ => {}
    }
}

/// Rebuilds the page when it, the settings tab or the level list changes.
#[allow(clippy::too_many_arguments)]
fn build_pages(
    mut commands: Commands,
    menu: Res<Menu>,
    screen: Res<State<Screen>>,
    catalog: Res<LevelCatalog>,
    settings: Res<Settings>,
    notice: Res<MatchNotice>,
    active: Res<ActiveMatch>,
    level: Option<Res<LoadedLevel>>,
    cli: Res<Cli>,
    asset_server: Res<AssetServer>,
    monitors: Query<&Monitor, With<PrimaryMonitor>>,
    roots: Query<(Entity, Option<&Children>), With<PageRoot>>,
    mut built: Local<Option<(Entity, Page, SettingsTab, u32, Option<String>)>>,
) {
    let Ok((root, children)) = roots.single() else {
        return;
    };
    let key = (root, menu.page, menu.tab, catalog.version, notice.0.clone());
    if built.as_ref() == Some(&key) {
        return;
    }
    *built = Some(key);
    for child in children.into_iter().flatten() {
        commands.entity(*child).despawn();
    }
    let in_game = *screen.get() == Screen::InGame;
    let monitor = monitors.iter().next();
    commands.entity(root).with_children(|p| match (in_game, menu.page) {
        (true, Page::Settings) => {
            settings_page(p, menu.tab, &settings, &cli, monitor);
            p.spawn(Node {
                margin: UiRect::top(px(16)),
                ..default()
            })
            .with_children(|b| button(b, MenuButton::Back, Look::Plain, "Back"));
        }
        (true, _) => pause_page(p, &active, level.as_deref()),
        (false, Page::Home) => home_page(p, &settings, &catalog, notice.0.as_deref(), &asset_server),
        (false, Page::Play) => local_page(p, false, &catalog),
        (false, Page::Host) => local_page(p, true, &catalog),
        (false, Page::Join) => join_page(p, &settings, notice.0.as_deref()),
        (false, Page::Settings) => settings_page(p, menu.tab, &settings, &cli, monitor),
    });
}

fn home_page(
    p: &mut ChildSpawnerCommands,
    settings: &Settings,
    catalog: &LevelCatalog,
    notice: Option<&str>,
    asset_server: &AssetServer,
) {
    heading(p, &format!("Welcome, {}", settings.player_name), "Conquest with bots, a listen server, or someone else's server.");
    if let Some(notice) = notice {
        notice_box(p, notice);
    }
    let last = &settings.last_match;
    let level = catalog.get(&last.level).or(catalog.levels.first());
    if let Some(level) = level {
        section(p, "Continue");
        p.spawn((
            Node {
                padding: UiRect::all(px(16)),
                column_gap: px(20),
                align_items: AlignItems::Center,
                border_radius: BorderRadius::all(px(10)),
                max_width: px(640),
                ..default()
            },
            BackgroundColor(CARD),
        ))
        .with_children(|card| {
            map_preview(card, level.minimap.as_deref(), 132.0, asset_server);
            card.spawn(Node {
                flex_direction: FlexDirection::Column,
                row_gap: px(6),
                flex_grow: 1.0,
                ..default()
            })
            .with_children(|info| {
                info.spawn(text(level.display_name.clone(), 22.0, TEXT));
                let (mode, size) = pick_layout(level, &last.mode, last.size).unwrap_or_default();
                let bots = if last.bots == 1 { "1 bot".to_string() } else { format!("{} bots", last.bots) };
                info.spawn(text(format!("{} {size}  |  {bots}", mode_label(&mode)), 15.0, DIM));
                info.spawn(Node {
                    margin: UiRect::top(px(8)),
                    ..default()
                })
                .with_children(|b| button(b, MenuButton::QuickPlay, Look::Primary, "Play"));
            });
        });
    }
    let imported = catalog.levels.len().saturating_sub(1);
    let status = if catalog.scan.is_some() {
        "Looking for imported levels...".to_string()
    } else if imported == 0 {
        "No imported levels found: run bf2-import. The test range is always available.".to_string()
    } else {
        format!("{imported} imported levels and the test range.")
    };
    p.spawn((
        text(status, 13.0, DIM),
        Node {
            margin: UiRect::top(px(14)),
            ..default()
        },
    ));
}

/// Play (singleplayer) or Host: pick a level, layout, team and bots.
fn local_page(p: &mut ChildSpawnerCommands, host: bool, catalog: &LevelCatalog) {
    if host {
        heading(p, "Host", "A listen server: others join with your address.");
    } else {
        heading(p, "Play", "Singleplayer against bots.");
    }
    p.spawn(Node {
        column_gap: px(20),
        flex_grow: 1.0,
        min_height: px(0),
        ..default()
    })
    .with_children(|columns| {
        columns
            .spawn((
                ScrollArea,
                Node {
                    width: px(250),
                    flex_shrink: 0.0,
                    flex_direction: FlexDirection::Column,
                    row_gap: px(2),
                    padding: UiRect::all(px(6)),
                    overflow: Overflow::scroll_y(),
                    border_radius: BorderRadius::all(px(10)),
                    ..default()
                },
                BackgroundColor(CARD),
            ))
            .with_children(|list| {
                for level in &catalog.levels {
                    list.spawn((
                        Name::new(MenuButton::Level(level.name.clone()).element_name()),
                        MenuButton::Level(level.name.clone()),
                        Look::Item,
                        Button,
                        Node {
                            padding: UiRect::axes(px(12), px(8)),
                            flex_shrink: 0.0,
                            border_radius: BorderRadius::all(px(6)),
                            ..default()
                        },
                        BackgroundColor(Color::NONE),
                        children![text(level.display_name.clone(), 15.0, TEXT)],
                    ));
                }
            });
        columns.spawn((
            LevelDetails { host },
            Node {
                flex_grow: 1.0,
                column_gap: px(24),
                min_width: px(0),
                ..default()
            },
        ));
    });
}

/// The level-dependent part of the play/host page, rebuilt when another level is picked.
fn build_level_details(
    mut commands: Commands,
    settings: Res<Settings>,
    catalog: Res<LevelCatalog>,
    asset_server: Res<AssetServer>,
    details: Query<(Entity, &LevelDetails, Option<&Children>)>,
    mut built: Local<Option<(Entity, String)>>,
) {
    let Ok((entity, info, children)) = details.single() else {
        return;
    };
    let last = &settings.last_match;
    let key = (entity, last.level.clone());
    if built.as_ref() == Some(&key) {
        return;
    }
    *built = Some(key);
    for child in children.into_iter().flatten() {
        commands.entity(*child).despawn();
    }
    let Some(level) = catalog.get(&last.level).or(catalog.levels.first()) else {
        return;
    };
    let host = info.host;
    commands.entity(entity).with_children(|p| {
        map_preview(p, level.minimap.as_deref(), 300.0, &asset_server);
        p.spawn(Node {
            flex_direction: FlexDirection::Column,
            row_gap: px(6),
            flex_grow: 1.0,
            min_width: px(0),
            ..default()
        })
        .with_children(|options| {
            options.spawn(text(level.display_name.clone(), 24.0, TEXT));
            section(options, "Game mode");
            options
                .spawn(Node {
                    column_gap: px(6),
                    row_gap: px(6),
                    flex_wrap: FlexWrap::Wrap,
                    ..default()
                })
                .with_children(|chips| {
                    for (mode, size) in &level.layouts {
                        let label = format!("{} {size}", mode_label(mode));
                        button(chips, MenuButton::Layout(mode.clone(), *size), Look::Plain, label);
                    }
                });
            section(options, "Team");
            options
                .spawn(Node {
                    column_gap: px(6),
                    align_items: AlignItems::Center,
                    ..default()
                })
                .with_children(|chips| {
                    button(chips, MenuButton::Team(1), Look::Plain, level.teams[0].clone());
                    button(chips, MenuButton::Team(2), Look::Plain, level.teams[1].clone());
                    chips.spawn(Node {
                        width: px(12),
                        ..default()
                    });
                    switch(chips, Toggle::Spectate);
                    chips.spawn(text("Spectate", 15.0, DIM));
                });
            section(options, "Bots");
            options
                .spawn(Node {
                    column_gap: px(8),
                    align_items: AlignItems::Center,
                    ..default()
                })
                .with_children(|row| slider(row, Slider::Bots));
            if host {
                section(options, "Port");
                text_field(options, TextField::Port, &settings.last_match.port.to_string(), 110.0);
            }
            options.spawn(Node {
                flex_grow: 1.0,
                ..default()
            });
            options
                .spawn(Node {
                    margin: UiRect::top(px(12)),
                    ..default()
                })
                .with_children(|b| {
                    button(b, MenuButton::Start, Look::Primary, if host { "Host match" } else { "Start" });
                });
        });
    });
}

fn join_page(p: &mut ChildSpawnerCommands, settings: &Settings, notice: Option<&str>) {
    heading(p, "Join", "Play on a dedicated server or someone's listen server.");
    if let Some(notice) = notice {
        notice_box(p, notice);
    }
    let last = &settings.last_match;
    row(p, "Address", |c| text_field(c, TextField::Address, &last.address, 300.0));
    row(p, "Port", |c| text_field(c, TextField::Port, &last.port.to_string(), 110.0));
    row(p, "Name", |c| text_field(c, TextField::PlayerName, &settings.player_name, 300.0));
    p.spawn(Node {
        margin: UiRect::top(px(20)),
        ..default()
    })
    .with_children(|b| button(b, MenuButton::Connect, Look::Primary, "Connect"));
}

fn settings_page(p: &mut ChildSpawnerCommands, tab: SettingsTab, settings: &Settings, cli: &Cli, monitor: Option<&Monitor>) {
    heading(p, "Settings", "Changes apply right away and are saved.");
    p.spawn(Node {
        column_gap: px(6),
        margin: UiRect::bottom(px(14)),
        ..default()
    })
    .with_children(|tabs| {
        for tab in SettingsTab::ALL {
            button(tabs, MenuButton::Tab(tab), Look::Plain, tab.label());
        }
    });
    p.spawn(Node {
        flex_direction: FlexDirection::Column,
        min_width: px(640),
        ..default()
    })
    .with_children(|p| match tab {
        SettingsTab::Game => {
            row(p, "Player name", |c| text_field(c, TextField::PlayerName, &settings.player_name, 300.0));
            row(p, "Mouse sensitivity", |c| slider(c, Slider::Sensitivity));
            row(p, "Invert mouse Y", |c| switch(c, Toggle::InvertY));
            row(p, "Field of view", |c| slider(c, Slider::FieldOfView));
        }
        SettingsTab::Graphics => {
            row(p, "Window mode", |c| {
                for mode in DisplayMode::ALL {
                    button(c, MenuButton::Display(mode), Look::Plain, mode.label());
                }
            });
            row(p, "Window size", |c| {
                // Sizes that fit the monitor, and the current one.
                let fits = |w: u32, h: u32| {
                    monitor.is_none_or(|m| {
                        let scale = m.scale_factor.max(0.5) as f32;
                        w as f32 <= m.physical_width as f32 / scale && h as f32 <= m.physical_height as f32 / scale
                    })
                };
                let mut sizes: Vec<(u32, u32)> = [(1280, 720), (1600, 900), (1920, 1080), (2560, 1440)]
                    .into_iter()
                    .filter(|(w, h)| fits(*w, *h))
                    .collect();
                if !sizes.contains(&settings.window_size) {
                    sizes.push(settings.window_size);
                }
                for (w, h) in sizes {
                    button(c, MenuButton::WindowSize(w, h), Look::Plain, format!("{w}x{h}"));
                }
            });
            row(p, "VSync", |c| switch(c, Toggle::VSync));
            row(p, "Sun shadows", |c| {
                switch(c, Toggle::Shadows);
                if cli.no_shadows {
                    c.spawn(text("off for this run (--no-shadows)", 13.0, DIM));
                }
            });
            row(p, "Ambient occlusion", |c| {
                switch(c, Toggle::Ssao);
                if cli.no_ssao {
                    c.spawn(text("off for this run (--no-ssao)", 13.0, DIM));
                }
            });
        }
        SettingsTab::Audio => {
            row(p, "Master volume", |c| slider(c, Slider::Volume));
        }
        SettingsTab::Controls => {
            p.spawn(Node {
                column_gap: px(28),
                ..default()
            })
            .with_children(|columns| {
                let half = Action::ALL.len().div_ceil(2);
                for chunk in Action::ALL.chunks(half) {
                    columns
                        .spawn(Node {
                            flex_direction: FlexDirection::Column,
                            row_gap: px(3),
                            width: px(320),
                            ..default()
                        })
                        .with_children(|column| {
                            for action in chunk {
                                binding_row(column, *action);
                            }
                        });
                }
            });
            p.spawn(Node {
                margin: UiRect::top(px(12)),
                column_gap: px(12),
                align_items: AlignItems::Center,
                ..default()
            })
            .with_children(|b| {
                button(b, MenuButton::ResetBindings, Look::Plain, "Reset to defaults");
                b.spawn(text("Click a key, then press the new key or mouse button. Esc cancels.", 13.0, DIM));
            });
        }
    });
}

fn binding_row(p: &mut ChildSpawnerCommands, action: Action) {
    p.spawn(Node {
        align_items: AlignItems::Center,
        justify_content: JustifyContent::SpaceBetween,
        ..default()
    })
    .with_children(|row| {
        row.spawn(text(action.label(), 14.0, DIM));
        let button_action = MenuButton::Rebind(action);
        row.spawn((
            Name::new(button_action.element_name()),
            button_action,
            Look::Plain,
            Button,
            Node {
                min_width: px(110),
                padding: UiRect::axes(px(10), px(5)),
                justify_content: JustifyContent::Center,
                border_radius: BorderRadius::all(px(6)),
                ..default()
            },
            BackgroundColor(Color::NONE),
            children![(Value::Binding(action), text("", 14.0, TEXT))],
        ));
    });
}

fn pause_page(p: &mut ChildSpawnerCommands, active: &ActiveMatch, level: Option<&LoadedLevel>) {
    let title = level.map_or("Match".to_string(), |l| l.desc.display_name.clone());
    let subtitle = match &active.setup {
        Some(MatchSetup::Local(s)) if s.network => format!("Hosting on port {}", s.port),
        Some(MatchSetup::Local(_)) => "Singleplayer - paused".to_string(),
        Some(MatchSetup::Join { server, .. }) => format!("Online at {server}"),
        None => String::new(),
    };
    p.spawn(Node {
        flex_direction: FlexDirection::Column,
        row_gap: px(8),
        width: px(320),
        ..default()
    })
    .with_children(|p| {
        heading(p, &title, &subtitle);
        button(p, MenuButton::Resume, Look::Primary, "Resume");
        button(p, MenuButton::Page(Page::Settings), Look::Plain, "Settings");
        button(p, MenuButton::Leave, Look::Danger, "Leave match");
    });
}

// ---------------------------------------------------------------------------------------
// Interaction

#[allow(clippy::too_many_arguments)]
fn press_buttons(
    mut commands: Commands,
    mut menu: ResMut<Menu>,
    mut settings: ResMut<Settings>,
    catalog: Res<LevelCatalog>,
    mut next_screen: ResMut<NextState<Screen>>,
    mut notice: ResMut<MatchNotice>,
    mut exit: MessageWriter<AppExit>,
    buttons: Query<(&Interaction, &MenuButton), Changed<Interaction>>,
    window: Single<(&Window, &mut CursorOptions), With<PrimaryWindow>>,
    deploy: Res<DeployScreen>,
    scripted: Option<Res<ScenarioInput>>,
) {
    let (window, mut cursor) = window.into_inner();
    if std::mem::take(&mut menu.swallow_click) {
        return;
    }
    let submitted = match menu.page {
        Page::Play | Page::Host => Some(MenuButton::Start),
        Page::Join => Some(MenuButton::Connect),
        _ => None,
    }
    .filter(|_| std::mem::take(&mut menu.submit));
    let pressed: Vec<MenuButton> = buttons
        .iter()
        .filter(|(interaction, _)| **interaction == Interaction::Pressed)
        .map(|(_, button)| button.clone())
        .chain(submitted)
        .collect();
    menu.submit = false;
    for button in &pressed {
        if menu.rebinding.is_some() {
            continue;
        }
        match button {
            MenuButton::Page(page) => {
                menu.page = *page;
                if *page != Page::Home {
                    notice.0 = None;
                }
            }
            MenuButton::Back => menu.page = Page::Home,
            MenuButton::Quit => {
                exit.write(AppExit::Success);
            }
            MenuButton::QuickPlay | MenuButton::Start => {
                let host = *button == MenuButton::Start && menu.page == Page::Host;
                let setup = local_setup(&settings, host);
                begin(&mut menu, &mut next_screen, &mut notice, setup);
            }
            MenuButton::Connect => match join_setup(&settings) {
                Ok(setup) => begin(&mut menu, &mut next_screen, &mut notice, setup),
                Err(err) => notice.0 = Some(err),
            },
            MenuButton::Level(name) => {
                if settings.last_match.level != *name {
                    let last = &settings.last_match;
                    let layout = catalog.get(name).and_then(|l| pick_layout(l, &last.mode, last.size));
                    let last = &mut settings.last_match;
                    last.level = name.clone();
                    if let Some((mode, size)) = layout {
                        last.mode = mode;
                        last.size = size;
                    }
                }
            }
            MenuButton::Layout(mode, size) => {
                settings.last_match.mode = mode.clone();
                settings.last_match.size = *size;
            }
            MenuButton::Team(team) => {
                settings.last_match.team = *team;
                settings.last_match.spectate = false;
            }
            MenuButton::Resume => resume(&mut menu, window, &mut cursor, !deploy.open && scripted.is_none()),
            MenuButton::Leave | MenuButton::CancelLoading => commands.queue(net::leave_match),
            MenuButton::Tab(tab) => menu.tab = *tab,
            MenuButton::Toggle(toggle) => toggle.flip(&mut settings),
            MenuButton::Step(slider, dir) => {
                let (_, _, step) = slider.range();
                let value = slider.get(&settings) + step * *dir as f32;
                slider.set(&mut settings, value);
            }
            MenuButton::Display(mode) => settings.window_mode = *mode,
            MenuButton::WindowSize(w, h) => settings.window_size = (*w, *h),
            MenuButton::Rebind(action) => menu.rebinding = Some(*action),
            MenuButton::ResetBindings => {
                settings.bindings = Settings::default().bindings;
            }
        }
    }
}

/// Closes the Esc menu and gives the mouse back to the game if `grab`.
fn resume(menu: &mut Menu, window: &Window, cursor: &mut CursorOptions, grab: bool) {
    menu.paused = false;
    if grab && window.focused {
        cursor.visible = false;
        cursor.grab_mode = CursorGrabMode::Locked;
    }
}

/// Puts the loading screen up; the match starts once it has been drawn.
fn begin(menu: &mut Menu, next_screen: &mut NextState<Screen>, notice: &mut MatchNotice, setup: MatchSetup) {
    notice.0 = None;
    menu.pending = Some((setup, 2));
    menu.grab_on_start = true;
    next_screen.set_if_neq(Screen::Loading);
}

/// Singleplayer or a listen server with the menu's choices: the same as
/// `client --level L --mode M --size S --bots B --team T [--spectate] [--host --port P]`.
fn local_setup(settings: &Settings, host: bool) -> MatchSetup {
    let last = &settings.last_match;
    MatchSetup::Local(ServerSettings {
        level: last.level.clone(),
        mode: last.mode.clone(),
        size: last.size,
        bots: last.bots,
        port: last.port,
        network: host,
        local_player: (!last.spectate).then(|| settings.player_name.clone()),
        local_team: last.team,
        ..default()
    })
}

/// Joining the menu's address: the same as `client --connect A --port P --name N`, but
/// host names work too.
fn join_setup(settings: &Settings) -> Result<MatchSetup, String> {
    let last = &settings.last_match;
    let address = last.address.trim();
    if address.is_empty() {
        return Err("Enter the server's address.".into());
    }
    let resolved: Vec<SocketAddr> = (address, last.port)
        .to_socket_addrs()
        .map_err(|err| format!("Can't find {address}: {err}"))?
        .collect();
    let server = resolved
        .iter()
        .find(|a| a.is_ipv4())
        .or(resolved.first())
        .copied()
        .ok_or_else(|| format!("Can't find {address}"))?;
    Ok(MatchSetup::Join {
        server,
        name: settings.player_name.clone(),
        spectate: false,
    })
}

fn drag_sliders(
    mut settings: ResMut<Settings>,
    bars: Query<(&Interaction, &RelativeCursorPosition, &SliderBar)>,
) {
    for (interaction, cursor, bar) in &bars {
        if *interaction != Interaction::Pressed {
            continue;
        }
        if let Some(position) = cursor.normalized {
            let (min, max, _) = bar.0.range();
            let fraction = (position.x + 0.5).clamp(0.0, 1.0);
            bar.0.set(&mut settings, min + (max - min) * fraction);
        }
    }
}

fn sync_text_fields(
    mut settings: ResMut<Settings>,
    fields: Query<(&EditableText, &TextField), Changed<EditableText>>,
) {
    for (editable, field) in &fields {
        let value = editable.value().to_string();
        match field {
            TextField::PlayerName => {
                let name = value.trim();
                if !name.is_empty() && settings.player_name != name {
                    settings.player_name = name.to_string();
                }
            }
            TextField::Address => {
                if settings.last_match.address != value {
                    settings.last_match.address = value;
                }
            }
            TextField::Port => {
                if let Ok(port) = value.parse::<u16>()
                    && port > 0
                    && settings.last_match.port != port
                {
                    settings.last_match.port = port;
                }
            }
        }
    }
}

// ---------------------------------------------------------------------------------------
// Painting

fn is_selected(button: &MenuButton, menu: &Menu, settings: &Settings) -> bool {
    let last = &settings.last_match;
    match button {
        MenuButton::Page(page) => menu.page == *page,
        MenuButton::Level(name) => last.level == *name,
        MenuButton::Layout(mode, size) => last.mode == *mode && last.size == *size,
        MenuButton::Team(team) => last.team == *team && !last.spectate,
        MenuButton::Tab(tab) => menu.tab == *tab,
        MenuButton::Display(mode) => settings.window_mode == *mode,
        MenuButton::WindowSize(w, h) => settings.window_size == (*w, *h),
        MenuButton::Rebind(action) => menu.rebinding == Some(*action),
        _ => false,
    }
}

fn paint_buttons(
    menu: Res<Menu>,
    settings: Res<Settings>,
    mut buttons: Query<(&MenuButton, &Look, &Interaction, &mut BackgroundColor)>,
) {
    for (button, look, interaction, mut background) in &mut buttons {
        let hovered = *interaction != Interaction::None;
        let selected = is_selected(button, &menu, &settings);
        let color = match look {
            Look::Custom => continue,
            Look::Nav | Look::Item if selected => ACCENT.with_alpha(0.3),
            Look::Nav | Look::Item if hovered => HOVER.with_alpha(0.7),
            Look::Nav | Look::Item => Color::NONE,
            Look::Primary if hovered => ACCENT.lighter(0.08),
            Look::Primary => ACCENT,
            Look::Danger if hovered => ENEMY.with_alpha(0.55),
            _ if selected => ACCENT.with_alpha(0.45),
            _ if hovered => HOVER,
            _ => BUTTON,
        };
        background.set_if_neq(BackgroundColor(color));
    }
}

fn paint_switches(
    settings: Res<Settings>,
    mut tracks: Query<(&MenuButton, &Interaction, &mut BackgroundColor), Without<SwitchKnob>>,
    mut knobs: Query<(&SwitchKnob, &mut Node)>,
) {
    for (button, interaction, mut background) in &mut tracks {
        let MenuButton::Toggle(toggle) = button else {
            continue;
        };
        let on = toggle.get(&settings);
        let mut color = if on { ACCENT } else { TRACK };
        if *interaction != Interaction::None {
            color = color.lighter(0.06);
        }
        background.set_if_neq(BackgroundColor(color));
    }
    for (knob, mut node) in &mut knobs {
        let left = px(if knob.0.get(&settings) { 23 } else { 3 });
        if node.left != left {
            node.left = left;
        }
    }
}

fn paint_sliders(
    settings: Res<Settings>,
    mut fills: Query<(&SliderFill, &mut Node), Without<SliderKnob>>,
    mut knobs: Query<(&SliderKnob, &mut Node), Without<SliderFill>>,
) {
    for (fill, mut node) in &mut fills {
        let width = percent(fill.0.fraction(&settings) * 100.0);
        if node.width != width {
            node.width = width;
        }
    }
    for (knob, mut node) in &mut knobs {
        let left = percent(knob.0.fraction(&settings) * 100.0);
        if node.left != left {
            node.left = left;
        }
    }
}

fn paint_text_fields(focus: Res<InputFocus>, mut boxes: Query<(&TextFieldBox, &mut BorderColor)>) {
    for (field, mut border) in &mut boxes {
        let color = if focus.get() == Some(field.0) { ACCENT } else { Color::NONE };
        border.set_if_neq(BorderColor::all(color));
    }
}

fn update_values(menu: Res<Menu>, settings: Res<Settings>, mut texts: Query<(&Value, &mut Text)>) {
    for (value, mut text) in &mut texts {
        let line = match value {
            Value::Slider(slider) => slider.display(&settings),
            Value::Binding(action) if menu.rebinding == Some(*action) => "Press a key".into(),
            Value::Binding(action) => settings.binding(*action).label(),
        };
        if text.0 != line {
            text.0 = line;
        }
    }
}

// ---------------------------------------------------------------------------------------
// Loading and pausing

/// When the level finished loading, and the last time an asset arrived or a shader was
/// compiling.
#[derive(Resource, Default)]
struct LoadingProgress {
    level_loaded: Option<f32>,
    last_busy: f32,
}

/// Pipelines still compiling, counted in the render world. The world renders behind the
/// loading screen, so what it needs compiles while that is up.
#[derive(Resource, Clone, Default)]
struct CompilingPipelines(Arc<AtomicUsize>);

fn count_compiling_pipelines(cache: Res<PipelineCache>, compiling: Res<CompilingPipelines>) {
    compiling.0.store(cache.waiting_pipelines().count(), Ordering::Relaxed);
}

#[derive(Component)]
struct LoadingTitle;
#[derive(Component)]
struct LoadingStatus;
#[derive(Component)]
struct LoadingMap;
#[derive(Component)]
struct LoadingBar;

fn spawn_loading_screen(mut commands: Commands, mut progress: ResMut<LoadingProgress>) {
    *progress = LoadingProgress::default();
    commands
        .spawn((
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
            GlobalZIndex(20),
        ))
        .with_children(|root| {
            root.spawn((
                LoadingMap,
                Node {
                    width: px(320),
                    height: px(320),
                    border_radius: BorderRadius::all(px(10)),
                    overflow: Overflow::clip(),
                    ..default()
                },
                BackgroundColor(MAP_BACKGROUND),
            ));
            root.spawn((LoadingTitle, text("", 30.0, TEXT)));
            root.spawn((LoadingStatus, text("", 15.0, DIM)));
            root.spawn((
                Node {
                    width: px(320),
                    height: px(4),
                    border_radius: BorderRadius::all(px(2)),
                    overflow: Overflow::clip(),
                    ..default()
                },
                BackgroundColor(TRACK),
            ))
            .with_child((
                LoadingBar,
                Node {
                    position_type: PositionType::Absolute,
                    width: percent(30),
                    height: percent(100),
                    border_radius: BorderRadius::all(px(2)),
                    ..default()
                },
                BackgroundColor(ACCENT),
            ));
            root.spawn(Node {
                margin: UiRect::top(px(10)),
                ..default()
            })
            .with_children(|b| button(b, MenuButton::CancelLoading, Look::Plain, "Cancel"));
        });
}

fn start_pending(mut commands: Commands, mut menu: ResMut<Menu>) {
    let Some((_, frames)) = menu.pending.as_mut() else {
        return;
    };
    if *frames > 0 {
        *frames -= 1;
        return;
    }
    let (setup, _) = menu.pending.take().unwrap();
    commands.queue(move |world: &mut World| net::start_match(world, setup));
}

/// In game once the level is there, we are (or watch), and assets and shaders are done.
#[allow(clippy::too_many_arguments)]
fn track_loading(
    time: Res<Time<Real>>,
    mut images: MessageReader<AssetEvent<Image>>,
    mut meshes: MessageReader<AssetEvent<Mesh>>,
    compiling: Res<CompilingPipelines>,
    scripted: Option<Res<ScenarioInput>>,
    mut progress: ResMut<LoadingProgress>,
    mut menu: ResMut<Menu>,
    active: Res<ActiveMatch>,
    level: Option<Res<LoadedLevel>>,
    player: Query<(), With<LocalPlayer>>,
    mut next_screen: ResMut<NextState<Screen>>,
    window: Single<(&Window, &mut CursorOptions), With<PrimaryWindow>>,
) {
    let now = time.elapsed_secs();
    let compiling = compiling.0.load(Ordering::Relaxed) > 0;
    if images.read().count() + meshes.read().count() > 0 || compiling {
        progress.last_busy = now;
    }
    if active.setup.is_none() || menu.pending.is_some() {
        return;
    }
    let Some(_) = level else {
        return;
    };
    let loaded = *progress.level_loaded.get_or_insert(now);
    let quiet = now - progress.last_busy > 0.4 || now - loaded > 20.0;
    if !quiet || (player.is_empty() && !active.spectating()) {
        return;
    }
    (*next_screen).set_if_neq(Screen::InGame);
    let (window, mut cursor) = window.into_inner();
    // Not in scripted runs: the person at the computer is doing something else.
    let grab = std::mem::take(&mut menu.grab_on_start) && scripted.is_none();
    if grab && window.focused && !active.spectating() {
        cursor.visible = false;
        cursor.grab_mode = CursorGrabMode::Locked;
    }
}

#[allow(clippy::too_many_arguments)]
fn update_loading_screen(
    mut commands: Commands,
    time: Res<Time<Real>>,
    menu: Res<Menu>,
    active: Res<ActiveMatch>,
    client: Res<State<ClientState>>,
    level: Option<Res<LoadedLevel>>,
    matches: Query<&MatchInfo>,
    catalog: Res<LevelCatalog>,
    compiling: Res<CompilingPipelines>,
    asset_server: Res<AssetServer>,
    mut title: Single<&mut Text, (With<LoadingTitle>, Without<LoadingStatus>)>,
    mut status: Single<&mut Text, (With<LoadingStatus>, Without<LoadingTitle>)>,
    map: Single<Entity, With<LoadingMap>>,
    mut bar: Single<&mut Node, With<LoadingBar>>,
    mut shown_map: Local<Option<String>>,
) {
    let setup = menu.pending.as_ref().map(|(s, _)| s).or(active.setup.as_ref());
    // The level: known up front when we run the server, from the server when joining.
    let level_name = match setup {
        Some(MatchSetup::Local(settings)) => Some(settings.level.clone()),
        _ => matches.iter().next().map(|m| m.level.clone()),
    };
    let info = level_name.as_deref().and_then(|name| catalog.get(name));
    let heading = level
        .as_ref()
        .map(|l| l.desc.display_name.clone())
        .or_else(|| info.map(|i| i.display_name.clone()))
        .unwrap_or_else(|| "Joining".into());
    if title.0 != heading {
        title.0 = heading;
    }
    let line = match setup {
        Some(MatchSetup::Join { server, .. }) if *client.get() != ClientState::Connected => {
            format!("Connecting to {server}...")
        }
        _ if level.is_none() => "Loading level...".to_string(),
        _ if compiling.0.load(Ordering::Relaxed) > 0 => "Compiling shaders...".to_string(),
        _ => "Loading assets...".to_string(),
    };
    if status.0 != line {
        status.0 = line;
    }
    let minimap = level
        .as_ref()
        .and_then(|l| l.desc.minimap.clone())
        .or_else(|| info.and_then(|i| i.minimap.clone()));
    if minimap.is_some() && *shown_map != minimap {
        *shown_map = minimap.clone();
        let path = minimap.unwrap();
        commands.entity(*map).despawn_related::<Children>().with_child((
            ImageNode::new(asset_server.load(format!("imported://{path}"))),
            Node {
                width: percent(100),
                height: percent(100),
                ..default()
            },
        ));
    }
    // An indeterminate bar sweeping across.
    let t = (time.elapsed_secs() * 0.6).fract();
    bar.left = percent(t * 130.0 - 30.0);
}

/// Singleplayer stops the world while the Esc menu is open.
fn pause_time(menu: Res<Menu>, active: Res<ActiveMatch>, mut time: ResMut<Time<Virtual>>) {
    let singleplayer = matches!(&active.setup, Some(MatchSetup::Local(s)) if !s.network);
    let pause = menu.paused && singleplayer;
    if pause != time.is_paused() {
        if pause {
            time.pause();
        } else {
            time.unpause();
        }
    }
}
