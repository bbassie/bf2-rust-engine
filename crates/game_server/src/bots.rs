//! AI soldiers.
//!
//! A bot is an ordinary [`Player`] whose [`InputBuffer`] is filled by a [`BotBrain`] instead
//! of the network, so bots move with exactly the same rules as humans.
//!
//! This is the first, small step towards BF2-style bots: they roam between control points,
//! get themselves unstuck, and fight enemies they can see. The BF2 AI is layered (commander
//! strategy -> squad orders -> individual behaviours with navmesh pathfinding) and will be
//! built up here.

use std::f32::consts::{PI, TAU};

use avian3d::prelude::*;
use bevy::prelude::*;
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

use crate::{Controls, InputBuffer, ServerSettings, ServerSimSystems, balanced_team};

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
        .add_systems(
            FixedUpdate,
            think
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
) {
    let dt = time.delta_secs();
    let _ = &match_info;

    for (mut brain, mut buffer, team, controls) in &mut bots {
        let Some((own, motion, _, inventory)) = controls.and_then(|c| soldiers.get(c.0).ok()) else {
            brain.target = None;
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

        let reached = brain
            .goal
            .is_none_or(|goal| flat(goal - motion.position).length() < 3.0);
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
        let to_goal = flat(brain.goal.unwrap_or(motion.position) - motion.position);

        // Turn smoothly towards the goal, like a human with a mouse would.
        if to_goal.length_squared() > 0.01 {
            let desired = (-to_goal.x).atan2(-to_goal.z);
            brain.yaw = turn_towards(brain.yaw, desired, 4.0 * dt);
        }

        // Detect being stuck on geometry and wiggle free.
        let moved = flat(motion.position - brain.last_position).length();
        brain.last_position = motion.position;
        if moved < 0.5 * dt && brain.unstuck_timer <= 0.0 {
            brain.stuck_time += dt;
        } else {
            brain.stuck_time = 0.0;
        }
        if brain.stuck_time > 0.75 {
            brain.stuck_time = 0.0;
            brain.unstuck_timer = 0.6 + fastrand::f32() * 0.8;
            brain.unstuck_dir = if fastrand::bool() { 1.0 } else { -1.0 };
        }

        let mut frame = InputFrame {
            seq: brain.seq,
            yaw: brain.yaw,
            pitch: 0.0,
            weapon,
            ..default()
        };
        let mut movement = Vec2::Y;
        if brain.unstuck_timer > 0.0 {
            brain.unstuck_timer -= dt;
            movement = Vec2::new(brain.unstuck_dir, 0.3);
            frame.buttons |= Buttons::JUMP;
        } else if to_goal.length() > 40.0 {
            frame.buttons |= Buttons::SPRINT;
        }
        frame.set_movement(movement);
        buffer.push(frame);
    }
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
