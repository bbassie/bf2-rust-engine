//! User settings: saved as RON and applied live.
//!
//! The file is `--settings <path>` if given, else `settings.local.ron` in the working
//! directory if it exists, else `settings.ron` in the platform config directory
//! (`%APPDATA%\bf2-rust-engine` on Windows, `~/.config/bf2-rust-engine` on Linux,
//! `~/Library/Application Support/bf2-rust-engine` on macOS). Scenarios and `--screenshot`
//! use the defaults and save nothing unless `--settings` is given, so their screenshots and
//! `Key` steps don't depend on anyone's preferences. Missing fields take their defaults.
//! `--no-shadows` and `--no-ssao` override the graphics settings without changing them.
//! Scenarios change settings for their run with `Setting(key, value)` ([`Settings::set`]).

use std::{collections::BTreeMap, path::PathBuf};

use bevy::{
    audio::{GlobalVolume, Volume},
    core_pipeline::tonemapping::Tonemapping,
    post_process::bloom::Bloom,
    camera::Hdr,
    ecs::system::SystemParam,
    pbr::ScreenSpaceAmbientOcclusion,
    prelude::*,
    window::{
        MonitorSelection, PresentMode, PrimaryWindow, VideoModeSelection, WindowMode,
        WindowResolution,
    },
};
use serde::{Deserialize, Serialize};

use crate::{
    Cli,
    audio::AudioMix,
    camera::PlayerCamera,
    content::ContentDownloads,
    local_input::{BASE_SENSITIVITY, LookState},
    render::environment::Sun,
};

pub struct SettingsPlugin;

impl Plugin for SettingsPlugin {
    fn build(&self, app: &mut App) {
        app.add_systems(
            PostUpdate,
            (
                apply_look.run_if(resource_changed::<Settings>),
                apply_volume.run_if(resource_changed::<Settings>),
                apply_window
                    .run_if(resource_changed::<Settings>)
                    .before(bevy::camera::CameraUpdateSystems),
                apply_graphics,
                apply_post_processing,
                save_settings,
            ),
        );
    }
}

/// Where settings are loaded from and saved to; `None` keeps them in memory only.
#[derive(Resource, Clone, Debug)]
pub struct SettingsFile(pub Option<PathBuf>);

impl SettingsFile {
    pub fn locate(explicit: Option<PathBuf>, scripted: bool) -> Self {
        if explicit.is_some() || scripted {
            return Self(explicit);
        }
        let local = PathBuf::from("settings.local.ron");
        if local.exists() {
            return Self(Some(local));
        }
        Self(config_dir().map(|dir| dir.join("bf2-rust-engine").join("settings.ron")))
    }

    /// The saved settings, or the defaults.
    pub fn load(&self) -> Settings {
        let Some(path) = self.0.as_ref().filter(|p| p.exists()) else {
            return Settings::default();
        };
        match game_data::read_ron::<Settings>(path) {
            Ok(mut settings) => {
                settings.fill_missing_bindings();
                settings
            }
            Err(err) => {
                warn!("{err}; using default settings");
                Settings::default()
            }
        }
    }
}

pub(crate) fn config_dir() -> Option<PathBuf> {
    let env = |name| {
        std::env::var_os(name)
            .filter(|v| !v.is_empty())
            .map(PathBuf::from)
    };
    if cfg!(windows) {
        env("APPDATA")
    } else if cfg!(target_os = "macos") {
        env("HOME").map(|home| home.join("Library/Application Support"))
    } else {
        env("XDG_CONFIG_HOME").or_else(|| env("HOME").map(|home| home.join(".config")))
    }
}

#[derive(Resource, Serialize, Deserialize, Clone, Debug, PartialEq)]
#[serde(default)]
pub struct Settings {
    pub player_name: String,
    /// Multiplier on the base mouse sensitivity.
    pub mouse_sensitivity: f32,
    pub invert_mouse_y: bool,
    /// Vertical field of view in degrees (unzoomed).
    pub field_of_view: f32,
    /// 0..1.
    pub master_volume: f32,
    /// On top of the master volume, 0..1: weapons, footsteps, voices, vehicles...
    pub effects_volume: f32,
    /// Level ambience.
    pub ambience_volume: f32,
    pub window_mode: DisplayMode,
    /// Window size in windowed mode (logical pixels). Fullscreen uses the monitor's.
    pub window_size: (u32, u32),
    pub vsync: bool,
    pub shadows: bool,
    pub ambient_occlusion: bool,
    /// Directional sky light (brighter from above than from below) instead of one uniform
    /// ambient colour.
    pub sky_light: bool,
    /// Ambient occlusion baked into the levels (BF2's lightmaps: how much sky the ground and
    /// buildings see) and measured for soldiers and vehicles: darker interiors and alleys.
    pub baked_ao: bool,
    /// Glow around bright light (renders in HDR).
    pub bloom: bool,
    /// How the rendered light is mapped to screen colours.
    pub tone_mapping: ToneMapping,
    /// How far the world fades into the fog.
    pub view_distance: ViewDistance,
    pub bindings: BTreeMap<Action, Binding>,
    /// What the menu last started, to offer it again.
    pub last_match: LastMatch,
    /// Servers starred in the server browser.
    pub favourite_servers: Vec<SavedServer>,
    /// Servers joined lately, newest first.
    pub recent_servers: Vec<SavedServer>,
    /// Master server the browser asks for servers (`host[:port]`); none by default.
    pub master_server: Option<String>,
    /// Whether to download a server's content (its mods, and its BF2 assets if it shares
    /// them) when joining: ask, always or never (see `content`).
    pub content_downloads: ContentDownloads,
    /// Size limit of the content cache in GB; the files used longest ago go first.
    pub content_cache_gb: u32,
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            player_name: "Player".into(),
            mouse_sensitivity: 1.0,
            invert_mouse_y: false,
            field_of_view: 75.0,
            master_volume: 1.0,
            effects_volume: 1.0,
            ambience_volume: 1.0,
            window_mode: DisplayMode::Windowed,
            window_size: (1280, 720),
            vsync: false,
            shadows: true,
            ambient_occlusion: true,
            sky_light: true,
            baked_ao: true,
            bloom: false,
            tone_mapping: ToneMapping::default(),
            view_distance: ViewDistance::default(),
            bindings: Action::ALL
                .iter()
                .map(|a| (*a, a.default_binding()))
                .collect(),
            last_match: LastMatch::default(),
            favourite_servers: Vec::new(),
            recent_servers: Vec::new(),
            master_server: None,
            content_downloads: ContentDownloads::default(),
            content_cache_gb: 10,
        }
    }
}

impl Settings {
    pub fn binding(&self, action: Action) -> Binding {
        self.bindings
            .get(&action)
            .copied()
            .unwrap_or(action.default_binding())
    }

    /// Binds `action`, giving its old binding to whichever action had `binding`.
    pub fn rebind(&mut self, action: Action, binding: Binding) {
        let old = self.binding(action);
        if let Some(other) = Action::ALL
            .iter()
            .find(|a| **a != action && self.binding(**a) == binding)
        {
            self.bindings.insert(*other, old);
        }
        self.bindings.insert(action, binding);
    }

    fn fill_missing_bindings(&mut self) {
        for action in Action::ALL {
            self.bindings
                .entry(action)
                .or_insert(action.default_binding());
        }
    }

    /// Sun shadows, unless `--no-shadows`.
    pub fn shadows_on(&self, cli: &Cli) -> bool {
        self.shadows && !cli.no_shadows
    }

    /// Ambient occlusion, unless `--no-ssao`.
    pub fn ssao_on(&self, cli: &Cli) -> bool {
        self.ambient_occlusion && !cli.no_ssao
    }

    /// Sets a graphics setting by name (for scenarios): `shadows`, `ssao`, `sky_light`,
    /// `baked_ao`, `bloom` (`on`/`off`) or `tone_mapping` (see [`ToneMapping::parse`]).
    pub fn set(&mut self, key: &str, value: &str) -> Result<(), String> {
        let on = || match value.to_ascii_lowercase().as_str() {
            "on" | "true" | "1" | "yes" => Ok(true),
            "off" | "false" | "0" | "no" => Ok(false),
            _ => Err(format!("setting {key}: expected on or off, got {value}")),
        };
        match key {
            "shadows" => self.shadows = on()?,
            "ssao" | "ambient_occlusion" => self.ambient_occlusion = on()?,
            "sky_light" => self.sky_light = on()?,
            "baked_ao" => self.baked_ao = on()?,
            "bloom" => self.bloom = on()?,
            "tone_mapping" => {
                self.tone_mapping = ToneMapping::parse(value).ok_or_else(|| format!("unknown tone mapping {value}"))?
            }
            _ => return Err(format!("unknown setting {key}")),
        }
        Ok(())
    }

    /// The primary window as configured.
    pub fn window(&self) -> Window {
        let (width, height) = self.window_size;
        Window {
            title: "bf2-rust-engine".into(),
            mode: self.window_mode.to_window_mode(MonitorSelection::Primary),
            resolution: WindowResolution::new(width.max(640), height.max(360)),
            present_mode: self.present_mode(),
            ..default()
        }
    }

    fn present_mode(&self) -> PresentMode {
        if self.vsync {
            PresentMode::AutoVsync
        } else {
            PresentMode::AutoNoVsync
        }
    }
}

#[derive(Serialize, Deserialize, Clone, Copy, Debug, PartialEq, Eq, Default)]
pub enum DisplayMode {
    #[default]
    Windowed,
    Borderless,
    Fullscreen,
}

impl DisplayMode {
    pub const ALL: [DisplayMode; 3] = [
        DisplayMode::Windowed,
        DisplayMode::Borderless,
        DisplayMode::Fullscreen,
    ];

    pub fn label(self) -> &'static str {
        match self {
            DisplayMode::Windowed => "Windowed",
            DisplayMode::Borderless => "Borderless",
            DisplayMode::Fullscreen => "Fullscreen",
        }
    }

    fn to_window_mode(self, monitor: MonitorSelection) -> WindowMode {
        match self {
            DisplayMode::Windowed => WindowMode::Windowed,
            DisplayMode::Borderless => WindowMode::BorderlessFullscreen(monitor),
            DisplayMode::Fullscreen => WindowMode::Fullscreen(monitor, VideoModeSelection::Current),
        }
    }
}

/// How rendered light (which can be brighter than white) becomes screen colours.
#[derive(Serialize, Deserialize, Clone, Copy, Debug, PartialEq, Eq, Default)]
pub enum ToneMapping {
    /// Bevy's default: rolls off highlights softly and desaturates very bright colours.
    #[default]
    TonyMcMapface,
    /// Film-like: lower contrast, strong desaturation of highlights.
    AgX,
    /// Closest to BF2, which clamped: colours stay as lit up to the highlights, which are
    /// compressed without hue shifts (Khronos PBR Neutral).
    Neutral,
}

impl ToneMapping {
    pub const ALL: [ToneMapping; 3] = [ToneMapping::TonyMcMapface, ToneMapping::AgX, ToneMapping::Neutral];

    pub fn label(self) -> &'static str {
        match self {
            ToneMapping::TonyMcMapface => "Tony McMapface",
            ToneMapping::AgX => "AgX",
            ToneMapping::Neutral => "Neutral",
        }
    }

    /// `tony`/`tonymcmapface`, `agx` or `neutral`.
    pub fn parse(name: &str) -> Option<Self> {
        match name.to_ascii_lowercase().replace(['_', '-', ' '], "").as_str() {
            "tony" | "tonymcmapface" => Some(ToneMapping::TonyMcMapface),
            "agx" => Some(ToneMapping::AgX),
            "neutral" | "pbrneutral" | "khronospbrneutral" => Some(ToneMapping::Neutral),
            _ => None,
        }
    }

    pub fn tonemapping(self) -> Tonemapping {
        match self {
            ToneMapping::TonyMcMapface => Tonemapping::TonyMcMapface,
            ToneMapping::AgX => Tonemapping::AgX,
            ToneMapping::Neutral => Tonemapping::KhronosPbrNeutral,
        }
    }
}

/// Subtle bloom: bright light (sunlit glass, fire, muzzle flashes, lamps at night) glows a
/// little, the rest of the image is untouched.
fn bloom() -> Bloom {
    Bloom {
        intensity: 0.08,
        ..Bloom::NATURAL
    }
}

/// Tone mapping and bloom on the 3D cameras. The player's camera and the view model's draw
/// into one image: without bloom each tone maps its own pixels (in the shader); with bloom
/// both render in HDR and only the last one (the view model's) blooms and tone maps the
/// whole image.
#[allow(clippy::type_complexity)]
fn apply_post_processing(
    mut commands: Commands,
    settings: Res<Settings>,
    cameras: Query<(Entity, &Camera, Option<&Tonemapping>, Has<Bloom>), With<Camera3d>>,
    added: Query<(), Added<Camera3d>>,
) {
    if !settings.is_changed() && added.is_empty() {
        return;
    }
    let last = cameras.iter().max_by_key(|(_, camera, ..)| camera.order).map(|(e, ..)| e);
    let wanted = settings.tone_mapping.tonemapping();
    for (entity, _, tonemapping, has_bloom) in &cameras {
        let mut camera = commands.entity(entity);
        if settings.bloom {
            let is_last = Some(entity) == last;
            let tonemapping_here = if is_last { wanted } else { Tonemapping::None };
            if tonemapping != Some(&tonemapping_here) {
                camera.insert(tonemapping_here);
            }
            match (is_last, has_bloom) {
                (true, false) => {
                    camera.insert((Hdr, bloom()));
                }
                (false, _) => {
                    camera.remove::<Bloom>().insert(Hdr);
                }
                _ => {}
            }
        } else {
            if tonemapping != Some(&wanted) {
                camera.insert(wanted);
            }
            camera.remove::<(Bloom, Hdr)>();
        }
    }
}

/// View distance presets: multipliers on the level's fog distance (BF2's, stretched).
#[derive(Serialize, Deserialize, Clone, Copy, Debug, PartialEq, Eq, Default)]
pub enum ViewDistance {
    Short,
    #[default]
    Normal,
    Far,
    Extreme,
}

impl ViewDistance {
    pub const ALL: [ViewDistance; 4] = [
        ViewDistance::Short,
        ViewDistance::Normal,
        ViewDistance::Far,
        ViewDistance::Extreme,
    ];

    pub fn label(self) -> &'static str {
        match self {
            ViewDistance::Short => "Short",
            ViewDistance::Normal => "Normal",
            ViewDistance::Far => "Far",
            ViewDistance::Extreme => "Extreme",
        }
    }

    pub fn scale(self) -> f32 {
        match self {
            ViewDistance::Short => 0.6,
            ViewDistance::Normal => 1.0,
            ViewDistance::Far => 1.6,
            ViewDistance::Extreme => 2.5,
        }
    }
}

/// The menu's last choices.
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
#[serde(default)]
pub struct LastMatch {
    pub level: String,
    pub mode: String,
    pub size: u32,
    pub bots: u32,
    pub team: u8,
    pub spectate: bool,
    /// Host: let players on other machines join (otherwise only this machine can).
    pub public: bool,
    /// Server address for Join (IP or host name).
    pub address: String,
    pub port: u16,
}

impl Default for LastMatch {
    fn default() -> Self {
        Self {
            level: game_shared::level::TEST_RANGE.into(),
            mode: "gpm_cq".into(),
            size: 16,
            bots: 7,
            team: 1,
            spectate: false,
            public: true,
            address: "127.0.0.1".into(),
            port: game_shared::DEFAULT_PORT,
        }
    }
}

/// A server remembered by the browser.
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
#[serde(default)]
pub struct SavedServer {
    /// IP or host name.
    pub address: String,
    pub port: u16,
    /// Its name when we last heard from it.
    pub name: String,
}

impl Default for SavedServer {
    fn default() -> Self {
        Self {
            address: String::new(),
            port: game_shared::DEFAULT_PORT,
            name: String::new(),
        }
    }
}

impl SavedServer {
    pub fn is(&self, address: &str, port: u16) -> bool {
        self.address.eq_ignore_ascii_case(address.trim()) && self.port == port
    }
}

/// Recent servers kept.
pub const MAX_RECENT_SERVERS: usize = 8;

/// Something a key or mouse button does.
#[derive(Serialize, Deserialize, Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum Action {
    MoveForward,
    MoveBack,
    MoveLeft,
    MoveRight,
    Jump,
    Crouch,
    Prone,
    Sprint,
    Fire,
    Zoom,
    Reload,
    FireMode,
    Use,
    /// Weapon slot 1-9 (pressing again cycles weapons in the slot).
    WeaponSlot(u8),
    ThirdPerson,
    Deploy,
    Scoreboard,
    /// Full-screen map while held.
    Map,
    MinimapRotation,
    /// Type a chat message to everyone.
    ChatAll,
    ChatTeam,
    ChatSquad,
    /// Critically wounded: stop waiting for a medic and go to the deploy screen.
    GiveUp,
    /// BF2's commo rose while held: radio messages, spotting.
    CommoRose,
    /// The commander screen (toggles).
    CommanderScreen,
    /// Special Forces night vision goggles on or off.
    NightVision,
    /// Special Forces gas mask on or off.
    GasMask,
    /// Flying: the stick on the keyboard (the mouse also moves it).
    PitchUp,
    PitchDown,
    RollLeft,
    RollRight,
    /// Flying: the mouse looks around instead of moving the stick while held.
    FreeLook,
    /// In a vehicle: decoy flares or smoke grenades.
    Countermeasures,
}

impl Action {
    pub const ALL: [Action; 41] = [
        Action::MoveForward,
        Action::MoveBack,
        Action::MoveLeft,
        Action::MoveRight,
        Action::Jump,
        Action::Crouch,
        Action::Prone,
        Action::Sprint,
        Action::Fire,
        Action::Zoom,
        Action::Reload,
        Action::FireMode,
        Action::Use,
        Action::ThirdPerson,
        Action::Deploy,
        Action::Scoreboard,
        Action::Map,
        Action::MinimapRotation,
        Action::ChatAll,
        Action::ChatTeam,
        Action::ChatSquad,
        Action::GiveUp,
        Action::CommoRose,
        Action::CommanderScreen,
        Action::NightVision,
        Action::GasMask,
        Action::PitchUp,
        Action::PitchDown,
        Action::RollLeft,
        Action::RollRight,
        Action::FreeLook,
        Action::Countermeasures,
        Action::WeaponSlot(1),
        Action::WeaponSlot(2),
        Action::WeaponSlot(3),
        Action::WeaponSlot(4),
        Action::WeaponSlot(5),
        Action::WeaponSlot(6),
        Action::WeaponSlot(7),
        Action::WeaponSlot(8),
        Action::WeaponSlot(9),
    ];

    pub fn label(self) -> String {
        match self {
            Action::MoveForward => "Move forward".into(),
            Action::MoveBack => "Move back".into(),
            Action::MoveLeft => "Move left".into(),
            Action::MoveRight => "Move right".into(),
            Action::Jump => "Jump".into(),
            Action::Crouch => "Crouch".into(),
            Action::Prone => "Prone".into(),
            Action::Sprint => "Sprint".into(),
            Action::Fire => "Fire".into(),
            Action::Zoom => "Zoom".into(),
            Action::Reload => "Reload".into(),
            Action::FireMode => "Fire mode".into(),
            Action::Use => "Use".into(),
            Action::WeaponSlot(slot) => format!("Weapon slot {slot}"),
            Action::ThirdPerson => "Third person view".into(),
            Action::Deploy => "Deploy screen".into(),
            Action::Scoreboard => "Scoreboard".into(),
            Action::Map => "Map".into(),
            Action::MinimapRotation => "Minimap rotation".into(),
            Action::ChatAll => "Chat to everyone".into(),
            Action::ChatTeam => "Chat to team".into(),
            Action::ChatSquad => "Chat to squad".into(),
            Action::GiveUp => "Give up when wounded".into(),
            Action::CommoRose => "Commo rose (radio)".into(),
            Action::CommanderScreen => "Commander screen".into(),
            Action::NightVision => "Night vision".into(),
            Action::GasMask => "Gas mask".into(),
            Action::PitchUp => "Pitch up (flying)".into(),
            Action::PitchDown => "Pitch down (flying)".into(),
            Action::RollLeft => "Roll left (flying)".into(),
            Action::RollRight => "Roll right (flying)".into(),
            Action::FreeLook => "Free look (flying)".into(),
            Action::Countermeasures => "Countermeasures (flares, smoke)".into(),
        }
    }

    /// Short name for the settings file and UI element names.
    pub fn id(self) -> String {
        match self {
            Action::WeaponSlot(slot) => format!("slot{slot}"),
            other => format!("{other:?}").to_lowercase(),
        }
    }

    pub fn default_binding(self) -> Binding {
        use Binding::{Key, Mouse};
        match self {
            Action::MoveForward => Key(KeyCode::KeyW),
            Action::MoveBack => Key(KeyCode::KeyS),
            Action::MoveLeft => Key(KeyCode::KeyA),
            Action::MoveRight => Key(KeyCode::KeyD),
            Action::Jump => Key(KeyCode::Space),
            Action::Crouch => Key(KeyCode::ControlLeft),
            Action::Prone => Key(KeyCode::KeyZ),
            Action::Sprint => Key(KeyCode::ShiftLeft),
            Action::Fire => Mouse(MouseButton::Left),
            Action::Zoom => Mouse(MouseButton::Right),
            Action::Reload => Key(KeyCode::KeyR),
            Action::FireMode => Key(KeyCode::KeyB),
            Action::Use => Key(KeyCode::KeyE),
            Action::WeaponSlot(slot) => Key(match slot {
                1 => KeyCode::Digit1,
                2 => KeyCode::Digit2,
                3 => KeyCode::Digit3,
                4 => KeyCode::Digit4,
                5 => KeyCode::Digit5,
                6 => KeyCode::Digit6,
                7 => KeyCode::Digit7,
                8 => KeyCode::Digit8,
                _ => KeyCode::Digit9,
            }),
            Action::ThirdPerson => Key(KeyCode::KeyV),
            Action::Deploy => Key(KeyCode::Enter),
            Action::Scoreboard => Key(KeyCode::Tab),
            Action::Map => Key(KeyCode::KeyM),
            Action::MinimapRotation => Key(KeyCode::KeyN),
            Action::ChatAll => Key(KeyCode::KeyT),
            Action::ChatTeam => Key(KeyCode::KeyY),
            Action::ChatSquad => Key(KeyCode::KeyU),
            Action::GiveUp => Key(KeyCode::KeyX),
            Action::CommoRose => Key(KeyCode::KeyQ),
            Action::CommanderScreen => Key(KeyCode::CapsLock),
            Action::NightVision => Key(KeyCode::KeyL),
            Action::GasMask => Key(KeyCode::KeyK),
            Action::PitchUp => Key(KeyCode::ArrowDown),
            Action::PitchDown => Key(KeyCode::ArrowUp),
            Action::RollLeft => Key(KeyCode::ArrowLeft),
            Action::RollRight => Key(KeyCode::ArrowRight),
            Action::FreeLook => Key(KeyCode::AltLeft),
            Action::Countermeasures => Key(KeyCode::KeyG),
        }
    }
}

/// A key or a mouse button.
#[derive(Serialize, Deserialize, Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Binding {
    Key(KeyCode),
    Mouse(MouseButton),
}

impl Binding {
    /// `W`, `Shift`, `Mouse 1`, ...
    pub fn label(self) -> String {
        match self {
            Binding::Mouse(button) => match button {
                MouseButton::Left => "Mouse 1".into(),
                MouseButton::Right => "Mouse 2".into(),
                MouseButton::Middle => "Mouse 3".into(),
                MouseButton::Back => "Mouse 4".into(),
                MouseButton::Forward => "Mouse 5".into(),
                MouseButton::Other(n) => format!("Mouse {n}"),
            },
            Binding::Key(key) => match key {
                KeyCode::ShiftLeft => "Shift".into(),
                KeyCode::ControlLeft => "Ctrl".into(),
                KeyCode::AltLeft => "Alt".into(),
                KeyCode::ShiftRight => "Right Shift".into(),
                KeyCode::ControlRight => "Right Ctrl".into(),
                KeyCode::AltRight => "Right Alt".into(),
                KeyCode::Backquote => "`".into(),
                KeyCode::Minus => "-".into(),
                KeyCode::Equal => "=".into(),
                KeyCode::BracketLeft => "[".into(),
                KeyCode::BracketRight => "]".into(),
                KeyCode::Backslash => "\\".into(),
                KeyCode::Semicolon => ";".into(),
                KeyCode::Quote => "'".into(),
                KeyCode::Comma => ",".into(),
                KeyCode::Period => ".".into(),
                KeyCode::Slash => "/".into(),
                other => {
                    let name = format!("{other:?}");
                    name.strip_prefix("Key")
                        .or_else(|| name.strip_prefix("Digit"))
                        .unwrap_or(&name)
                        .to_string()
                }
            },
        }
    }
}

/// Reads input by [`Action`] through the configured bindings.
#[derive(SystemParam)]
pub struct Actions<'w> {
    keys: Res<'w, ButtonInput<KeyCode>>,
    mouse: Res<'w, ButtonInput<MouseButton>>,
    settings: Res<'w, Settings>,
}

impl Actions<'_> {
    pub fn pressed(&self, action: Action) -> bool {
        match self.settings.binding(action) {
            Binding::Key(key) => self.keys.pressed(key),
            Binding::Mouse(button) => self.mouse.pressed(button),
        }
    }

    pub fn just_pressed(&self, action: Action) -> bool {
        match self.settings.binding(action) {
            Binding::Key(key) => self.keys.just_pressed(key),
            Binding::Mouse(button) => self.mouse.just_pressed(button),
        }
    }

    /// 1 while `positive` is held, -1 while `negative` is, 0 for both or neither.
    pub fn axis(&self, positive: Action, negative: Action) -> f32 {
        self.pressed(positive) as i8 as f32 - self.pressed(negative) as i8 as f32
    }

    /// The label of an action's binding, for help texts.
    pub fn label(&self, action: Action) -> String {
        self.settings.binding(action).label()
    }
}

fn apply_look(settings: Res<Settings>, mut look: ResMut<LookState>) {
    look.sensitivity = BASE_SENSITIVITY * settings.mouse_sensitivity.clamp(0.05, 10.0);
    look.invert_y = settings.invert_mouse_y;
}

/// Master volume as Bevy's global volume, the others as the audio mix (`audio` applies both
/// to what's playing).
fn apply_volume(settings: Res<Settings>, mut global: ResMut<GlobalVolume>, mut mix: ResMut<AudioMix>) {
    global.volume = Volume::Linear(settings.master_volume.clamp(0.0, 1.0));
    mix.effects = settings.effects_volume.clamp(0.0, 1.0);
    mix.ambience = settings.ambience_volume.clamp(0.0, 1.0);
}

/// Changes only what changed in the settings, so a window resized by hand stays that size.
fn apply_window(
    settings: Res<Settings>,
    mut window: Single<&mut Window, With<PrimaryWindow>>,
    mut projections: Query<&mut Projection>,
    mut applied: Local<Option<(DisplayMode, (u32, u32), bool)>>,
) {
    let wanted = (settings.window_mode, settings.window_size, settings.vsync);
    // The window was created from the loaded settings.
    let Some(last) = applied.replace(wanted) else {
        return;
    };
    if last.0 != wanted.0 {
        window.mode = wanted.0.to_window_mode(MonitorSelection::Current);
    }
    if (last.0 != wanted.0 || last.1 != wanted.1) && wanted.0 == DisplayMode::Windowed {
        let (width, height) = wanted.1;
        window
            .resolution
            .set(width.max(640) as f32, height.max(360) as f32);
    }
    if last.0 != wanted.0 || last.1 != wanted.1 {
        // Every camera must pick up the new size this frame: cameras on one window share
        // their depth and color textures, and would otherwise disagree about their size
        // until the window's resize event (a crash in the render passes).
        for mut projection in &mut projections {
            projection.set_changed();
        }
    }
    if last.2 != wanted.2 {
        window.present_mode = settings.present_mode();
    }
}

/// Shadows on the sun and ambient occlusion on the camera, when the settings change or a
/// level adds a new sun.
fn apply_graphics(
    mut commands: Commands,
    settings: Res<Settings>,
    cli: Res<Cli>,
    mut suns: Query<(Ref<Sun>, &mut DirectionalLight)>,
    cameras: Query<(Entity, Has<ScreenSpaceAmbientOcclusion>), With<PlayerCamera>>,
) {
    let changed = settings.is_changed();
    let shadows = settings.shadows_on(&cli);
    for (sun, mut light) in &mut suns {
        if (changed || sun.is_added()) && light.shadow_maps_enabled != shadows {
            light.shadow_maps_enabled = shadows;
        }
    }
    if !changed {
        return;
    }
    let ssao = settings.ssao_on(&cli);
    for (camera, has_ssao) in &cameras {
        match (ssao, has_ssao) {
            // SSAO needs MSAA off; the view model camera smooths the final image then.
            (true, false) => {
                commands
                    .entity(camera)
                    .insert((ScreenSpaceAmbientOcclusion::default(), Msaa::Off));
            }
            (false, true) => {
                commands
                    .entity(camera)
                    .remove::<ScreenSpaceAmbientOcclusion>()
                    .insert(Msaa::default());
            }
            _ => {}
        }
    }
}

/// Writes the settings half a second after the last change.
fn save_settings(
    time: Res<Time<Real>>,
    settings: Res<Settings>,
    file: Res<SettingsFile>,
    mut dirty_since: Local<Option<f32>>,
) {
    let now = time.elapsed_secs();
    if settings.is_changed() && !settings.is_added() {
        *dirty_since = Some(now);
    }
    let Some(since) = *dirty_since else {
        return;
    };
    if now - since < 0.5 {
        return;
    }
    *dirty_since = None;
    if let Some(path) = &file.0 {
        match game_data::write_ron(path, &*settings) {
            Ok(()) => info!("saved settings to {}", path.display()),
            Err(err) => warn!("can't save settings: {err}"),
        }
    }
}
