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
//!
//! # Graphics presets
//!
//! [`GraphicsPreset::Low`]..[`GraphicsPreset::Ultra`] set [`ShadowQuality`], [`AntiAliasing`],
//! [`SsaoQuality`], [`Anisotropy`], render scale, LOD detail scale, vegetation density,
//! particle quality, bloom and view distance together; picking any of those individually
//! switches the preset to [`GraphicsPreset::Custom`] (`menu::input`). The default preset
//! (`High`) is tuned to match this engine's previous hardcoded defaults exactly, so a fresh
//! install looks the same as before this file grew presets.
//!
//! # Bindings
//!
//! Every [`Action`] has up to three bindings at once ([`BindingSet`]): a primary key or mouse
//! button, a secondary one, and a gamepad button. [`Actions`] (the `SystemParam` every input
//! system already used) checks all three, so every existing call site gained secondary and
//! gamepad bindings for free. Movement, look and vehicle throttle/steering/flight are analog
//! and read the gamepad sticks and triggers directly instead ([`local_input`], `vehicles::fly`).

use std::{collections::BTreeMap, path::PathBuf};

use bevy::{
    anti_alias::{
        fxaa::Fxaa,
        smaa::{Smaa, SmaaPreset},
        taa::TemporalAntiAliasing,
    },
    audio::{GlobalVolume, Volume},
    core_pipeline::tonemapping::Tonemapping,
    ecs::system::SystemParam,
    image::{ImageSampler, ImageSamplerDescriptor},
    input::gamepad::{Gamepad, GamepadButton},
    light::DirectionalLightShadowMap,
    pbr::{ScreenSpaceAmbientOcclusion, ScreenSpaceAmbientOcclusionQualityLevel},
    post_process::bloom::Bloom,
    camera::Hdr,
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
                apply_anisotropy,
                apply_frame_cap,
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
                settings.migrate();
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
    /// Exponential smoothing on the mouse delta, 0 (off) to 1 (heavy); trades latency for a
    /// steadier aim. Bevy has no OS raw-input toggle to expose, so `mouse_raw_input` is stored
    /// for the day it does; today the accumulated OS delta is already used directly.
    pub mouse_smoothing: f32,
    pub mouse_raw_input: bool,
    /// Vertical field of view in degrees (unzoomed).
    pub field_of_view: f32,
    /// 0..1.
    pub master_volume: f32,
    /// On top of the master volume, 0..1: weapons, footsteps, voices, vehicles...
    pub effects_volume: f32,
    /// Level ambience.
    pub ambience_volume: f32,
    /// Requested audio output device name; `None` uses the system default. Bevy's audio
    /// backend doesn't expose switching devices at runtime, so this is stored for a future
    /// engine hook and has no effect yet.
    pub audio_output_device: Option<String>,
    pub window_mode: DisplayMode,
    /// Window size in windowed mode (logical pixels). Fullscreen uses the monitor's.
    pub window_size: (u32, u32),
    pub vsync: bool,
    /// Caps the frame rate (frames per second); 0 is uncapped.
    pub frame_rate_cap: u32,
    /// Preset the other graphics settings were last set from; `Custom` once any of them is
    /// changed individually. See [`GraphicsPreset::apply`].
    pub graphics_preset: GraphicsPreset,
    pub shadow_quality: ShadowQuality,
    pub anti_aliasing: AntiAliasing,
    pub ssao_quality: SsaoQuality,
    pub anisotropic_filtering: Anisotropy,
    /// Multiplies the resolution the 3D scene renders at, before the HUD and menus (which
    /// stay at whatever resolution `bevy_ui` draws to the same target). 0.5..2.0.
    pub render_scale: f32,
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
    /// Multiplies the distance at which static, vehicle and soldier meshes switch to a lower
    /// LOD (`render::statics::update_lod_scales`); independent of `view_distance`, which
    /// scales draw/cull distance instead.
    pub lod_detail_scale: f32,
    /// Multiplies how far undergrowth (grass, small plants) is drawn.
    pub vegetation_density: f32,
    /// Scales the particle budget (`effects::simulate`).
    pub particle_quality: Quality,
    pub hud_scale: f32,
    /// Multiplies the minimap's on-screen size.
    pub minimap_size: f32,
    pub crosshair_style: CrosshairStyle,
    pub crosshair_color: [f32; 4],
    /// Swaps the team colours for a colour-blind friendly pair. Stored; the HUD/minimap/map
    /// colour constants (`conquest_hud::{FRIENDLY, ENEMY}` and friends) aren't parametrized on
    /// it yet (see the settings agent's final report for the exact hooks needed).
    pub colorblind_team_colors: bool,
    pub bindings: BTreeMap<Action, BindingSet>,
    pub gamepad: GamepadSettings,
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
    // --- Lighting (render::lamps, render::environment) ---
    /// Real-time lights at the level's lamps (night levels, and indoors on day levels): off,
    /// on, or on with shadows from the nearest few.
    pub dynamic_lamps: crate::render::lamps::DynamicLamps,
    /// Levels as made, or night versions of the day levels (applies when a level loads).
    pub time_of_day: crate::render::environment::TimeOfDay,
    // --- end lighting ---
    // --- Online: trusted servers, accounts (content, join, account) ---
    /// Servers whose content we agreed to download, by their identity key (see `content`).
    pub trusted_servers: Vec<crate::content::TrustedServer>,
    /// Ask before downloading from a server whose key isn't trusted yet, even with
    /// `content_downloads: Always`.
    pub confirm_new_servers: bool,
    /// Master server web address for accounts, stats and quick join
    /// (`https://master.example.com`); none by default (see `account`).
    pub master_url: Option<String>,
    // --- end online ---
    // --- Flight controls (vehicles::fly) ---
    /// Jets: pushing the mouse or the right stick forward (and the pitch-up key) pitches the
    /// nose down, like a flight stick (BF2's default), instead of up like looking around.
    pub invert_jet_pitch: bool,
    /// The same for helicopters.
    pub invert_heli_pitch: bool,
    /// The one-off updates of an older settings file that have been applied (see
    /// `migrate`); files from before it have none.
    #[serde(default)]
    pub controls_revision: u32,
    // --- end flight controls ---
}

/// The latest of [`Settings::migrate`]'s updates.
const CONTROLS_REVISION: u32 = 1;

impl Default for Settings {
    fn default() -> Self {
        Self {
            player_name: "Player".into(),
            mouse_sensitivity: 1.0,
            invert_mouse_y: false,
            mouse_smoothing: 0.0,
            mouse_raw_input: true,
            field_of_view: 75.0,
            master_volume: 1.0,
            effects_volume: 1.0,
            ambience_volume: 1.0,
            audio_output_device: None,
            window_mode: DisplayMode::Windowed,
            window_size: (1280, 720),
            vsync: false,
            frame_rate_cap: 0,
            graphics_preset: GraphicsPreset::default(),
            shadow_quality: ShadowQuality::default(),
            anti_aliasing: AntiAliasing::default(),
            ssao_quality: SsaoQuality::default(),
            anisotropic_filtering: Anisotropy::default(),
            render_scale: 1.0,
            sky_light: true,
            baked_ao: true,
            bloom: false,
            tone_mapping: ToneMapping::default(),
            view_distance: ViewDistance::default(),
            lod_detail_scale: 1.0,
            vegetation_density: 1.0,
            particle_quality: Quality::default(),
            hud_scale: 1.0,
            minimap_size: 1.0,
            crosshair_style: CrosshairStyle::default(),
            crosshair_color: [1.0, 1.0, 1.0, 0.85],
            colorblind_team_colors: false,
            bindings: Action::ALL
                .iter()
                .map(|a| (*a, a.default_bindings()))
                .collect(),
            gamepad: GamepadSettings::default(),
            last_match: LastMatch::default(),
            favourite_servers: Vec::new(),
            recent_servers: Vec::new(),
            master_server: None,
            content_downloads: ContentDownloads::default(),
            content_cache_gb: 10,
            dynamic_lamps: Default::default(),
            time_of_day: Default::default(),
            trusted_servers: Vec::new(),
            confirm_new_servers: true,
            master_url: None,
            invert_jet_pitch: false,
            invert_heli_pitch: false,
            controls_revision: CONTROLS_REVISION,
        }
    }
}

impl Settings {
    /// The bindings for `action`, falling back to its defaults for anything missing (an older
    /// save, or an action added since).
    pub fn bindings(&self, action: Action) -> BindingSet {
        let defaults = action.default_bindings();
        match self.bindings.get(&action) {
            Some(set) => *set,
            None => defaults,
        }
    }

    /// The primary binding, for places that only show one (e.g. a short help text).
    pub fn binding(&self, action: Action) -> Option<Binding> {
        self.bindings(action).primary
    }

    /// Binds `action`'s `slot`, giving its old binding to whichever action had it in the same
    /// slot (so two actions never silently share a key).
    pub fn rebind(&mut self, action: Action, slot: BindSlot, binding: Binding) {
        let old = self.bindings(action);
        if let Some(other) = Action::ALL.iter().find(|a| {
            **a != action && slot.get(&self.bindings(**a)) == Some(binding)
        }) {
            let mut set = self.bindings(*other);
            slot.set(&mut set, slot.get(&old));
            self.bindings.insert(*other, set);
        }
        let mut set = old;
        slot.set(&mut set, Some(binding));
        self.bindings.insert(action, set);
    }

    /// Binds `action`'s gamepad slot, stealing it from whichever action had it.
    pub fn rebind_gamepad(&mut self, action: Action, button: GamepadButton) {
        if let Some(other) = Action::ALL
            .iter()
            .find(|a| **a != action && self.bindings(**a).gamepad == Some(button))
        {
            let mut set = self.bindings(*other);
            set.gamepad = self.bindings(action).gamepad;
            self.bindings.insert(*other, set);
        }
        let mut set = self.bindings(action);
        set.gamepad = Some(button);
        self.bindings.insert(action, set);
    }

    /// Other actions whose binding collides with `action`'s current bindings, for the bindings
    /// page's conflict warnings. Rebinding a key, mouse button or gamepad button always steals
    /// it from the other action first (see `rebind`/`rebind_gamepad`), so conflicts only
    /// remain across primary/secondary (e.g. one action's secondary is another's primary).
    pub fn conflicts(&self, action: Action) -> Vec<Action> {
        let set = self.bindings(action);
        let keys: Vec<Binding> = [set.primary, set.secondary].into_iter().flatten().collect();
        Action::ALL
            .into_iter()
            .filter(|other| *other != action)
            .filter(|other| {
                let o = self.bindings(*other);
                let o_keys = [o.primary, o.secondary];
                keys.iter().any(|k| o_keys.contains(&Some(*k)))
                    || (set.gamepad.is_some() && set.gamepad == o.gamepad)
            })
            .collect()
    }

    /// Updates settings saved by an older version. Revision 1: the pitch keys follow the
    /// mouse (up pitches up unless `invert_*_pitch`), so the old defaults (down arrow pulls
    /// up) swap.
    fn migrate(&mut self) {
        if self.controls_revision < 1 {
            let (up, down) = (self.bindings(Action::PitchUp), self.bindings(Action::PitchDown));
            if up.primary == Some(Binding::Key(KeyCode::ArrowDown)) && down.primary == Some(Binding::Key(KeyCode::ArrowUp)) {
                self.bindings.insert(Action::PitchUp, down);
                self.bindings.insert(Action::PitchDown, up);
            }
        }
        self.controls_revision = CONTROLS_REVISION;
    }

    /// Whether pitch is inverted (flight-stick style) for this kind of aircraft.
    pub fn invert_pitch(&self, helicopter: bool) -> bool {
        if helicopter { self.invert_heli_pitch } else { self.invert_jet_pitch }
    }

    fn fill_missing_bindings(&mut self) {
        for action in Action::ALL {
            self.bindings.entry(action).or_insert(action.default_bindings());
        }
    }

    /// Sun shadows, unless `--no-shadows`.
    pub fn shadows_on(&self, cli: &Cli) -> bool {
        self.shadow_quality != ShadowQuality::Off && !cli.no_shadows
    }

    /// Ambient occlusion, unless `--no-ssao`.
    pub fn ssao_on(&self, cli: &Cli) -> bool {
        self.ssao_quality != SsaoQuality::Off && !cli.no_ssao
    }

    /// Sets a graphics or gameplay setting by name (for scenarios), e.g.
    /// `Setting("shadows", "off")`, `Setting("preset", "low")`, `Setting("render_scale",
    /// "0.75")`, `Setting("tone_mapping", "agx")`.
    pub fn set(&mut self, key: &str, value: &str) -> Result<(), String> {
        let on = || match value.to_ascii_lowercase().as_str() {
            "on" | "true" | "1" | "yes" => Ok(true),
            "off" | "false" | "0" | "no" => Ok(false),
            _ => Err(format!("setting {key}: expected on or off, got {value}")),
        };
        let num = || value.parse::<f32>().map_err(|_| format!("setting {key}: expected a number, got {value}"));
        match key {
            "shadows" => self.shadow_quality = if on()? { ShadowQuality::High } else { ShadowQuality::Off },
            "shadow_quality" => {
                self.shadow_quality = ShadowQuality::parse(value).ok_or_else(|| format!("unknown shadow quality {value}"))?
            }
            "ssao" | "ambient_occlusion" => self.ssao_quality = if on()? { SsaoQuality::High } else { SsaoQuality::Off },
            "ssao_quality" => {
                self.ssao_quality = SsaoQuality::parse(value).ok_or_else(|| format!("unknown SSAO quality {value}"))?
            }
            "anti_aliasing" | "aa" => {
                self.anti_aliasing = AntiAliasing::parse(value).ok_or_else(|| format!("unknown AA mode {value}"))?
            }
            "anisotropic_filtering" | "anisotropy" => {
                self.anisotropic_filtering =
                    Anisotropy::parse(value).ok_or_else(|| format!("unknown anisotropy {value}"))?
            }
            "preset" | "graphics_preset" => {
                GraphicsPreset::parse(value).ok_or_else(|| format!("unknown preset {value}"))?.apply(self)
            }
            "render_scale" => self.render_scale = num()?.clamp(0.5, 2.0),
            "lod_detail_scale" => self.lod_detail_scale = num()?.clamp(0.2, 2.0),
            "vegetation_density" => self.vegetation_density = num()?.clamp(0.0, 2.0),
            "particle_quality" => {
                self.particle_quality = Quality::parse(value).ok_or_else(|| format!("unknown quality {value}"))?
            }
            "frame_rate_cap" => self.frame_rate_cap = num()?.max(0.0) as u32,
            "sky_light" => self.sky_light = on()?,
            "baked_ao" => self.baked_ao = on()?,
            "bloom" => self.bloom = on()?,
            "tone_mapping" => {
                self.tone_mapping = ToneMapping::parse(value).ok_or_else(|| format!("unknown tone mapping {value}"))?
            }
            "view_distance" => {
                self.view_distance = ViewDistance::parse(value).ok_or_else(|| format!("unknown view distance {value}"))?
            }
            "hud_scale" => self.hud_scale = num()?.clamp(0.5, 1.75),
            "minimap_size" => self.minimap_size = num()?.clamp(0.5, 1.75),
            "colorblind_team_colors" => self.colorblind_team_colors = on()?,
            "gamepad_enabled" => self.gamepad.enabled = on()?,
            "invert_jet_pitch" => self.invert_jet_pitch = on()?,
            "invert_heli_pitch" => self.invert_heli_pitch = on()?,
            // Lighting: `off`, `on` or `shadows`; `level` or `night`.
            "dynamic_lamps" | "lamps" => {
                self.dynamic_lamps = crate::render::lamps::DynamicLamps::parse(value)
                    .ok_or_else(|| format!("setting {key}: expected off, on or shadows, got {value}"))?
            }
            "time_of_day" => {
                self.time_of_day = crate::render::environment::TimeOfDay::parse(value)
                    .ok_or_else(|| format!("setting {key}: expected level or night, got {value}"))?
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

    fn parse(name: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|v| v.label().eq_ignore_ascii_case(name))
    }
}

/// Shadow map cascades, resolution and range, from `Off` to `Ultra`. Applied in
/// `render::environment::shadow_cascades` (cascade count and distance) and here
/// (`DirectionalLightShadowMap`'s per-cascade texel resolution).
#[derive(Serialize, Deserialize, Clone, Copy, Debug, PartialEq, Eq, Default)]
pub enum ShadowQuality {
    Off,
    Low,
    Medium,
    #[default]
    High,
    Ultra,
}

impl ShadowQuality {
    pub const ALL: [ShadowQuality; 5] = [
        ShadowQuality::Off,
        ShadowQuality::Low,
        ShadowQuality::Medium,
        ShadowQuality::High,
        ShadowQuality::Ultra,
    ];

    pub fn label(self) -> &'static str {
        match self {
            ShadowQuality::Off => "Off",
            ShadowQuality::Low => "Low",
            ShadowQuality::Medium => "Medium",
            ShadowQuality::High => "High",
            ShadowQuality::Ultra => "Ultra",
        }
    }

    fn parse(name: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|v| v.label().eq_ignore_ascii_case(name))
    }

    /// Cascades (perspective-aliasing bands); unused when `Off`.
    pub fn cascades(self) -> usize {
        match self {
            ShadowQuality::Off | ShadowQuality::Low => 1,
            ShadowQuality::Medium => 2,
            ShadowQuality::High => 3,
            ShadowQuality::Ultra => 4,
        }
    }

    /// Multiplies the cascades' maximum distance.
    pub fn distance_scale(self) -> f32 {
        match self {
            ShadowQuality::Off => 1.0,
            ShadowQuality::Low => 0.6,
            ShadowQuality::Medium => 0.85,
            ShadowQuality::High => 1.0,
            ShadowQuality::Ultra => 1.4,
        }
    }

    /// Shadow map texel resolution (`DirectionalLightShadowMap`), a power of two.
    pub fn map_size(self) -> usize {
        // Must be a power of two (`DirectionalLightShadowMap`'s requirement).
        match self {
            ShadowQuality::Off | ShadowQuality::Low => 1024,
            ShadowQuality::Medium => 2048,
            ShadowQuality::High => 2048,
            ShadowQuality::Ultra => 4096,
        }
    }
}

/// Post-process anti-aliasing. Off leaves Bevy's default MSAA running; the others turn MSAA
/// off (SMAA and FXAA are MSAA alternatives, TAA needs it off) and add their own pass.
#[derive(Serialize, Deserialize, Clone, Copy, Debug, PartialEq, Eq, Default)]
pub enum AntiAliasing {
    #[default]
    Off,
    Fxaa,
    Smaa,
    Taa,
}

impl AntiAliasing {
    pub const ALL: [AntiAliasing; 4] = [AntiAliasing::Off, AntiAliasing::Fxaa, AntiAliasing::Smaa, AntiAliasing::Taa];

    pub fn label(self) -> &'static str {
        match self {
            AntiAliasing::Off => "Off (MSAA)",
            AntiAliasing::Fxaa => "FXAA",
            AntiAliasing::Smaa => "SMAA",
            AntiAliasing::Taa => "TAA",
        }
    }

    fn parse(name: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|v| format!("{v:?}").eq_ignore_ascii_case(name))
    }

    /// MSAA must be off for SMAA, FXAA and TAA (all incompatible with it, and TAA is a
    /// multi-frame alternative to it).
    fn needs_msaa_off(self) -> bool {
        self != AntiAliasing::Off
    }
}

/// SSAO quality, wrapping Bevy's [`ScreenSpaceAmbientOcclusionQualityLevel`]; `Off` removes the
/// component entirely (and needs MSAA on for the view model's cheaper smoothing, as before).
#[derive(Serialize, Deserialize, Clone, Copy, Debug, PartialEq, Eq, Default)]
pub enum SsaoQuality {
    Off,
    Low,
    Medium,
    #[default]
    High,
    Ultra,
}

impl SsaoQuality {
    pub const ALL: [SsaoQuality; 5] = [
        SsaoQuality::Off,
        SsaoQuality::Low,
        SsaoQuality::Medium,
        SsaoQuality::High,
        SsaoQuality::Ultra,
    ];

    pub fn label(self) -> &'static str {
        match self {
            SsaoQuality::Off => "Off",
            SsaoQuality::Low => "Low",
            SsaoQuality::Medium => "Medium",
            SsaoQuality::High => "High",
            SsaoQuality::Ultra => "Ultra",
        }
    }

    fn parse(name: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|v| v.label().eq_ignore_ascii_case(name))
    }

    fn level(self) -> Option<ScreenSpaceAmbientOcclusionQualityLevel> {
        match self {
            SsaoQuality::Off => None,
            SsaoQuality::Low => Some(ScreenSpaceAmbientOcclusionQualityLevel::Low),
            SsaoQuality::Medium => Some(ScreenSpaceAmbientOcclusionQualityLevel::Medium),
            SsaoQuality::High => Some(ScreenSpaceAmbientOcclusionQualityLevel::High),
            SsaoQuality::Ultra => Some(ScreenSpaceAmbientOcclusionQualityLevel::Ultra),
        }
    }
}

/// Anisotropic texture filtering clamp, applied to every loaded image
/// (`apply_anisotropy`): ground and walls seen at grazing angles stay sharp.
#[derive(Serialize, Deserialize, Clone, Copy, Debug, PartialEq, Eq, Default)]
pub enum Anisotropy {
    Off,
    X2,
    X4,
    X8,
    #[default]
    X16,
}

impl Anisotropy {
    pub const ALL: [Anisotropy; 5] = [
        Anisotropy::Off,
        Anisotropy::X2,
        Anisotropy::X4,
        Anisotropy::X8,
        Anisotropy::X16,
    ];

    pub fn label(self) -> &'static str {
        match self {
            Anisotropy::Off => "Off",
            Anisotropy::X2 => "2x",
            Anisotropy::X4 => "4x",
            Anisotropy::X8 => "8x",
            Anisotropy::X16 => "16x",
        }
    }

    fn parse(name: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|v| v.label().eq_ignore_ascii_case(name))
    }

    fn clamp(self) -> u16 {
        match self {
            Anisotropy::Off => 1,
            Anisotropy::X2 => 2,
            Anisotropy::X4 => 4,
            Anisotropy::X8 => 8,
            Anisotropy::X16 => 16,
        }
    }
}

/// A generic Low/Medium/High quality knob (particle budget today).
#[derive(Serialize, Deserialize, Clone, Copy, Debug, PartialEq, Eq, Default)]
pub enum Quality {
    Low,
    #[default]
    Medium,
    High,
}

impl Quality {
    pub const ALL: [Quality; 3] = [Quality::Low, Quality::Medium, Quality::High];

    pub fn label(self) -> &'static str {
        match self {
            Quality::Low => "Low",
            Quality::Medium => "Medium",
            Quality::High => "High",
        }
    }

    fn parse(name: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|v| v.label().eq_ignore_ascii_case(name))
    }

    /// Multiplier on `effects::MAX_PARTICLES`.
    pub fn scale(self) -> f32 {
        match self {
            Quality::Low => 0.35,
            Quality::Medium => 0.65,
            Quality::High => 1.0,
        }
    }
}

#[derive(Serialize, Deserialize, Clone, Copy, Debug, PartialEq, Eq, Default)]
pub enum CrosshairStyle {
    #[default]
    Cross,
    Dot,
}

impl CrosshairStyle {
    pub const ALL: [CrosshairStyle; 2] = [CrosshairStyle::Cross, CrosshairStyle::Dot];

    pub fn label(self) -> &'static str {
        match self {
            CrosshairStyle::Cross => "Cross",
            CrosshairStyle::Dot => "Dot",
        }
    }
}

/// Presets bundling [`ShadowQuality`], [`AntiAliasing`], [`SsaoQuality`], [`Anisotropy`],
/// render scale, LOD detail scale, vegetation density, particle quality, bloom and view
/// distance. `High` is tuned to be a no-op against this engine's previous hardcoded defaults.
#[derive(Serialize, Deserialize, Clone, Copy, Debug, PartialEq, Eq, Default)]
pub enum GraphicsPreset {
    Low,
    Medium,
    #[default]
    High,
    Ultra,
    /// At least one covered setting was changed individually.
    Custom,
}

impl GraphicsPreset {
    pub const ALL: [GraphicsPreset; 5] = [
        GraphicsPreset::Low,
        GraphicsPreset::Medium,
        GraphicsPreset::High,
        GraphicsPreset::Ultra,
        GraphicsPreset::Custom,
    ];

    pub fn label(self) -> &'static str {
        match self {
            GraphicsPreset::Low => "Low",
            GraphicsPreset::Medium => "Medium",
            GraphicsPreset::High => "High",
            GraphicsPreset::Ultra => "Ultra",
            GraphicsPreset::Custom => "Custom",
        }
    }

    pub fn parse(name: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|v| v.label().eq_ignore_ascii_case(name))
    }

    /// Applies the preset's values (a no-op for `Custom`).
    pub fn apply(self, s: &mut Settings) {
        let (shadow, aa, ssao, aniso, scale, lod, veg, particle, bloom, view) = match self {
            GraphicsPreset::Low => (
                ShadowQuality::Low,
                AntiAliasing::Off,
                SsaoQuality::Off,
                Anisotropy::X4,
                0.85,
                0.6,
                0.4,
                Quality::Low,
                false,
                ViewDistance::Short,
            ),
            GraphicsPreset::Medium => (
                ShadowQuality::Medium,
                AntiAliasing::Fxaa,
                SsaoQuality::Medium,
                Anisotropy::X8,
                1.0,
                0.85,
                0.7,
                Quality::Medium,
                false,
                ViewDistance::Normal,
            ),
            GraphicsPreset::High => (
                ShadowQuality::High,
                AntiAliasing::Off,
                SsaoQuality::High,
                Anisotropy::X16,
                1.0,
                1.0,
                1.0,
                Quality::High,
                false,
                ViewDistance::Normal,
            ),
            GraphicsPreset::Ultra => (
                ShadowQuality::Ultra,
                AntiAliasing::Taa,
                SsaoQuality::Ultra,
                Anisotropy::X16,
                1.0,
                1.3,
                1.3,
                Quality::High,
                true,
                ViewDistance::Far,
            ),
            GraphicsPreset::Custom => return,
        };
        s.shadow_quality = shadow;
        s.anti_aliasing = aa;
        s.ssao_quality = ssao;
        s.anisotropic_filtering = aniso;
        s.render_scale = scale;
        s.lod_detail_scale = lod;
        s.vegetation_density = veg;
        s.particle_quality = particle;
        s.bloom = bloom;
        s.view_distance = view;
        s.graphics_preset = self;
    }
}

/// Gamepad-wide tuning; per-action bindings live in [`Settings::bindings`].
#[derive(Serialize, Deserialize, Clone, Copy, Debug, PartialEq)]
#[serde(default)]
pub struct GamepadSettings {
    pub enabled: bool,
    /// Multiplier on the base look turn rate.
    pub look_sensitivity: f32,
    pub invert_look_y: bool,
    /// Radius (0..1) of the left stick's dead zone (movement).
    pub move_deadzone: f32,
    /// Radius (0..1) of the right stick's dead zone (look/flight).
    pub look_deadzone: f32,
    /// Slows the look turn rate near a target in the crosshair; off by default.
    pub aim_assist: bool,
}

impl Default for GamepadSettings {
    fn default() -> Self {
        Self {
            enabled: true,
            look_sensitivity: 1.0,
            invert_look_y: false,
            move_deadzone: 0.18,
            look_deadzone: 0.15,
            aim_assist: false,
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

/// Something a key, mouse button or gamepad button does.
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
    /// Flying: the stick on the keyboard (the mouse, or a gamepad's right stick, also moves
    /// it directly; see `vehicles::fly`). Pitch up raises the nose, like moving the mouse
    /// up, unless the invert pitch settings make both flight-stick style.
    PitchUp,
    PitchDown,
    RollLeft,
    RollRight,
    /// Flying: the mouse looks around instead of moving the stick while held.
    FreeLook,
    /// In a vehicle: decoy flares or smoke grenades.
    Countermeasures,
    /// In a vehicle: move to this seat (1-based), like F1..F8.
    Seat(u8),
}

impl Action {
    pub const ALL: [Action; 49] = [
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
        Action::Seat(1),
        Action::Seat(2),
        Action::Seat(3),
        Action::Seat(4),
        Action::Seat(5),
        Action::Seat(6),
        Action::Seat(7),
        Action::Seat(8),
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
            Action::ThirdPerson => "Third person / chase view".into(),
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
            Action::Seat(seat) => format!("Vehicle seat {seat}"),
        }
    }

    /// Short name for the settings file and UI element names.
    pub fn id(self) -> String {
        match self {
            Action::WeaponSlot(slot) => format!("slot{slot}"),
            Action::Seat(seat) => format!("seat{seat}"),
            other => format!("{other:?}").to_lowercase(),
        }
    }

    /// The default primary, secondary and gamepad bindings.
    pub fn default_bindings(self) -> BindingSet {
        use Binding::{Key, Mouse};
        use GamepadButton::{
            DPadDown, DPadLeft, DPadRight, DPadUp, East, LeftThumb, LeftTrigger, LeftTrigger2, North,
            RightThumb, RightTrigger, RightTrigger2, Select, South, Start, West, Z,
        };
        let (primary, gamepad): (Option<Binding>, Option<GamepadButton>) = match self {
            Action::MoveForward => (Some(Key(KeyCode::KeyW)), None),
            Action::MoveBack => (Some(Key(KeyCode::KeyS)), None),
            Action::MoveLeft => (Some(Key(KeyCode::KeyA)), None),
            Action::MoveRight => (Some(Key(KeyCode::KeyD)), None),
            Action::Jump => (Some(Key(KeyCode::Space)), Some(South)),
            Action::Crouch => (Some(Key(KeyCode::ControlLeft)), Some(East)),
            Action::Prone => (Some(Key(KeyCode::KeyZ)), Some(RightThumb)),
            Action::Sprint => (Some(Key(KeyCode::ShiftLeft)), Some(LeftThumb)),
            Action::Fire => (Some(Mouse(MouseButton::Left)), Some(RightTrigger2)),
            Action::Zoom => (Some(Mouse(MouseButton::Right)), Some(LeftTrigger2)),
            Action::Reload => (Some(Key(KeyCode::KeyR)), Some(West)),
            Action::FireMode => (Some(Key(KeyCode::KeyB)), Some(DPadRight)),
            Action::Use => (Some(Key(KeyCode::KeyE)), Some(RightTrigger)),
            Action::WeaponSlot(slot) => (
                Some(Key(match slot {
                    1 => KeyCode::Digit1,
                    2 => KeyCode::Digit2,
                    3 => KeyCode::Digit3,
                    4 => KeyCode::Digit4,
                    5 => KeyCode::Digit5,
                    6 => KeyCode::Digit6,
                    7 => KeyCode::Digit7,
                    8 => KeyCode::Digit8,
                    _ => KeyCode::Digit9,
                })),
                match slot {
                    2 => Some(North),
                    3 => Some(LeftTrigger),
                    _ => None,
                },
            ),
            Action::ThirdPerson => (Some(Key(KeyCode::KeyV)), Some(Z)),
            Action::Deploy => (Some(Key(KeyCode::Enter)), Some(Start)),
            Action::Scoreboard => (Some(Key(KeyCode::Tab)), Some(Select)),
            Action::Map => (Some(Key(KeyCode::KeyM)), Some(DPadDown)),
            Action::MinimapRotation => (Some(Key(KeyCode::KeyN)), None),
            Action::ChatAll => (Some(Key(KeyCode::KeyT)), None),
            Action::ChatTeam => (Some(Key(KeyCode::KeyY)), None),
            Action::ChatSquad => (Some(Key(KeyCode::KeyU)), None),
            Action::GiveUp => (Some(Key(KeyCode::KeyX)), None),
            Action::CommoRose => (Some(Key(KeyCode::KeyQ)), Some(DPadLeft)),
            Action::CommanderScreen => (Some(Key(KeyCode::CapsLock)), Some(DPadUp)),
            Action::NightVision => (Some(Key(KeyCode::KeyL)), None),
            Action::GasMask => (Some(Key(KeyCode::KeyK)), None),
            Action::PitchUp => (Some(Key(KeyCode::ArrowUp)), None),
            Action::PitchDown => (Some(Key(KeyCode::ArrowDown)), None),
            Action::RollLeft => (Some(Key(KeyCode::ArrowLeft)), None),
            Action::RollRight => (Some(Key(KeyCode::ArrowRight)), None),
            Action::FreeLook => (Some(Key(KeyCode::AltLeft)), Some(LeftTrigger)),
            Action::Countermeasures => (Some(Key(KeyCode::KeyG)), Some(GamepadButton::C)),
            Action::Seat(seat) => (
                Some(Key(match seat {
                    1 => KeyCode::F1,
                    2 => KeyCode::F2,
                    3 => KeyCode::F3,
                    4 => KeyCode::F4,
                    5 => KeyCode::F5,
                    6 => KeyCode::F6,
                    7 => KeyCode::F7,
                    _ => KeyCode::F8,
                })),
                None,
            ),
        };
        BindingSet { primary, secondary: None, gamepad }
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

/// A gamepad button's name, Xbox-style (the layout the bindings page shows glyphs/names for).
pub fn gamepad_button_label(button: GamepadButton) -> String {
    match button {
        GamepadButton::South => "A".into(),
        GamepadButton::East => "B".into(),
        GamepadButton::North => "Y".into(),
        GamepadButton::West => "X".into(),
        GamepadButton::C => "C".into(),
        GamepadButton::Z => "Z".into(),
        GamepadButton::LeftTrigger => "LB".into(),
        GamepadButton::LeftTrigger2 => "LT".into(),
        GamepadButton::RightTrigger => "RB".into(),
        GamepadButton::RightTrigger2 => "RT".into(),
        GamepadButton::Select => "Back".into(),
        GamepadButton::Start => "Start".into(),
        GamepadButton::Mode => "Guide".into(),
        GamepadButton::LeftThumb => "L3".into(),
        GamepadButton::RightThumb => "R3".into(),
        GamepadButton::DPadUp => "D-Pad Up".into(),
        GamepadButton::DPadDown => "D-Pad Down".into(),
        GamepadButton::DPadLeft => "D-Pad Left".into(),
        GamepadButton::DPadRight => "D-Pad Right".into(),
        GamepadButton::Other(n) => format!("Button {n}"),
    }
}

/// Which slot of a [`BindingSet`] to read or write.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum BindSlot {
    Primary,
    Secondary,
}

impl BindSlot {
    fn get(self, set: &BindingSet) -> Option<Binding> {
        match self {
            BindSlot::Primary => set.primary,
            BindSlot::Secondary => set.secondary,
        }
    }

    fn set(self, set: &mut BindingSet, binding: Option<Binding>) {
        match self {
            BindSlot::Primary => set.primary = binding,
            BindSlot::Secondary => set.secondary = binding,
        }
    }
}

/// An action's key/mouse and gamepad bindings.
#[derive(Serialize, Deserialize, Clone, Copy, Debug, PartialEq, Eq, Default)]
pub struct BindingSet {
    pub primary: Option<Binding>,
    pub secondary: Option<Binding>,
    pub gamepad: Option<GamepadButton>,
}

/// Reads input by [`Action`] through the configured bindings: primary and secondary key or
/// mouse binding, and a gamepad button (any connected gamepad, if `gamepad.enabled`).
#[derive(SystemParam)]
pub struct Actions<'w, 's> {
    keys: Res<'w, ButtonInput<KeyCode>>,
    mouse: Res<'w, ButtonInput<MouseButton>>,
    gamepads: Query<'w, 's, &'static Gamepad>,
    settings: Res<'w, Settings>,
}

impl Actions<'_, '_> {
    fn binding_pressed(&self, binding: Binding) -> bool {
        match binding {
            Binding::Key(key) => self.keys.pressed(key),
            Binding::Mouse(button) => self.mouse.pressed(button),
        }
    }

    fn binding_just_pressed(&self, binding: Binding) -> bool {
        match binding {
            Binding::Key(key) => self.keys.just_pressed(key),
            Binding::Mouse(button) => self.mouse.just_pressed(button),
        }
    }

    pub fn pressed(&self, action: Action) -> bool {
        let set = self.settings.bindings(action);
        set.primary.is_some_and(|b| self.binding_pressed(b))
            || set.secondary.is_some_and(|b| self.binding_pressed(b))
            || (self.settings.gamepad.enabled
                && set.gamepad.is_some_and(|g| self.gamepads.iter().any(|gp| gp.pressed(g))))
    }

    pub fn just_pressed(&self, action: Action) -> bool {
        let set = self.settings.bindings(action);
        set.primary.is_some_and(|b| self.binding_just_pressed(b))
            || set.secondary.is_some_and(|b| self.binding_just_pressed(b))
            || (self.settings.gamepad.enabled
                && set.gamepad.is_some_and(|g| self.gamepads.iter().any(|gp| gp.just_pressed(g))))
    }

    /// 1 while `positive` is held, -1 while `negative` is, 0 for both or neither.
    pub fn axis(&self, positive: Action, negative: Action) -> f32 {
        self.pressed(positive) as i8 as f32 - self.pressed(negative) as i8 as f32
    }

    /// The label of an action's primary binding, for help texts.
    pub fn label(&self, action: Action) -> String {
        self.settings
            .binding(action)
            .map(Binding::label)
            .unwrap_or_else(|| "unbound".into())
    }

    /// The connected gamepad currently being used (most active sticks and buttons), if any
    /// and gamepads are enabled. Picking "most active" rather than just the first means an
    /// idle second pad (or a scenario's synthetic one, or vice versa) doesn't shadow the one
    /// actually being moved.
    pub fn gamepad(&self) -> Option<&Gamepad> {
        if !self.settings.gamepad.enabled {
            return None;
        }
        self.gamepads.iter().max_by(|a, b| gamepad_activity(a).total_cmp(&gamepad_activity(b)))
    }

    /// The gamepad tuning settings, for systems that would otherwise need their own `Res<Settings>`
    /// just for this (Bevy systems top out at 16 parameters).
    pub fn gamepad_settings(&self) -> GamepadSettings {
        self.settings.gamepad
    }
}

/// How much a gamepad is currently being moved: both sticks' length plus the number of
/// digitally pressed buttons (triggers included). Used to pick which connected gamepad to
/// read from when more than one is present.
pub fn gamepad_activity(gamepad: &Gamepad) -> f32 {
    gamepad.left_stick().length() + gamepad.right_stick().length() + gamepad.get_pressed().count() as f32
}

fn apply_look(settings: Res<Settings>, mut look: ResMut<LookState>) {
    look.sensitivity = BASE_SENSITIVITY * settings.mouse_sensitivity.clamp(0.05, 10.0);
    look.invert_y = settings.invert_mouse_y;
    look.smoothing = settings.mouse_smoothing.clamp(0.0, 0.95);
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

/// Shadows and their quality on the sun, ambient occlusion and anti-aliasing on the camera,
/// when the settings change or a level adds a new sun.
#[allow(clippy::type_complexity)]
fn apply_graphics(
    mut commands: Commands,
    settings: Res<Settings>,
    cli: Res<Cli>,
    mut suns: Query<(Ref<Sun>, &mut DirectionalLight)>,
    cameras: Query<
        (Entity, Has<ScreenSpaceAmbientOcclusion>, Has<Fxaa>, Has<Smaa>, Has<TemporalAntiAliasing>),
        With<PlayerCamera>,
    >,
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
    commands.insert_resource(DirectionalLightShadowMap { size: settings.shadow_quality.map_size() });
    let ssao = settings.ssao_on(&cli);
    let msaa_off = ssao || settings.anti_aliasing.needs_msaa_off();
    for (camera, has_ssao, has_fxaa, has_smaa, has_taa) in &cameras {
        let mut entity = commands.entity(camera);
        match (ssao, has_ssao) {
            (true, _) => {
                entity.insert(ScreenSpaceAmbientOcclusion {
                    quality_level: settings.ssao_quality.level().unwrap_or_default(),
                    ..default()
                });
            }
            (false, true) => {
                entity.remove::<ScreenSpaceAmbientOcclusion>();
            }
            _ => {}
        }
        entity.insert(if msaa_off { Msaa::Off } else { Msaa::default() });
        match settings.anti_aliasing {
            AntiAliasing::Off => {
                entity.remove::<(Fxaa, Smaa, TemporalAntiAliasing)>();
            }
            AntiAliasing::Fxaa => {
                if !has_fxaa {
                    entity.remove::<(Smaa, TemporalAntiAliasing)>().insert(Fxaa::default());
                }
            }
            AntiAliasing::Smaa => {
                if !has_smaa {
                    entity
                        .remove::<(Fxaa, TemporalAntiAliasing)>()
                        .insert(Smaa { preset: SmaaPreset::High });
                }
            }
            AntiAliasing::Taa => {
                if !has_taa {
                    entity.remove::<(Fxaa, Smaa)>().insert(TemporalAntiAliasing::default());
                }
            }
        }
    }
}

/// Overrides every loaded (and newly loading) image's anisotropic filter clamp to match the
/// setting; `render::materials::default_sampler` still sets the loader's own default (16x, for
/// BF2 ground and wall textures seen at grazing angles), so this only has to lower it for
/// lighter presets and correct new images as they arrive.
fn apply_anisotropy(
    settings: Res<Settings>,
    mut images: ResMut<Assets<Image>>,
    mut events: MessageReader<AssetEvent<Image>>,
) {
    let mut targets: Vec<AssetId<Image>> = events
        .read()
        .filter_map(|event| match event {
            AssetEvent::Added { id } | AssetEvent::Modified { id } => Some(*id),
            _ => None,
        })
        .collect();
    if settings.is_changed() {
        targets.extend(images.ids());
    }
    if targets.is_empty() {
        return;
    }
    let clamp = settings.anisotropic_filtering.clamp();
    targets.sort_unstable();
    targets.dedup();
    for id in targets {
        let Some(mut image) = images.get_mut(id) else { continue };
        let mut descriptor = match &image.sampler {
            ImageSampler::Descriptor(d) => d.clone(),
            ImageSampler::Default => ImageSamplerDescriptor::linear(),
        };
        if descriptor.anisotropy_clamp != clamp {
            descriptor.anisotropy_clamp = clamp;
            image.sampler = ImageSampler::Descriptor(descriptor);
        }
    }
}

/// Sleeps out the rest of the frame's budget when a frame rate cap is set. A simple main-world
/// throttle (Bevy has no built-in frame limiter); not exact, but keeps a laptop from running
/// flat out for no visual benefit.
fn apply_frame_cap(settings: Res<Settings>, mut last: Local<Option<std::time::Instant>>) {
    let now = std::time::Instant::now();
    if let Some(prev) = *last
        && settings.frame_rate_cap > 0
    {
        let target = std::time::Duration::from_secs_f64(1.0 / settings.frame_rate_cap as f64);
        let elapsed = now.duration_since(prev);
        if elapsed < target {
            std::thread::sleep(target - elapsed);
        }
    }
    *last = Some(std::time::Instant::now());
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

#[cfg(test)]
mod flight_control_tests {
    use super::*;

    #[test]
    fn old_pitch_keys_swap_once() {
        // A file from before the flight-control revision: the down arrow pulled up.
        let mut settings = Settings {
            controls_revision: 0,
            ..Settings::default()
        };
        let (up, down) = (Action::PitchUp.default_bindings(), Action::PitchDown.default_bindings());
        settings.bindings.insert(Action::PitchUp, down);
        settings.bindings.insert(Action::PitchDown, up);
        settings.migrate();
        assert_eq!(settings.binding(Action::PitchUp), Some(Binding::Key(KeyCode::ArrowUp)));
        assert_eq!(settings.controls_revision, CONTROLS_REVISION);
        // Rebound by the player since: left alone.
        settings.bindings.insert(Action::PitchUp, down);
        settings.migrate();
        assert_eq!(settings.binding(Action::PitchUp), Some(Binding::Key(KeyCode::ArrowDown)));
    }
}
