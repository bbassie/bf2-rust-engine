//! `--debug-nav`: draws the bots' navigation grid around the camera and the paths bots are
//! walking. Singleplayer and listen server only: the grid and the paths are our own server's
//! (`local_server`), fetched from its world every frame.

use std::sync::Arc;

use bevy::prelude::*;
use game_server::{
    Controls,
    bots::BotBrain,
    embedded::crossbeam_channel::Receiver,
    nav::{NavGrid, Navigation},
};
use game_shared::{protocol::Team, soldier::SoldierMotion};

use crate::{Cli, camera::PlayerCamera, local_server::LocalServer};

pub struct NavDebugPlugin;

impl Plugin for NavDebugPlugin {
    fn build(&self, app: &mut App) {
        app.init_gizmo_group::<PathGizmos>()
            .init_resource::<NavSnapshot>()
            .add_systems(Startup, configure_gizmos)
            .add_systems(
                Update,
                (fetch, (draw_grid, draw_paths))
                    .chain()
                    .run_if(|cli: Res<Cli>| cli.debug_nav)
                    .run_if(resource_exists::<LocalServer>),
            );
    }
}

/// A bot's way, from the server.
struct BotPath {
    team: Team,
    from: Vec3,
    /// Waypoints, and whether each needs a jump.
    path: Vec<(Vec3, bool)>,
    goal: Option<Vec3>,
}

/// The server's grid and the bots' paths, as last fetched.
#[derive(Resource, Default)]
struct NavSnapshot {
    grid: Option<Arc<NavGrid>>,
    paths: Vec<BotPath>,
    pending: Option<Receiver<(Option<Arc<NavGrid>>, Vec<BotPath>)>>,
}

fn fetch(server: Res<LocalServer>, mut snapshot: ResMut<NavSnapshot>) {
    if let Some((grid, paths)) = snapshot.pending.as_ref().and_then(|rx| rx.try_recv().ok()) {
        snapshot.grid = grid;
        snapshot.paths = paths;
        snapshot.pending = None;
    }
    if snapshot.pending.is_none() {
        snapshot.pending = Some(server.query(|world| {
            let grid = world.get_resource::<Navigation>().map(|nav| nav.0.clone());
            let mut bots = world.query::<(&BotBrain, &Team, Option<&Controls>)>();
            let paths = bots
                .iter(world)
                .filter_map(|(brain, team, controls)| {
                    let from = world.get::<SoldierMotion>(controls?.0)?.position;
                    Some(BotPath {
                        team: *team,
                        from,
                        path: brain.remaining_path().iter().map(|w| (w.position, w.jump)).collect(),
                        goal: brain.goal(),
                    })
                })
                .collect();
            (grid, paths)
        }));
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
    nav: Res<NavSnapshot>,
    camera: Query<&GlobalTransform, With<PlayerCamera>>,
) {
    let (Ok(camera), Some(grid)) = (camera.single(), nav.grid.as_deref()) else {
        return;
    };
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
fn draw_paths(mut gizmos: Gizmos<PathGizmos>, nav: Res<NavSnapshot>) {
    for bot in &nav.paths {
        let color = match bot.team {
            Team::Two => Color::srgb(1.0, 0.35, 0.2),
            _ => Color::srgb(0.25, 0.6, 1.0),
        };
        let lift = Vec3::Y * 0.3;
        gizmos.linestrip(
            std::iter::once(bot.from).chain(bot.path.iter().map(|w| w.0)).map(|p| p + lift),
            color,
        );
        for (waypoint, _) in bot.path.iter().filter(|w| w.1) {
            gizmos.sphere(Isometry3d::from_translation(*waypoint + lift), 0.3, Color::srgb(1.0, 1.0, 0.2));
        }
        if let Some(goal) = bot.goal {
            gizmos.circle(
                Isometry3d::new(goal + lift, Quat::from_rotation_x(std::f32::consts::FRAC_PI_2)),
                1.0,
                color,
            );
        }
    }
}
