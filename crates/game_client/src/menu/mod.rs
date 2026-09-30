//! Our own menus: the main menu (play, host, join, settings), the loading screen, and the
//! in-game menu on Esc. Same style as the HUD: dark translucent rounded panels, white text,
//! blue accent.
//!
//! Every button has a `Name` (`menu:play`, `level:strike_at_karkand`, `layout:gpm_cq:32`,
//! `start`, `pause:leave`, `tab:graphics`, `toggle:shadows`, ...) so scenarios can press it
//! with `Click`. A scenario with `menu: true` starts here instead of in a match.

mod account;
mod browser;
mod download;
mod input;
mod levels;
mod loading;
mod pages;
mod preview;
pub mod text_input;
mod voice;
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
    input_focus::tab_navigation::TabGroup,
    prelude::*,
    render::{Render, RenderApp, RenderSystems, render_resource::PipelineCache},
    tasks::{AsyncComputeTaskPool, Task, futures::check_ready},
    text::EditableText,
    ui::RelativeCursorPosition,
    ui_widgets::{ControlOrientation, ScrollArea, ScrollIntoView, Scrollbar, ScrollbarThumb},
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
    settings::{
        Action, Anisotropy, AntiAliasing, BindSlot, Binding, CrosshairStyle, DisplayMode, GraphicsPreset,
        Quality, Settings, ShadowQuality, SsaoQuality, StanceMode, ToneMapping, ViewDistance,
    },
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
            .add_plugins((
                download::DownloadUiPlugin,
                account::AccountUiPlugin,
                voice::VoiceUiPlugin,
                text_input::TextInputPlugin,
            ))
            .insert_state(self.start)
            .init_resource::<Menu>()
            .init_resource::<LevelCatalog>()
            .init_resource::<LoadingProgress>()
            .init_resource::<ServerBrowser>()
            .add_observer(level_changed)
            .add_systems(Startup, scan_levels)
            .add_systems(First, leave_when_asked)
            .add_systems(
                PreUpdate,
                menu_keys
                    .in_set(MenuKeys)
                    .after(InputSystems)
                    // `menu_keys` resets `ButtonInput<KeyCode>` while not (actively) in-game so
                    // gameplay never sees a menu keypress; ordered after Bevy's own input
                    // dispatch (which is what its Tab/Shift+Tab navigation observer, reading
                    // that same resource for the Shift modifier, hangs off of) so it doesn't
                    // wipe Shift's state out from under it first.
                    .after(bevy::input_focus::InputFocusSystems::Dispatch),
            )
            .add_systems(OnEnter(Screen::Menu), spawn_main_menu)
            .add_systems(OnEnter(Screen::Loading), spawn_loading_screen)
            .add_systems(OnExit(Screen::InGame), |mut menu: ResMut<Menu>| {
                menu.paused = false
            })
            .add_systems(
                Update,
                (
                    (collect_levels, open_browser, poll_browser, poll_dns),
                    gamepad_menu_nav,
                    (press_buttons, drag_sliders, sync_text_fields),
                    (sync_pause_overlay, build_pages, build_level_details, build_server_list),
                    (paint_buttons, paint_switches, paint_sliders, update_values),
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

/// Leaves the match a button or Esc asked to leave. Leaving removes the level and the
/// server's resources, so it runs first thing in the frame: queued as a command from `Update`
/// it landed between a system set's `resource_exists::<LoadedLevel>` check and the set's
/// systems, whose `Res<LoadedLevel>` then panicked.
fn leave_when_asked(world: &mut World) {
    if world.resource::<Menu>().leave {
        world.resource_mut::<Menu>().leave = false;
        net::leave_match(world);
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
    /// Optional master server account (`account`).
    Account,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
pub enum SettingsTab {
    #[default]
    Game,
    Graphics,
    Audio,
    Controls,
    Gamepad,
}

impl SettingsTab {
    const ALL: [SettingsTab; 5] = [
        SettingsTab::Game,
        SettingsTab::Graphics,
        SettingsTab::Audio,
        SettingsTab::Controls,
        SettingsTab::Gamepad,
    ];

    fn label(self) -> &'static str {
        match self {
            SettingsTab::Game => "Game",
            SettingsTab::Graphics => "Graphics",
            SettingsTab::Audio => "Audio",
            SettingsTab::Controls => "Controls",
            SettingsTab::Gamepad => "Gamepad",
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
    /// Waiting for a key or mouse button to bind to this action's slot.
    rebinding: Option<(Action, BindSlot)>,
    /// Waiting for a gamepad button to bind to this action.
    rebinding_gamepad: Option<Action>,
    /// The press that finished rebinding shouldn't also press a button.
    swallow_click: bool,
    /// Match to start once the loading screen has been drawn, and frames to wait for that.
    pending: Option<(MatchSetup, u32)>,
    /// The match was started here: take the mouse when it begins.
    grab_on_start: bool,
    /// Enter was pressed: the page's main button.
    submit: bool,
    /// Leave the match at the start of the next frame (see [`leave_when_asked`]).
    leave: bool,
    /// The button a gamepad has moved focus to, for D-pad/stick menu navigation.
    gamepad_focus: Option<Entity>,
    /// Debounces gamepad navigation repeats.
    nav_cooldown: f32,
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
    /// Play/Host page: the bots' difficulty.
    BotDifficulty(game_server::ai::skill::BotDifficulty),
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
    ToneMapping(ToneMapping),
    /// Bindings page: start waiting for a key or mouse button for this action's slot.
    RebindSlot(Action, BindSlot),
    /// Bindings page: start waiting for a gamepad button for this action.
    RebindGamepad(Action),
    /// Bindings page: clears an action's gamepad binding without waiting for a new one.
    ClearGamepad(Action),
    ResetBindings,
    StanceMode(StanceKind, StanceMode),
    Preset(GraphicsPreset),
    ShadowQuality(ShadowQuality),
    AntiAliasing(AntiAliasing),
    SsaoQuality(SsaoQuality),
    Anisotropy(Anisotropy),
    ParticleQuality(Quality),
    CrosshairStyle(CrosshairStyle),
    FrameCap(u32),
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
            MenuButton::BotDifficulty(d) => format!("difficulty:{}", d.name().to_lowercase()),
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
            MenuButton::ToneMapping(t) => format!("tonemap:{}", t.label().to_lowercase().replace(' ', "")),
            MenuButton::RebindSlot(action, BindSlot::Primary) => format!("bind:{}", action.id()),
            MenuButton::RebindSlot(action, BindSlot::Secondary) => format!("bind2:{}", action.id()),
            MenuButton::RebindGamepad(action) => format!("bindpad:{}", action.id()),
            MenuButton::ClearGamepad(action) => format!("bindpad:{}:clear", action.id()),
            MenuButton::ResetBindings => "bind:reset".into(),
            MenuButton::StanceMode(kind, mode) => format!("stance:{}:{}", kind.id(), mode.label().to_lowercase()),
            MenuButton::Preset(preset) => format!("preset:{}", preset.label().to_lowercase()),
            MenuButton::ShadowQuality(q) => format!("shadowquality:{}", q.label().to_lowercase()),
            MenuButton::AntiAliasing(aa) => format!("aa:{}", format!("{aa:?}").to_lowercase()),
            MenuButton::SsaoQuality(q) => format!("ssaoquality:{}", q.label().to_lowercase()),
            MenuButton::Anisotropy(a) => format!("anisotropy:{}", a.label().to_lowercase()),
            MenuButton::ParticleQuality(q) => format!("particlequality:{}", q.label().to_lowercase()),
            MenuButton::CrosshairStyle(style) => format!("crosshairstyle:{}", style.label().to_lowercase()),
            MenuButton::FrameCap(fps) => format!("framecap:{fps}"),
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

/// Which stance (or sprint) a [`MenuButton::StanceMode`] sets the hold/toggle mode of.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum StanceKind {
    Prone,
    Crouch,
    Sprint,
}

impl StanceKind {
    fn id(self) -> &'static str {
        match self {
            StanceKind::Prone => "prone",
            StanceKind::Crouch => "crouch",
            StanceKind::Sprint => "sprint",
        }
    }

    fn get(self, settings: &Settings) -> StanceMode {
        match self {
            StanceKind::Prone => settings.prone_mode,
            StanceKind::Crouch => settings.crouch_mode,
            StanceKind::Sprint => settings.sprint_mode,
        }
    }

    fn set(self, settings: &mut Settings, mode: StanceMode) {
        match self {
            StanceKind::Prone => settings.prone_mode = mode,
            StanceKind::Crouch => settings.crouch_mode = mode,
            StanceKind::Sprint => settings.sprint_mode = mode,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Toggle {
    Spectate,
    Public,
    InvertY,
    VSync,
    SkyLight,
    BakedAo,
    Bloom,
    MouseRawInput,
    ColorblindTeamColors,
    GamepadEnabled,
    GamepadInvertY,
    GamepadAimAssist,
    InvertJetPitch,
    InvertHeliPitch,
    HeliPedalsRoll,
}

impl Toggle {
    fn id(self) -> &'static str {
        match self {
            Toggle::Spectate => "spectate",
            Toggle::Public => "public",
            Toggle::InvertY => "invert_y",
            Toggle::VSync => "vsync",
            Toggle::SkyLight => "sky_light",
            Toggle::BakedAo => "baked_ao",
            Toggle::Bloom => "bloom",
            Toggle::MouseRawInput => "mouse_raw_input",
            Toggle::ColorblindTeamColors => "colorblind_team_colors",
            Toggle::GamepadEnabled => "gamepad_enabled",
            Toggle::GamepadInvertY => "gamepad_invert_y",
            Toggle::GamepadAimAssist => "gamepad_aim_assist",
            Toggle::InvertJetPitch => "invert_jet_pitch",
            Toggle::InvertHeliPitch => "invert_heli_pitch",
            Toggle::HeliPedalsRoll => "heli_pedals_roll",
        }
    }

    fn get(self, settings: &Settings) -> bool {
        match self {
            Toggle::Spectate => settings.last_match.spectate,
            Toggle::Public => settings.last_match.public,
            Toggle::InvertY => settings.invert_mouse_y,
            Toggle::VSync => settings.vsync,
            Toggle::SkyLight => settings.sky_light,
            Toggle::BakedAo => settings.baked_ao,
            Toggle::Bloom => settings.bloom,
            Toggle::MouseRawInput => settings.mouse_raw_input,
            Toggle::ColorblindTeamColors => settings.colorblind_team_colors,
            Toggle::GamepadEnabled => settings.gamepad.enabled,
            Toggle::GamepadInvertY => settings.gamepad.invert_look_y,
            Toggle::GamepadAimAssist => settings.gamepad.aim_assist,
            Toggle::InvertJetPitch => settings.invert_jet_pitch,
            Toggle::InvertHeliPitch => settings.invert_heli_pitch,
            Toggle::HeliPedalsRoll => settings.heli_pedals_roll,
        }
    }

    fn flip(self, settings: &mut Settings) {
        match self {
            Toggle::Spectate => settings.last_match.spectate ^= true,
            Toggle::Public => settings.last_match.public ^= true,
            Toggle::InvertY => settings.invert_mouse_y ^= true,
            Toggle::VSync => settings.vsync ^= true,
            Toggle::SkyLight => settings.sky_light ^= true,
            Toggle::BakedAo => settings.baked_ao ^= true,
            Toggle::Bloom => settings.bloom ^= true,
            Toggle::MouseRawInput => settings.mouse_raw_input ^= true,
            Toggle::ColorblindTeamColors => settings.colorblind_team_colors ^= true,
            Toggle::GamepadEnabled => settings.gamepad.enabled ^= true,
            Toggle::GamepadInvertY => settings.gamepad.invert_look_y ^= true,
            Toggle::GamepadAimAssist => settings.gamepad.aim_assist ^= true,
            Toggle::InvertJetPitch => settings.invert_jet_pitch ^= true,
            Toggle::InvertHeliPitch => settings.invert_heli_pitch ^= true,
            Toggle::HeliPedalsRoll => settings.heli_pedals_roll ^= true,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Slider {
    Sensitivity,
    MouseSmoothing,
    FieldOfView,
    Volume,
    EffectsVolume,
    AmbienceVolume,
    Bots,
    RenderScale,
    LodDetailScale,
    VegetationDensity,
    HudScale,
    MinimapSize,
    GamepadLookSensitivity,
    GamepadMoveDeadzone,
    GamepadLookDeadzone,
    VoiceVolume,
    VoiceGain,
    VoiceThreshold,
}

impl Slider {
    fn id(self) -> &'static str {
        match self {
            Slider::Sensitivity => "sensitivity",
            Slider::MouseSmoothing => "mouse_smoothing",
            Slider::FieldOfView => "fov",
            Slider::Volume => "volume",
            Slider::EffectsVolume => "effects_volume",
            Slider::AmbienceVolume => "ambience_volume",
            Slider::Bots => "bots",
            Slider::RenderScale => "render_scale",
            Slider::LodDetailScale => "lod_detail_scale",
            Slider::VegetationDensity => "vegetation_density",
            Slider::HudScale => "hud_scale",
            Slider::MinimapSize => "minimap_size",
            Slider::GamepadLookSensitivity => "gamepad_look_sensitivity",
            Slider::GamepadMoveDeadzone => "gamepad_move_deadzone",
            Slider::GamepadLookDeadzone => "gamepad_look_deadzone",
            Slider::VoiceVolume => "voice_volume",
            Slider::VoiceGain => "voice_gain",
            Slider::VoiceThreshold => "voice_threshold",
        }
    }

    /// Minimum, maximum, step.
    fn range(self) -> (f32, f32, f32) {
        match self {
            Slider::Sensitivity => (0.1, 4.0, 0.05),
            Slider::MouseSmoothing => (0.0, 0.9, 0.05),
            Slider::FieldOfView => (60.0, 100.0, 1.0),
            Slider::Volume | Slider::EffectsVolume | Slider::AmbienceVolume => (0.0, 1.0, 0.05),
            Slider::Bots => (0.0, 63.0, 1.0),
            Slider::RenderScale => (0.5, 1.5, 0.05),
            Slider::LodDetailScale | Slider::VegetationDensity => (0.2, 1.5, 0.05),
            Slider::HudScale | Slider::MinimapSize => (0.6, 1.6, 0.05),
            Slider::GamepadLookSensitivity => (0.2, 3.0, 0.1),
            Slider::GamepadMoveDeadzone | Slider::GamepadLookDeadzone => (0.0, 0.5, 0.02),
            Slider::VoiceVolume => (0.0, 2.0, 0.05),
            Slider::VoiceGain => (0.0, 4.0, 0.1),
            Slider::VoiceThreshold => (-70.0, -10.0, 1.0),
        }
    }

    fn get(self, settings: &Settings) -> f32 {
        match self {
            Slider::Sensitivity => settings.mouse_sensitivity,
            Slider::MouseSmoothing => settings.mouse_smoothing,
            Slider::FieldOfView => settings.field_of_view,
            Slider::Volume => settings.master_volume,
            Slider::EffectsVolume => settings.effects_volume,
            Slider::AmbienceVolume => settings.ambience_volume,
            Slider::Bots => settings.last_match.bots as f32,
            Slider::RenderScale => settings.render_scale,
            Slider::LodDetailScale => settings.lod_detail_scale,
            Slider::VegetationDensity => settings.vegetation_density,
            Slider::HudScale => settings.hud_scale,
            Slider::MinimapSize => settings.minimap_size,
            Slider::GamepadLookSensitivity => settings.gamepad.look_sensitivity,
            Slider::GamepadMoveDeadzone => settings.gamepad.move_deadzone,
            Slider::GamepadLookDeadzone => settings.gamepad.look_deadzone,
            Slider::VoiceVolume => settings.voice.volume,
            Slider::VoiceGain => settings.voice.input_gain,
            Slider::VoiceThreshold => settings.voice.activation_threshold_db,
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
            Slider::MouseSmoothing => settings.mouse_smoothing = value,
            Slider::FieldOfView => settings.field_of_view = value,
            Slider::Volume => settings.master_volume = value,
            Slider::EffectsVolume => settings.effects_volume = value,
            Slider::AmbienceVolume => settings.ambience_volume = value,
            Slider::Bots => settings.last_match.bots = value as u32,
            Slider::RenderScale => settings.render_scale = value,
            Slider::LodDetailScale => settings.lod_detail_scale = value,
            Slider::VegetationDensity => settings.vegetation_density = value,
            Slider::HudScale => settings.hud_scale = value,
            Slider::MinimapSize => settings.minimap_size = value,
            Slider::GamepadLookSensitivity => settings.gamepad.look_sensitivity = value,
            Slider::GamepadMoveDeadzone => settings.gamepad.move_deadzone = value,
            Slider::GamepadLookDeadzone => settings.gamepad.look_deadzone = value,
            Slider::VoiceVolume => settings.voice.volume = value,
            Slider::VoiceGain => settings.voice.input_gain = value,
            Slider::VoiceThreshold => settings.voice.activation_threshold_db = value,
        }
        if matches!(
            self,
            Slider::RenderScale | Slider::LodDetailScale | Slider::VegetationDensity
        ) {
            settings.graphics_preset = GraphicsPreset::Custom;
        }
    }

    fn fraction(self, settings: &Settings) -> f32 {
        let (min, max, _) = self.range();
        ((self.get(settings) - min) / (max - min)).clamp(0.0, 1.0)
    }

    fn display(self, settings: &Settings) -> String {
        let value = self.get(settings);
        match self {
            Slider::Sensitivity | Slider::GamepadLookSensitivity => format!("{value:.2}"),
            Slider::MouseSmoothing
            | Slider::RenderScale
            | Slider::LodDetailScale
            | Slider::VegetationDensity
            | Slider::HudScale
            | Slider::MinimapSize
            | Slider::GamepadMoveDeadzone
            | Slider::GamepadLookDeadzone => format!("{value:.2}"),
            Slider::FieldOfView => format!("{value:.0} deg"),
            Slider::Volume | Slider::EffectsVolume | Slider::AmbienceVolume | Slider::VoiceVolume => {
                format!("{:.0}%", value * 100.0)
            }
            Slider::VoiceGain => format!("x{value:.1}"),
            Slider::VoiceThreshold => format!("{value:.0} dB"),
            Slider::Bots => format!("{value:.0}"),
        }
    }
}

/// Text kept up to date by [`update_values`].
#[derive(Component, Clone, Copy, PartialEq)]
enum Value {
    Slider(Slider),
    Binding(Action, BindSlot),
    GamepadBinding(Action),
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
