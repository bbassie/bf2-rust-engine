//! `BF2_PERF_STATS=1` logs every 5 s where a frame's time goes and what drives it:
//!
//! - how long the main world's schedules take (before, in and after the fixed tick, split into
//!   `Update` and `PostUpdate`) and how long the render schedule takes on the render thread;
//! - how many entities draw meshes (and how many were seen), skinned meshes, animated bones,
//!   physics bodies and colliders;
//! - how often materials change: a changed bindless material rewrites its whole slab of up to
//!   2048 materials on the GPU and rebuilds the slab's bind group.
//!
//! `BF2_PERF_STATS=full` also counts the transforms that changed each frame and which
//! hierarchies they belong to (every changed `Transform` moves everything under it); walking
//! them costs about a millisecond a frame in a big battle.
//!
//! `BF2_PERF_EXP=novm` turns the view model camera off, to measure what the second camera costs.

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
            .add_systems(First, start_frame)
            .add_systems(
                RunFixedMainLoop,
                (
                    (|start: Option<Res<FrameStart>>| mark(start, &FIXED_START)).in_set(RunFixedMainLoopSystems::BeforeFixedMainLoop),
                    (|start: Option<Res<FrameStart>>| mark(start, &FIXED_END)).in_set(RunFixedMainLoopSystems::AfterFixedMainLoop),
                ),
            )
            .add_systems(UpdateDone, |start: Option<Res<FrameStart>>| mark(start, &UPDATE_END))
            .add_systems(PostUpdateDone, |start: Option<Res<FrameStart>>| mark(start, &POST_UPDATE_END))
            .add_systems(Last, (count_frame, log_stats, end_frame).chain());
            // Marks between the frame's schedules.
            let mut order = app.world_mut().resource_mut::<MainScheduleOrder>();
            order.insert_after(Update, UpdateDone);
            order.insert_after(PostUpdate, PostUpdateDone);
            if let Some(render_app) = app.get_sub_app_mut(RenderApp) {
                render_app
                    .add_systems(Render, start_frame.in_set(RenderSystems::ExtractCommands))
                    .add_systems(Render, end_render.in_set(RenderSystems::PostCleanup));
            }
        }
        if std::env::var("BF2_PERF_EXP").is_ok_and(|e| e.contains("novm")) {
            app.add_systems(Last, |mut cameras: Query<&mut Camera>| {
                for mut camera in &mut cameras {
                    if camera.order == 1 && camera.is_active {
                        camera.is_active = false;
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

fn start_frame(mut commands: Commands) {
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
    // The marks of this frame, in order (they are left over from an earlier frame otherwise).
    if marks.windows(2).all(|w| w[0] <= w[1]) && marks[3] <= now {
        TOTALS.before_fixed.fetch_add(marks[0], Ordering::Relaxed);
        TOTALS.fixed.fetch_add(marks[1] - marks[0], Ordering::Relaxed);
        TOTALS.update.fetch_add(marks[2] - marks[1], Ordering::Relaxed);
        TOTALS.post_update.fetch_add(marks[3] - marks[2], Ordering::Relaxed);
        TOTALS.last.fetch_add(now - marks[3], Ordering::Relaxed);
    }
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
    bodies: Query<(&avian3d::prelude::RigidBody, Has<avian3d::prelude::Sleeping>)>,
    colliders: Query<(), With<avian3d::prelude::Collider>>,
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
         SpawnScene + PostUpdate {:.2}, Last {:.2}), render thread {:.2} ms a frame",
        take(&TOTALS.main, main_frames),
        take(&TOTALS.before_fixed, main_frames),
        take(&TOTALS.fixed, main_frames),
        take(&TOTALS.update, main_frames),
        take(&TOTALS.post_update, main_frames),
        take(&TOTALS.last, main_frames),
        take(&TOTALS.render, render_frames),
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
