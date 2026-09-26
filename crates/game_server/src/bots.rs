//! AI soldiers.
//!
//! A bot is an ordinary [`Player`] whose [`InputBuffer`] is filled by a [`BotBrain`] instead
//! of the network, so bots move with exactly the same rules as humans.
//!
//! This is the first, very small step towards BF2-style bots: for now they roam between
//! control points and get themselves unstuck. The BF2 AI is layered (commander strategy →
//! squad orders → individual behaviours with navmesh pathfinding) and will be built up here.

use std::f32::consts::{PI, TAU};

use bevy::prelude::*;
use bevy_replicon::prelude::*;
use game_shared::{
    input::{Buttons, InputFrame},
    level::LoadedLevel,
    protocol::{MatchInfo, Player, Team},
    soldier::SoldierMotion,
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

#[derive(Component)]
pub struct BotBrain {
    seq: u32,
    goal: Option<Vec3>,
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
            goal: None,
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
            Replicated,
        ));
    }
    info!("added {} bots", settings.bots - existing);
}

fn think(
    time: Res<Time>,
    level: Res<LoadedLevel>,
    match_info: Single<&MatchInfo>,
    mut bots: Query<(&mut BotBrain, &mut InputBuffer, &Team, Option<&Controls>)>,
    soldiers: Query<&SoldierMotion>,
) {
    let dt = time.delta_secs();
    let layout = level.game_mode(&match_info.mode, match_info.size);

    for (mut brain, mut buffer, team, controls) in &mut bots {
        let Some(motion) = controls.and_then(|c| soldiers.get(c.0).ok()) else {
            continue;
        };
        brain.seq = brain.seq.wrapping_add(1);

        let reached = brain
            .goal
            .is_none_or(|goal| flat(goal - motion.position).length() < 5.0);
        brain.goal_timer -= dt;
        if reached || brain.goal_timer <= 0.0 {
            brain.goal = Some(choose_goal(&level, layout, *team, motion.position));
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

/// Picks somewhere worth going: a control point the team doesn't hold, or a random spot.
fn choose_goal(
    level: &LoadedLevel,
    layout: Option<&game_data::GameModeDesc>,
    team: Team,
    from: Vec3,
) -> Vec3 {
    let team_id = match team {
        Team::One => 1,
        Team::Two => 2,
        Team::Spectator => 0,
    };
    if let Some(layout) = layout {
        let targets: Vec<_> = layout
            .control_points
            .iter()
            .filter(|cp| cp.initial_team != team_id)
            .collect();
        if let Some(cp) = fastrand::choice(&targets) {
            let angle = fastrand::f32() * TAU;
            let r = fastrand::f32() * cp.radius * 0.8;
            return Vec3::from_array(cp.position) + Vec3::new(angle.cos() * r, 0.0, angle.sin() * r);
        }
    }
    let Some(heightmap) = &level.heightmap else {
        return from;
    };
    let half = heightmap.world_size() * 0.4;
    let center = heightmap.center();
    center + Vec3::new(fastrand::f32() * 2.0 - 1.0, 0.0, fastrand::f32() * 2.0 - 1.0) * half
}

fn flat(v: Vec3) -> Vec3 {
    Vec3::new(v.x, 0.0, v.z)
}

fn turn_towards(current: f32, target: f32, max_step: f32) -> f32 {
    let mut delta = (target - current) % TAU;
    if delta > PI {
        delta -= TAU;
    } else if delta < -PI {
        delta += TAU;
    }
    current + delta.clamp(-max_step, max_step)
}
