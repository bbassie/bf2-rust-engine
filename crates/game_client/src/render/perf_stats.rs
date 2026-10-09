//! `BF2_PERF_STATS=1` logs every 5 s where a frame's time goes and what drives it:
//!
//! - how long the main world's schedules take (before, in and after the fixed tick, split into
//!   `Update` and `PostUpdate`), how long the main thread then waits for the render world and
//!   extracts to it, and how long the render schedule takes on the render thread, split into
//!   its system sets and each camera's `Core3d` schedule (the time after a camera's schedule is
//!   wgpu finishing its command buffers: most of the cost of every draw);
//! - how many entities draw meshes (and how many were seen, by what they belong to),
//!   skinned meshes, animated bones, physics bodies and colliders, UI nodes, and the draws of
//!   each render phase;
//! - how often materials change: a changed bindless material rewrites its whole slab of up to
//!   2048 materials on the GPU and rebuilds the slab's bind group;
//! - which UI trees change each frame (every change lays out the UI again).
//!
//! `BF2_PERF_STATS=full` also counts the transforms that changed each frame and which
//! hierarchies they belong to (every changed `Transform` moves everything under it); walking
//! them costs about a millisecond a frame in a big battle.
//!
//! `BF2_PERF_EXP=noui` hides the whole UI.

use std::{
    sync::atomic::{AtomicU64, Ordering},
    time::Instant,
};

use bevy::{
    animation::{AnimatedBy, AnimationTargetId},
    app::{MainScheduleOrder, RunFixedMainLoopSystems},
    camera::visibility::VisibilityRange,
    ecs::schedule::ScheduleLabel,
    light::NotShadowCaster,
    mesh::skinning::SkinnedMesh,
    platform::collections::HashMap,
    prelude::*,
    render::{Render, RenderApp, RenderSystems},
};

use super::materials::Bf2Material;

pub struct PerfStatsPlugin;

impl Plugin for PerfStatsPlugin {
    fn build(&self, app: &mut App) {
        if let Ok(mode) = std::env::var("BF2_PERF_STATS") {
            app.insert_resource(Counts {
                full: mode == "full",
                ..default()
            })
            .init_schedule(UpdateDone)
            .init_schedule(PostUpdateDone)
            .add_systems(First, start_main_frame)
            .add_systems(
                RunFixedMainLoop,
                (
                    (|start: Option<Res<FrameStart>>| mark(start, &FIXED_START)).in_set(RunFixedMainLoopSystems::BeforeFixedMainLoop),
                    (|start: Option<Res<FrameStart>>| mark(start, &FIXED_END)).in_set(RunFixedMainLoopSystems::AfterFixedMainLoop),
                ),
            )
            .add_systems(UpdateDone, |start: Option<Res<FrameStart>>| mark(start, &UPDATE_END))
            .add_systems(PostUpdateDone, |start: Option<Res<FrameStart>>| mark(start, &POST_UPDATE_END))
            .init_resource::<UiChanges>()
            .add_systems(Last, (count_frame, count_ui_changes, log_stats, end_frame).chain());
            // Marks between the frame's schedules.
            let mut order = app.world_mut().resource_mut::<MainScheduleOrder>();
            order.insert_after(Update, UpdateDone);
            order.insert_after(PostUpdate, PostUpdateDone);
            if let Some(render_app) = app.get_sub_app_mut(RenderApp) {
                render_app
                    .add_systems(Render, start_render.in_set(RenderSystems::ExtractCommands))
                    .add_systems(Render, end_render.in_set(RenderSystems::PostCleanup));
                render_phases::add(render_app);
            }
            if let Some(extract_app) = app.get_sub_app_mut(bevy::render::pipelined_rendering::RenderExtractApp) {
                extract_app.set_extract(timed_renderer_extract);
            }
        }
        if std::env::var("BF2_PERF_EXP").is_ok_and(|e| e.contains("noui")) {
            app.add_systems(Last, |mut roots: Query<&mut Node, Without<ChildOf>>| {
                for mut node in &mut roots {
                    if node.display != Display::None {
                        node.display = Display::None;
                    }
                }
            });
        }
    }
}

#[derive(ScheduleLabel, Debug, Clone, PartialEq, Eq, Hash)]
struct UpdateDone;

#[derive(ScheduleLabel, Debug, Clone, PartialEq, Eq, Hash)]
struct PostUpdateDone;

/// When this world's frame began (the main world's `First`, the render schedule's start).
#[derive(Resource)]
struct FrameStart(Instant);

/// Microseconds since [`EPOCH`] at which the main world's last frame ended.
static MAIN_END: AtomicU64 = AtomicU64::new(0);
static EPOCH: std::sync::OnceLock<Instant> = std::sync::OnceLock::new();

fn since_epoch() -> u64 {
    EPOCH.get_or_init(Instant::now).elapsed().as_micros() as u64
}

/// Also adds up the time between the main world's frames: extracting to the render world,
/// waiting for the render thread, window events.
fn start_main_frame(mut commands: Commands) {
    let end = MAIN_END.load(Ordering::Relaxed);
    if end > 0 {
        TOTALS.gap.fetch_add(since_epoch().saturating_sub(end), Ordering::Relaxed);
    }
    commands.insert_resource(FrameStart(Instant::now()));
}

/// Microseconds into this frame at which the fixed loop began and ended, `Update` and
/// `PostUpdate` ended.
static FIXED_START: AtomicU64 = AtomicU64::new(0);
static FIXED_END: AtomicU64 = AtomicU64::new(0);
static UPDATE_END: AtomicU64 = AtomicU64::new(0);
static POST_UPDATE_END: AtomicU64 = AtomicU64::new(0);

fn mark(start: Option<Res<FrameStart>>, at: &AtomicU64) {
    if let Some(start) = start {
        at.store(start.0.elapsed().as_micros() as u64, Ordering::Relaxed);
    }
}

/// Microseconds summed since the last report: the main world's frames and their parts, the
/// render schedule's frames; and how many frames each.
#[derive(Default)]
struct Totals {
    main: AtomicU64,
    before_fixed: AtomicU64,
    fixed: AtomicU64,
    update: AtomicU64,
    post_update: AtomicU64,
    last: AtomicU64,
    gap: AtomicU64,
    render_start: AtomicU64,
    extract_wait: AtomicU64,
    extract_run: AtomicU64,
    main_frames: AtomicU64,
    render: AtomicU64,
    render_frames: AtomicU64,
}

static TOTALS: Totals = Totals {
    main: AtomicU64::new(0),
    before_fixed: AtomicU64::new(0),
    fixed: AtomicU64::new(0),
    update: AtomicU64::new(0),
    post_update: AtomicU64::new(0),
    last: AtomicU64::new(0),
    gap: AtomicU64::new(0),
    render_start: AtomicU64::new(0),
    extract_wait: AtomicU64::new(0),
    extract_run: AtomicU64::new(0),
    main_frames: AtomicU64::new(0),
    render: AtomicU64::new(0),
    render_frames: AtomicU64::new(0),
};

fn end_frame(start: Option<Res<FrameStart>>) {
    let Some(start) = start else {
        return;
    };
    let now = start.0.elapsed().as_micros() as u64;
    let marks = [&FIXED_START, &FIXED_END, &UPDATE_END, &POST_UPDATE_END].map(|m| m.load(Ordering::Relaxed));
    TOTALS.main.fetch_add(now, Ordering::Relaxed);
    TOTALS.main_frames.fetch_add(1, Ordering::Relaxed);
    MAIN_END.store(since_epoch(), Ordering::Relaxed);
    // The marks of this frame, in order (they are left over from an earlier frame otherwise).
    if marks.windows(2).all(|w| w[0] <= w[1]) && marks[3] <= now {
        TOTALS.before_fixed.fetch_add(marks[0], Ordering::Relaxed);
        TOTALS.fixed.fetch_add(marks[1] - marks[0], Ordering::Relaxed);
        TOTALS.update.fetch_add(marks[2] - marks[1], Ordering::Relaxed);
        TOTALS.post_update.fetch_add(marks[3] - marks[2], Ordering::Relaxed);
        TOTALS.last.fetch_add(now - marks[3], Ordering::Relaxed);
    }
}

/// The render world's frame start; also adds up the time since the main world's frame
/// ended (waiting for the render world, extracting).
fn start_render(mut commands: Commands) {
    let end = MAIN_END.load(Ordering::Relaxed);
    if end > 0 {
        TOTALS.render_start.fetch_add(since_epoch().saturating_sub(end), Ordering::Relaxed);
    }
    commands.insert_resource(FrameStart(Instant::now()));
    render_phases::mark(0);
}

fn end_render(start: Option<Res<FrameStart>>) {
    if let Some(start) = start {
        TOTALS.render.fetch_add(start.0.elapsed().as_micros() as u64, Ordering::Relaxed);
        TOTALS.render_frames.fetch_add(1, Ordering::Relaxed);
    }
}

#[derive(Resource, Default)]
struct Counts {
    frames: u32,
    since: f32,
    /// Added, modified, removed, per material type.
    bf2: [u32; 3],
    standard: [u32; 3],
    /// A few of the BF2 materials modified lately.
    modified: Vec<AssetId<Bf2Material>>,
    /// `BF2_PERF_STATS=full`: entities whose `Transform` / `GlobalTransform` changed, summed
    /// over the frames, by what they are (the kind of hierarchy, the depth in it and the name).
    full: bool,
    moved: u64,
    moved_global: u64,
    moved_names: HashMap<String, u32>,
    moved_roots: HashMap<String, u32>,
}

fn tally<A: Asset>(event: &AssetEvent<A>, into: &mut [u32; 3]) {
    match event {
        AssetEvent::Added { .. } => into[0] += 1,
        AssetEvent::Modified { .. } => into[1] += 1,
        AssetEvent::Removed { .. } => into[2] += 1,
        _ => {}
    }
}

#[allow(clippy::type_complexity)]
fn count_frame(
    mut counts: ResMut<Counts>,
    mut bf2: MessageReader<AssetEvent<Bf2Material>>,
    mut standard: MessageReader<AssetEvent<StandardMaterial>>,
    moved: Query<(Entity, Option<&Name>, Option<&ChildOf>), Changed<Transform>>,
    moved_global: Query<Entity, Changed<GlobalTransform>>,
    parents: Query<&ChildOf>,
    kinds: Query<(Has<crate::vehicles::VehicleView>, Has<game_shared::statics::StaticMesh>, Has<Camera>)>,
    names: Query<&Name>,
) {
    let counts = &mut *counts;
    counts.frames += 1;
    for event in bf2.read() {
        if let AssetEvent::Modified { id } = event
            && counts.modified.len() < 8
            && !counts.modified.contains(id)
        {
            counts.modified.push(*id);
        }
        tally(event, &mut counts.bf2);
    }
    for event in standard.read() {
        tally(event, &mut counts.standard);
    }
    if !counts.full {
        return;
    }
    let root_of = |mut entity: Entity| {
        let mut depth = 0;
        while let Ok(parent) = parents.get(entity) {
            entity = parent.parent();
            depth += 1;
        }
        let kind = match kinds.get(entity) {
            Ok((true, ..)) => "vehicle",
            Ok((_, true, _)) => "static",
            Ok((.., true)) => "camera",
            _ => "other",
        };
        (kind, depth)
    };
    for entity in &moved_global {
        counts.moved_global += 1;
        *counts.moved_roots.entry(root_of(entity).0.to_string()).or_default() += 1;
    }
    for (entity, name, parent) in &moved {
        counts.moved += 1;
        let (kind, depth) = root_of(entity);
        let name = name
            .map(|n| n.as_str().to_string())
            .or_else(|| parent.and_then(|p| names.get(p.parent()).ok()).map(|n| format!("child of {n}")))
            .unwrap_or_else(|| "unnamed".into());
        *counts.moved_names.entry(format!("{kind} depth {depth} {name}")).or_default() += 1;
    }
}

/// The `n` biggest counts, per frame.
fn top(counts: &HashMap<String, u32>, n: usize, frames: f32) -> Vec<String> {
    let mut rows: Vec<_> = counts.iter().collect();
    rows.sort_by_key(|(_, count)| std::cmp::Reverse(**count));
    rows.iter().take(n).map(|(name, count)| format!("{name}: {:.1}", **count as f32 / frames)).collect()
}

#[allow(clippy::type_complexity, clippy::too_many_arguments)]
fn log_stats(
    time: Res<Time<Real>>,
    mut counts: ResMut<Counts>,
    entities: Query<()>,
    meshes: Query<(&ViewVisibility, Has<VisibilityRange>, Has<NotShadowCaster>, Has<SkinnedMesh>), With<Mesh3d>>,
    targets: Query<(), (With<AnimationTargetId>, With<AnimatedBy>)>,
    players: Query<(), With<AnimationGraphHandle>>,
    materials: Res<Assets<Bf2Material>>,
    (standard_materials, images): (Res<Assets<StandardMaterial>>, Res<Assets<Image>>),
    (ui_nodes, mut ui_changes): (Query<(Option<&ChildOf>, Has<Text>), With<Node>>, ResMut<UiChanges>),
    bodies: Query<(&avian3d::prelude::RigidBody, Has<avian3d::prelude::Sleeping>)>,
    colliders: Query<(), With<avian3d::prelude::Collider>>,
    kinds: MeshKinds,
) {
    let now = time.elapsed_secs();
    if now - counts.since < 5.0 {
        return;
    }
    let take = |us: &AtomicU64, frames: u64| us.swap(0, Ordering::Relaxed) as f64 / 1000.0 / frames.max(1) as f64;
    let main_frames = TOTALS.main_frames.swap(0, Ordering::Relaxed);
    let render_frames = TOTALS.render_frames.swap(0, Ordering::Relaxed);
    info!(
        "perf stats: main world {:.2} ms a frame (before the fixed tick {:.2}, fixed tick {:.2}, Update {:.2}, \
         SpawnScene + PostUpdate {:.2}, Last {:.2}), between frames {:.2} (waiting for the render world {:.2}, \
         extracting {:.2}), main end to render start {:.2}, render thread {:.2} ms a frame ({})",
        take(&TOTALS.main, main_frames),
        take(&TOTALS.before_fixed, main_frames),
        take(&TOTALS.fixed, main_frames),
        take(&TOTALS.update, main_frames),
        take(&TOTALS.post_update, main_frames),
        take(&TOTALS.last, main_frames),
        take(&TOTALS.gap, main_frames),
        take(&TOTALS.extract_wait, main_frames),
        take(&TOTALS.extract_run, main_frames),
        take(&TOTALS.render_start, render_frames),
        take(&TOTALS.render, render_frames),
        render_phases::take(render_frames),
    );

    let frames = counts.frames.max(1) as f32;
    let (mut total, mut seen, mut ranged, mut casters, mut skinned, mut skinned_seen) = (0, 0, 0, 0, 0, 0);
    for (visibility, range, no_shadow, skin) in &meshes {
        total += 1;
        seen += visibility.get() as u32;
        ranged += range as u32;
        casters += (!no_shadow) as u32;
        skinned += skin as u32;
        skinned_seen += (skin && visibility.get()) as u32;
    }
    let per = |c: [u32; 3]| format!("+{:.1} ~{:.1} -{:.1}", c[0] as f32 / frames, c[1] as f32 / frames, c[2] as f32 / frames);
    info!(
        "perf stats: {} entities; meshes {total} ({seen} seen, {ranged} with visibility ranges, {casters} shadow \
         casters), skinned {skinned} ({skinned_seen} seen), animated targets {} of {} active players; material events \
         per frame: bf2 {}, standard {}",
        entities.iter().count(),
        targets.iter().count(),
        players.iter().count(),
        per(counts.bf2),
        per(counts.standard),
    );
    info!(
        "perf stats: assets: {} BF2 materials, {} standard materials, {} images",
        materials.len(),
        standard_materials.len(),
        images.len()
    );
    // UI nodes: the layout walks all of them every frame, and lays out again what changed.
    let (mut ui_total, mut ui_texts, mut ui_roots) = (0, 0, 0);
    for (parent, text) in &ui_nodes {
        ui_total += 1;
        ui_texts += text as u32;
        ui_roots += parent.is_none() as u32;
    }
    let changes = std::mem::take(&mut *ui_changes);
    let mut changed: Vec<(Entity, u32, u32)> = changes
        .nodes
        .keys()
        .chain(changes.texts.keys())
        .copied()
        .collect::<bevy::platform::collections::HashSet<_>>()
        .into_iter()
        .map(|e| (e, changes.nodes.get(&e).copied().unwrap_or(0), changes.texts.get(&e).copied().unwrap_or(0)))
        .collect();
    changed.sort_by_key(|(_, n, t)| std::cmp::Reverse(n + t));
    let ui_frames = changes.frames.max(1) as f32;
    let changed: Vec<String> = changed
        .iter()
        .take(6)
        .map(|(e, n, t)| {
            let sample = changes.samples.get(e).map_or("", String::as_str);
            format!("{e:?} ({sample}): nodes {:.1}, texts {:.1}", *n as f32 / ui_frames, *t as f32 / ui_frames)
        })
        .collect();
    info!(
        "perf stats: UI: {ui_total} nodes ({ui_texts} texts) under {ui_roots} roots; changed per frame by root: {}",
        changed.join(" | ")
    );
    info!("perf stats: meshes by kind (total/seen/distinct mesh+material): {}", kinds.summary());
    info!(
        "perf stats: draws per render phase (views / multidraw batch sets / batchable bins / unbatchable or \
         sorted items): {}",
        render_phases::take_phases()
    );
    let mut kinds = [0u32; 4];
    for (body, sleeping) in &bodies {
        let index = match body {
            avian3d::prelude::RigidBody::Static => 0,
            avian3d::prelude::RigidBody::Kinematic => 1,
            avian3d::prelude::RigidBody::Dynamic => 2,
        };
        kinds[index] += 1;
        kinds[3] += sleeping as u32;
    }
    info!(
        "perf stats: bodies {} static, {} kinematic, {} dynamic ({} sleeping); {} colliders",
        kinds[0],
        kinds[1],
        kinds[2],
        kinds[3],
        colliders.iter().count(),
    );
    let names: Vec<String> = counts
        .modified
        .iter()
        .filter_map(|id| materials.get(*id)?.base.base_color_texture.as_ref()?.path().map(|p| p.to_string()))
        .collect();
    if !names.is_empty() {
        info!("perf stats: modified BF2 materials, e.g. {names:?}");
    }
    if counts.full {
        info!(
            "perf stats: transforms changed per frame {:.0} (global transforms {:.0}, by hierarchy: {:?}), most: {:?}",
            counts.moved as f32 / frames,
            counts.moved_global as f32 / frames,
            top(&counts.moved_roots, 6, frames),
            top(&counts.moved_names, 14, frames),
        );
    }
    *counts = Counts {
        since: now,
        full: counts.full,
        ..default()
    };
}

/// Counts mesh entities by what they belong to (for `BF2_PERF_STATS`).
#[derive(bevy::ecs::system::SystemParam)]
#[allow(clippy::type_complexity)]
struct MeshKinds<'w, 's> {
    meshes: Query<'w, 's, (Entity, &'static ViewVisibility, &'static Mesh3d, Option<&'static MeshMaterial3d<Bf2Material>>, Has<VisibilityRange>)>,
    parents: Query<'w, 's, &'static ChildOf>,
    roots: Query<
        'w,
        's,
        (
            Has<super::soldiers::SoldierVisual>,
            Has<crate::vehicles::VehicleView>,
            Has<game_shared::statics::StaticMesh>,
            Option<&'static Name>,
        ),
    >,
    statics: Query<'w, 's, (), With<game_shared::statics::StaticMesh>>,
}

impl MeshKinds<'_, '_> {
    fn summary(&self) -> String {
        let mut rows: HashMap<String, (u32, u32, u32, bevy::platform::collections::HashSet<(AssetId<Mesh>, Option<AssetId<Bf2Material>>)>)> =
            HashMap::default();
        for (entity, visibility, mesh, material, ranged) in &self.meshes {
            let mut root = entity;
            let mut in_static = false;
            while let Ok(parent) = self.parents.get(root) {
                root = parent.parent();
                in_static |= self.statics.contains(root);
            }
            let kind = match self.roots.get(root) {
                _ if in_static => "static".to_string(),
                Ok((true, ..)) => "soldier".to_string(),
                Ok((_, true, ..)) => "vehicle".to_string(),
                Ok((.., true, _)) => "static".to_string(),
                Ok((.., Some(name))) => format!("'{}'", name.as_str().chars().take(24).collect::<String>()),
                _ => "other".to_string(),
            };
            let row = rows.entry(kind).or_default();
            row.0 += 1;
            row.1 += visibility.get() as u32;
            row.2 += ranged as u32;
            row.3.insert((mesh.id(), material.map(|m| m.id())));
        }
        let mut rows: Vec<_> = rows.into_iter().collect();
        rows.sort_by_key(|(_, r)| std::cmp::Reverse(r.0));
        rows.iter()
            .take(12)
            .map(|(kind, r)| format!("{kind} {}/{}/{} (ranged {})", r.0, r.1, r.3.len(), r.2))
            .collect::<Vec<_>>()
            .join(", ")
    }
}

/// Where the render thread's frame goes: between the `Render` schedule's system sets, and in
/// each camera's `Core3d` schedule (its systems, and from its end to the next thing: the
/// command buffers its systems recorded being finished, the next schedule starting).
mod render_phases {
    use std::sync::atomic::{AtomicU64, Ordering};

    use bevy::{
        core_pipeline::{Core3dSystems, schedule::Core3d},
        prelude::*,
        render::{Render as RenderSchedule, RenderSystems},
    };

    const MARKS: usize = 9;
    const NAMES: [&str; MARKS - 1] = [
        "prepare meshes",
        "views+specialize",
        "queue",
        "sort",
        "prepare resources",
        "bind groups",
        "render",
        "cleanup",
    ];
    /// When each mark was last passed, and the time between successive marks summed over
    /// frames (µs).
    static LAST: [AtomicU64; MARKS] = [const { AtomicU64::new(0) }; MARKS];
    static SUM: [AtomicU64; MARKS] = [const { AtomicU64::new(0) }; MARKS];
    /// Per camera run in a frame (up to 4): its Core3d systems, and from its end to the next.
    static CAMERA: [AtomicU64; 4] = [const { AtomicU64::new(0) }; 4];
    static AFTER: [AtomicU64; 4] = [const { AtomicU64::new(0) }; 4];
    static RUN: AtomicU64 = AtomicU64::new(0);
    static CAMERA_START: AtomicU64 = AtomicU64::new(0);
    static CAMERA_END: AtomicU64 = AtomicU64::new(0);

    fn now() -> u64 {
        super::since_epoch()
    }

    pub fn mark(index: usize) {
        let t = now();
        LAST[index].store(t, Ordering::Relaxed);
        if index > 0 {
            let before = LAST[index - 1].load(Ordering::Relaxed);
            SUM[index - 1].fetch_add(t.saturating_sub(before), Ordering::Relaxed);
        } else {
            RUN.store(0, Ordering::Relaxed);
            CAMERA_END.store(0, Ordering::Relaxed);
        }
    }

    fn camera_start() {
        let t = now();
        let run = RUN.load(Ordering::Relaxed) as usize;
        let end = CAMERA_END.load(Ordering::Relaxed);
        if run > 0 && end > 0 && run <= 4 {
            AFTER[run - 1].fetch_add(t.saturating_sub(end), Ordering::Relaxed);
        }
        CAMERA_START.store(t, Ordering::Relaxed);
    }

    fn camera_end() {
        let t = now();
        let run = RUN.fetch_add(1, Ordering::Relaxed) as usize;
        if run < 4 {
            CAMERA[run].fetch_add(t.saturating_sub(CAMERA_START.load(Ordering::Relaxed)), Ordering::Relaxed);
        }
        CAMERA_END.store(t, Ordering::Relaxed);
    }

    /// After the render set: from the last camera to the end of rendering (submitting,
    /// presenting).
    fn render_done() {
        let t = now();
        let run = RUN.load(Ordering::Relaxed) as usize;
        let end = CAMERA_END.load(Ordering::Relaxed);
        if run > 0 && run <= 4 && end > 0 {
            AFTER[run - 1].fetch_add(t.saturating_sub(end), Ordering::Relaxed);
        }
        mark(7);
    }

    pub fn add(render_app: &mut SubApp) {
        use RenderSystems::*;
        render_app
            .add_systems(RenderSchedule, (|| mark(1)).after(PrepareMeshes).before(CreateViews))
            .add_systems(RenderSchedule, (|| mark(2)).after(PrepareViews).before(Queue))
            .add_systems(RenderSchedule, (|| mark(3)).after(Queue).before(PhaseSort))
            .add_systems(RenderSchedule, (|| mark(4)).after(PhaseSort).before(Prepare))
            .add_systems(RenderSchedule, (|| mark(5)).after(PrepareResourcesFlush).before(PrepareBindGroups))
            .add_systems(RenderSchedule, (|| mark(6)).after(Prepare).before(RenderSystems::Render))
            .add_systems(RenderSchedule, render_done.after(RenderSystems::Render).before(Cleanup))
            .add_systems(RenderSchedule, (|| mark(8)).in_set(PostCleanup))
            .add_systems(RenderSchedule, count_phases.after(Queue).before(PhaseSort))
            .add_systems(Core3d, camera_start.before(Core3dSystems::Prepass))
            .add_systems(Core3d, camera_end.after(Core3dSystems::PostProcess));
    }

    /// Per phase: views, multidraw batch sets, batchable bins, unbatchable entities, summed
    /// over frames.
    const PHASES: [&str; 6] = ["opaque", "alpha mask", "opaque prepass", "alpha mask prepass", "shadow", "transparent"];
    static PHASE_COUNTS: [[AtomicU64; 4]; 6] = [const { [const { AtomicU64::new(0) }; 4] }; 6];
    static PHASE_FRAMES: AtomicU64 = AtomicU64::new(0);

    fn binned<P: bevy::render::render_phase::BinnedPhaseItem>(
        phases: &bevy::render::render_phase::ViewBinnedRenderPhases<P>,
        into: &[AtomicU64; 4],
    ) {
        for phase in phases.0.values() {
            into[0].fetch_add(1, Ordering::Relaxed);
            into[1].fetch_add(phase.multidrawable_meshes.len() as u64, Ordering::Relaxed);
            into[2].fetch_add(phase.batchable_meshes.len() as u64, Ordering::Relaxed);
            let unbatchable: usize = phase.unbatchable_meshes.values().map(|u| u.entities.len()).sum();
            into[3].fetch_add(unbatchable as u64, Ordering::Relaxed);
        }
    }

    #[allow(clippy::type_complexity)]
    fn count_phases(
        opaque: Res<bevy::render::render_phase::ViewBinnedRenderPhases<bevy::core_pipeline::core_3d::Opaque3d>>,
        alpha: Res<bevy::render::render_phase::ViewBinnedRenderPhases<bevy::core_pipeline::core_3d::AlphaMask3d>>,
        opaque_prepass: Res<bevy::render::render_phase::ViewBinnedRenderPhases<bevy::core_pipeline::prepass::Opaque3dPrepass>>,
        alpha_prepass: Res<bevy::render::render_phase::ViewBinnedRenderPhases<bevy::core_pipeline::prepass::AlphaMask3dPrepass>>,
        shadow: Res<bevy::render::render_phase::ViewBinnedRenderPhases<bevy::pbr::Shadow>>,
        transparent: Res<bevy::render::render_phase::ViewSortedRenderPhases<bevy::core_pipeline::core_3d::Transparent3d>>,
    ) {
        PHASE_FRAMES.fetch_add(1, Ordering::Relaxed);
        binned(&opaque, &PHASE_COUNTS[0]);
        binned(&alpha, &PHASE_COUNTS[1]);
        binned(&opaque_prepass, &PHASE_COUNTS[2]);
        binned(&alpha_prepass, &PHASE_COUNTS[3]);
        binned(&shadow, &PHASE_COUNTS[4]);
        for phase in transparent.0.values() {
            PHASE_COUNTS[5][0].fetch_add(1, Ordering::Relaxed);
            PHASE_COUNTS[5][3].fetch_add(phase.items.len() as u64, Ordering::Relaxed);
        }
    }

    /// Per phase: views / multidraw batch sets / batchable bins / unbatchable (or sorted) items.
    pub fn take_phases() -> String {
        let frames = PHASE_FRAMES.swap(0, Ordering::Relaxed).max(1) as f64;
        PHASES
            .iter()
            .zip(&PHASE_COUNTS)
            .map(|(name, counts)| {
                let c: Vec<String> = counts.iter().map(|c| format!("{:.0}", c.swap(0, Ordering::Relaxed) as f64 / frames)).collect();
                format!("{name} {}", c.join("/"))
            })
            .collect::<Vec<_>>()
            .join(", ")
    }

    pub fn take(frames: u64) -> String {
        let per = |us: u64| us as f64 / 1000.0 / frames.max(1) as f64;
        let mut parts: Vec<String> = (0..MARKS - 1)
            .map(|i| format!("{} {:.2}", NAMES[i], per(SUM[i].swap(0, Ordering::Relaxed))))
            .collect();
        for i in 0..4 {
            let camera = CAMERA[i].swap(0, Ordering::Relaxed);
            let after = AFTER[i].swap(0, Ordering::Relaxed);
            if camera > 0 {
                parts.push(format!("camera {i} {:.2} + after {:.2}", per(camera), per(after)));
            }
        }
        parts.join(", ")
    }
}

/// UI nodes whose `Node` or text changed, per frame, by UI root: every change makes the layout
/// run again for its root's tree.
#[derive(Resource, Default)]
struct UiChanges {
    frames: u32,
    nodes: HashMap<Entity, u32>,
    texts: HashMap<Entity, u32>,
    /// By root: the nearest name above the last node seen changing, and how far up it is.
    samples: HashMap<Entity, String>,
}

#[allow(clippy::type_complexity)]
fn count_ui_changes(
    mut changes: ResMut<UiChanges>,
    nodes: Query<Entity, Changed<Node>>,
    texts: Query<Entity, Or<(Changed<Text>, Changed<TextSpan>, Changed<TextFont>)>>,
    parents: Query<&ChildOf>,
    names: Query<&Name>,
) {
    let root = |mut e: Entity| {
        while let Ok(parent) = parents.get(e) {
            e = parent.parent();
        }
        e
    };
    let named = |mut e: Entity| {
        for up in 0.. {
            if let Ok(name) = names.get(e) {
                return format!("{name} +{up}");
            }
            match parents.get(e) {
                Ok(parent) => e = parent.parent(),
                Err(_) => break,
            }
        }
        "unnamed".to_string()
    };
    changes.frames += 1;
    for entity in &nodes {
        let root = root(entity);
        *changes.nodes.entry(root).or_default() += 1;
        changes.samples.insert(root, named(entity));
    }
    for entity in &texts {
        *changes.texts.entry(root(entity)).or_default() += 1;
    }
}

/// Bevy's `renderer_extract` (pipelined rendering) with timings: how long the main thread
/// waits for the render world to come back, and how long extracting takes.
fn timed_renderer_extract(app_world: &mut World, _world: &mut World) {
    use bevy::{ecs::schedule::MainThreadExecutor, render::pipelined_rendering::RenderAppChannels, tasks::ComputeTaskPool};
    app_world.resource_scope(|world, main_thread_executor: Mut<MainThreadExecutor>| {
        world.resource_scope(|world, mut render_channels: Mut<RenderAppChannels>| {
            let waiting = Instant::now();
            let received = ComputeTaskPool::get()
                .scope_with_executor(true, Some(&*main_thread_executor.0), |s| {
                    s.spawn(async { render_channels.recv().await });
                })
                .pop()
                .unwrap();
            TOTALS.extract_wait.fetch_add(waiting.elapsed().as_micros() as u64, Ordering::Relaxed);
            if let Some(mut render_app) = received {
                let extracting = Instant::now();
                render_app.extract(world);
                TOTALS.extract_run.fetch_add(extracting.elapsed().as_micros() as u64, Ordering::Relaxed);
                render_channels.send_blocking(render_app);
            } else {
                world.write_message(AppExit::error());
            }
        });
    });
}
