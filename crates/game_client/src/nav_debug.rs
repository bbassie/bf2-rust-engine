//! `--debug-nav`: draws the bots' navigation grid around the camera and the paths bots are
//! walking. Singleplayer and listen server only, where the server's resources live in the
//! client's world.

use bevy::prelude::*;
use game_server::{
    Controls,
    bots::BotBrain,
    nav::{NavGrid, Navigation},
};
use game_shared::{protocol::Team, soldier::SoldierMotion};

use crate::{Cli, camera::PlayerCamera};

pub struct NavDebugPlugin;

impl Plugin for NavDebugPlugin {
    fn build(&self, app: &mut App) {
        app.init_gizmo_group::<PathGizmos>()
            .add_systems(Startup, configure_gizmos)
            .add_systems(
                Update,
                (draw_grid, draw_paths)
                    .run_if(|cli: Res<Cli>| cli.debug_nav)
                    .run_if(resource_exists::<Navigation>),
            );
    }
}

/// Bot paths, drawn on top of everything so they show through buildings.
#[derive(Default, Reflect, GizmoConfigGroup)]
struct PathGizmos;

/// Grid cells are drawn within this distance of the camera, meters.
const GRID_RADIUS: f32 = 30.0;

fn configure_gizmos(mut store: ResMut<GizmoConfigStore>) {
    let (config, _) = store.config_mut::<PathGizmos>();
    config.depth_bias = -1.0;
    config.line.width = 3.0;
}

/// Each cell as lines to its +X and +Z neighbours: green in the open, orange near walls, red
/// at the edge, blue where a jump or drop is needed. Cells without any link are magenta
/// crosses.
fn draw_grid(
    mut gizmos: Gizmos,
    nav: Res<Navigation>,
    camera: Query<&GlobalTransform, With<PlayerCamera>>,
) {
    let Ok(camera) = camera.single() else {
        return;
    };
    let grid: &NavGrid = &nav.0;
    let lift = Vec3::Y * 0.08;
    for c in grid.cells_near(camera.translation().xz(), GRID_RADIUS) {
        let cell = grid.cell(c);
        let at = grid.position(c) + lift;
        if cell.links.iter().all(|&l| l == NavGrid::NONE) {
            gizmos.cross(
                Isometry3d::from_translation(at),
                0.1,
                Color::srgb(1.0, 0.0, 1.0),
            );
            continue;
        }
        let color = match cell.dist {
            0 => Color::srgb(0.9, 0.1, 0.1),
            1..=2 => Color::srgb(1.0, 0.6, 0.1),
            _ => Color::srgb(0.2, 0.9, 0.3),
        };
        for dir in [0, 1] {
            let Some(n) = grid.neighbour(c, dir) else {
                continue;
            };
            let other = grid.cell(n);
            let color = if (other.y - cell.y).abs() > grid.walk_climb(cell, other) {
                Color::srgb(0.2, 0.5, 1.0)
            } else {
                color
            };
            gizmos.line(at, grid.position(n) + lift, color);
        }
    }
}

/// Every bot's remaining path from its soldier on, in its team's color, with jumps marked.
fn draw_paths(
    mut gizmos: Gizmos<PathGizmos>,
    bots: Query<(&BotBrain, &Team, Option<&Controls>)>,
    soldiers: Query<&SoldierMotion>,
) {
    for (brain, team, controls) in &bots {
        let Some(motion) = controls.and_then(|c| soldiers.get(c.0).ok()) else {
            continue;
        };
        let color = match team {
            Team::Two => Color::srgb(1.0, 0.35, 0.2),
            _ => Color::srgb(0.25, 0.6, 1.0),
        };
        let lift = Vec3::Y * 0.3;
        let path = brain.remaining_path();
        gizmos.linestrip(
            std::iter::once(motion.position)
                .chain(path.iter().map(|w| w.position))
                .map(|p| p + lift),
            color,
        );
        for waypoint in path.iter().filter(|w| w.jump) {
            gizmos.sphere(
                Isometry3d::from_translation(waypoint.position + lift),
                0.3,
                Color::srgb(1.0, 1.0, 0.2),
            );
        }
        if let Some(goal) = brain.goal() {
            gizmos.circle(
                Isometry3d::new(
                    goal + lift,
                    Quat::from_rotation_x(std::f32::consts::FRAC_PI_2),
                ),
                1.0,
                color,
            );
        }
    }
}
