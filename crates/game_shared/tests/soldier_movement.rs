//! Soldier movement against simple geometry: steps, stairs, slopes, jumps, determinism.
//! Runs `step_soldier` headless with avian's spatial queries, no level needed.

use avian3d::prelude::*;
use bevy::{ecs::system::RunSystemOnce, prelude::*, time::TimeUpdateStrategy};
use game_data::RopeKind;
use game_shared::{
    input::{Buttons, InputFrame},
    ladder::LadderPart,
    physics::GameLayer,
    rope::{self, Rope},
    soldier::{SoldierMotion, SoldierPlugin, SoldierShapes, SoldierTuning, Stance, step_soldier},
};

const DT: f32 = 1.0 / 60.0;

struct Block {
    size: Vec3,
    transform: Transform,
}

/// A box given by its minimum corner and size.
fn block(min: [f32; 3], size: [f32; 3]) -> Block {
    let (min, size) = (Vec3::from(min), Vec3::from(size));
    Block {
        size,
        transform: Transform::from_translation(min + size * 0.5),
    }
}

/// A ramp whose top starts at ground level at `z` and rises towards -Z.
fn ramp(z: f32, degrees: f32, length: f32) -> Block {
    let angle = degrees.to_radians();
    let normal = Vec3::new(0.0, angle.cos(), angle.sin());
    let top_middle = Vec3::new(
        0.0,
        0.5 * length * angle.sin(),
        z - 0.5 * length * angle.cos(),
    );
    let size = Vec3::new(6.0, 0.5, length);
    Block {
        size,
        transform: Transform::from_translation(top_middle - normal * size.y * 0.5)
            .with_rotation(Quat::from_rotation_x(angle)),
    }
}

/// Ground is the top of a big slab at y = 0. `trimesh` builds everything as one triangle
/// mesh like the level's static objects, otherwise cuboids.
fn world(blocks: &[Block], trimesh: bool) -> App {
    let mut app = physics_app();
    let layers = CollisionLayers::new(GameLayer::World, LayerMask::ALL);
    let ground = block([-100.0, -1.0, -100.0], [200.0, 1.0, 200.0]);
    let all = std::iter::once(&ground).chain(blocks);
    if trimesh {
        let (mut vertices, mut indices) = (Vec::new(), Vec::new());
        for b in all {
            let transform = b.transform;
            let base = vertices.len() as u32;
            for i in 0..8 {
                let corner = Vec3::new(
                    if i & 1 == 0 { -0.5 } else { 0.5 },
                    if i & 2 == 0 { -0.5 } else { 0.5 },
                    if i & 4 == 0 { -0.5 } else { 0.5 },
                ) * b.size;
                vertices.push(transform.transform_point(corner));
            }
            const FACES: [[u32; 4]; 6] = [
                [0, 2, 6, 4],
                [1, 5, 7, 3],
                [0, 4, 5, 1],
                [2, 3, 7, 6],
                [0, 1, 3, 2],
                [4, 6, 7, 5],
            ];
            // Counter-clockwise seen from outside, like the imported meshes.
            for [a, b, c, d] in FACES {
                indices.push([base + a, base + c, base + b]);
                indices.push([base + a, base + d, base + c]);
            }
        }
        app.world_mut().spawn((
            RigidBody::Static,
            Collider::trimesh(vertices, indices),
            Transform::default(),
            layers,
        ));
    } else {
        for b in all {
            app.world_mut().spawn((
                RigidBody::Static,
                Collider::cuboid(b.size.x, b.size.y, b.size.z),
                b.transform,
                layers,
            ));
        }
    }
    ready(app)
}

fn physics_app() -> App {
    let mut app = App::new();
    app.add_plugins((MinimalPlugins, TransformPlugin, AssetPlugin::default()))
        .init_asset::<Mesh>()
        .add_plugins((PhysicsPlugins::default(), SoldierPlugin))
        .insert_resource(TimeUpdateStrategy::FixedTimesteps(1));
    app
}

/// Lets physics pick up the colliders.
fn ready(mut app: App) -> App {
    app.finish();
    app.cleanup();
    for _ in 0..5 {
        app.update();
    }
    app
}

fn input(right: f32, forward: f32, buttons: Buttons) -> InputFrame {
    let mut frame = InputFrame {
        buttons,
        ..default()
    };
    frame.set_movement(Vec2::new(right, forward));
    frame
}

/// Runs `inputs` from `start` and returns the state after every tick.
fn simulate(app: &mut App, start: SoldierMotion, inputs: Vec<InputFrame>) -> Vec<SoldierMotion> {
    simulate_water(app, start, inputs, None)
}

/// [`simulate`] with a level water height, for the swimming tests.
fn simulate_water(
    app: &mut App,
    start: SoldierMotion,
    inputs: Vec<InputFrame>,
    water: Option<f32>,
) -> Vec<SoldierMotion> {
    app.world_mut()
        .run_system_once(
            move |tuning: Res<SoldierTuning>, shapes: Res<SoldierShapes>, mover: MoveAndSlide| {
                let mut m = start;
                inputs
                    .iter()
                    .map(|frame| {
                        step_soldier(&mut m, frame, DT, &tuning, &shapes, &mover, water);
                        m
                    })
                    .collect::<Vec<_>>()
            },
        )
        .unwrap()
}

/// Standing still at `feet` until settled.
fn settled(app: &mut App, feet: Vec3) -> SoldierMotion {
    let trace = simulate(
        app,
        SoldierMotion::at(feet, 0.0),
        vec![input(0.0, 0.0, Buttons::empty()); 60],
    );
    let m = *trace.last().unwrap();
    assert!(m.grounded, "not grounded after settling: {m:?}");
    m
}

/// Yaw 0 faces -Z: "forward" walks towards -Z.
fn walk(app: &mut App, start: SoldierMotion, ticks: usize, buttons: Buttons) -> Vec<SoldierMotion> {
    simulate(app, start, vec![input(0.0, 1.0, buttons); ticks])
}

fn print_trace(name: &str, trace: &[SoldierMotion]) {
    println!("--- {name}");
    for (i, m) in trace.iter().enumerate() {
        println!(
            "{i:3} pos ({:7.3} {:6.3} {:7.3}) vel ({:6.2} {:6.2} {:6.2}) {} {:?}",
            m.position.x,
            m.position.y,
            m.position.z,
            m.velocity.x,
            m.velocity.y,
            m.velocity.z,
            if m.grounded { "G" } else { "-" },
            m.stance,
        );
    }
}

fn horizontal_speed(m: &SoldierMotion) -> f32 {
    Vec2::new(m.velocity.x, m.velocity.z).length()
}

#[test]
fn stands_still_without_drifting() {
    for trimesh in [false, true] {
        let mut app = world(&[], trimesh);
        let m = settled(&mut app, Vec3::new(0.0, 0.5, 0.0));
        assert!((m.position.y - 0.01).abs() < 0.005, "{m:?}");
        let trace = simulate(&mut app, m, vec![input(0.0, 0.0, Buttons::empty()); 30]);
        if let Some(t) = trace.iter().find(|t| **t != m) {
            panic!(
                "standing still changed state (trimesh {trimesh}):
{m:?}
{t:?}"
            );
        }
    }
}

#[test]
fn walks_at_bf2_speeds() {
    let tuning = SoldierTuning::default();
    let mut app = world(&[], true);
    let m = settled(&mut app, Vec3::ZERO);
    for (buttons, speed) in [
        (Buttons::empty(), tuning.run_speed),
        (Buttons::SPRINT, tuning.sprint_speed),
        (Buttons::CROUCH, tuning.crouch_speed),
        (Buttons::PRONE, tuning.prone_speed),
    ] {
        let trace = walk(&mut app, m, 90, buttons);
        let last = trace.last().unwrap();
        assert!(
            (horizontal_speed(last) - speed).abs() < 1e-3,
            "{buttons:?}: {last:?}"
        );
        if let Some(t) = trace
            .iter()
            .find(|t| !t.grounded || (t.position.y - m.position.y).abs() > 1e-3)
        {
            panic!(
                "{buttons:?} bumped: {m:?}
{t:?}"
            );
        }
    }
}

#[test]
fn steps_up_ledges() {
    let tuning = SoldierTuning::default();
    for trimesh in [false, true] {
        for height in [0.1, 0.2, 0.3, 0.4, 0.45] {
            // A platform starting 1 m ahead.
            let mut app = world(&[block([-3.0, 0.0, -6.0], [6.0, height, 5.0])], trimesh);
            let m = settled(&mut app, Vec3::ZERO);
            let trace = walk(&mut app, m, 90, Buttons::empty());
            let last = trace.last().unwrap();
            if (last.position.y - (height + 0.01)).abs() >= 0.01
                || trace.iter().any(|t| !t.grounded)
            {
                print_trace(&format!("step {height} trimesh {trimesh}"), &trace[..40]);
            }
            assert!(
                (last.position.y - (height + 0.01)).abs() < 0.01 && last.position.z < -3.0,
                "stuck at a {height} m step (trimesh {trimesh}): {last:?}"
            );
            assert!(
                trace.iter().all(|t| t.grounded),
                "left the ground on a {height} m step"
            );
            // Walking over the step costs no speed.
            assert!((horizontal_speed(last) - tuning.run_speed).abs() < 1e-3);
        }
        // Higher than a step: blocked.
        let mut app = world(&[block([-3.0, 0.0, -6.0], [6.0, 0.6, 5.0])], trimesh);
        let m = settled(&mut app, Vec3::ZERO);
        let trace = walk(&mut app, m, 90, Buttons::empty());
        let last = *trace.last().unwrap();
        if !(last.position.y < 0.05 && last.position.z > -1.0) {
            print_trace("0.6 m", &trace[..40]);
            panic!("climbed 0.6 m (trimesh {trimesh}): {last:?}");
        }
    }
}

#[test]
fn crosses_curbs_diagonally() {
    for trimesh in [false, true] {
        let mut app = world(&[block([-10.0, 0.0, -20.0], [20.0, 0.15, 19.0])], trimesh);
        let m = settled(&mut app, Vec3::ZERO);
        let trace = simulate(&mut app, m, vec![input(0.7, 0.7, Buttons::empty()); 90]);
        let last = trace.last().unwrap();
        assert!(
            (last.position.y - 0.16).abs() < 0.01 && last.position.z < -2.0,
            "{last:?}"
        );
    }
}

fn stairs(steps: usize, rise: f32, run: f32) -> Vec<Block> {
    // Going up towards -Z from z = -1.
    (0..steps)
        .map(|i| {
            let z = -1.0 - run * (i + 1) as f32;
            block([-2.0, 0.0, z], [4.0, rise * (i + 1) as f32, run])
        })
        .chain(std::iter::once(block(
            [-2.0, 0.0, -1.0 - run * steps as f32 - 40.0],
            [4.0, rise * steps as f32, 40.0],
        )))
        .collect()
}

#[test]
fn climbs_and_descends_stairs() {
    let tuning = SoldierTuning::default();
    for trimesh in [false, true] {
        for (rise, run) in [(0.17, 0.28), (0.25, 0.3), (0.4, 0.4)] {
            let steps = 10;
            let top = rise * steps as f32;
            let mut app = world(&stairs(steps, rise, run), trimesh);
            for buttons in [Buttons::empty(), Buttons::SPRINT, Buttons::CROUCH] {
                let m = settled(&mut app, Vec3::ZERO);
                let up = walk(&mut app, m, 240, buttons);
                let last = up.last().unwrap();
                if (last.position.y - (top + 0.01)).abs() >= 0.01 {
                    print_trace("stairs up", &up[..80]);
                }
                assert!(
                    (last.position.y - (top + 0.01)).abs() < 0.01,
                    "didn't reach the top of {rise}/{run} stairs ({buttons:?}, trimesh {trimesh}): {last:?}"
                );
                let airborne = up.iter().filter(|t| !t.grounded).count();
                assert_eq!(
                    airborne, 0,
                    "left the ground walking up {rise}/{run} stairs"
                );
                if rise == 0.17 && buttons == Buttons::empty() && trimesh {
                    print_trace("up 0.17/0.28", &up[..70]);
                }
                // Height never goes down while climbing (beyond millimeters): no bouncing.
                if let Some(i) = up
                    .windows(2)
                    .position(|w| w[1].position.y < w[0].position.y - 5e-3)
                {
                    print_trace("dip", &up[i.saturating_sub(10)..i + 5]);
                    panic!(
                        "dipped walking up {rise}/{run} stairs ({buttons:?}, trimesh {trimesh}) at {i}"
                    );
                }

                // Turn around and walk down.
                let mut top_state = *last;
                top_state.yaw = std::f32::consts::PI;
                let mut frames = vec![input(0.0, 1.0, buttons); 240];
                for f in &mut frames {
                    f.yaw = std::f32::consts::PI;
                }
                let down = simulate(&mut app, top_state, frames);
                let last = down.last().unwrap();
                assert!(last.position.y < 0.02, "didn't get down: {last:?}");
                let airborne = down.iter().filter(|t| !t.grounded).count();
                assert_eq!(
                    airborne, 0,
                    "fell walking down {rise}/{run} stairs ({buttons:?}, trimesh {trimesh})"
                );
                if let Some(i) = down
                    .windows(2)
                    .position(|w| w[1].position.y > w[0].position.y + 5e-3)
                {
                    print_trace("bounce", &down[i.saturating_sub(10)..i + 5]);
                    panic!(
                        "bounced walking down {rise}/{run} stairs ({buttons:?}, trimesh {trimesh}) at {i}"
                    );
                }
                let speed = match buttons {
                    Buttons::SPRINT => tuning.sprint_speed,
                    Buttons::CROUCH => tuning.crouch_speed,
                    _ => tuning.run_speed,
                };
                // Stairs slow nobody down, up or down.
                let slow_up = up[60..]
                    .iter()
                    .filter(|t| horizontal_speed(t) < speed - 1e-3)
                    .count();
                if slow_up > 0 {
                    print_trace("slow up", &up[..110]);
                }
                assert_eq!(
                    slow_up, 0,
                    "lost speed walking up {rise}/{run} stairs ({buttons:?}, trimesh {trimesh})"
                );
                // Full speed all the way down once accelerated.
                let slow = down[60..]
                    .iter()
                    .filter(|t| t.position.z < -1.5 && horizontal_speed(t) < speed - 1e-3)
                    .count();
                assert_eq!(
                    slow, 0,
                    "lost speed walking down {rise}/{run} stairs ({buttons:?})"
                );
            }
        }
    }
}

#[test]
fn walks_slopes() {
    let tuning = SoldierTuning::default();
    for trimesh in [false, true] {
        for degrees in [15.0f32, 30.0, 45.0] {
            let mut app = world(&[ramp(-1.0, degrees, 30.0)], trimesh);
            let m = settled(&mut app, Vec3::ZERO);
            let up = walk(&mut app, m, 150, Buttons::empty());
            let last = up.last().unwrap();
            let rise = last.position.y - m.position.y;
            println!("{degrees}° trimesh {trimesh}: rose {rise:.2} m, {last:?}");
            assert!(
                up.iter().all(|t| t.grounded),
                "left the ground walking up {degrees}°"
            );
            assert!(
                (horizontal_speed(last) - tuning.run_speed).abs() < 1e-3,
                "slowed down on {degrees}°: {last:?}"
            );
            assert!(rise > 1.0, "didn't climb {degrees}°");

            // And back down without leaving the ground.
            let mut top = *last;
            top.yaw = std::f32::consts::PI;
            let mut frames = vec![input(0.0, 1.0, Buttons::SPRINT); 150];
            for f in &mut frames {
                f.yaw = std::f32::consts::PI;
            }
            let down = simulate(&mut app, top, frames);
            assert!(
                down.iter().all(|t| t.grounded),
                "left the ground walking down {degrees}°"
            );
        }
    }
}

#[test]
fn slides_off_steep_slopes() {
    for trimesh in [false, true] {
        let mut app = world(&[ramp(-1.0, 65.0, 10.0)], trimesh);
        let m = settled(&mut app, Vec3::ZERO);
        let up = walk(&mut app, m, 120, Buttons::SPRINT);
        let highest = up.iter().map(|t| t.position.y).fold(0.0, f32::max);
        assert!(
            highest < 0.5,
            "climbed a 65° slope to {highest} (trimesh {trimesh})"
        );

        // Dropped onto it: slides down, doesn't stick.
        let start = SoldierMotion::at(Vec3::new(0.0, 4.0, -3.5), 0.0);
        let trace = simulate(
            &mut app,
            start,
            vec![input(0.0, 0.0, Buttons::empty()); 180],
        );
        let last = trace.last().unwrap();
        assert!(
            last.position.y < 0.05 && last.grounded,
            "stuck on a 65° slope: {last:?}"
        );
    }
}

#[test]
fn jumps_like_bf2() {
    let tuning = SoldierTuning::default();
    let mut app = world(&[], true);
    let m = settled(&mut app, Vec3::ZERO);

    // Standing jump, button held throughout: one jump, no bunny hopping.
    let trace = simulate(&mut app, m, vec![input(0.0, 0.0, Buttons::JUMP); 150]);
    let apex = trace.iter().map(|t| t.position.y).fold(0.0, f32::max) - m.position.y;
    let airtime = trace.iter().filter(|t| !t.grounded).count() as f32 * DT;
    println!("apex {apex:.3} m, airtime {airtime:.3} s");
    // Semi-implicit Euler peaks half a tick of launch speed lower than the continuous apex.
    let expected = tuning.jump_speed * tuning.jump_speed / (2.0 * tuning.gravity)
        - 0.5 * tuning.jump_speed * DT;
    assert!((apex - expected).abs() < 0.05, "apex {apex}");
    assert!(
        (airtime - 2.0 * tuning.jump_speed / tuning.gravity).abs() < 0.05,
        "airtime {airtime}"
    );
    let jumps = std::iter::once(&m)
        .chain(&trace)
        .collect::<Vec<_>>()
        .windows(2)
        .filter(|w| w[0].grounded && !w[1].grounded)
        .count();
    assert_eq!(jumps, 1, "holding jump jumped again");

    // Running jump: keeps its momentum, little air control, slowed on landing.
    let run = *walk(&mut app, m, 60, Buttons::empty()).last().unwrap();
    let mut frames = vec![input(0.0, 1.0, Buttons::JUMP)];
    frames.extend(vec![input(1.0, 0.0, Buttons::empty()); 60]);
    let trace = simulate(&mut app, run, frames);
    let landed = trace.iter().position(|t| t.grounded).expect("never landed");
    let at_landing = trace[landed];
    println!("landed after {landed} ticks: {at_landing:?}");
    assert!(at_landing.recovery > 0.0, "no landing recovery");
    let drift = at_landing.position.x - run.position.x;
    let distance = run.position.z - at_landing.position.z;
    assert!(
        distance > 0.9 * tuning.run_speed * landed as f32 * DT,
        "lost momentum"
    );
    assert!(drift.abs() < 0.5, "too much air control: {drift}");

    // A fresh press right on landing doesn't jump until recovered.
    let mut frames = vec![input(0.0, 1.0, Buttons::JUMP)];
    frames.extend(vec![input(0.0, 1.0, Buttons::empty()); landed]);
    frames.extend(vec![input(0.0, 1.0, Buttons::JUMP); 2]);
    let trace = simulate(&mut app, run, frames);
    assert!(
        trace.last().unwrap().grounded,
        "jumped again right on landing"
    );
}

#[test]
fn replays_identically() {
    let mut app = world(&stairs(10, 0.17, 0.28), true);
    let m = settled(&mut app, Vec3::new(0.3, 0.0, 0.0));
    let mut frames = Vec::new();
    for i in 0..300u32 {
        let mut f = input((i as f32 * 0.05).sin(), 1.0, Buttons::empty());
        f.yaw = (i as f32 * 0.013).sin() * 0.6;
        if i % 70 == 5 {
            f.buttons |= Buttons::JUMP;
        }
        if (100..140).contains(&i) {
            f.buttons |= Buttons::CROUCH;
        }
        if i > 200 {
            f.buttons |= Buttons::SPRINT;
        }
        frames.push(f);
    }
    let a = simulate(&mut app, m, frames.clone());
    // Replaying from any intermediate state reproduces the rest exactly.
    for from in [37, 111, 180] {
        let b = simulate(&mut app, a[from], frames[from + 1..].to_vec());
        assert_eq!(&a[from + 1..], &b[..], "replay from tick {from} diverged");
    }

    // On and off a ladder too.
    let mut app = ladder_world();
    let m = settled(&mut app, Vec3::new(0.1, 0.0, 0.0));
    let mut frames = vec![input(0.0, 1.0, Buttons::empty()); 120];
    frames.push(input(0.0, 1.0, Buttons::JUMP));
    frames.extend(vec![input(0.0, 1.0, Buttons::empty()); 200]);
    let a = simulate(&mut app, m, frames.clone());
    assert!(a.iter().any(|t| t.climbing));
    for from in [40, 100, 125, 200] {
        let b = simulate(&mut app, a[from], frames[from + 1..].to_vec());
        assert_eq!(
            &a[from + 1..],
            &b[..],
            "ladder replay from tick {from} diverged"
        );
    }
}

#[test]
fn stands_up_only_with_room() {
    // A 1.5 m high ceiling: crouching fits under it, standing doesn't.
    let mut app = world(&[block([-3.0, 1.5, -10.0], [6.0, 0.5, 8.0])], true);
    let m = settled(&mut app, Vec3::ZERO);
    let crouched = walk(&mut app, m, 180, Buttons::CROUCH);
    let under = *crouched.last().unwrap();
    assert!(
        under.position.z < -4.0 && under.stance == Stance::Crouching,
        "{under:?}"
    );
    let released = simulate(&mut app, under, vec![input(0.0, 0.0, Buttons::empty()); 10]);
    assert_eq!(
        released.last().unwrap().stance,
        Stance::Crouching,
        "stood up into the ceiling"
    );
    let out = walk(&mut app, under, 300, Buttons::empty());
    let last = out.last().unwrap();
    assert!(
        last.position.z < -10.5 && last.stance == Stance::Standing,
        "{last:?}"
    );
}

#[test]
fn stays_on_rolling_terrain() {
    // Terrain like the levels': a heightfield with hills up to ~30°, 2 m between samples.
    let (samples, spacing) = (101, 2.0);
    let heights = (0..samples)
        .map(|x| {
            (0..samples)
                .map(|z| {
                    let (x, z) = (x as f32 * spacing, z as f32 * spacing);
                    3.0 * (x * 0.13).sin() * (z * 0.11).cos() + 0.3 * (x * 0.9 + z * 1.3).sin()
                })
                .collect()
        })
        .collect();
    let size = (samples - 1) as f32 * spacing;
    let mut app = physics_app();
    app.world_mut().spawn((
        RigidBody::Static,
        Collider::heightfield(heights, Vec3::new(size, 1.0, size)),
        Transform::default(),
        CollisionLayers::new(GameLayer::World, LayerMask::ALL),
    ));
    let mut app = ready(app);
    let tuning = SoldierTuning::default();
    for (yaw, buttons) in [
        (0.3f32, Buttons::SPRINT),
        (2.0, Buttons::empty()),
        (4.0, Buttons::CROUCH),
    ] {
        let start = SoldierMotion::at(Vec3::new(0.0, 10.0, 0.0), yaw);
        let mut frames = vec![input(0.0, 0.0, Buttons::empty()); 120];
        frames.extend(vec![input(0.0, 1.0, buttons); 600]);
        for f in &mut frames {
            f.yaw = yaw;
        }
        let trace = simulate(&mut app, start, frames);
        let walking = &trace[200..];
        let airborne = walking.iter().filter(|t| !t.grounded).count();
        let min_speed = walking
            .iter()
            .map(horizontal_speed)
            .fold(f32::MAX, f32::min);
        let climbed = walking
            .iter()
            .map(|t| t.position.y)
            .fold(f32::MIN, f32::max)
            - walking
                .iter()
                .map(|t| t.position.y)
                .fold(f32::MAX, f32::min);
        println!(
            "yaw {yaw} {buttons:?}: airborne {airborne}, min speed {min_speed:.2}, height range {climbed:.1} m"
        );
        assert_eq!(airborne, 0, "left the ground on terrain ({buttons:?})");
        let speed = match buttons {
            Buttons::SPRINT => tuning.sprint_speed,
            Buttons::CROUCH => tuning.crouch_speed,
            _ => tuning.run_speed,
        };
        assert!(
            min_speed > speed - 1e-3,
            "slowed down on terrain ({buttons:?})"
        );
    }
}

#[test]
fn walls_stop_and_slide() {
    let mut app = world(&[block([-10.0, 0.0, -3.0], [20.0, 3.0, 1.0])], true);
    let m = settled(&mut app, Vec3::ZERO);
    // Straight into the wall: stops at it, doesn't climb or jitter.
    let trace = walk(&mut app, m, 120, Buttons::SPRINT);
    let last = trace.last().unwrap();
    assert!(
        last.position.y < 0.02 && last.position.z > -2.0 && last.position.z < -1.7,
        "{last:?}"
    );
    assert_eq!(
        (trace[100].position, trace[100].velocity),
        (trace[119].position, trace[119].velocity),
        "jitters against a wall"
    );
    // At 45°: slides along it.
    let trace = simulate(&mut app, m, vec![input(0.7, 0.7, Buttons::empty()); 120]);
    let last = trace.last().unwrap();
    assert!(last.position.x > 3.0 && last.position.y < 0.02, "{last:?}");
}

#[test]
fn falls_off_ledges() {
    let tuning = SoldierTuning::default();
    // Standing on a 3 m block, walking off its edge 2 m ahead.
    let mut app = world(&[block([-5.0, 0.0, -2.0], [10.0, 3.0, 10.0])], true);
    let m = settled(&mut app, Vec3::new(0.0, 3.5, 0.0));
    let trace = walk(&mut app, m, 120, Buttons::empty());
    let fell = trace
        .iter()
        .position(|t| !t.grounded)
        .expect("never left the ledge");
    let landed = fell
        + trace[fell..]
            .iter()
            .position(|t| t.grounded)
            .expect("never landed");
    let airtime = (landed - fell) as f32 * DT;
    println!(
        "fell at {fell}, airtime {airtime:.2} s, {:?}",
        trace[landed]
    );
    assert!((airtime - (2.0 * 3.0 / tuning.gravity).sqrt()).abs() < 0.1);
    assert!(
        trace[landed].recovery > 0.0,
        "a 3 m drop should be a hard landing"
    );
    assert!(trace[landed].position.y < 0.02);
}

/// A 4 m high building (roof from z = -2 to -10) with a ladder on its wall facing +Z.
fn ladder_world() -> App {
    let mut app = world(&[block([-5.0, 0.0, -10.0], [10.0, 4.0, 8.0])], true);
    // Like BF2's ladder collision: a thin plate along the rails, 0.5 m out from the wall on
    // its brackets, climbed on its local +Z side.
    app.world_mut().spawn((
        RigidBody::Static,
        Collider::cuboid(0.52, 4.3, 0.03),
        Transform::from_xyz(0.0, 1.85, -1.5),
        CollisionLayers::new(GameLayer::World, LayerMask::ALL),
        LadderPart,
    ));
    ready(app)
}

/// A swimmer pulls himself onto a ledge at the surface: a carrier's well deck floor, 0.4 m
/// under the water, seen from the deep water behind its stern.
#[test]
fn swims_onto_shallow_ledges() {
    let tuning = SoldierTuning::default();
    let water = 1.5;
    // 1.5 m of water over the ground, 0.4 m over a ledge whose edge is at z = -2.
    let mut app = world(&[block([-5.0, 0.0, -10.0], [10.0, water - tuning.wade_depth, 8.0])], true);
    let start = SoldierMotion::at(Vec3::new(0.0, 0.0, 2.0), 0.0);
    let trace = simulate_water(&mut app, start, vec![input(0.0, 1.0, Buttons::empty()); 300], Some(water));
    assert!(trace.iter().any(|t| t.swimming), "never swam");
    let up = trace
        .iter()
        .find(|t| !t.swimming && t.grounded)
        .expect("never got out of the water onto the ledge");
    assert!(up.position.z < -1.5 && (up.position.y - (water - tuning.wade_depth)).abs() < 0.05, "{up:?}");
}

/// A swimmer gets over a submerged step a little above his feet (the ledge a carrier's well
/// deck ladder stands on, 1.2 m under the water) and onto the ladder on it.
#[test]
fn swims_over_submerged_steps_to_ladders() {
    let tuning = SoldierTuning::default();
    // The building and ladder of `ladder_world` stand on a 0.8 m step whose edge is 2 m in
    // front of the ladder; the water is 2 m deep over the step, 2.8 m in front of it.
    let mut app = world(
        &[
            block([-5.0, 0.0, -10.0], [10.0, 0.8, 12.0]),
            block([-5.0, 0.8, -10.0], [10.0, 4.0, 8.0]),
        ],
        true,
    );
    app.world_mut().spawn((
        RigidBody::Static,
        Collider::cuboid(0.52, 4.3, 0.03),
        Transform::from_xyz(0.0, 0.8 + 1.85, -1.5),
        CollisionLayers::new(GameLayer::World, LayerMask::ALL),
        LadderPart,
    ));
    let mut app = ready(app);
    let water = 0.8 + 1.2;
    assert!(water - tuning.swim_float_depth < 0.8, "the step is above a floating swimmer's feet");
    let start = SoldierMotion::at(Vec3::new(0.1, 0.0, 6.0), 0.0);
    let trace = simulate_water(&mut app, start, vec![input(0.0, 1.0, Buttons::empty()); 600], Some(water));
    assert!(trace.iter().any(|t| t.swimming), "never swam");
    assert!(trace.iter().any(|t| t.climbing), "never got on the ladder: {:?}", trace.last().unwrap());
}

/// Swimming into a ladder gets on it, like walking into one (a carrier's well deck: its
/// ladders start in the water).
#[test]
fn swims_onto_ladders() {
    let mut app = ladder_world();
    let water = 2.5;
    let start = SoldierMotion::at(Vec3::new(0.1, water - 1.0, 3.0), 0.0);
    let trace = simulate_water(&mut app, start, vec![input(0.0, 1.0, Buttons::empty()); 420], Some(water));
    assert!(trace.iter().any(|t| t.swimming), "never swam");
    let mounted = trace.iter().position(|t| t.climbing).expect("never got on the ladder from the water");
    let last = trace.last().unwrap();
    println!("mounted at {mounted}: {:?}
{last:?}", trace[mounted]);
    assert!(last.grounded && (last.position.y - 4.01).abs() < 0.05 && !last.swimming, "{last:?}");
}

#[test]
fn climbs_ladders() {
    let tuning = SoldierTuning::default();
    let mut app = ladder_world();
    let m = settled(&mut app, Vec3::new(0.1, 0.0, 0.0));

    // Walk into the ladder and keep pushing forward: up and over onto the roof.
    let trace = walk(&mut app, m, 300, Buttons::empty());
    let mounted = trace
        .iter()
        .position(|t| t.climbing)
        .expect("never got on the ladder");
    let off = mounted
        + trace[mounted..]
            .iter()
            .position(|t| !t.climbing)
            .expect("never got off");
    let last = trace.last().unwrap();
    println!(
        "mounted at {mounted}, off at {off}: {:?}\n{last:?}",
        trace[off]
    );
    assert!(
        last.grounded && (last.position.y - 4.01).abs() < 0.01 && last.position.z < -2.2,
        "{last:?}"
    );
    let climb_time = (off - mounted) as f32 * DT;
    assert!(
        (climb_time - 4.0 / tuning.climb_speed).abs() < 0.5,
        "climbing took {climb_time} s"
    );
    // Held on to the ladder's line, not drifting sideways.
    assert!(
        trace[mounted + 10..off]
            .iter()
            .all(|t| t.position.x.abs() < 0.01 && !t.grounded)
    );

    // From the roof back onto it, looking down: down to the ground.
    let mut top = *last;
    top.yaw = std::f32::consts::PI;
    top.pitch = -0.8;
    let mut frames = vec![input(0.0, 1.0, Buttons::empty()); 240];
    for f in &mut frames {
        f.yaw = std::f32::consts::PI;
        f.pitch = -0.8;
    }
    let down = simulate(&mut app, top, frames);
    let mounted = down
        .iter()
        .position(|t| t.climbing)
        .expect("never got on from the top");
    let landed = mounted
        + down[mounted..]
            .iter()
            .position(|t| t.grounded)
            .expect("never got down");
    println!(
        "got on from the top at {mounted}: {:?}, down at {landed}: {:?}",
        down[mounted], down[landed]
    );
    assert!(
        down[mounted].position.z > -2.0,
        "got on behind the ladder: {:?}",
        down[mounted]
    );
    assert!(down[landed].position.y < 0.02 && !down[landed].climbing);

    // Jumping off pushes away from it; no getting straight back on.
    let climbing = *trace
        .iter()
        .find(|t| t.climbing && t.position.y > 1.0)
        .unwrap();
    let mut frames = vec![input(0.0, 1.0, Buttons::JUMP)];
    frames.extend(vec![input(0.0, 1.0, Buttons::empty()); 60]);
    let jumped = simulate(&mut app, climbing, frames);
    assert!(
        !jumped[0].climbing && jumped[0].velocity.z > 2.0,
        "{:?}",
        jumped[0]
    );
    let landed = jumped
        .iter()
        .position(|t| t.grounded)
        .expect("never landed");
    assert!(
        jumped[..landed].iter().all(|t| !t.climbing),
        "grabbed the ladder again"
    );
}

#[test]
fn rests_on_a_sill_against_a_wall() {
    // A 0.6 m high, 0.3 m deep sill in front of a wall: jump onto it and keep pushing.
    let mut app = world(
        &[
            block([-2.0, 0.0, -2.3], [4.0, 0.6, 0.3]),
            block([-2.0, 0.0, -4.0], [4.0, 3.0, 1.7]),
        ],
        true,
    );
    let m = settled(&mut app, Vec3::ZERO);
    let mut frames = vec![input(0.0, 1.0, Buttons::SPRINT); 40];
    frames.push(input(0.0, 1.0, Buttons::SPRINT | Buttons::JUMP));
    frames.extend(vec![input(0.0, 1.0, Buttons::SPRINT); 120]);
    let trace = simulate(&mut app, m, frames);
    print_trace("sill", &trace[35..100]);
    let last = trace.last().unwrap();
    assert!(
        last.grounded && last.position.y > 0.5,
        "didn't get onto the sill: {last:?}"
    );
    let settled = &trace[trace.len() - 30..];
    if let Some(t) = settled
        .iter()
        .find(|t| t.position != last.position || t.velocity != last.velocity)
    {
        panic!(
            "jitters on the sill:
{t:?}
{last:?}"
        );
    }
}

/// Karkand's eastern barrack, from the imported level (skipped without it): jumping onto
/// its 0.6 m ledge and pushing against the wall above used to jitter.
#[test]
fn rests_on_karkand_barrack_ledge() {
    let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join(
        "../../imported/objects/staticobjects/military/buildings/mi_barrack_mech/meshes/mi_barrack_mech.collision.glb",
    );
    let Ok(bytes) = std::fs::read(&path) else {
        println!("skipped: {} not imported", path.display());
        return;
    };
    let gltf = gltf::Gltf::from_slice(&bytes).unwrap();
    let blob = gltf.blob.as_deref().unwrap();
    let mesh = gltf
        .meshes()
        .find(|m| m.name() == Some("part0_soldier"))
        .unwrap();
    let (mut vertices, mut indices) = (Vec::new(), Vec::new());
    for primitive in mesh.primitives() {
        let reader = primitive.reader(|_| Some(blob));
        let base = vertices.len() as u32;
        vertices.extend(reader.read_positions().unwrap().map(Vec3::from_array));
        let flat: Vec<u32> = reader.read_indices().unwrap().into_u32().collect();
        indices.extend(
            flat.chunks_exact(3)
                .map(|t| [base + t[0], base + t[1], base + t[2]]),
        );
    }
    let mut app = physics_app();
    let layers = CollisionLayers::new(GameLayer::World, LayerMask::ALL);
    app.world_mut().spawn((
        RigidBody::Static,
        Collider::cuboid(200.0, 1.0, 200.0),
        Transform::from_xyz(226.0, 152.706 - 0.5, -185.0),
        layers,
    ));
    app.world_mut().spawn((
        RigidBody::Static,
        Collider::trimesh(vertices, indices),
        Transform::from_xyz(228.5, 154.91, -189.7)
            .with_rotation(Quat::from_rotation_y(std::f32::consts::PI)),
        layers,
    ));
    let mut app = ready(app);
    let m = settled(&mut app, Vec3::new(226.73, 153.0, -181.0));
    let mut frames = vec![input(0.0, 1.0, Buttons::SPRINT); 90];
    frames.push(input(0.0, 1.0, Buttons::SPRINT | Buttons::JUMP));
    frames.extend(vec![input(0.0, 1.0, Buttons::SPRINT); 180]);
    let trace = simulate(&mut app, m, frames);
    let last = *trace.last().unwrap();
    print_trace("barrack", &trace[trace.len() - 20..]);
    let moving = trace[trace.len() - 60..]
        .iter()
        .filter(|t| t.position != last.position)
        .count();
    assert_eq!(moving, 0, "jitters on the ledge: {last:?}");
}

#[test]
fn sprint_uses_stamina() {
    let tuning = SoldierTuning::default();
    let mut app = world(&[], true);
    let m = settled(&mut app, Vec3::ZERO);
    let ticks = |seconds: f32| (seconds / DT).round() as usize;

    // A light soldier sprints for its 10 s, then runs, and sprints again once a little has
    // recovered: 0.05 of a 17 s recovery.
    let trace = walk(&mut app, m, ticks(14.0), Buttons::SPRINT);
    let stopped = trace
        .iter()
        .position(|t| !t.sprinting && t.stamina < 0.5)
        .expect("never ran out");
    let seconds = stopped as f32 * DT;
    println!("light: out of stamina after {seconds:.2} s");
    assert!(
        (seconds - tuning.light.sprint_time).abs() < 0.1,
        "{seconds}"
    );
    assert!((horizontal_speed(&trace[stopped + 30]) - tuning.run_speed).abs() < 0.3);
    let again = stopped
        + trace[stopped..]
            .iter()
            .position(|t| t.sprinting)
            .expect("never sprinted again");
    let wait = (again - stopped) as f32 * DT;
    println!("sprinting again after {wait:.2} s");
    assert!(
        (wait - tuning.sprint_min_stamina * tuning.light.recover_time).abs() < 0.1,
        "{wait}"
    );

    // Heavy kits run out sooner.
    let heavy = SoldierMotion { heavy: true, ..m };
    let trace = walk(&mut app, heavy, ticks(10.0), Buttons::SPRINT);
    let stopped = trace
        .iter()
        .position(|t| !t.sprinting && t.stamina < 0.5)
        .expect("never ran out");
    assert!((stopped as f32 * DT - tuning.heavy.sprint_time).abs() < 0.1);

    // Standing still recovers, but not right after a jump; jumping costs stamina.
    let tired = SoldierMotion { stamina: 0.5, ..m };
    let mut frames = vec![input(0.0, 0.0, Buttons::JUMP)];
    frames.extend(vec![input(0.0, 0.0, Buttons::empty()); ticks(2.0)]);
    let trace = simulate(&mut app, tired, frames);
    let cost = 0.5 - trace[0].stamina;
    // Less the stamina recovered in that tick before the jump.
    assert!(
        (cost - tuning.light.jump_cost).abs() < 2e-3,
        "jump cost {cost}"
    );
    let paused = ticks(tuning.stamina_delay_after_jump) - 2;
    assert_eq!(
        trace[paused].stamina, trace[0].stamina,
        "recovered during the jump delay"
    );
    assert!(trace.last().unwrap().stamina > trace[0].stamina);
}

#[test]
fn stance_and_fire_delays() {
    let tuning = SoldierTuning::default();
    let mut app = world(&[], true);
    let m = settled(&mut app, Vec3::ZERO);
    let ticks = |seconds: f32| (seconds / DT).round() as usize;

    // Tap prone and let go: stays down for the switch delay, then stands up and can't
    // fire for a moment.
    let mut frames = vec![input(0.0, 0.0, Buttons::PRONE)];
    frames.extend(vec![input(0.0, 0.0, Buttons::empty()); ticks(2.0)]);
    let trace = simulate(&mut app, m, frames);
    let up = trace
        .iter()
        .position(|t| t.stance != Stance::Prone)
        .unwrap();
    println!("got up after {:.2} s", up as f32 * DT);
    assert!((up as f32 * DT - tuning.prone_switch_delay).abs() < 0.05);
    assert!(!trace[up].can_fire() && trace.last().unwrap().can_fire());

    // Right after getting up, prone is blocked for the switch delay too.
    let got_up = trace[up];
    let trace = simulate(
        &mut app,
        got_up,
        vec![input(0.0, 0.0, Buttons::PRONE); ticks(1.5)],
    );
    let down = trace
        .iter()
        .position(|t| t.stance == Stance::Prone)
        .unwrap();
    assert!((down as f32 * DT - tuning.prone_switch_delay).abs() < 0.05);

    // Jumping: no firing for 0.7 s.
    let trace = simulate(
        &mut app,
        m,
        vec![input(0.0, 0.0, Buttons::JUMP); ticks(1.0)],
    );
    let ready = trace.iter().position(|t| t.can_fire()).unwrap();
    assert!((ready as f32 * DT - tuning.fire_delay_after_jump).abs() < 0.05);
}

/// The 4 m building of [`ladder_world`], without its ladder, and `rope` strung.
fn rope_world(rope: Rope) -> App {
    let mut app = world(&[block([-5.0, 0.0, -10.0], [10.0, 4.0, 8.0])], true);
    app.world_mut().spawn(rope);
    ready(app)
}

#[test]
fn strings_grappling_ropes_over_ledges() {
    let mut app = world(&[block([-5.0, 0.0, -10.0], [10.0, 4.0, 8.0])], true);
    let ropes = app
        .world_mut()
        .run_system_once(|spatial: SpatialQuery| {
            (
                // On the roof, thrown from the street in front: over the front edge.
                rope::grapple(
                    &spatial,
                    Vec3::new(0.5, 4.0, -5.0),
                    Vec3::new(0.0, 0.0, 6.0),
                    14.0,
                ),
                // On the street itself: nothing to climb.
                rope::grapple(
                    &spatial,
                    Vec3::new(0.5, 0.0, 3.0),
                    Vec3::new(0.0, 0.0, 6.0),
                    14.0,
                ),
            )
        })
        .unwrap();
    println!("{ropes:?}");
    let rope = ropes.0.expect("no rope over the ledge");
    assert!(
        (rope.top.z - -(2.0 - rope::OFF_WALL)).abs() < 0.12 && (rope.top.y - 4.0).abs() < 0.05,
        "{rope:?}"
    );
    assert!(rope.end.y.abs() < 0.05, "{rope:?}");
    assert!(ropes.1.is_none());
}

#[test]
fn grappling_hooks_catch_on_sloped_roofs_and_parapets() {
    // The 4 m building with a roof sloping up 25° from its front edge, and a second one
    // with a 1 m parapet along its front edge.
    let mut roof = ramp(-2.0, 25.0, 6.0);
    roof.transform.translation.y += 4.0;
    let mut app = world(
        &[
            block([-5.0, 0.0, -10.0], [10.0, 4.0, 8.0]),
            roof,
            block([15.0, 0.0, -10.0], [10.0, 4.0, 8.0]),
            block([15.0, 4.0, -2.3], [10.0, 1.0, 0.3]),
            // A third, 6 m high, with a cornice sticking out 0.4 m at 2.5 m.
            block([35.0, 0.0, -10.0], [10.0, 6.0, 8.0]),
            block([35.0, 2.5, -2.0], [10.0, 0.3, 0.4]),
        ],
        true,
    );
    let cornice = app
        .world_mut()
        .run_system_once(|spatial: SpatialQuery| {
            rope::grapple(&spatial, Vec3::new(40.0, 6.02, -5.0), Vec3::new(40.0, 0.0, 6.0), 14.0)
        })
        .unwrap()
        .expect("didn't catch on the cornice building");
    // Hangs (and is climbed) past the cornice to the ground.
    assert!(cornice.end.y.abs() < 0.05 && (cornice.top.y - 6.0).abs() < 0.05, "{cornice:?}");
    let (sloped, parapet, behind_wall) = app
        .world_mut()
        .run_system_once(|spatial: SpatialQuery| {
            let slope_y = 4.0 + 2.0 * 25f32.to_radians().tan();
            (
                // On the slope 2 m up from the edge: slides down to the eave.
                rope::grapple(&spatial, Vec3::new(0.5, slope_y + 0.02, -4.0), Vec3::new(0.0, 0.0, 6.0), 14.0),
                // Behind the parapet: over it.
                rope::grapple(&spatial, Vec3::new(20.0, 4.02, -5.0), Vec3::new(20.0, 0.0, 6.0), 14.0),
                // Against the building's back wall, thrown from behind it: held by the wall.
                rope::grapple(&spatial, Vec3::new(20.0, 0.02, -10.2), Vec3::new(20.0, 0.0, -20.0), 14.0),
            )
        })
        .unwrap();
    println!("{sloped:?} / {parapet:?} / {behind_wall:?}");
    let sloped = sloped.expect("didn't catch on the sloped roof's eave");
    assert!(
        (sloped.top.y - 4.0).abs() < 0.15 && (sloped.top.z - -(2.0 - rope::OFF_WALL)).abs() < 0.2,
        "{sloped:?}"
    );
    assert!(sloped.end.y.abs() < 0.05);
    let parapet = parapet.expect("didn't catch on the parapet");
    assert!((parapet.top.y - 5.0).abs() < 0.05 && parapet.top.z > -2.0, "{parapet:?}");
    assert!(behind_wall.is_none(), "{behind_wall:?}");
}

#[test]
fn climbs_grappling_ropes() {
    let tuning = SoldierTuning::default();
    let rope = Rope {
        kind: RopeKind::Grapple,
        anchor: Vec3::new(0.0, 4.0, -5.0),
        top: Vec3::new(0.0, 4.0, -2.0 + rope::OFF_WALL),
        end: Vec3::new(0.0, 0.0, -2.0 + rope::OFF_WALL),
        length: 14.0,
        links: 26,
    };
    let mut app = rope_world(rope);
    let m = settled(&mut app, Vec3::new(0.2, 0.0, 1.0));
    let trace = walk(&mut app, m, 240, Buttons::empty());
    let on = trace
        .iter()
        .position(|t| t.climbing)
        .expect("never got on the rope");
    assert!(trace[on].on_rope && !trace[on].can_fire());
    let off = on
        + trace[on..]
            .iter()
            .position(|t| !t.climbing)
            .expect("never got off");
    let seconds = (off - on) as f32 * DT;
    println!("on the rope for {seconds:.2} s, then {:?}", trace[off]);
    // Up until the hands reach the top (the feet 1.3 m below it), then over the edge.
    assert!(
        (seconds - (4.0 - 1.3) / tuning.rope_climb_speed).abs() < 0.3,
        "{seconds}"
    );
    assert!(trace[off].mantling(), "didn't climb over the edge: {:?}", trace[off]);
    let standing = off
        + trace[off..]
            .iter()
            .position(|t| !t.mantling())
            .expect("never got onto the roof");
    let over = (standing - off) as f32 * DT;
    println!("over the edge in {over:.2} s");
    assert!(over > 0.3 && over < 1.0, "{over}");
    let last = trace.last().unwrap();
    assert!(
        last.grounded && (last.position.y - 4.01).abs() < 0.01 && last.position.z < -2.0,
        "{last:?}"
    );
}

#[test]
fn rides_ziplines() {
    let tuning = SoldierTuning::default();
    // From the roof's front edge (the shooter's stand) down to the street 30 m away.
    let start = Vec3::new(0.0, 4.0 + rope::ZIPLINE_STAND, -2.5);
    let rope = Rope {
        kind: RopeKind::Zipline,
        anchor: start,
        top: start,
        end: Vec3::new(0.0, 1.0, 27.5),
        length: 30.0,
        links: 1,
    };
    let mut app = rope_world(rope);
    let mut m = settled(&mut app, Vec3::new(0.0, 4.5, -4.0));
    m.yaw = std::f32::consts::PI;
    let mut frames = vec![input(0.0, 1.0, Buttons::empty()); 600];
    for f in &mut frames {
        f.yaw = std::f32::consts::PI;
    }
    let trace = simulate(&mut app, m, frames);
    let on = trace
        .iter()
        .position(|t| t.riding)
        .expect("never grabbed the wire");
    let off = on
        + trace[on..]
            .iter()
            .position(|t| !t.riding)
            .expect("never got off");
    let top_speed = trace[on..off]
        .iter()
        .map(|t| t.velocity.length())
        .fold(0.0, f32::max);
    println!(
        "rode {:.2} s, top speed {top_speed:.1} m/s, off at {:?}",
        (off - on) as f32 * DT,
        trace[off]
    );
    assert!(!trace[on].can_fire());
    assert!(top_speed > tuning.zipline_min_speed && top_speed <= tuning.zipline_max_speed);
    // Hung under the wire until the feet reach the ground, never below it.
    assert!(
        trace[off].position.z > 15.0 && trace[off].grounded,
        "{:?}",
        trace[off]
    );
    if let Some(i) = trace.iter().position(|t| t.position.y < -0.01) {
        print_trace(
            "below",
            &trace[i.saturating_sub(5)..(i + 5).min(trace.len())],
        );
        panic!("below the ground at {i}");
    }
    let landed = trace.last().unwrap();
    assert!(landed.grounded, "{landed:?}");

    // Jumping off halfway.
    let mut frames = vec![input(0.0, 1.0, Buttons::empty()); on + 60];
    frames.push(input(0.0, 1.0, Buttons::JUMP));
    frames.extend(vec![input(0.0, 0.0, Buttons::empty()); 60]);
    for f in &mut frames {
        f.yaw = std::f32::consts::PI;
    }
    let trace = simulate(&mut app, m, frames);
    let jumped = trace[on + 61];
    assert!(!jumped.riding && jumped.velocity.z > 1.0, "{jumped:?}");
}

#[test]
fn swims_in_deep_water_and_climbs_out_at_shore() {
    use game_shared::soldier::SOLDIER_HEIGHT;
    let tuning = SoldierTuning::default();
    let water = 3.0;
    // Flat sea floor at y = 0 (3 m deep, well past swimming depth), a gentle 8° ramp rising
    // out of it from z = -10 towards the shore.
    let mut app = world(&[ramp(-10.0, 8.0, 50.0)], true);
    let start = SoldierMotion::at(Vec3::new(0.0, 0.05, 0.0), 0.0);
    let trace = simulate_water(&mut app, start, vec![input(0.0, 0.0, Buttons::empty()); 90], Some(water));
    let last = *trace.last().unwrap();
    assert!(last.swimming && !last.grounded, "didn't start swimming in deep water: {last:?}");
    assert!(!last.can_fire(), "shouldn't be able to fire while swimming");
    // Floats with the head above the surface, feet well below it.
    assert!(last.position.y + SOLDIER_HEIGHT > water, "head not above the surface: {last:?}");
    assert!(
        (last.position.y - (water - tuning.swim_float_depth)).abs() < 0.1,
        "not floating at the swim depth: {last:?}"
    );

    // Swim towards the ramp (yaw 0 heads -Z) and climb out where it shallows enough.
    let frames = vec![input(0.0, 1.0, Buttons::empty()); 1500];
    let trace = simulate_water(&mut app, last, frames, Some(water));
    let speed = trace[60..90].iter().map(horizontal_speed).sum::<f32>() / 30.0;
    println!("swim speed {speed:.2} m/s (tuning {})", tuning.swim_speed);
    assert!((speed - tuning.swim_speed).abs() < 0.1, "wrong swim speed: {speed}");
    let out = trace
        .iter()
        .position(|t| !t.swimming && t.grounded)
        .expect("never climbed out at the shore");
    let landed = trace[out];
    println!("climbed out after {out} ticks: {landed:?}");
    assert!(landed.can_fire(), "should be able to fire once ashore");
    assert!(water - landed.position.y < tuning.wade_depth + 0.05, "{landed:?}");

    // Sprint-swimming is faster.
    let frames = vec![input(0.0, 1.0, Buttons::SPRINT); 90];
    let trace = simulate_water(&mut app, SoldierMotion::at(Vec3::new(0.0, 0.05, 20.0), 0.0), frames, Some(water));
    let sprint_speed = trace[60..90].iter().map(horizontal_speed).sum::<f32>() / 30.0;
    println!("swim sprint speed {sprint_speed:.2} m/s (tuning {})", tuning.swim_sprint_speed);
    assert!((sprint_speed - tuning.swim_sprint_speed).abs() < 0.15, "{sprint_speed}");
}

/// Runs up to a wall `height` m high and `depth` m thick (its face 2 m ahead) and presses
/// jump right in front of it (holding it `hold` ticks). Returns the trace from the press.
fn jump_at_wall(height: f32, depth: f32, mantle: bool, hold: usize) -> (Vec<SoldierMotion>, f32) {
    let mut app = world(&[block([-5.0, 0.0, -2.0 - depth], [10.0, height, depth])], true);
    app.world_mut().resource_mut::<SoldierTuning>().mantle = mantle;
    let m = settled(&mut app, Vec3::ZERO);
    // Up to half a meter in front of it.
    let run = walk(&mut app, m, 60, Buttons::empty());
    let near = run.iter().position(|t| t.position.z < -1.25).expect("never got near the wall");
    let mut frames = vec![input(0.0, 1.0, Buttons::JUMP); hold];
    frames.extend(vec![input(0.0, 1.0, Buttons::empty()); 90]);
    (simulate(&mut app, run[near], frames), -2.0 - depth)
}

#[test]
fn mantles_onto_ledges() {
    for height in [0.6, 1.0, 1.4, 1.6] {
        let (trace, back) = jump_at_wall(height, 10.0, true, 1);
        let started = trace.iter().position(|t| t.mantling()).unwrap_or_else(|| {
            print_trace(&format!("{height} m"), &trace[..30]);
            panic!("didn't climb onto a {height} m ledge")
        });
        let done = started + trace[started..].iter().position(|t| !t.mantling()).unwrap();
        let seconds = (done - started) as f32 * DT;
        let last = trace.last().unwrap();
        println!("{height} m ledge: climbed in {seconds:.2} s, {last:?}");
        assert!(started <= 1, "started late: {started}");
        assert!(seconds < 0.7, "{height} m took {seconds} s");
        assert!(last.grounded && (last.position.y - (height + 0.01)).abs() < 0.02 && last.position.z < -2.2 && last.position.z > back, "{last:?}");
        assert!(trace[started..done].iter().all(|t| !t.can_fire()));
    }
    // Too high to reach from the ground; nothing to climb on a jump without holding it.
    let (trace, _) = jump_at_wall(2.0, 10.0, true, 1);
    assert!(trace.iter().all(|t| !t.mantling() && t.position.y < 1.3), "climbed a 2 m wall");
    // Holding jump grabs its top on the way up.
    let (trace, _) = jump_at_wall(2.0, 10.0, true, 40);
    let last = trace.last().unwrap();
    assert!((last.position.y - 2.01).abs() < 0.02, "didn't grab a 2 m wall: {last:?}");
    let (trace, _) = jump_at_wall(3.0, 10.0, true, 40);
    assert!(trace.iter().all(|t| !t.mantling()), "grabbed a 3 m wall");
}

#[test]
fn vaults_over_thin_walls() {
    // A 1.1 m wall 0.2 m thick: over it and down the other side.
    let (trace, back) = jump_at_wall(1.1, 0.2, true, 1);
    assert!(trace.iter().any(|t| t.mantling()), "didn't vault");
    let last = trace.last().unwrap();
    assert!(last.grounded && last.position.y < 0.02 && last.position.z < back, "{last:?}");
}

#[test]
fn no_mantle_without_room_or_with_it_off() {
    // A 1.2 m ledge under a 1.5 m high ceiling: no room to stand on it.
    let mut app = world(
        &[
            block([-5.0, 0.0, -5.0], [10.0, 1.2, 3.0]),
            block([-5.0, 2.7, -5.0], [10.0, 0.3, 3.0]),
        ],
        true,
    );
    let m = settled(&mut app, Vec3::ZERO);
    let run = walk(&mut app, m, 60, Buttons::empty());
    let near = run.iter().position(|t| t.position.z < -1.25).unwrap();
    let mut frames = vec![input(0.0, 1.0, Buttons::JUMP)];
    frames.extend(vec![input(0.0, 1.0, Buttons::empty()); 60]);
    let trace = simulate(&mut app, run[near], frames);
    assert!(trace.iter().all(|t| !t.mantling()), "climbed under a ceiling");

    // BF2's movement: a 1.4 m ledge is out of reach, a 1 m wall can be jumped onto.
    let (trace, _) = jump_at_wall(1.4, 10.0, false, 1);
    assert!(trace.iter().all(|t| !t.mantling() && t.position.y < 0.1 || !t.grounded), "mantled with it off");
    assert!(trace.last().unwrap().position.y < 0.1);
    let (trace, _) = jump_at_wall(1.0, 10.0, false, 1);
    let last = trace.last().unwrap();
    assert!(trace.iter().all(|t| !t.mantling()));
    assert!((last.position.y - 1.01).abs() < 0.02, "couldn't jump onto a 1 m wall: {last:?}");
}

/// A corner of Karkand's square (around (-191.5, 155.81, 49.0); here the ground is at y = 0):
/// a jersey barrier (`concretebarrier`) with a beam (`beam_01`) leaning on it at 57.5°, a
/// little steeper than soldiers walk up, its foot in the ground. Both as BF2's soldier
/// collision has them, triangle meshes like the level's: the barrier a prism 1.1 m high,
/// 0.64 m wide at the foot and 0.34 m on top (82° sides), 1.6 m long; the beam a plank
/// 0.71 m wide, 0.15 m thick and 3.6 m long.
fn barrier_world() -> App {
    let origin = Vec3::new(-191.5, 155.81, 49.0);
    let (w0, w1, l) = (0.322, 0.17, 0.807);
    let vertices = vec![
        Vec3::new(-w0, -0.5, -l),
        Vec3::new(w0, -0.5, -l),
        Vec3::new(w0, -0.5, l),
        Vec3::new(-w0, -0.5, l),
        Vec3::new(-w1, 0.6, -l),
        Vec3::new(w1, 0.6, -l),
        Vec3::new(w1, 0.6, l),
        Vec3::new(-w1, 0.6, l),
    ];
    // Counter-clockwise seen from outside.
    let indices = vec![
        [0, 1, 2],
        [0, 2, 3],
        [4, 6, 5],
        [4, 7, 6],
        [0, 4, 5],
        [0, 5, 1],
        [3, 2, 6],
        [3, 6, 7],
        [0, 3, 7],
        [0, 7, 4],
        [1, 5, 6],
        [1, 6, 2],
    ];
    let mut app = physics_app();
    let layers = CollisionLayers::new(GameLayer::World, LayerMask::ALL);
    app.world_mut().spawn((
        RigidBody::Static,
        Collider::cuboid(200.0, 1.0, 200.0),
        Transform::from_xyz(0.0, -0.5, 0.0),
        layers,
    ));
    app.world_mut().spawn((
        RigidBody::Static,
        Collider::trimesh(vertices, indices),
        Transform::from_translation(Vec3::new(-191.532, 156.29, 49.323) - origin)
            .with_rotation(Quat::from_xyzw(0.0, 0.07149744, 0.0, 0.99744076)),
        layers,
    ));
    // The beam: a plank with bevelled edges.
    let vertices = [
        [0.353, -0.037, 1.798],
        [0.246, 0.047, 1.633],
        [-0.347, -0.037, 1.778],
        [-0.242, 0.047, 1.613],
        [-0.233, 0.077, -1.674],
        [-0.242, 0.026, 0.856],
        [0.248, 0.040, -1.611],
        [0.246, 0.007, 0.887],
        [-0.347, -0.058, 0.830],
        [-0.355, -0.003, -1.794],
        [0.349, -0.044, -1.731],
        [0.353, -0.077, 0.860],
    ]
    .map(Vec3::from_array)
    .to_vec();
    let indices = vec![
        [0, 1, 2],
        [1, 3, 2],
        [4, 5, 6],
        [6, 5, 7],
        [2, 3, 8],
        [3, 5, 8],
        [9, 4, 10],
        [10, 4, 6],
        [8, 11, 2],
        [2, 11, 0],
        [7, 11, 6],
        [6, 11, 10],
        [5, 3, 7],
        [7, 3, 1],
        [5, 4, 8],
        [8, 4, 9],
        [9, 10, 8],
        [11, 8, 10],
        [0, 11, 1],
        [11, 7, 1],
    ];
    app.world_mut().spawn((
        RigidBody::Static,
        Collider::trimesh(vertices, indices),
        Transform::from_translation(Vec3::new(-191.197, 156.532, 48.94) - origin)
            .with_rotation(Quat::from_xyzw(0.30511707, 0.61282957, -0.37771323, 0.623439).normalize()),
        layers,
    ));
    // Sandbags (`concretebags_01`) right beside the barrier, a hand's width away: a lumpy
    // block 2.2 m by 2 m, 1.5 m high.
    let vertices = [
        [0.946, 0.534, -0.971],
        [1.093, -0.748, -1.02],
        [-1.126, 0.48, -0.99],
        [-1.038, -0.748, -1.03],
        [-1.035, 0.65, 0.07],
        [-0.056, 0.748, -0.112],
        [0.398, 0.659, -0.957],
        [0.926, 0.662, -0.281],
        [1.063, 0.423, 0.916],
        [-0.993, 0.392, 0.88],
        [1.138, -0.748, 0.903],
        [-1.038, -0.748, 0.973],
        [-0.51, 0.631, 0.733],
    ]
    .map(Vec3::from_array)
    .to_vec();
    let indices = vec![
        [0, 1, 2],
        [1, 3, 2],
        [4, 5, 6],
        [6, 5, 7],
        [8, 9, 10],
        [9, 11, 10],
        [10, 11, 1],
        [11, 3, 1],
        [3, 11, 2],
        [11, 9, 2],
        [1, 0, 10],
        [10, 0, 8],
        [0, 2, 6],
        [2, 4, 6],
        [2, 9, 4],
        [9, 12, 4],
        [9, 8, 12],
        [8, 7, 12],
        [8, 0, 7],
        [0, 6, 7],
        [4, 12, 5],
        [7, 5, 12],
    ];
    app.world_mut().spawn((
        RigidBody::Static,
        Collider::trimesh(vertices, indices),
        Transform::from_translation(Vec3::new(-189.963, 156.538, 49.447) - origin),
        layers,
    ));
    ready(app)
}

/// Hanging in the air without moving for a while, or moving further in a tick than
/// anyone walks.
fn wedged_or_jumped(trace: &[SoldierMotion]) -> Option<String> {
    let mut stuck = 0;
    for (i, pair) in trace.windows(2).enumerate() {
        let moved = pair[1].position.distance(pair[0].position);
        if moved > 0.4 && !pair[1].mantling() && !pair[0].mantling() {
            return Some(format!("jumped {moved:.2} m at tick {}", i + 1));
        }
        let hanging = !pair[1].grounded && !pair[1].mantling() && moved < 1e-3;
        stuck = if hanging { stuck + 1 } else { 0 };
        if stuck >= 10 {
            return Some(format!("wedged in the air at tick {}", i + 1));
        }
    }
    None
}

#[test]
fn walks_into_sloped_statics_without_wedging() {
    let mut app = barrier_world();
    // Walking at the middle from all round (or past it at an angle), and off the tops of the
    // barrier and the sandbags every way, into the crevice between them.
    let middle = Vec3::new(0.1, 0.0, 0.1);
    let mut starts = Vec::new();
    for approach in (0..360).step_by(10) {
        let a = (approach as f32).to_radians();
        let from = middle + Vec3::new(a.sin(), 0.0, a.cos()) * 3.2 + Vec3::Y * 0.3;
        for aim in [-40.0f32, -15.0, 0.0, 15.0, 40.0] {
            starts.push((format!("from {approach} deg, aim {aim} deg"), from, a + aim.to_radians()));
        }
    }
    for (top, feet) in [("barrier", Vec3::new(-0.03, 1.4, 0.32)), ("sandbags", Vec3::new(1.4, 1.8, 0.45))] {
        for heading in (0..360).step_by(15) {
            starts.push((format!("off the {top} heading {heading} deg"), feet, (heading as f32).to_radians()));
        }
    }
    let mut failures = Vec::new();
    for (label, feet, yaw) in starts {
        let m = settled(&mut app, feet);
        run_at_statics(&mut app, &label, m, yaw, &mut failures);
    }
    // Dropped in anywhere around and on them, from up to 2 m, running and jumping any way.
    let mut seed = 0x2545_f491_u32;
    let mut random = move || {
        seed ^= seed << 13;
        seed ^= seed >> 17;
        seed ^= seed << 5;
        seed as f32 / u32::MAX as f32
    };
    // Into the crevice between the barrier's top and the beam, where soldiers hung.
    for feet in [Vec3::new(0.09, 1.59, -0.29), Vec3::new(0.2, 1.47, -0.31), Vec3::new(-0.11, 1.13, 0.58)] {
        let yaw = 0.3;
        run_at_statics(&mut app, &format!("dropped into the crevice at {feet:.2}"), SoldierMotion::at(feet, yaw), yaw, &mut failures);
    }
    let mut dropped = 0;
    while dropped < 250 {
        let feet = Vec3::new(-1.2 + 3.8 * random(), 0.3 + 2.0 * random(), -1.4 + 3.2 * random());
        let yaw = random() * std::f32::consts::TAU;
        if !room_to_stand(&mut app, feet) {
            continue;
        }
        dropped += 1;
        let m = SoldierMotion::at(feet, yaw);
        run_at_statics(&mut app, &format!("dropped at {feet:.2}"), m, yaw, &mut failures);
    }
    assert!(failures.is_empty(), "{} failures:\n{}", failures.len(), failures.join("\n"));
}

/// Nothing in the way of a standing soldier with his feet here.
fn room_to_stand(app: &mut App, feet: Vec3) -> bool {
    app.world_mut()
        .run_system_once(move |shapes: Res<SoldierShapes>, spatial: SpatialQuery| {
            spatial
                .shape_intersections(
                    shapes.movement(Stance::Standing),
                    feet + Stance::Standing.collision_center(),
                    Quat::IDENTITY,
                    &SpatialQueryFilter::default(),
                )
                .is_empty()
        })
        .unwrap()
}

/// Walks, sprints and jumps from `m` heading `yaw` for a few seconds each; notes where the
/// soldier wedges in the air or leaps.
fn run_at_statics(app: &mut App, label: &str, m: SoldierMotion, yaw: f32, failures: &mut Vec<String>) {
    for (name, buttons, jump_at) in [
        ("walk", Buttons::empty(), None),
        ("sprint", Buttons::SPRINT, None),
        ("jump", Buttons::SPRINT, Some(12)),
        ("jump late", Buttons::empty(), Some(25)),
    ] {
        let mut frames: Vec<InputFrame> = (0..200)
            .map(|i| input(0.0, 1.0, if jump_at == Some(i) { buttons | Buttons::JUMP } else { buttons }))
            .collect();
        for f in &mut frames {
            f.yaw = yaw;
        }
        let mut start = m;
        start.yaw = yaw;
        let trace = simulate(app, start, frames);
        if let Some(what) = wedged_or_jumped(&trace) {
            failures.push(format!("{label} {name}: {what}"));
            if failures.len() <= 3 {
                print_trace(&failures[failures.len() - 1], &trace);
            }
        }
    }
}

#[test]
fn replays_mantles_identically() {
    let mut app = world(&[block([-5.0, 0.0, -5.0], [10.0, 1.4, 3.0])], true);
    let m = settled(&mut app, Vec3::ZERO);
    let mut frames = vec![input(0.0, 1.0, Buttons::empty()); 30];
    frames.push(input(0.0, 1.0, Buttons::JUMP));
    frames.extend(vec![input(0.0, 1.0, Buttons::empty()); 60]);
    let a = simulate(&mut app, m, frames.clone());
    assert!(a.iter().any(|t| t.mantling()));
    for from in [30, 34, 40] {
        let b = simulate(&mut app, a[from], frames[from + 1..].to_vec());
        assert_eq!(&a[from + 1..], &b[..], "mantle replay from tick {from} diverged");
    }
}
