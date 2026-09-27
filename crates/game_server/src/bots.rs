//! AI soldiers.
//!
//! A bot is an ordinary [`Player`] whose [`InputBuffer`] is filled by a [`BotBrain`] instead
//! of the network, so bots move with exactly the same rules as humans.
//!
//! This is the first, small step towards BF2-style bots: they roam between control points
//! along paths on the level's navigation grid ([`crate::nav`]), get themselves unstuck, and
//! fight enemies they can see. The BF2 AI is layered (commander strategy -> squad orders ->
//! individual behaviours) and will be built up here.

use std::{
    f32::consts::{PI, TAU},
    time::Instant,
};

use avian3d::prelude::*;
use bevy::{
    prelude::*,
    tasks::{AsyncComputeTaskPool, Task, futures::check_ready},
};
use bevy_replicon::prelude::*;
use game_shared::{
    conquest::{ControlPoint, Deployment, FlagState},
    input::{Buttons, InputFrame},
    level::LoadedLevel,
    physics::GameLayer,
    protocol::{ControlledBy, MatchInfo, Player, Team},
    soldier::{Soldier, SoldierMotion},
    weapons::Inventory,
};

use crate::{
    Controls, InputBuffer, ServerSettings, ServerSimSystems, balanced_team,
    nav::{NavPath, Navigation, Waypoint},
};

pub struct BotPlugin;

impl Plugin for BotPlugin {
    fn build(&self, app: &mut App) {
        app.configure_sets(
            FixedUpdate,
            (ServerSimSystems::Think, ServerSimSystems::ApplyInputs).chain(),
        )
        .add_systems(
            Update,
            spawn_bots
                .run_if(resource_exists::<LoadedLevel>)
                .run_if(in_state(ClientState::Disconnected)),
        )
        .init_resource::<BotStats>()
        .add_systems(
            FixedUpdate,
            (think, log_stats)
                .chain()
                .in_set(ServerSimSystems::Think)
                .run_if(resource_exists::<LoadedLevel>)
                .run_if(in_state(ClientState::Disconnected)),
        );
    }
}

const BOT_NAMES: &[&str] = &[
    "Alpha", "Bravo", "Charlie", "Delta", "Echo", "Foxtrot", "Golf", "Hotel", "India", "Juliet",
    "Kilo", "Lima", "Mike", "November", "Oscar", "Papa", "Quebec", "Romeo", "Sierra", "Tango",
    "Uniform", "Victor", "Whiskey", "Xray", "Yankee", "Zulu",
];

/// How far bots look for enemies, meters.
const SIGHT_RANGE: f32 = 150.0;

/// Per-minute movement statistics, logged to see how well bots get around.
#[derive(Resource, Default)]
pub struct BotStats {
    elapsed: f32,
    stuck_events: u32,
    stuck_seconds: f32,
    /// Bot-seconds spent alive and not fighting (the time stuck events can happen in).
    moving_seconds: f32,
    /// Meters walked in that time.
    moved: f32,
    /// New paths asked for because a bot made no progress towards its waypoint.
    no_progress: u32,
    paths: u32,
    partial_paths: u32,
    failed_paths: u32,
    path_seconds: f32,
    max_path_seconds: f32,
}

/// A finished path request.
struct PathResult {
    path: Option<NavPath>,
    seconds: f32,
}

#[derive(Component)]
pub struct BotBrain {
    seq: u32,
    /// Enemy soldier being engaged.
    target: Option<Entity>,
    scan_timer: f32,
    /// Seconds before reacting to a newly seen target.
    reaction: f32,
    pitch: f32,
    /// Slowly wandering aim offset (radians), so bots aren't perfect shots.
    aim_error: Vec2,
    /// Positive: holding the trigger for that long; negative: pausing between bursts.
    burst: f32,
    strafe: f32,
    goal: Option<Vec3>,
    /// The control point the goal is at; bots stay until their team holds it.
    goal_point: Option<Entity>,
    goal_timer: f32,
    yaw: f32,
    last_position: Vec3,
    stuck_time: f32,
    /// While positive, strafe in `unstuck_dir` and jump to get free.
    unstuck_timer: f32,
    unstuck_dir: f32,
    /// Recent stuck events; decays over time. Too many and the bot gives up on its goal.
    stuck_strikes: f32,
    path: Option<NavPath>,
    /// Index of the waypoint being walked to.
    waypoint: usize,
    /// The goal `path` (or the pending request) leads to.
    path_goal: Option<Vec3>,
    path_task: Option<Task<PathResult>>,
    /// Ask for a new path to the same goal (after getting stuck or pushed off the path).
    repath: bool,
    repath_cooldown: f32,
    /// Closest the bot got to the current waypoint, and for how long it hasn't got closer.
    waypoint_best: f32,
    waypoint_timer: f32,
    debug_slow: f32,
}

impl Default for BotBrain {
    fn default() -> Self {
        Self {
            seq: 0,
            target: None,
            scan_timer: 0.0,
            reaction: 0.0,
            pitch: 0.0,
            aim_error: Vec2::ZERO,
            burst: 0.0,
            strafe: 1.0,
            goal: None,
            goal_point: None,
            goal_timer: 0.0,
            yaw: fastrand::f32() * TAU,
            last_position: Vec3::ZERO,
            stuck_time: 0.0,
            unstuck_timer: 0.0,
            unstuck_dir: 1.0,
            stuck_strikes: 0.0,
            path: None,
            waypoint: 0,
            path_goal: None,
            path_task: None,
            repath: false,
            repath_cooldown: 0.0,
            waypoint_best: f32::MAX,
            waypoint_timer: 0.0,
            debug_slow: 0.0,
        }
    }
}

/// Where path following wants to go this tick.
enum Steer {
    Toward { target: Vec3, jump: bool },
    /// At the end of the path.
    Arrived,
}

impl BotBrain {
    /// Where the bot is heading.
    pub fn goal(&self) -> Option<Vec3> {
        self.goal
    }

    /// The part of the path still ahead, for debug views.
    pub fn remaining_path(&self) -> &[Waypoint] {
        self.path
            .as_ref()
            .map_or(&[], |p| &p.waypoints[self.waypoint.min(p.waypoints.len())..])
    }

    /// Requests paths as needed and walks them waypoint by waypoint. Without a path yet,
    /// heads straight for the goal.
    fn follow_path(
        &mut self,
        nav: &Navigation,
        goal: Vec3,
        motion: &SoldierMotion,
        dt: f32,
        stats: &mut BotStats,
    ) -> Steer {
        let position = motion.position;
        if let Some(task) = &mut self.path_task
            && let Some(result) = check_ready(task)
        {
            self.path_task = None;
            stats.paths += 1;
            stats.path_seconds += result.seconds;
            stats.max_path_seconds = stats.max_path_seconds.max(result.seconds);
            match &result.path {
                Some(path) if !path.complete => stats.partial_paths += 1,
                Some(_) => {}
                None => {
                    stats.failed_paths += 1;
                    self.repath_cooldown = 1.0;
                }
            }
            self.path = result.path;
            self.waypoint = 0;
            self.waypoint_best = f32::MAX;
        }

        self.repath_cooldown -= dt;
        if self.path_goal.is_none_or(|g| g.distance_squared(goal) > 0.25) {
            // New goal: the old path and any request for it are useless now.
            self.path = None;
            self.path_task = None;
            self.repath = true;
        }
        if self.repath && self.path_task.is_none() && self.repath_cooldown <= 0.0 {
            self.repath = false;
            self.repath_cooldown = 0.5;
            self.path_goal = Some(goal);
            let grid = nav.0.clone();
            self.path_task = Some(AsyncComputeTaskPool::get().spawn(async move {
                let started = Instant::now();
                let path = grid.find_path(position, goal);
                PathResult {
                    path,
                    seconds: started.elapsed().as_secs_f32(),
                }
            }));
        }

        let Some(path) = &self.path else {
            return Steer::Toward {
                target: goal,
                jump: false,
            };
        };
        while let Some(waypoint) = path.waypoints.get(self.waypoint) {
            let last = self.waypoint + 1 == path.waypoints.len();
            let close = flat(waypoint.position - position).length() < if last { 0.6 } else { 0.8 };
            if close && (waypoint.position.y - position.y).abs() < 1.5 {
                self.waypoint += 1;
                self.waypoint_best = f32::MAX;
            } else {
                break;
            }
        }
        let Some(waypoint) = path.waypoints.get(self.waypoint) else {
            self.path = None;
            return Steer::Arrived;
        };
        // Pushed off the path (by fighting, say) or fell off a ledge: find a new one.
        let previous = path.waypoints[self.waypoint.saturating_sub(1)].position;
        let (off_path, t) = segment_offset(flat(position), flat(previous), flat(waypoint.position));
        let path_height = previous.y + (waypoint.position.y - previous.y) * t;
        // Long straight stretches may cross humps.
        let height_tolerance = 1.5 + 0.05 * flat(waypoint.position - previous).length();
        if off_path > 3.0 || (position.y - path_height).abs() > height_tolerance {
            self.repath = true;
        }
        // Not getting any closer to the waypoint (sliding along a wall, say): same.
        let distance = flat(waypoint.position - position).length();
        if distance < self.waypoint_best - 0.3 {
            self.waypoint_best = distance;
            self.waypoint_timer = 0.0;
        } else {
            self.waypoint_timer += dt;
            if self.waypoint_timer > 2.0 {
                stats.no_progress += 1;
                self.repath = true;
                self.waypoint_best = distance;
                self.waypoint_timer = 0.0;
            }
        }
        Steer::Toward {
            target: waypoint.position,
            jump: waypoint.jump && motion.grounded && flat(waypoint.position - position).length() < 1.2,
        }
    }
}

fn spawn_bots(
    mut commands: Commands,
    settings: Res<ServerSettings>,
    bots: Query<(), With<BotBrain>>,
    teams: Query<&Team, With<Player>>,
) {
    let existing = bots.iter().count() as u32;
    if existing >= settings.bots {
        return;
    }
    let mut teams: Vec<Team> = teams.iter().copied().collect();
    for i in existing..settings.bots {
        let team = balanced_team(teams.iter());
        teams.push(team);
        let name = BOT_NAMES[i as usize % BOT_NAMES.len()];
        commands.spawn((
            Player {
                name: format!("{name} (bot)"),
                is_bot: true,
            },
            team,
            InputBuffer::default(),
            BotBrain::default(),
            Deployment {
                kit: fastrand::u8(0..7),
                ..default()
            },
            Replicated,
        ));
    }
    info!("added {} bots", settings.bots - existing);
}

#[allow(clippy::type_complexity)]
fn think(
    time: Res<Time>,
    level: Res<LoadedLevel>,
    spatial: SpatialQuery,
    match_info: Single<&MatchInfo>,
    mut bots: Query<(&mut BotBrain, &mut InputBuffer, &Team, Option<&Controls>)>,
    soldiers: Query<(Entity, &SoldierMotion, &ControlledBy, Option<&Inventory>), With<Soldier>>,
    teams: Query<&Team>,
    control_points: Query<(Entity, &ControlPoint, &FlagState)>,
    nav: Option<Res<Navigation>>,
    mut stats: ResMut<BotStats>,
) {
    let dt = time.delta_secs();
    let _ = &match_info;

    for (mut brain, mut buffer, team, controls) in &mut bots {
        let Some((own, motion, _, inventory)) = controls.and_then(|c| soldiers.get(c.0).ok()) else {
            // Dead: start over from the next spawn.
            brain.target = None;
            brain.goal = None;
            brain.path = None;
            brain.path_task = None;
            brain.path_goal = None;
            continue;
        };
        brain.seq = brain.seq.wrapping_add(1);
        let eye = motion.eye_position();

        // Look for the nearest enemy in sight a few times per second.
        brain.scan_timer -= dt;
        if brain.scan_timer <= 0.0 {
            brain.scan_timer = 0.25;
            let visible = |target: Vec3| {
                let to = target - eye;
                Dir3::new(to).is_ok_and(|dir| {
                    spatial
                        .cast_ray(eye, dir, to.length(), true, &SpatialQueryFilter::from_mask(GameLayer::World))
                        .is_none()
                })
            };
            let best = soldiers
                .iter()
                .filter(|(entity, _, controlled, _)| {
                    *entity != own && teams.get(controlled.0).is_ok_and(|t| *t != *team)
                })
                .map(|(entity, m, _, _)| (entity, m.position.distance(motion.position), m.position))
                .filter(|(_, distance, _)| *distance < SIGHT_RANGE)
                .filter(|(_, _, position)| visible(*position + Vec3::Y * 1.2))
                .min_by(|a, b| a.1.total_cmp(&b.1))
                .map(|(entity, _, _)| entity);
            if best != brain.target {
                brain.target = best;
                brain.reaction = 0.25 + fastrand::f32() * 0.5;
                brain.aim_error = Vec2::new(fastrand::f32() - 0.5, fastrand::f32() - 0.5) * 0.15;
            }
        }
        let target = brain.target.and_then(|t| soldiers.get(t).ok()).map(|(_, m, _, _)| *m);
        if target.is_none() {
            brain.target = None;
        }
        let weapon = inventory.map_or(0, |i| i.active);

        if let Some(target) = target {
            // Aim at the chest, with an error that shrinks while tracking.
            let aim_at = target.position + Vec3::Y * if target.stance == game_shared::soldier::Stance::Prone { 0.3 } else { 1.2 };
            let to = aim_at - eye;
            let desired_yaw = (-to.x).atan2(-to.z) + brain.aim_error.x;
            let desired_pitch = to.y.atan2(Vec2::new(to.x, to.z).length()) + brain.aim_error.y;
            brain.aim_error *= 1.0 - (0.8 * dt).min(1.0);
            brain.aim_error += Vec2::new(fastrand::f32() - 0.5, fastrand::f32() - 0.5) * 0.02 * (to.length() / 50.0);
            brain.yaw = turn_towards(brain.yaw, desired_yaw, 5.0 * dt);
            brain.pitch += (desired_pitch - brain.pitch).clamp(-4.0 * dt, 4.0 * dt);
            brain.reaction -= dt;

            let on_target = (angle_delta(brain.yaw, desired_yaw).abs() + (brain.pitch - desired_pitch).abs()) < 0.08;
            let mut frame = InputFrame {
                seq: brain.seq,
                yaw: brain.yaw,
                pitch: brain.pitch,
                weapon,
                ..default()
            };
            if brain.reaction <= 0.0 && on_target {
                brain.burst -= dt;
                if brain.burst < -0.3 - fastrand::f32() * 0.3 {
                    brain.burst = 0.2 + fastrand::f32() * 0.4;
                }
                if brain.burst > 0.0 {
                    frame.buttons |= Buttons::FIRE;
                }
            }
            // Strafe while fighting; switch direction now and then.
            if fastrand::f32() < dt * 0.7 {
                brain.strafe = -brain.strafe;
            }
            frame.set_movement(Vec2::new(brain.strafe, 0.0));
            buffer.push(frame);
            continue;
        }
        brain.pitch = 0.0;

        // Following a path, its end tells when the goal is reached (see `Steer::Arrived`).
        let reach = if nav.is_some() { 0.5 } else { 3.0 };
        let reached = brain
            .goal
            .is_none_or(|goal| flat(goal - motion.position).length() < reach);
        brain.goal_timer -= dt;
        // At a flag we don't hold yet: keep moving around inside its radius until it's ours.
        let holding_on = brain.goal_point.and_then(|e| control_points.get(e).ok()).filter(
            |(_, cp, state)| state.owner != *team && cp.contains(motion.position),
        );
        if let Some((_, cp, _)) = holding_on {
            if reached {
                brain.goal = Some(point_near(cp.position, cp.radius * 0.6));
            }
        } else if reached || brain.goal_timer <= 0.0 {
            let (goal, point) = choose_goal(&level, &control_points, *team, motion.position);
            brain.goal = Some(goal);
            brain.goal_point = point;
            brain.goal_timer = 40.0 + fastrand::f32() * 40.0;
        }
        let goal = brain.goal.unwrap_or(motion.position);

        // Detect being stuck on geometry: wiggle free, then find a new path from there.
        let moved = flat(motion.position - brain.last_position).length();
        // More than a tick's worth: respawned.
        let moved = if moved > 1.0 { 0.0 } else { moved };
        brain.last_position = motion.position;
        stats.moving_seconds += dt;
        stats.moved += moved;
        if moved < 0.5 * dt && brain.unstuck_timer <= 0.0 {
            brain.stuck_time += dt;
            stats.stuck_seconds += dt;
            brain.debug_slow += dt;
            if brain.debug_slow > 5.0 {
                brain.debug_slow = 0.0;
                let target = brain.path.as_ref().and_then(|p| p.waypoints.get(brain.waypoint)).map(|w| w.position);
                warn!(
                    "DEBUG slow bot at {:?} goal {:?} target {:?} wp {}/{} task {} unstuck {:.2} stuck_time {:.2} grounded {} vel {:?}",
                    motion.position,
                    brain.goal,
                    target,
                    brain.waypoint,
                    brain.path.as_ref().map_or(0, |p| p.waypoints.len()),
                    brain.path_task.is_some(),
                    brain.unstuck_timer,
                    brain.stuck_time,
                    motion.grounded,
                    motion.velocity,
                );
                if let Some(nav) = nav.as_deref()
                    && let Some((cx, cz)) = nav.0.column_at(motion.position.x, motion.position.z)
                {
                    let mut dump = String::new();
                    for z in cz.saturating_sub(4)..cz + 5 {
                        for x in cx.saturating_sub(4)..cx + 5 {
                            let cells: Vec<String> = nav.0.column(x, z).map(|i| {
                                let c = nav.0.cell(crate::nav::CellRef { x, z, index: i });
                                let links: String = c.links.iter().map(|&l| if l == 255 { '-' } else { char::from(b'0' + l.min(9)) }).collect();
                                format!("{:.2}/{}/{}", c.y, c.dist, links)
                            }).collect();
                            dump += &format!("{:>28}", cells.join(","));
                        }
                        dump += "\n";
                    }
                    warn!("DEBUG grid around {cx},{cz} (+x right, +z down):\n{dump}");
                }
            }
        } else {
            brain.stuck_time = 0.0;
        }
        brain.stuck_strikes = (brain.stuck_strikes - dt / 10.0).max(0.0);
        if brain.stuck_time > 0.75 {
            stats.stuck_events += 1;
            brain.stuck_time = 0.0;
            brain.unstuck_timer = 0.6 + fastrand::f32() * 0.8;
            // First try jumping ahead (a ledge the grid thinks is lower), then sideways.
            brain.unstuck_dir = match brain.stuck_strikes < 0.5 {
                true => 0.0,
                false if fastrand::bool() => 1.0,
                false => -1.0,
            };
            brain.repath = true;
            brain.stuck_strikes += 1.0;
            if brain.stuck_strikes > 3.0 {
                // Keeps failing here: go somewhere else.
                brain.stuck_strikes = 0.0;
                brain.goal = None;
            }
        }

        // Walk to the next corner of the path, or straight at the goal without a grid.
        let (target, jump) = match nav.as_deref() {
            Some(nav) => match brain.follow_path(nav, goal, motion, dt, &mut stats) {
                Steer::Toward { target, jump } => (target, jump),
                Steer::Arrived => {
                    // Done; the goal logic picks what's next.
                    brain.goal = Some(motion.position);
                    (motion.position, false)
                }
            },
            None => (goal, false),
        };
        let to_target = flat(target - motion.position);

        // Turn smoothly towards where we're going, like a human with a mouse would, while
        // moving straight there.
        if to_target.length_squared() > 0.01 {
            let desired = (-to_target.x).atan2(-to_target.z);
            brain.yaw = turn_towards(brain.yaw, desired, 5.0 * dt);
        }
        let local = Quat::from_rotation_y(-brain.yaw) * to_target.normalize_or_zero();
        let mut movement = Vec2::new(local.x, -local.z);

        let mut frame = InputFrame {
            seq: brain.seq,
            yaw: brain.yaw,
            pitch: 0.0,
            weapon,
            ..default()
        };
        if brain.unstuck_timer > 0.0 {
            brain.unstuck_timer -= dt;
            movement = Vec2::new(brain.unstuck_dir, if brain.unstuck_dir == 0.0 { 1.0 } else { 0.3 });
            frame.buttons |= Buttons::JUMP;
        } else {
            if jump {
                frame.buttons |= Buttons::JUMP;
            }
            if movement.y > 0.7 && flat(goal - motion.position).length() > 30.0 {
                frame.buttons |= Buttons::SPRINT;
            }
        }
        frame.set_movement(movement);
        buffer.push(frame);
    }
}

fn log_stats(time: Res<Time>, mut stats: ResMut<BotStats>, bots: Query<(), With<BotBrain>>) {
    stats.elapsed += time.delta_secs();
    if stats.elapsed < 60.0 {
        return;
    }
    if !bots.is_empty() {
        info!(
            "bots: {} stuck events in the last minute ({:.0} s stuck of {:.0} bot-seconds moving, \
             {:.1} m/s, {} bots); {} paths ({} partial, {} failed, {} for lack of progress), \
             {:.1} ms avg, {:.1} ms max",
            stats.stuck_events,
            stats.stuck_seconds,
            stats.moving_seconds,
            stats.moved / stats.moving_seconds.max(1.0),
            bots.iter().count(),
            stats.paths,
            stats.partial_paths,
            stats.failed_paths,
            stats.no_progress,
            stats.path_seconds * 1000.0 / stats.paths.max(1) as f32,
            stats.max_path_seconds * 1000.0,
        );
    }
    *stats = BotStats::default();
}

/// Picks somewhere worth going: preferably a nearby control point the team doesn't hold,
/// sometimes one it holds (to defend), otherwise a random spot.
fn choose_goal(
    level: &LoadedLevel,
    control_points: &Query<(Entity, &ControlPoint, &FlagState)>,
    team: Team,
    from: Vec3,
) -> (Vec3, Option<Entity>) {
    let mut targets: Vec<(Entity, &ControlPoint, f32)> = control_points
        .iter()
        .filter(|(_, cp, _)| !cp.uncapturable)
        .map(|(entity, cp, state)| {
            // Closer and enemy-held points are more attractive.
            let distance = flat(cp.position - from).length().max(20.0);
            let interest = if state.owner == team { 0.25 } else { 1.0 };
            (entity, cp, interest / distance)
        })
        .collect();
    let total: f32 = targets.iter().map(|(.., w)| w).sum();
    if total > 0.0 {
        let mut pick = fastrand::f32() * total;
        targets.sort_by(|a, b| b.2.total_cmp(&a.2));
        for (entity, cp, weight) in &targets {
            pick -= weight;
            if pick <= 0.0 {
                return (point_near(cp.position, cp.radius * 0.6), Some(*entity));
            }
        }
    }
    let Some(heightmap) = &level.heightmap else {
        return (from, None);
    };
    let half = heightmap.world_size() * 0.4;
    let center = heightmap.center();
    let spot = center + Vec3::new(fastrand::f32() * 2.0 - 1.0, 0.0, fastrand::f32() * 2.0 - 1.0) * half;
    (spot, None)
}

/// A random point within `radius` of `center`, on the same height.
fn point_near(center: Vec3, radius: f32) -> Vec3 {
    let angle = fastrand::f32() * TAU;
    let r = fastrand::f32().sqrt() * radius;
    center + Vec3::new(angle.cos() * r, 0.0, angle.sin() * r)
}

/// Distance from `p` to the segment `a`-`b`, and how far along the segment (0..1) the
/// closest point is.
fn segment_offset(p: Vec3, a: Vec3, b: Vec3) -> (f32, f32) {
    let ab = b - a;
    let t = ((p - a).dot(ab) / ab.length_squared().max(1e-6)).clamp(0.0, 1.0);
    (p.distance(a + ab * t), t)
}

fn flat(v: Vec3) -> Vec3 {
    Vec3::new(v.x, 0.0, v.z)
}

fn angle_delta(from: f32, to: f32) -> f32 {
    let mut delta = (to - from) % TAU;
    if delta > PI {
        delta -= TAU;
    } else if delta < -PI {
        delta += TAU;
    }
    delta
}

fn turn_towards(current: f32, target: f32, max_step: f32) -> f32 {
    current + angle_delta(current, target).clamp(-max_step, max_step)
}
