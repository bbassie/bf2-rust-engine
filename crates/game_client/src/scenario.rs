//! Scripted runs for testing and screenshots without a human at the keyboard.
//!
//! `client --scenario scenarios/viewmodel.ron` loads the level once, waits until it has
//! streamed in and every shader is compiled, then runs the steps in order: place the
//! camera, hold buttons, toggle render features, take screenshots, measure frame times.
//! Screenshots and `report.txt` go to `--out` (default `target/scenarios/<name>/`).
//! `--screenshot <path>` is shorthand for "wait until ready, take one screenshot, quit".
//! With `menu: true` the client starts at the main menu, to script it with `Click` steps.
//! Scenarios use the default settings (see `settings`) unless `--settings` is given.
//!
//! ```ron
//! (
//!     level: "strike_at_karkand",
//!     bots: 0,
//!     steps: [
//!         WaitReady,
//!         Teleport((-184.0, 155.6, -80.0), 20.0, -5.0),
//!         Wait(1.0),
//!         Screenshot("hip"),
//!         Hold([Aim]),
//!         Wait(0.5),
//!         Screenshot("zoomed"),
//!         Measure("zoomed", 3.0),
//!         Quit,
//!     ],
//! )
//! ```

use std::{
    fmt::Write as _,
    path::{Path, PathBuf},
    sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    },
};

use bevy::{
    gltf::Gltf,
    input::{
        ButtonState,
        keyboard::{Key, KeyboardInput, NativeKey},
    },
    window::PrimaryWindow,
    pbr::ScreenSpaceAmbientOcclusion,
    prelude::*,
    render::{
        Render, RenderApp, RenderSystems,
        render_resource::PipelineCache,
        view::screenshot::{Screenshot, ScreenshotCaptured, save_to_disk},
    },
};
use game_shared::{
    input::Buttons,
    level::LoadedLevel,
    soldier::{Health, SoldierMotion},
};
use serde::Deserialize;

use crate::{
    Cli,
    camera::{PlayerCamera, Spectator, ThirdPerson},
    combat::WeaponSelection,
    local_input::LookState,
    menu::Screen,
    net::{ActiveMatch, LocalSoldier},
    render::environment::Sun,
};

/// A scenario file. Fields other than `steps` override the command line.
#[derive(Deserialize, Debug, Clone, Default)]
#[serde(default)]
pub struct Scenario {
    pub level: Option<String>,
    pub mode: Option<String>,
    pub bots: Option<u32>,
    pub spectate: Option<bool>,
    pub third_person: Option<bool>,
    /// Our team, 1 or 2.
    pub team: Option<u8>,
    /// Start at the main menu instead of in a match (the other fields then don't apply).
    pub menu: bool,
    pub steps: Vec<Step>,
}

#[derive(Deserialize, Debug, Clone)]
pub enum Step {
    /// Until the level is loaded, assets stopped streaming in, no shader is compiling and
    /// (unless spectating) our soldier exists; at the main menu, until assets and shaders
    /// are done. Gives up after 90 s.
    WaitReady,
    /// Real-time seconds.
    Wait(f32),
    Frames(u32),
    /// Spectator camera position and look direction (yaw, pitch in degrees).
    Camera((f32, f32, f32), f32, f32),
    /// Moves our soldier (feet position) and sets the look direction. Singleplayer and
    /// listen server only: a remote server would move it back.
    Teleport((f32, f32, f32), f32, f32),
    /// Look direction: yaw, pitch in degrees.
    Look(f32, f32),
    /// Buttons stay held until released.
    Hold(Vec<Button>),
    Release(Vec<Button>),
    /// Movement input (right, forward), each -1..1; `Move(0, 0)` stops.
    Move(f32, f32),
    /// Weapon index in the kit.
    Weapon(u8),
    ThirdPerson(bool),
    Ssao(bool),
    Shadows(bool),
    /// Presses and releases a key, e.g. `Key(Enter)`.
    Key(KeyCode),
    /// Clicks the UI button with this `Name`, e.g. `Click("kit:5")`.
    Click(String),
    /// Our soldier dies (singleplayer and listen server only).
    Kill,
    /// Saves `<out>/<name>.png` (or `name` itself if it ends in `.png`).
    Screenshot(String),
    /// Frame time statistics over this many seconds, logged and added to the report.
    Measure(String, f32),
    Log(String),
    /// Adds our soldier's movement state (and the last prediction correction when
    /// connected) to the report every frame for this many seconds.
    Trace(String, f32),
    Quit,
}

#[derive(Deserialize, Debug, Clone, Copy)]
pub enum Button {
    Jump,
    Sprint,
    Crouch,
    Prone,
    Fire,
    Aim,
    Use,
    Reload,
    FireMode,
}

impl Button {
    fn bits(self) -> Buttons {
        match self {
            Button::Jump => Buttons::JUMP,
            Button::Sprint => Buttons::SPRINT,
            Button::Crouch => Buttons::CROUCH,
            Button::Prone => Buttons::PRONE,
            Button::Fire => Buttons::FIRE,
            Button::Aim => Buttons::AIM,
            Button::Use => Buttons::USE,
            Button::Reload => Buttons::RELOAD,
            Button::FireMode => Buttons::FIRE_MODE,
        }
    }
}

/// Input the scenario holds, merged into every input frame.
#[derive(Resource, Default)]
pub struct ScenarioInput {
    pub buttons: Buttons,
    pub movement: Option<Vec2>,
}

impl Scenario {
    pub fn load(path: &Path) -> anyhow::Result<Self> {
        let text = std::fs::read_to_string(path)?;
        let options = ron::Options::default()
            .with_default_extension(ron::extensions::Extensions::IMPLICIT_SOME);
        Ok(options.from_str(&text)?)
    }

    /// Applies the scenario's overrides to the command line.
    pub fn apply(&self, cli: &mut Cli) {
        if let Some(level) = &self.level {
            cli.level = Some(level.clone());
        }
        if let Some(mode) = &self.mode {
            cli.mode = mode.clone();
        }
        if let Some(bots) = self.bots {
            cli.bots = bots;
        }
        if let Some(spectate) = self.spectate {
            cli.spectate = spectate;
        }
        if let Some(third_person) = self.third_person {
            cli.third_person = third_person;
        }
        if let Some(team) = self.team {
            cli.team = team;
        }
    }

    /// `--screenshot <path>`: one screenshot once everything is loaded.
    pub fn screenshot(path: &Path, delay: f32) -> Self {
        Self {
            steps: vec![
                Step::WaitReady,
                Step::Wait(delay),
                Step::Screenshot(path.to_string_lossy().into_owned()),
                Step::Quit,
            ],
            ..default()
        }
    }
}

pub struct ScenarioPlugin {
    pub scenario: Scenario,
    pub out: PathBuf,
}

impl Plugin for ScenarioPlugin {
    fn build(&self, app: &mut App) {
        let pipelines = WaitingPipelines::default();
        app.insert_resource(Runner {
            steps: self.scenario.steps.clone(),
            out: self.out.clone(),
            ..default()
        })
        .insert_resource(pipelines.clone())
        .init_resource::<ScenarioInput>()
        .init_resource::<Readiness>()
        .add_systems(
            Update,
            (
                (
                    note_asset_events::<Image>,
                    note_asset_events::<Mesh>,
                    note_asset_events::<Gltf>,
                    note_asset_events::<StandardMaterial>,
                    note_asset_events::<AnimationClip>,
                    note_asset_events::<Shader>,
                ),
                run_scenario.in_set(ScenarioSystems),
            )
                .chain(),
        );
        if let Some(render_app) = app.get_sub_app_mut(RenderApp) {
            render_app
                .insert_resource(pipelines)
                .add_systems(Render, count_waiting_pipelines.in_set(RenderSystems::Cleanup));
        }
    }
}

/// Runs the scenario's steps in `Update`. UI reacting to `Key`/`Click` steps runs after it.
#[derive(SystemSet, Debug, Clone, PartialEq, Eq, Hash)]
pub struct ScenarioSystems;

/// Pipelines still compiling, counted in the render world.
#[derive(Resource, Clone, Default)]
struct WaitingPipelines(Arc<AtomicUsize>);

fn count_waiting_pipelines(cache: Res<PipelineCache>, waiting: Res<WaitingPipelines>) {
    waiting.0.store(cache.waiting_pipelines().count(), Ordering::Relaxed);
}

/// When assets last changed, as a sign that loading is still going on.
#[derive(Resource, Default)]
struct Readiness {
    last_asset_event: f32,
    last_pipeline_wait: f32,
}

fn note_asset_events<A: Asset>(
    mut events: MessageReader<AssetEvent<A>>,
    time: Res<Time<Real>>,
    mut readiness: ResMut<Readiness>,
) {
    if events.read().next().is_some() {
        readiness.last_asset_event = time.elapsed_secs();
    }
}

#[derive(Resource, Default)]
struct Runner {
    steps: Vec<Step>,
    next: usize,
    out: PathBuf,
    /// Real time the current step started.
    step_started: Option<f32>,
    frames: u32,
    screenshot_pending: bool,
    samples: Vec<f32>,
    report: String,
    /// Keys pressed by the last `Key` step, released the next frame.
    released: Vec<KeyCode>,
}

/// What a player can do, for [`run_scenario`].
#[derive(bevy::ecs::system::SystemParam)]
struct PlayerControls<'w, 's> {
    input: ResMut<'w, ScenarioInput>,
    look: ResMut<'w, LookState>,
    third_person: ResMut<'w, ThirdPerson>,
    selection: ResMut<'w, WeaponSelection>,
    keyboard: MessageWriter<'w, KeyboardInput>,
    window: Single<'w, 's, Entity, With<PrimaryWindow>>,
    buttons: Query<'w, 's, (&'static Name, &'static mut Interaction)>,
}

#[derive(Clone, Copy, PartialEq)]
enum Progress {
    Done,
    Waiting,
}

#[allow(clippy::too_many_arguments, clippy::type_complexity)]
fn run_scenario(
    mut commands: Commands,
    time: Res<Time<Real>>,
    active: Res<ActiveMatch>,
    screen: Res<State<Screen>>,
    mut runner: ResMut<Runner>,
    mut readiness: ResMut<Readiness>,
    waiting_pipelines: Res<WaitingPipelines>,
    level: Option<Res<LoadedLevel>>,
    player: PlayerControls,
    mut soldier: Query<(&mut SoldierMotion, &mut Health), With<LocalSoldier>>,
    prediction: Res<crate::prediction::PredictionStats>,
    mut spectator: Query<&mut Spectator>,
    camera: Query<Entity, With<PlayerCamera>>,
    mut suns: Query<&mut DirectionalLight, With<Sun>>,
    mut exit: MessageWriter<AppExit>,
) {
    let PlayerControls {
        mut input,
        mut look,
        mut third_person,
        mut selection,
        mut keyboard,
        window,
        mut buttons,
    } = player;
    let now = time.elapsed_secs();
    // Keys go through the same input events as a real keyboard, so every system sees them.
    let mut key_event = |key_code: KeyCode, state: ButtonState| {
        keyboard.write(KeyboardInput {
            key_code,
            logical_key: Key::Unidentified(NativeKey::Unidentified),
            state,
            text: None,
            repeat: false,
            window: *window,
        });
    };
    for key in runner.released.drain(..) {
        key_event(key, ButtonState::Released);
    }
    if waiting_pipelines.0.load(Ordering::Relaxed) > 0 {
        readiness.last_pipeline_wait = now;
    }
    // Several instant steps can run in one frame; waits end the frame.
    for _ in 0..32 {
        let Some(step) = runner.steps.get(runner.next).cloned() else {
            return;
        };
        let started = *runner.step_started.get_or_insert(now);
        let elapsed = now - started;
        let progress = match &step {
            Step::WaitReady => {
                // At the main menu there is nothing to wait for but assets and shaders. A
                // match started by a `Click` just before shows up a frame later.
                runner.frames += 1;
                let in_place = match screen.get() {
                    Screen::Menu => runner.frames > 1,
                    Screen::Loading => false,
                    Screen::InGame => level.is_some() && (active.spectating() || !soldier.is_empty()),
                };
                let ready = in_place
                    && now - readiness.last_asset_event > 1.0
                    && now - readiness.last_pipeline_wait > 0.5;
                if ready || elapsed > 90.0 {
                    let note = if ready { "" } else { " (timed out)" };
                    info!("scenario: ready after {now:.1} s{note}");
                    Progress::Done
                } else {
                    Progress::Waiting
                }
            }
            Step::Wait(seconds) => done_if(elapsed >= *seconds),
            Step::Frames(frames) => {
                runner.frames += 1;
                done_if(runner.frames > *frames)
            }
            Step::Camera(position, yaw, pitch) => {
                if let Ok(mut spectator) = spectator.single_mut() {
                    spectator.position = Vec3::from(*position);
                }
                set_look(&mut look, *yaw, *pitch);
                Progress::Done
            }
            Step::Teleport(position, yaw, pitch) => {
                if let Ok((mut motion, _)) = soldier.single_mut() {
                    motion.position = Vec3::from(*position);
                    motion.velocity = Vec3::ZERO;
                } else {
                    warn!("scenario: no soldier to teleport");
                }
                set_look(&mut look, *yaw, *pitch);
                Progress::Done
            }
            Step::Look(yaw, pitch) => {
                set_look(&mut look, *yaw, *pitch);
                Progress::Done
            }
            Step::Hold(buttons) => {
                buttons.iter().for_each(|b| input.buttons.insert(b.bits()));
                Progress::Done
            }
            Step::Release(buttons) => {
                buttons.iter().for_each(|b| input.buttons.remove(b.bits()));
                Progress::Done
            }
            Step::Move(right, forward) => {
                let movement = Vec2::new(*right, *forward);
                input.movement = (movement != Vec2::ZERO).then_some(movement);
                Progress::Done
            }
            Step::Weapon(index) => {
                selection.index = *index;
                Progress::Done
            }
            Step::ThirdPerson(on) => {
                third_person.0 = *on;
                Progress::Done
            }
            Step::Ssao(on) => {
                for camera in &camera {
                    if *on {
                        commands
                            .entity(camera)
                            .insert((ScreenSpaceAmbientOcclusion::default(), Msaa::Off));
                    } else {
                        commands
                            .entity(camera)
                            .remove::<ScreenSpaceAmbientOcclusion>()
                            .insert(Msaa::default());
                    }
                }
                Progress::Done
            }
            Step::Shadows(on) => {
                for mut sun in &mut suns {
                    sun.shadow_maps_enabled = *on;
                }
                Progress::Done
            }
            Step::Key(key) => {
                key_event(*key, ButtonState::Pressed);
                runner.released.push(*key);
                Progress::Done
            }
            Step::Click(name) => {
                match buttons.iter_mut().find(|(n, _)| n.as_str() == name) {
                    Some((_, mut interaction)) => *interaction = Interaction::Pressed,
                    None => warn!("scenario: no button named {name}"),
                }
                Progress::Done
            }
            Step::Kill => {
                for (_, mut health) in &mut soldier {
                    health.current = 0.0;
                }
                Progress::Done
            }
            Step::Screenshot(name) => {
                if runner.frames == 0 {
                    let path = if name.ends_with(".png") {
                        PathBuf::from(name)
                    } else {
                        runner.out.join(format!("{name}.png"))
                    };
                    if let Some(parent) = path.parent() {
                        let _ = std::fs::create_dir_all(parent);
                    }
                    runner.screenshot_pending = true;
                    commands
                        .spawn(Screenshot::primary_window())
                        .observe(save_to_disk(path))
                        .observe(|_: On<ScreenshotCaptured>, mut runner: ResMut<Runner>| {
                            runner.screenshot_pending = false;
                        });
                }
                runner.frames += 1;
                done_if(!runner.screenshot_pending || elapsed > 10.0)
            }
            Step::Measure(name, seconds) => {
                if elapsed > 0.0 {
                    runner.samples.push(time.delta_secs() * 1000.0);
                }
                if elapsed >= *seconds {
                    let line = frame_stats(name, &mut runner.samples);
                    info!("scenario: {line}");
                    writeln!(runner.report, "{line}").ok();
                    Progress::Done
                } else {
                    Progress::Waiting
                }
            }
            Step::Trace(name, seconds) => {
                if let Ok((m, _)) = soldier.single() {
                    let (p, v) = (m.position, m.velocity);
                    writeln!(
                        runner.report,
                        "{name} {elapsed:6.3} pos {:8.3} {:7.3} {:8.3} vel {:6.2} {:6.2} {:6.2} {} {:?} correction {:.4}",
                        p.x,
                        p.y,
                        p.z,
                        v.x,
                        v.y,
                        v.z,
                        if m.grounded { "G" } else { "-" },
                        m.stance,
                        prediction.last_correction,
                    )
                    .ok();
                }
                done_if(elapsed >= *seconds)
            }
            Step::Log(text) => {
                info!("scenario: {text}");
                writeln!(runner.report, "{text}").ok();
                Progress::Done
            }
            Step::Quit => {
                if !runner.report.is_empty() {
                    let _ = std::fs::create_dir_all(&runner.out);
                    let _ = std::fs::write(runner.out.join("report.txt"), &runner.report);
                }
                info!("scenario: finished after {now:.1} s");
                exit.write(AppExit::Success);
                Progress::Waiting
            }
        };
        if progress == Progress::Waiting {
            return;
        }
        runner.next += 1;
        runner.step_started = None;
        runner.frames = 0;
        runner.samples.clear();
    }
}

fn done_if(done: bool) -> Progress {
    if done { Progress::Done } else { Progress::Waiting }
}

fn set_look(look: &mut LookState, yaw: f32, pitch: f32) {
    look.yaw = yaw.to_radians();
    look.pitch = pitch.to_radians();
}

/// `name: avg 5.8 ms (172 fps), p50 5.6 ms, p95 7.9 ms, max 12.1 ms over 520 frames`
fn frame_stats(name: &str, samples: &mut [f32]) -> String {
    if samples.is_empty() {
        return format!("{name}: no frames");
    }
    samples.sort_by(f32::total_cmp);
    let avg = samples.iter().sum::<f32>() / samples.len() as f32;
    let at = |q: f32| samples[((samples.len() - 1) as f32 * q) as usize];
    format!(
        "{name}: avg {avg:.2} ms ({:.0} fps), p50 {:.2} ms, p95 {:.2} ms, max {:.2} ms over {} frames",
        1000.0 / avg,
        at(0.5),
        at(0.95),
        samples[samples.len() - 1],
        samples.len()
    )
}
