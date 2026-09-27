//! User settings: saved as RON and applied live.
//!
//! The file is `--settings <path>` if given, else `settings.local.ron` in the working
//! directory if it exists, else `settings.ron` in the platform config directory
//! (`%APPDATA%\bf2-rust-engine` on Windows, `~/.config/bf2-rust-engine` on Linux,
//! `~/Library/Application Support/bf2-rust-engine` on macOS). Scenarios and `--screenshot`
//! use the defaults and save nothing unless `--settings` is given, so their screenshots and
//! `Key` steps don't depend on anyone's preferences. Missing fields take their defaults.
//! `--no-shadows` and `--no-ssao` override the graphics settings without changing them.

use std::{collections::BTreeMap, path::PathBuf};

use bevy::{
    audio::{AudioSinkPlayback, GlobalVolume, SpatialAudioSink, Volume},
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
    camera::PlayerCamera,
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

fn config_dir() -> Option<PathBuf> {
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
    pub window_mode: DisplayMode,
    /// Window size in windowed mode (logical pixels). Fullscreen uses the monitor's.
    pub window_size: (u32, u32),
    pub vsync: bool,
    pub shadows: bool,
    pub ambient_occlusion: bool,
    pub bindings: BTreeMap<Action, Binding>,
    /// What the menu last started, to offer it again.
    pub last_match: LastMatch,
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            player_name: "Player".into(),
            mouse_sensitivity: 1.0,
            invert_mouse_y: false,
            field_of_view: 75.0,
            master_volume: 1.0,
            window_mode: DisplayMode::Windowed,
            window_size: (1280, 720),
            vsync: false,
            shadows: true,
            ambient_occlusion: true,
            bindings: Action::ALL
                .iter()
                .map(|a| (*a, a.default_binding()))
                .collect(),
            last_match: LastMatch::default(),
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
    /// Flying: the stick on the keyboard (the mouse also moves it).
    PitchUp,
    PitchDown,
    RollLeft,
    RollRight,
    /// Flying: the mouse looks around instead of moving the stick while held.
    FreeLook,
}

impl Action {
    pub const ALL: [Action; 32] = [
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
        Action::PitchUp,
        Action::PitchDown,
        Action::RollLeft,
        Action::RollRight,
        Action::FreeLook,
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
            Action::PitchUp => "Pitch up (flying)".into(),
            Action::PitchDown => "Pitch down (flying)".into(),
            Action::RollLeft => "Roll left (flying)".into(),
            Action::RollRight => "Roll right (flying)".into(),
            Action::FreeLook => "Free look (flying)".into(),
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
            Action::PitchUp => Key(KeyCode::ArrowDown),
            Action::PitchDown => Key(KeyCode::ArrowUp),
            Action::RollLeft => Key(KeyCode::ArrowLeft),
            Action::RollRight => Key(KeyCode::ArrowRight),
            Action::FreeLook => Key(KeyCode::AltLeft),
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

/// Sets the global volume for new sounds and adjusts the ones already playing.
fn apply_volume(
    settings: Res<Settings>,
    mut global: ResMut<GlobalVolume>,
    mut sinks: Query<(&PlaybackSettings, &mut AudioSink)>,
    mut spatial_sinks: Query<(&PlaybackSettings, &mut SpatialAudioSink)>,
) {
    let volume = Volume::Linear(settings.master_volume.clamp(0.0, 1.0));
    if global.volume == volume {
        return;
    }
    global.volume = volume;
    for (playback, mut sink) in &mut sinks {
        sink.set_volume(playback.volume * volume);
    }
    for (playback, mut sink) in &mut spatial_sinks {
        sink.set_volume(playback.volume * volume);
    }
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
