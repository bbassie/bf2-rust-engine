//! First run and Settings > Game > "BF2 content": finds the player's Battlefield 2
//! installation, shows its mods, and imports it with progress.
//!
//! The game never reads BF2 files itself (CLAUDE.md): this page runs the bundled `bf2-import`
//! (next to the game's executable) in a subprocess, `list --json` to see what is installed and
//! `--progress-json level ...` to import, and shows the JSON lines it prints. The import goes
//! to the data folder's `imported/` (`game_data::paths`), BF2 mods such as AIX 2 into
//! `mods/<namespace>` as mods of their own; `--resume` makes a cancelled import continue where
//! it stopped. When it finishes the game picks the new content up without a restart (like
//! mounting a server's content, `crate::content`).
//!
//! The page opens by itself at the main menu when `imported/` has no level (or with
//! `--setup`). Buttons are named `setup:*` for scenarios (`setup:candidate:0`, `setup:check`,
//! `setup:mod:aix2`, `setup:quick`, `setup:import`, `setup:cancel`, `setup:close`,
//! `setup:open` in Settings); the path field is `field:bf2_path`.

use std::{
    collections::{BTreeSet, VecDeque},
    io::{BufRead, BufReader, Write as _},
    path::{Path, PathBuf},
    process::{Child, Stdio},
    sync::{Arc, Mutex, atomic::AtomicBool},
    time::Instant,
};

use bevy::text::TextEdit;
use game_shared::config::DataDirs;

use super::*;

pub(super) struct SetupUiPlugin;

impl Plugin for SetupUiPlugin {
    fn build(&self, app: &mut App) {
        app.init_resource::<SetupUi>()
            .add_systems(OnEnter(Screen::Menu), open_on_first_run)
            .add_systems(OnExit(Screen::Menu), |mut ui: ResMut<SetupUi>| ui.open = false)
            .add_systems(
                Update,
                (
                    press_setup_buttons,
                    read_path_field,
                    poll_setup_tasks,
                    sync_overlay,
                    build_dynamic,
                    update_progress,
                    update_settings_row,
                    paint_setup_buttons,
                )
                    .chain()
                    .after(ScenarioSystems),
            );
    }
}

/// The game's data folders (`game_data::paths`), as resolved at startup.
#[derive(Resource, Clone, Debug)]
pub struct DataLocation(pub DataDirs);

/// The importer: `$BF2_IMPORT_EXE`, else `bf2-import` next to the game's executable (or one
/// folder up, for `target/debug/deps/client.exe`).
fn importer_exe() -> Option<PathBuf> {
    if let Some(exe) = std::env::var_os("BF2_IMPORT_EXE").filter(|v| !v.is_empty()) {
        return Some(PathBuf::from(exe));
    }
    let name = format!("bf2-import{}", std::env::consts::EXE_SUFFIX);
    let exe = std::env::current_exe().ok()?;
    let dir = exe.parent()?;
    [Some(dir), dir.parent()].into_iter().flatten().map(|d| d.join(&name)).find(|p| p.is_file())
}

/// A subprocess without a console window of its own.
fn command(exe: &Path) -> std::process::Command {
    #[allow(unused_mut)]
    let mut cmd = std::process::Command::new(exe);
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        const CREATE_NO_WINDOW: u32 = 0x0800_0000;
        cmd.creation_flags(CREATE_NO_WINDOW);
    }
    cmd.stdin(Stdio::null());
    cmd
}

/// Whether a folder is a Battlefield 2 installation (has `mods/bf2`).
fn is_install(path: &Path) -> bool {
    path.join("mods").join("bf2").is_dir()
}

/// Battlefield 2 installations on this machine, with where each was found: `$BF2_DIR`, the
/// EA and BF2Hub registry keys (Windows), and the usual folders on every drive.
fn detect_installs() -> Vec<(PathBuf, String)> {
    let mut found: Vec<(PathBuf, String)> = Vec::new();
    let mut add = |path: PathBuf, source: &str| {
        let path = if path.is_file() { path.parent().map(Path::to_path_buf).unwrap_or(path) } else { path };
        if is_install(&path) {
            let key = path.to_string_lossy().trim_end_matches(['\\', '/']).to_lowercase();
            if !found.iter().any(|(p, _)| p.to_string_lossy().trim_end_matches(['\\', '/']).to_lowercase() == key) {
                found.push((path, source.to_string()));
            }
        }
    };
    if let Some(dir) = std::env::var_os("BF2_DIR").filter(|v| !v.is_empty()) {
        add(PathBuf::from(dir), "BF2_DIR");
    }
    if cfg!(windows) {
        const KEYS: [&str; 8] = [
            r"HKLM\SOFTWARE\WOW6432Node\Electronic Arts\EA Games\Battlefield 2",
            r"HKLM\SOFTWARE\Electronic Arts\EA Games\Battlefield 2",
            r"HKLM\SOFTWARE\WOW6432Node\Electronic Arts\EA Games\Battlefield 2 Special Forces",
            r"HKLM\SOFTWARE\Electronic Arts\EA Games\Battlefield 2 Special Forces",
            r"HKLM\SOFTWARE\WOW6432Node\BF2Hub Systems\BF2Hub Client",
            r"HKCU\Software\BF2Hub Systems\BF2Hub Client",
            r"HKLM\SOFTWARE\WOW6432Node\EA Games\Battlefield 2",
            r"HKCU\Software\Electronic Arts\EA Games\Battlefield 2",
        ];
        for key in KEYS {
            let Ok(output) = command(Path::new("reg")).args(["query", key]).output() else {
                continue;
            };
            for line in String::from_utf8_lossy(&output.stdout).lines() {
                // `    InstallDir    REG_SZ    C:\Program Files (x86)\EA Games\Battlefield 2`
                let Some(value) = ["REG_SZ", "REG_EXPAND_SZ"].iter().find_map(|t| line.split_once(t).map(|(_, v)| v)) else {
                    continue;
                };
                let value = value.trim().trim_matches('"');
                if !value.is_empty() {
                    add(PathBuf::from(value), if key.contains("BF2Hub") { "BF2Hub" } else { "registry" });
                }
            }
        }
        const FOLDERS: [&str; 9] = [
            r"Program Files (x86)\EA Games\Battlefield 2",
            r"Program Files\EA Games\Battlefield 2",
            r"Program Files (x86)\Battlefield 2",
            r"Program Files\Battlefield 2",
            r"EA Games\Battlefield 2",
            r"Games\Battlefield 2",
            r"Games\EA Games\Battlefield 2",
            r"Battlefield 2",
            r"Program Files (x86)\Steam\steamapps\common\Battlefield 2",
        ];
        for drive in b'C'..=b'Z' {
            let root = PathBuf::from(format!("{}:\\", drive as char));
            if !root.is_dir() {
                continue;
            }
            for folder in FOLDERS {
                add(root.join(folder), "common folder");
            }
        }
    } else if let Some(home) = std::env::var_os("HOME").map(PathBuf::from) {
        for prefix in [".wine", ".local/share/wineprefixes/bf2", "Games/battlefield-2"] {
            for folder in ["drive_c/Program Files (x86)/EA Games/Battlefield 2", "drive_c/Program Files/EA Games/Battlefield 2"] {
                add(home.join(prefix).join(folder), "Wine prefix");
            }
        }
        add(home.join("Battlefield 2"), "home folder");
    }
    found
}

/// What `bf2-import list --json` says is installed.
#[derive(Clone, Debug, Deserialize)]
struct InstallInfo {
    mods: Vec<ModEntry>,
}

#[derive(Clone, Debug, Deserialize)]
struct ModEntry {
    name: String,
    title: String,
    base: bool,
    levels: Vec<String>,
}

impl ModEntry {
    /// The game mod folder a BF2 mod imports into: `AIX2` -> `aix2`.
    fn namespace(&self) -> String {
        self.name.to_lowercase().chars().filter(|c| c.is_ascii_alphanumeric() || *c == '_').collect()
    }
}

fn scan_install(exe: &Path, root: &Path) -> Result<InstallInfo, String> {
    let output = command(exe)
        .arg("--bf2")
        .arg(root)
        .args(["list", "--json"])
        .output()
        .map_err(|err| format!("running {}: {err}", exe.display()))?;
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        let message = stderr.lines().rev().find(|l| !l.trim().is_empty()).unwrap_or("").to_string();
        return Err(if message.is_empty() { format!("bf2-import failed ({})", output.status) } else { message });
    }
    serde_json::from_slice(&output.stdout).map_err(|err| format!("bf2-import list: {err}"))
}

/// Levels the quick start imports: two well-known small-to-medium maps.
const QUICK_LEVELS: [&str; 4] = ["strike_at_karkand", "sharqi_peninsula", "gulf_of_oman", "dalian_plant"];

/// One `bf2-import` run of an import.
#[derive(Clone, Debug)]
struct ImportStep {
    label: String,
    args: Vec<std::ffi::OsString>,
    /// Levels it is expected to import (before `--resume` skips some).
    levels: usize,
}

/// How an import ended.
#[derive(Clone, Debug, PartialEq)]
enum Outcome {
    Done,
    Cancelled,
    Failed(String),
}

/// What the import's worker thread reports.
#[derive(Default)]
struct JobState {
    step: usize,
    steps: usize,
    step_label: String,
    stage: Option<String>,
    levels_total: usize,
    levels_done: usize,
    levels_ok: usize,
    current: Option<String>,
    errors: Vec<String>,
    warnings: usize,
    finished: Option<Outcome>,
}

struct ImportJob {
    state: Arc<Mutex<JobState>>,
    child: Arc<Mutex<Option<Child>>>,
    cancel: Arc<AtomicBool>,
    started: Instant,
    /// The finished import was picked up (paths and levels reloaded).
    applied: bool,
    elapsed_at_end: Option<f32>,
}

impl ImportJob {
    fn cancel(&self) {
        self.cancel.store(true, Ordering::Relaxed);
        if let Some(child) = self.child.lock().unwrap().as_mut() {
            let _ = child.kill();
        }
    }
}

impl Drop for ImportJob {
    /// Quitting the game stops the import (it resumes next time).
    fn drop(&mut self) {
        self.cancel();
    }
}

/// Something a background thread will hand over.
type Pending<T> = Arc<Mutex<Option<T>>>;

fn spawn_pending<T: Send + 'static>(work: impl FnOnce() -> T + Send + 'static) -> Pending<T> {
    let slot: Pending<T> = Arc::new(Mutex::new(None));
    let out = slot.clone();
    std::thread::spawn(move || {
        let value = work();
        *out.lock().unwrap() = Some(value);
    });
    slot
}

fn take<T>(pending: &mut Option<Pending<T>>) -> Option<T> {
    let value = pending.as_ref()?.lock().unwrap().take();
    if value.is_some() {
        *pending = None;
    }
    value
}

#[derive(Resource, Default)]
pub(super) struct SetupUi {
    open: bool,
    /// Opened because there is no content yet (the page says "Welcome").
    first_run: bool,
    /// It opened by itself once this session: coming back to the menu doesn't open it again.
    auto_opened: bool,
    /// The overlay's contents need building again.
    version: u32,
    built: Option<(Entity, u32)>,
    /// The path in the field.
    path: String,
    /// Put this into the field (a found installation was picked).
    set_field: Option<String>,
    candidates: Vec<(PathBuf, String)>,
    detect: Option<Pending<Vec<(PathBuf, String)>>>,
    detected: bool,
    scan: Option<Pending<Result<InstallInfo, String>>>,
    scanning: Option<PathBuf>,
    install: Option<(PathBuf, InstallInfo)>,
    error: Option<String>,
    /// Third-party BF2 mods to import as game mods (by BF2 folder name).
    selected: BTreeSet<String>,
    quick: bool,
    /// Import levels again even when a previous run finished them.
    reimport: bool,
    job: Option<ImportJob>,
}

impl SetupUi {
    fn open(&mut self, first_run: bool) {
        self.open = true;
        self.first_run = first_run;
        // A finished import's summary was seen: start from the choices again.
        if self.job.as_ref().is_some_and(|j| j.state.lock().unwrap().finished.is_some()) {
            self.job = None;
        }
        self.version += 1;
        if !self.detected && self.detect.is_none() {
            self.detect = Some(spawn_pending(detect_installs));
        }
    }

    fn check(&mut self, path: PathBuf) {
        self.error = None;
        self.install = None;
        let Some(exe) = importer_exe() else {
            self.error = Some(format!(
                "bf2-import{} was not found next to the game: it does the import. Reinstall the game, or set BF2_IMPORT_EXE.",
                std::env::consts::EXE_SUFFIX
            ));
            self.version += 1;
            return;
        };
        if !is_install(&path) {
            self.error = Some(format!("{} doesn't look like a Battlefield 2 folder (it has no mods\\bf2).", path.display()));
            self.version += 1;
            return;
        }
        info!("setup: checking BF2 install {}", path.display());
        let root = path.clone();
        self.scanning = Some(path);
        self.scan = Some(spawn_pending(move || scan_install(&exe, &root)));
        self.version += 1;
    }

    fn running(&self) -> bool {
        self.job.as_ref().is_some_and(|j| j.state.lock().unwrap().finished.is_none())
    }
}

fn open_on_first_run(mut ui: ResMut<SetupUi>, location: Option<Res<DataLocation>>, cli: Res<Cli>) {
    if ui.open || ui.auto_opened {
        return;
    }
    let empty = location.as_ref().is_some_and(|l| !l.0.has_imported_levels());
    if empty || cli.setup {
        info!("setup: no imported content in {}: opening the first-run page", location.map_or(String::new(), |l| l.0.imported.display().to_string()));
        ui.auto_opened = true;
        ui.open(true);
    }
}

/// What a setup button does.
#[derive(Component, Clone, Debug, PartialEq)]
enum SetupButton {
    Open,
    Candidate(usize),
    Check,
    Mod(String),
    Quick,
    Reimport,
    Import,
    Cancel,
    Close,
    /// After an import: back to choosing what to import.
    Back,
}

impl SetupButton {
    fn name(&self) -> String {
        match self {
            SetupButton::Open => "setup:open".into(),
            SetupButton::Candidate(i) => format!("setup:candidate:{i}"),
            SetupButton::Check => "setup:check".into(),
            SetupButton::Mod(name) => format!("setup:mod:{}", name.to_lowercase()),
            SetupButton::Quick => "setup:quick".into(),
            SetupButton::Reimport => "setup:reimport".into(),
            SetupButton::Import => "setup:import".into(),
            SetupButton::Cancel => "setup:cancel".into(),
            SetupButton::Close => "setup:close".into(),
            SetupButton::Back => "setup:back".into(),
        }
    }
}

/// How a setup button is drawn: `Look` plus whether it shows as on.
fn setup_button(p: &mut ChildSpawnerCommands, action: SetupButton, look: Look, label: impl Into<String>) {
    let (padding, size) = match look {
        Look::Primary => (UiRect::axes(px(28), px(11)), 18.0),
        Look::Item => (UiRect::axes(px(12), px(8)), 15.0),
        _ => (UiRect::axes(px(14), px(8)), 15.0),
    };
    p.spawn((
        Name::new(action.name()),
        action,
        look,
        Button,
        Node {
            padding,
            justify_content: if look == Look::Item { JustifyContent::Start } else { JustifyContent::Center },
            align_items: AlignItems::Center,
            border_radius: BorderRadius::all(px(6)),
            ..default()
        },
        BackgroundColor(Color::NONE),
    ))
    .with_child(text(label, size, TEXT));
}

/// A check box button with a label after it.
fn check_box(p: &mut ChildSpawnerCommands, action: SetupButton, on: bool, label: impl Into<String>, note: impl Into<String>) {
    p.spawn(Node { align_items: AlignItems::Center, column_gap: px(12), min_height: px(34), ..default() })
        .with_children(|row| {
            row.spawn((
                Name::new(action.name()),
                action,
                Look::Custom,
                Button,
                Node {
                    width: px(22),
                    height: px(22),
                    flex_shrink: 0.0,
                    justify_content: JustifyContent::Center,
                    align_items: AlignItems::Center,
                    border: UiRect::all(px(2)),
                    border_radius: BorderRadius::all(px(5)),
                    ..default()
                },
                BackgroundColor(Color::NONE),
                BorderColor::all(if on { ACCENT } else { TRACK }),
            ))
            .with_child(tick(on, 1.0));
            row.spawn(Node { flex_direction: FlexDirection::Column, ..default() }).with_children(|t| {
                t.spawn(text(label, 15.0, TEXT));
                let note = note.into();
                if !note.is_empty() {
                    t.spawn(text(note, 13.0, DIM));
                }
            });
        });
}

/// The inside of a check box: a filled square when on.
fn tick(on: bool, alpha: f32) -> impl Bundle {
    (
        Node { width: px(10), height: px(10), border_radius: BorderRadius::all(px(2)), ..default() },
        BackgroundColor(if on { ACCENT.with_alpha(alpha) } else { Color::NONE }),
        Pickable::IGNORE,
    )
}

/// The row on Settings > Game.
pub(super) fn settings_row(p: &mut ChildSpawnerCommands) {
    row(p, "BF2 content", |c| {
        setup_button(c, SetupButton::Open, Look::Plain, "Import more...");
        c.spawn((SettingsContentText, text("", 14.0, DIM)));
    });
}

#[derive(Component)]
struct SettingsContentText;

fn update_settings_row(
    location: Option<Res<DataLocation>>,
    catalog: Res<LevelCatalog>,
    mut texts: Query<&mut Text, With<SettingsContentText>>,
) {
    let Some(location) = location else { return };
    // The catalog's first entry is the built-in test range.
    let levels = catalog.levels.len().saturating_sub(1);
    let value = format!("{levels} levels; imported into {}", location.0.imported.display());
    for mut t in &mut texts {
        if t.0 != value {
            t.0 = value.clone();
        }
    }
}

/// The overlay's root and its rebuilt part.
#[derive(Component)]
struct SetupRoot;

#[derive(Component)]
struct SetupDynamic;

#[derive(Component)]
struct PathField;

/// Texts kept up to date while importing.
#[derive(Component, Clone, Copy, PartialEq)]
enum ProgressText {
    Headline,
    Detail,
    Elapsed,
    Errors,
}

#[derive(Component)]
struct ProgressFill;

fn sync_overlay(
    mut commands: Commands,
    mut ui: ResMut<SetupUi>,
    roots: Query<Entity, With<SetupRoot>>,
    location: Option<Res<DataLocation>>,
) {
    let exists = !roots.is_empty();
    if !ui.open {
        for root in &roots {
            commands.entity(root).despawn();
        }
        ui.built = None;
        return;
    }
    if exists {
        return;
    }
    let target = location.map_or(String::new(), |l| l.0.imported.display().to_string());
    let (title, intro) = if ui.first_run {
        (
            "Welcome",
            "This game plays on Battlefield 2's maps with Battlefield 2's vehicles, weapons and sounds, taken from your own copy of the game. \
             They are converted once, on this computer, into the game's own formats (about 5 GB for everything, 1.4 GB for the quick start). Nothing is uploaded.",
        )
    } else {
        ("Import BF2 content", "Import more levels or BF2 mods from your Battlefield 2 installation, or finish an import that was stopped.")
    };
    let path = ui.path.clone();
    commands
        .spawn((
            SetupRoot,
            DespawnOnExit(Screen::Menu),
            Node {
                position_type: PositionType::Absolute,
                width: percent(100),
                height: percent(100),
                justify_content: JustifyContent::Center,
                align_items: AlignItems::Center,
                ..default()
            },
            background(),
            // Above the main menu.
            GlobalZIndex(30),
            TabGroup::new(0),
        ))
        .with_children(|root| {
            root.spawn((
                Node {
                    width: percent(92),
                    max_width: px(900),
                    max_height: percent(94),
                    flex_direction: FlexDirection::Column,
                    padding: UiRect::axes(px(36), px(30)),
                    row_gap: px(6),
                    border_radius: BorderRadius::all(px(12)),
                    overflow: Overflow::clip_y(),
                    ..default()
                },
                BackgroundColor(PANEL),
            ))
            .with_children(|card| {
                card.spawn(Node { flex_direction: FlexDirection::Column, row_gap: px(6), margin: UiRect::bottom(px(8)), ..default() })
                    .with_children(|h| {
                        h.spawn(text("BF2 RUST ENGINE", 13.0, ACCENT));
                        h.spawn(text(title, 32.0, TEXT));
                        h.spawn((text(intro, 15.0, DIM), Node { max_width: px(800), ..default() }));
                        if !target.is_empty() {
                            h.spawn(text(format!("Content goes to {target}"), 13.0, DIM));
                        }
                    });
                section(card, "Battlefield 2 installation");
                card.spawn(Node { align_items: AlignItems::Center, column_gap: px(8), ..default() }).with_children(|r| {
                    text_input::text_input(r, PathField, "bf2_path", &path, 560.0, text_input::TextInputOptions {
                        max_characters: Some(260),
                        ..default()
                    });
                    setup_button(r, SetupButton::Check, Look::Plain, "Use this folder");
                });
                card.spawn((
                    SetupDynamic,
                    Node { flex_direction: FlexDirection::Column, row_gap: px(6), ..default() },
                ));
            });
        });
    ui.version += 1;
}

fn level_count_label(count: usize) -> String {
    if count == 1 { "1 level".into() } else { format!("{count} levels") }
}

fn build_dynamic(
    mut commands: Commands,
    mut ui: ResMut<SetupUi>,
    dynamic: Query<(Entity, Option<&Children>), With<SetupDynamic>>,
    location: Option<Res<DataLocation>>,
) {
    let Ok((root, children)) = dynamic.single() else { return };
    if ui.built == Some((root, ui.version)) {
        return;
    }
    ui.built = Some((root, ui.version));
    for child in children.into_iter().flatten() {
        commands.entity(*child).despawn();
    }
    let mods_dir = location.as_ref().map(|l| l.0.mods.clone());
    let running = ui.running();
    let finished = ui.job.as_ref().and_then(|j| j.state.lock().unwrap().finished.clone());
    commands.entity(root).with_children(|p| {
        // Found installations.
        if !ui.detected {
            p.spawn(text("Looking for Battlefield 2...", 14.0, DIM));
        } else if ui.candidates.is_empty() {
            p.spawn(text(
                "No Battlefield 2 installation found. Type its folder above (the one with BF2.exe and a mods folder).",
                14.0,
                DIM,
            ));
        } else if !running {
            p.spawn(Node { flex_direction: FlexDirection::Column, row_gap: px(2), ..default() }).with_children(|list| {
                for (i, (path, source)) in ui.candidates.iter().enumerate() {
                    setup_button(list, SetupButton::Candidate(i), Look::Item, format!("{}   ({source})", path.display()));
                }
            });
        }
        if let Some(error) = &ui.error {
            notice_box(p, error);
        }
        if let Some(path) = &ui.scanning {
            p.spawn(text(format!("Reading {}...", path.display()), 14.0, DIM));
        }
        let Some((root_path, install)) = &ui.install else {
            footer(p, &ui, running);
            return;
        };
        if running || finished.is_some() {
            // Just what is being imported, the progress and the buttons: fits a small window.
            let mut what: Vec<String> = install
                .mods
                .iter()
                .filter(|m| m.base || (!ui.quick && ui.selected.contains(&m.name)))
                .map(|m| if m.base { base_title(&m.name).to_string() } else { m.title.clone() })
                .collect();
            if ui.quick {
                what.push("quick start: two levels".into());
            }
            section(p, "Importing");
            p.spawn(text(format!("{} from {}", what.join(", "), root_path.display()), 14.0, DIM));
            progress_panel(p);
            footer(p, &ui, running);
            return;
        }
        section(p, "Content");
        p.spawn(text(format!("Battlefield 2 in {}", root_path.display()), 14.0, DIM));
        for entry in &install.mods {
            let levels = level_count_label(entry.levels.len());
            if entry.base {
                let what = base_title(&entry.name);
                p.spawn(Node { align_items: AlignItems::Center, column_gap: px(12), min_height: px(34), ..default() })
                    .with_children(|r| {
                        r.spawn((
                            Node {
                                width: px(22),
                                height: px(22),
                                flex_shrink: 0.0,
                                justify_content: JustifyContent::Center,
                                align_items: AlignItems::Center,
                                border: UiRect::all(px(2)),
                                border_radius: BorderRadius::all(px(5)),
                                ..default()
                            },
                            BorderColor::all(ACCENT.with_alpha(0.5)),
                            children![tick(true, 0.5)],
                        ));
                        r.spawn(Node { flex_direction: FlexDirection::Column, ..default() }).with_children(|t| {
                            t.spawn(text(format!("{what} ({})", entry.name), 15.0, TEXT));
                            t.spawn(text(format!("{levels}, base game: always imported"), 13.0, DIM));
                        });
                    });
            } else {
                let ns = entry.namespace();
                let imported = mods_dir.as_ref().is_some_and(|d| d.join(&ns).join("mod.ron").is_file());
                let note = format!(
                    "{levels}, imported as the game mod `{ns}` (levels named {ns}_*){}",
                    if imported { "; already imported" } else { "" }
                );
                check_box(p, SetupButton::Mod(entry.name.clone()), ui.selected.contains(&entry.name), format!("{} ({})", entry.title, entry.name), note);
            }
        }
        section(p, "Options");
        check_box(
            p,
            SetupButton::Quick,
            ui.quick,
            "Quick start: import two levels now",
            "About a minute. Import the rest later from Settings > Game > BF2 content.",
        );
        check_box(
            p,
            SetupButton::Reimport,
            ui.reimport,
            "Import levels again that were already imported",
            "Off: only what is missing, so a stopped import continues where it was.",
        );
        footer(p, &ui, running);
    });
}

fn base_title(name: &str) -> &'static str {
    if name.eq_ignore_ascii_case("xpack") { "Special Forces" } else { "Battlefield 2" }
}

fn progress_panel(p: &mut ChildSpawnerCommands) {
    p.spawn((
        Node {
            flex_direction: FlexDirection::Column,
            row_gap: px(6),
            padding: UiRect::all(px(14)),
            margin: UiRect::top(px(10)),
            border_radius: BorderRadius::all(px(8)),
            ..default()
        },
        BackgroundColor(CARD),
    ))
    .with_children(|panel| {
        panel.spawn((ProgressText::Headline, text("", 17.0, TEXT)));
        panel
            .spawn((
                Node { width: percent(100), height: px(8), border_radius: BorderRadius::all(px(4)), ..default() },
                BackgroundColor(TRACK),
            ))
            .with_child((
                ProgressFill,
                Node { width: percent(0), height: percent(100), border_radius: BorderRadius::all(px(4)), ..default() },
                BackgroundColor(ACCENT),
            ));
        panel.spawn((ProgressText::Detail, text("", 14.0, DIM)));
        panel.spawn((ProgressText::Elapsed, text("", 14.0, DIM)));
        panel.spawn((ProgressText::Errors, text("", 13.0, ENEMY.lighter(0.15))));
    });
}

fn footer(p: &mut ChildSpawnerCommands, ui: &SetupUi, running: bool) {
    let finished = ui.job.as_ref().and_then(|j| j.state.lock().unwrap().finished.clone());
    p.spawn(Node { column_gap: px(10), margin: UiRect::top(px(14)), align_items: AlignItems::Center, ..default() })
        .with_children(|f| {
            if running {
                setup_button(f, SetupButton::Cancel, Look::Danger, "Cancel");
                return;
            }
            match (&finished, ui.install.is_some()) {
                (Some(Outcome::Done), _) => {}
                (Some(_), true) => setup_button(f, SetupButton::Import, Look::Primary, "Resume import"),
                (None, true) => setup_button(f, SetupButton::Import, Look::Primary, "Import"),
                _ => {}
            }
            let close = match (&finished, ui.first_run) {
                (Some(Outcome::Done), _) => "Play",
                (_, true) => "Skip for now",
                _ => "Close",
            };
            setup_button(f, SetupButton::Close, if finished == Some(Outcome::Done) { Look::Primary } else { Look::Plain }, close);
            if finished.is_some() {
                setup_button(f, SetupButton::Back, Look::Plain, "Import more...");
            }
            if ui.first_run && finished.is_none() {
                f.spawn(text("Without imported content only the built-in test range is playable.", 13.0, DIM));
            }
        });
}

fn read_path_field(mut ui: ResMut<SetupUi>, mut fields: Query<&mut EditableText, With<PathField>>) {
    for mut editable in &mut fields {
        if let Some(value) = ui.set_field.take() {
            editable.editor_mut().set_text(&value);
            editable.queue_edit(TextEdit::TextEnd(false));
            ui.path = value;
            continue;
        }
        let value = editable.value().to_string();
        if ui.path != value {
            ui.path = value;
        }
    }
}

fn press_setup_buttons(
    mut ui: ResMut<SetupUi>,
    buttons: Query<(&Interaction, &SetupButton), Changed<Interaction>>,
    location: Option<Res<DataLocation>>,
) {
    let pressed: Vec<SetupButton> = buttons.iter().filter(|(i, _)| **i == Interaction::Pressed).map(|(_, b)| b.clone()).collect();
    for button in pressed {
        let running = ui.running();
        match button {
            SetupButton::Open => ui.open(false),
            SetupButton::Candidate(i) if !running => {
                if let Some((path, _)) = ui.candidates.get(i).cloned() {
                    ui.set_field = Some(path.display().to_string());
                    ui.check(path);
                }
            }
            SetupButton::Check if !running => {
                let path = PathBuf::from(ui.path.trim().trim_matches('"'));
                ui.check(path);
            }
            SetupButton::Mod(name) if !running => {
                if !ui.selected.remove(&name) {
                    ui.selected.insert(name);
                }
                ui.version += 1;
            }
            SetupButton::Quick if !running => {
                ui.quick ^= true;
                ui.version += 1;
            }
            SetupButton::Reimport if !running => {
                ui.reimport ^= true;
                ui.version += 1;
            }
            SetupButton::Import if !running => {
                if let Some(location) = &location {
                    start_import(&mut ui, &location.0);
                }
            }
            SetupButton::Cancel => {
                if let Some(job) = &ui.job {
                    info!("setup: cancelling the import");
                    job.cancel();
                }
            }
            SetupButton::Close if !running => {
                ui.open = false;
            }
            SetupButton::Back if !running => {
                ui.job = None;
                ui.version += 1;
            }
            _ => {}
        }
    }
}

/// The `bf2-import` runs for the current choices.
fn plan_import(ui: &SetupUi, dirs: &DataDirs) -> Vec<ImportStep> {
    let Some((root, install)) = &ui.install else { return Vec::new() };
    let common = |out: &Path| -> Vec<std::ffi::OsString> {
        vec!["--bf2".into(), root.into(), "--out".into(), out.into(), "--progress-json".into()]
    };
    let resume = (!ui.reimport).then(|| std::ffi::OsString::from("--resume"));
    let base: Vec<&ModEntry> = install.mods.iter().filter(|m| m.base).collect();
    let mut base_levels: Vec<String> = Vec::new();
    for level in base.iter().flat_map(|m| &m.levels) {
        if !base_levels.iter().any(|l| l.eq_ignore_ascii_case(level)) {
            base_levels.push(level.clone());
        }
    }
    let mut steps = Vec::new();
    if ui.quick {
        let mut picks: Vec<String> = QUICK_LEVELS
            .iter()
            .filter_map(|q| base_levels.iter().find(|l| l.eq_ignore_ascii_case(q)).cloned())
            .take(2)
            .collect();
        if picks.is_empty() {
            picks = base_levels.iter().take(2).cloned().collect();
        }
        let mut args = common(&dirs.imported);
        args.push("level".into());
        args.extend(resume.clone());
        args.extend(picks.iter().map(Into::into));
        steps.push(ImportStep { label: "Battlefield 2 (quick start)".into(), args, levels: picks.len() });
        return steps;
    }
    let mut args = common(&dirs.imported);
    args.extend(["level".into(), "--base-game".into()]);
    args.extend(resume.clone());
    steps.push(ImportStep { label: "Battlefield 2".into(), args, levels: base_levels.len() });
    for entry in &base {
        let mut args = common(&dirs.imported);
        args.extend(["kits".into(), "--bf2-mod".into(), entry.name.clone().into()]);
        steps.push(ImportStep { label: format!("Weapons and kits ({})", entry.name), args, levels: 0 });
    }
    for entry in install.mods.iter().filter(|m| !m.base && ui.selected.contains(&m.name)) {
        let ns = entry.namespace();
        let mut args = common(&dirs.mods.join(&ns));
        args.extend([
            "level".into(),
            "--all".into(),
            "--bf2-mod".into(),
            entry.name.clone().into(),
            "--namespace".into(),
            ns.clone().into(),
            "--mod-title".into(),
            entry.title.clone().into(),
        ]);
        args.extend(resume.clone());
        steps.push(ImportStep { label: entry.title.clone(), args, levels: entry.levels.len() });
    }
    steps
}

fn start_import(ui: &mut SetupUi, dirs: &DataDirs) {
    let Some(exe) = importer_exe() else {
        ui.error = Some("bf2-import was not found next to the game.".into());
        ui.version += 1;
        return;
    };
    let steps = plan_import(ui, dirs);
    if steps.is_empty() {
        return;
    }
    let state = Arc::new(Mutex::new(JobState {
        steps: steps.len(),
        levels_total: steps.iter().map(|s| s.levels).sum(),
        ..default()
    }));
    let child = Arc::new(Mutex::new(None));
    let cancel = Arc::new(AtomicBool::new(false));
    let log_path = dirs.root.join("import.log");
    info!(
        "setup: importing with {} in {} steps ({} levels), log in {}",
        exe.display(),
        steps.len(),
        state.lock().unwrap().levels_total,
        log_path.display()
    );
    {
        let (state, child, cancel) = (state.clone(), child.clone(), cancel.clone());
        std::thread::spawn(move || run_import(exe, steps, log_path, state, child, cancel));
    }
    ui.error = None;
    ui.job = Some(ImportJob { state, child, cancel, started: Instant::now(), applied: false, elapsed_at_end: None });
    ui.version += 1;
}

/// The worker thread: runs the steps one after the other and turns their JSON lines into
/// [`JobState`]. Their logs (standard error) go to `import.log` in the data folder.
fn run_import(
    exe: PathBuf,
    steps: Vec<ImportStep>,
    log_path: PathBuf,
    state: Arc<Mutex<JobState>>,
    child_slot: Arc<Mutex<Option<Child>>>,
    cancel: Arc<AtomicBool>,
) {
    let _ = std::fs::create_dir_all(log_path.parent().unwrap_or(Path::new(".")));
    let log = Arc::new(Mutex::new(std::fs::OpenOptions::new().create(true).append(true).open(&log_path).ok()));
    let finish = |outcome: Outcome| state.lock().unwrap().finished = Some(outcome);
    for (index, step) in steps.iter().enumerate() {
        if cancel.load(Ordering::Relaxed) {
            return finish(Outcome::Cancelled);
        }
        {
            let mut s = state.lock().unwrap();
            s.step = index;
            s.step_label = step.label.clone();
            s.stage = None;
            s.current = None;
        }
        if let Some(file) = log.lock().unwrap().as_mut() {
            let args: Vec<String> = step.args.iter().map(|a| a.to_string_lossy().into_owned()).collect();
            let _ = writeln!(file, "> bf2-import {}", args.join(" "));
        }
        let spawned = command(&exe).args(&step.args).stdout(Stdio::piped()).stderr(Stdio::piped()).spawn();
        let mut child = match spawned {
            Ok(child) => child,
            Err(err) => return finish(Outcome::Failed(format!("running {}: {err}", exe.display()))),
        };
        let stdout = child.stdout.take();
        let stderr = child.stderr.take();
        *child_slot.lock().unwrap() = Some(child);
        // A cancel between spawning and storing the child.
        if cancel.load(Ordering::Relaxed)
            && let Some(child) = child_slot.lock().unwrap().as_mut()
        {
            let _ = child.kill();
        }
        let tail: Arc<Mutex<VecDeque<String>>> = Arc::default();
        let errors_thread = stderr.map(|stderr| {
            let (log, tail) = (log.clone(), tail.clone());
            std::thread::spawn(move || {
                for line in BufReader::new(stderr).lines().map_while(Result::ok) {
                    if let Some(file) = log.lock().unwrap().as_mut() {
                        let _ = writeln!(file, "{line}");
                    }
                    let mut tail = tail.lock().unwrap();
                    tail.push_back(line);
                    if tail.len() > 20 {
                        tail.pop_front();
                    }
                }
            })
        });
        let mut planned = step.levels;
        for line in stdout.into_iter().flat_map(|out| BufReader::new(out).lines().map_while(Result::ok)) {
            let Ok(event) = serde_json::from_str::<serde_json::Value>(&line) else { continue };
            let text = |key: &str| event.get(key).and_then(|v| v.as_str()).unwrap_or_default().to_string();
            let number = |key: &str| event.get(key).and_then(|v| v.as_u64()).unwrap_or_default() as usize;
            let mut s = state.lock().unwrap();
            match event.get("event").and_then(|e| e.as_str()) {
                Some("plan") => {
                    let (total, skipped) = (number("total"), number("skipped"));
                    s.levels_total = s.levels_total + total + skipped - planned.min(s.levels_total);
                    planned = total + skipped;
                    s.levels_done += skipped;
                }
                Some("stage") => s.stage = Some(text("stage")),
                Some("level") => {
                    s.stage = None;
                    s.current = Some(text("name"));
                }
                Some("level_done") => {
                    s.levels_done += 1;
                    s.warnings += number("warnings");
                    if event.get("ok").and_then(|v| v.as_bool()) == Some(true) {
                        s.levels_ok += 1;
                    } else {
                        s.errors.push(format!("{}: {}", text("name"), text("error")));
                    }
                    s.current = None;
                }
                Some("error") => s.errors.push(text("message")),
                _ => {}
            }
        }
        let status = child_slot.lock().unwrap().take().map(|mut c| c.wait());
        if let Some(thread) = errors_thread {
            let _ = thread.join();
        }
        if cancel.load(Ordering::Relaxed) {
            return finish(Outcome::Cancelled);
        }
        match status {
            Some(Ok(status)) if status.success() => {}
            Some(Ok(status)) => {
                let last = tail.lock().unwrap().back().cloned().unwrap_or_default();
                let message = format!("{}: bf2-import stopped ({status}). {last}", step.label);
                state.lock().unwrap().errors.push(message.clone());
                return finish(Outcome::Failed(message));
            }
            Some(Err(err)) => return finish(Outcome::Failed(format!("{}: {err}", step.label))),
            None => return finish(Outcome::Cancelled),
        }
    }
    finish(Outcome::Done);
}

fn format_elapsed(seconds: f32) -> String {
    let s = seconds as u64;
    if s >= 3600 { format!("{}:{:02}:{:02}", s / 3600, s / 60 % 60, s % 60) } else { format!("{}:{:02}", s / 60, s % 60) }
}

fn poll_setup_tasks(world: &mut World) {
    let mut ui = world.resource_mut::<SetupUi>();
    if let Some(found) = take(&mut ui.detect) {
        ui.detected = true;
        for (path, source) in &found {
            info!("setup: found BF2 install {} ({source})", path.display());
        }
        if found.is_empty() {
            info!("setup: no BF2 install found");
        }
        let first = found.first().map(|(p, _)| p.clone());
        ui.candidates = found;
        ui.version += 1;
        if ui.path.trim().is_empty()
            && ui.install.is_none()
            && let Some(path) = first
        {
            ui.set_field = Some(path.display().to_string());
            ui.check(path);
        }
    }
    if let Some(result) = take(&mut ui.scan) {
        let path = ui.scanning.take().unwrap_or_default();
        match result {
            Ok(install) => {
                let summary: Vec<String> = install.mods.iter().map(|m| format!("{} ({} levels)", m.name, m.levels.len())).collect();
                info!("setup: {} has {}", path.display(), summary.join(", "));
                ui.install = Some((path, install));
            }
            Err(err) => {
                warn!("setup: {}: {err}", path.display());
                ui.error = Some(err);
            }
        }
        ui.version += 1;
    }
    // A finished import: pick the new content up.
    let Some(job) = ui.job.as_mut() else { return };
    let finished = job.state.lock().unwrap().finished.clone();
    let Some(outcome) = finished else { return };
    if job.applied {
        return;
    }
    job.applied = true;
    job.elapsed_at_end = Some(job.started.elapsed().as_secs_f32());
    let (ok, failed) = {
        let s = job.state.lock().unwrap();
        (s.levels_ok, s.errors.len())
    };
    ui.version += 1;
    match &outcome {
        Outcome::Done => info!("setup: import finished: {ok} levels imported, {failed} errors"),
        Outcome::Cancelled => info!("setup: import cancelled after {ok} levels"),
        Outcome::Failed(err) => warn!("setup: import failed: {err}"),
    }
    reload_content(world);
}

/// Like mounting a server's content: the paths (new mods), the asset layers, what was loaded
/// at startup, and the level list.
fn reload_content(world: &mut World) {
    let Some(location) = world.get_resource::<DataLocation>().cloned() else { return };
    let paths = GamePaths::from_dirs(&location.0);
    crate::mod_assets::set_layers(&paths);
    world.insert_resource(paths);
    crate::content::reload_libraries(world);
    let _ = world.run_system_cached(scan_levels);
}

fn update_progress(
    ui: Res<SetupUi>,
    mut texts: Query<(&ProgressText, &mut Text)>,
    mut fills: Query<&mut Node, With<ProgressFill>>,
) {
    let Some(job) = &ui.job else { return };
    let s = job.state.lock().unwrap();
    let fraction = if s.levels_total > 0 { s.levels_done as f32 / s.levels_total as f32 } else if s.finished.is_some() { 1.0 } else { 0.0 };
    let elapsed = job.elapsed_at_end.unwrap_or_else(|| job.started.elapsed().as_secs_f32());
    for (kind, mut t) in &mut texts {
        let value = match kind {
            ProgressText::Headline => match &s.finished {
                Some(Outcome::Done) => format!("Done: {} imported", level_count_label(s.levels_ok)),
                Some(Outcome::Cancelled) => "Stopped. \"Resume import\" continues where it stopped.".into(),
                Some(Outcome::Failed(_)) => "The import stopped with an error".into(),
                None => format!("Importing: {} of {} levels", s.levels_done, s.levels_total),
            },
            ProgressText::Detail => match (&s.finished, &s.current, &s.stage) {
                (Some(_), _, _) => format!("{} of {} levels done", s.levels_done, s.levels_total),
                (None, Some(level), _) => format!("{} ({} of {}): {}", s.step_label, s.step + 1, s.steps, level.replace('_', " ")),
                (None, None, Some(stage)) => format!("{} ({} of {}): {stage}", s.step_label, s.step + 1, s.steps),
                (None, None, None) => format!("{} ({} of {})", s.step_label, s.step + 1, s.steps),
            },
            ProgressText::Elapsed => {
                let mut v = format!("Elapsed {}", format_elapsed(elapsed));
                if s.finished.is_none() && s.levels_done > 0 && s.levels_done < s.levels_total {
                    let left = elapsed / s.levels_done as f32 * (s.levels_total - s.levels_done) as f32;
                    v.push_str(&format!(", about {} left", format_elapsed(left)));
                }
                if s.warnings > 0 {
                    v.push_str(&format!("; {} meshes could not be converted (see import.log)", s.warnings));
                }
                v
            }
            ProgressText::Errors => {
                let mut v = s.errors.iter().take(4).map(|e| e.chars().take(160).collect::<String>()).collect::<Vec<_>>().join("\n");
                if s.errors.len() > 4 {
                    v.push_str(&format!("\n... and {} more (see import.log)", s.errors.len() - 4));
                }
                v
            }
        };
        if t.0 != value {
            t.0 = value;
        }
    }
    for mut node in &mut fills {
        let width = percent(fraction.clamp(0.0, 1.0) * 100.0);
        if node.width != width {
            node.width = width;
        }
    }
}

fn paint_setup_buttons(
    ui: Res<SetupUi>,
    mut buttons: Query<(&SetupButton, &Look, &Interaction, &mut BackgroundColor)>,
) {
    let picked = |button: &SetupButton| match button {
        SetupButton::Candidate(i) => {
            ui.candidates.get(*i).zip(ui.install.as_ref()).is_some_and(|((p, _), (root, _))| p == root)
        }
        _ => false,
    };
    for (button, look, interaction, mut background) in &mut buttons {
        let hovered = *interaction != Interaction::None;
        let color = match look {
            Look::Custom => continue,
            Look::Item if picked(button) => ACCENT.with_alpha(0.3),
            Look::Item if hovered => HOVER.with_alpha(0.7),
            Look::Item => Color::NONE,
            Look::Primary if hovered => ACCENT.lighter(0.08),
            Look::Primary => ACCENT,
            Look::Danger if hovered => ENEMY.with_alpha(0.55),
            Look::Danger => ENEMY.with_alpha(0.35),
            _ if hovered => HOVER,
            _ => BUTTON,
        };
        background.set_if_neq(BackgroundColor(color));
    }
}
