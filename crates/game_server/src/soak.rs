//! Soak testing: `server --soak <minutes>` logs a `soak:` line with the server's health every
//! `--soak-every` seconds (entities, memory, frame times, players, the round) and quits with
//! a summary after that many minutes (0: never). `--soak-rotate <minutes>` moves on to the
//! next map of the rotation after that long on a map, so a run covers every map for a known
//! time whatever the tickets do. `scripts/soak.sh` runs one and greps the log.

use std::time::{Duration, Instant};

use bevy::{ecs::entity::Entities, platform::collections::HashMap, prelude::*};
use game_shared::{
    conquest::{ControlPoint, FlagState, RoundState, Tickets},
    level::LoadedLevel,
    projectile::Projectile,
    protocol::{ControlledBy, MatchInfo, Player, Team},
    revive::Downed,
    soldier::{Soldier, SoldierMotion},
    vehicle::{Seated, Vehicle},
};

/// Server frames that take longer than one simulation tick fall behind.
const OVERRUN_MS: f32 = 1000.0 / game_shared::TICK_HZ as f32;
/// Frames slower than this are logged with where their time went.
const SLOW_FRAME_MS: f32 = 100.0;
/// Alive bots that moved less than this between two reports count as idle, meters.
const IDLE_DISTANCE: f32 = 2.0;

pub struct SoakPlugin {
    /// Quit after this long; `None` runs until stopped.
    pub duration: Option<Duration>,
    /// Seconds between reports.
    pub every: f32,
    /// Play the next map of the rotation after this long on one; `None` leaves that to the
    /// rounds.
    pub rotate_every: Option<Duration>,
}

impl Plugin for SoakPlugin {
    fn build(&self, app: &mut App) {
        app.insert_resource(Soak {
            duration: self.duration,
            every: self.every.max(1.0),
            rotate_every: self.rotate_every,
            ..default()
        })
        .add_systems(First, frame_start)
        .add_systems(FixedFirst, tick_start)
        .add_systems(FixedLast, tick_end)
        .add_systems(Update, rotate.run_if(resource_exists::<LoadedLevel>))
        .add_systems(Last, (frame_end, report).chain());
    }
}

#[derive(Resource, Default)]
struct Soak {
    duration: Option<Duration>,
    every: f32,
    rotate_every: Option<Duration>,
    /// When the current level was loaded.
    level_started: Option<Instant>,
    started: Option<Instant>,
    frame_started: Option<Instant>,
    tick_started: Option<Instant>,
    /// Simulation ticks run this frame and the time they took.
    frame_ticks: u32,
    frame_tick_ms: f32,
    last_report: Option<Instant>,
    // Since the last report.
    frames: u32,
    ticks: u32,
    frame_ms_sum: f32,
    frame_ms_max: f32,
    overruns: u32,
    tick_ms_sum: f32,
    tick_ms_max: f32,
    slow_frames: u32,
    // Whole run.
    total_frames: u64,
    total_overruns: u64,
    worst_frame_ms: f32,
    peak_entities: u32,
    first_memory: Option<u64>,
    peak_memory: u64,
    levels: Vec<String>,
    idle_reports: u32,
    reports: u32,
    /// Where each alive bot's soldier was at the last report.
    bot_positions: HashMap<Entity, Vec3>,
}

fn frame_start(mut soak: ResMut<Soak>) {
    let now = Instant::now();
    soak.frame_started = Some(now);
    soak.started.get_or_insert(now);
    soak.last_report.get_or_insert(now);
}

fn tick_start(mut soak: ResMut<Soak>) {
    soak.tick_started = Some(Instant::now());
}

fn tick_end(mut soak: ResMut<Soak>) {
    let Some(started) = soak.tick_started.take() else {
        return;
    };
    let ms = started.elapsed().as_secs_f32() * 1000.0;
    soak.ticks += 1;
    soak.frame_ticks += 1;
    soak.frame_tick_ms += ms;
    soak.tick_ms_sum += ms;
    soak.tick_ms_max = soak.tick_ms_max.max(ms);
}

/// Moves on to the next map after `rotate_every` on this one.
fn rotate(mut soak: ResMut<Soak>, level: Res<LoadedLevel>, mut commands: Commands) {
    let now = Instant::now();
    if level.is_changed() {
        soak.level_started = Some(now);
    }
    let (Some(every), Some(started)) = (soak.rotate_every, soak.level_started) else {
        return;
    };
    if now.duration_since(started) >= every {
        info!("soak: {:.1} min on `{}`, next map", every.as_secs_f32() / 60.0, level.desc.name);
        // Not again before the new level is there.
        soak.level_started = None;
        commands.queue(crate::rotation::advance);
    }
}

fn frame_end(mut soak: ResMut<Soak>) {
    let Some(started) = soak.frame_started else {
        return;
    };
    let ms = started.elapsed().as_secs_f32() * 1000.0;
    soak.frames += 1;
    soak.total_frames += 1;
    soak.frame_ms_sum += ms;
    soak.frame_ms_max = soak.frame_ms_max.max(ms);
    soak.worst_frame_ms = soak.worst_frame_ms.max(ms);
    if ms > OVERRUN_MS {
        soak.overruns += 1;
        soak.total_overruns += 1;
    }
    // Where the time of a slow frame went (the first few per report).
    if ms > SLOW_FRAME_MS && soak.slow_frames < 5 {
        soak.slow_frames += 1;
        warn!(
            "soak: slow frame {ms:.0} ms: {} simulation ticks took {:.0} ms, the rest {:.0} ms",
            soak.frame_ticks,
            soak.frame_tick_ms,
            ms - soak.frame_tick_ms
        );
    }
    soak.frame_ticks = 0;
    soak.frame_tick_ms = 0.0;
}

#[allow(clippy::too_many_arguments, clippy::type_complexity)]
fn report(
    mut soak: ResMut<Soak>,
    entities: &Entities,
    level: Option<Res<LoadedLevel>>,
    matches: Query<(&MatchInfo, Option<&Tickets>, Option<&RoundState>)>,
    players: Query<&Player>,
    soldiers: Query<(Entity, &ControlledBy, &SoldierMotion, Has<Downed>, Has<Seated>), With<Soldier>>,
    vehicles: Query<(), With<Vehicle>>,
    projectiles: Query<(), With<Projectile>>,
    flags: Query<&FlagState, With<ControlPoint>>,
    mut exit: MessageWriter<AppExit>,
) {
    let (Some(started), Some(last)) = (soak.started, soak.last_report) else {
        return;
    };
    let now = Instant::now();
    let since = now.duration_since(last).as_secs_f32();
    let finished = soak.duration.is_some_and(|d| now.duration_since(started) >= d);
    if since < soak.every && !finished {
        return;
    }
    soak.last_report = Some(now);
    soak.reports += 1;

    let entity_count = entities.count_spawned();
    soak.peak_entities = soak.peak_entities.max(entity_count);
    let memory = resident_bytes().unwrap_or(0);
    soak.first_memory.get_or_insert(memory);
    soak.peak_memory = soak.peak_memory.max(memory);

    let (humans, bots) = players.iter().fold((0, 0), |(h, b), p| if p.is_bot { (h, b + 1) } else { (h + 1, b) });
    let (mut alive, mut downed, mut seated) = (0, 0, 0);
    let mut idle = 0;
    let mut idle_at: Vec<String> = Vec::new();
    let mut positions = HashMap::default();
    for (soldier, controlled_by, motion, is_downed, is_seated) in &soldiers {
        alive += 1;
        downed += is_downed as u32;
        seated += is_seated as u32;
        let is_bot = players.get(controlled_by.0).is_ok_and(|p| p.is_bot);
        if !is_bot || is_downed || is_seated {
            continue;
        }
        if let Some(before) = soak.bot_positions.get(&soldier)
            && before.distance(motion.position) < IDLE_DISTANCE
        {
            idle += 1;
            if idle_at.len() < 4 {
                idle_at.push(format!("{:.0} {:.0}", motion.position.x, motion.position.z));
            }
        }
        positions.insert(soldier, motion.position);
    }
    soak.bot_positions = positions;
    if idle > 0 {
        soak.idle_reports += 1;
    }

    let mut owners = [0; 3];
    for flag in &flags {
        owners[match flag.owner {
            Team::One => 0,
            Team::Two => 1,
            Team::Spectator => 2,
        }] += 1;
    }
    let (map, round) = match matches.iter().next() {
        Some((info, tickets, round)) => {
            let tickets = tickets.map_or(String::new(), |t| format!(" tickets {:.0}/{:.0}", t.remaining[0], t.remaining[1]));
            let round = match round {
                Some(RoundState::Playing) => "playing".to_string(),
                Some(RoundState::Ended { winner, restart_in }) => format!("ended ({winner:?}, next in {restart_in:.0} s)"),
                None => "starting".into(),
            };
            (format!("{} {} {}", info.level, info.mode, info.size), format!("{round}{tickets}"))
        }
        None => ("none".into(), String::new()),
    };
    if level.is_some() && soak.levels.last() != Some(&map) {
        soak.levels.push(map.clone());
    }

    let frames = soak.frames.max(1);
    info!(
        "soak: {:.0} s, {map}, {round}, flags {}/{}/{} (1/2/neutral); entities {entity_count}, memory {:.0} MB; \
         frame {:.2} ms avg, {:.1} ms max, {} of {} frames over {OVERRUN_MS:.1} ms, {:.1} ticks/s ({:.2} ms avg, {:.1} ms max); \
         players {humans} + {bots} bots, soldiers {alive} ({downed} down, {seated} seated), idle bots {idle}{}, \
         vehicles {}, projectiles {}",
        now.duration_since(started).as_secs_f32(),
        owners[0],
        owners[1],
        owners[2],
        memory as f64 / 1e6,
        soak.frame_ms_sum / frames as f32,
        soak.frame_ms_max,
        soak.overruns,
        soak.frames,
        soak.ticks as f32 / since.max(0.001),
        soak.tick_ms_sum / soak.ticks.max(1) as f32,
        soak.tick_ms_max,
        if idle_at.is_empty() { String::new() } else { format!(" (at {})", idle_at.join(", ")) },
        vehicles.iter().count(),
        projectiles.iter().count(),
    );
    soak.frames = 0;
    soak.ticks = 0;
    soak.frame_ms_sum = 0.0;
    soak.frame_ms_max = 0.0;
    soak.overruns = 0;
    soak.tick_ms_sum = 0.0;
    soak.tick_ms_max = 0.0;
    soak.slow_frames = 0;

    if finished {
        info!(
            "soak summary: {:.1} min, levels {}; peak entities {}, memory {:.0} MB at start, {:.0} MB at end, \
             {:.0} MB peak; worst frame {:.1} ms, {} of {} frames over {OVERRUN_MS:.1} ms; idle bots in {} of {} reports",
            now.duration_since(started).as_secs_f32() / 60.0,
            soak.levels.join(" -> "),
            soak.peak_entities,
            soak.first_memory.unwrap_or(0) as f64 / 1e6,
            memory as f64 / 1e6,
            soak.peak_memory as f64 / 1e6,
            soak.worst_frame_ms,
            soak.total_overruns,
            soak.total_frames,
            soak.idle_reports,
            soak.reports,
        );
        exit.write(AppExit::Success);
    }
}

/// The process's resident memory (working set) in bytes.
#[cfg(windows)]
fn resident_bytes() -> Option<u64> {
    #[repr(C)]
    #[derive(Default)]
    struct ProcessMemoryCounters {
        cb: u32,
        page_fault_count: u32,
        peak_working_set_size: usize,
        working_set_size: usize,
        quota_peak_paged_pool_usage: usize,
        quota_paged_pool_usage: usize,
        quota_peak_non_paged_pool_usage: usize,
        quota_non_paged_pool_usage: usize,
        pagefile_usage: usize,
        peak_pagefile_usage: usize,
    }
    #[link(name = "kernel32")]
    unsafe extern "system" {
        fn GetCurrentProcess() -> isize;
        fn K32GetProcessMemoryInfo(process: isize, counters: *mut ProcessMemoryCounters, size: u32) -> i32;
    }
    let mut counters = ProcessMemoryCounters {
        cb: size_of::<ProcessMemoryCounters>() as u32,
        ..default()
    };
    // SAFETY: plain Win32 calls with a correctly sized, writable struct.
    let ok = unsafe { K32GetProcessMemoryInfo(GetCurrentProcess(), &mut counters, counters.cb) };
    (ok != 0).then_some(counters.working_set_size as u64)
}

/// The process's resident memory in bytes.
#[cfg(not(windows))]
fn resident_bytes() -> Option<u64> {
    let statm = std::fs::read_to_string("/proc/self/statm").ok()?;
    let pages: u64 = statm.split_whitespace().nth(1)?.parse().ok()?;
    Some(pages * 4096)
}
