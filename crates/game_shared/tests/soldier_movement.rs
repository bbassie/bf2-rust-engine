//! Soldier movement against simple geometry: steps, stairs, slopes, jumps, determinism.
//! Runs `step_soldier` headless with avian's spatial queries, no level needed.

use avian3d::prelude::*;
use bevy::{ecs::system::RunSystemOnce, prelude::*, time::TimeUpdateStrategy};
use game_shared::{
    input::{Buttons, InputFrame},
    ladder::LadderPart,
    physics::GameLayer,
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
    app.world_mut()
        .run_system_once(
            move |tuning: Res<SoldierTuning>, shapes: Res<SoldierShapes>, mover: MoveAndSlide| {
                let mut m = start;
                inputs
                    .iter()
                    .map(|frame| {
                        step_soldier(&mut m, frame, DT, &tuning, &shapes, &mover);
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

#[test]
fn climbs_ladders() {
    let tuning = SoldierTuning::default();
    let mut app = ladder_world();
    let m = settled(&mut app, Vec3::new(0.1, 0.0, 0.0));

    // Walk into the ladder and keep pushing forward: up and over onto the roof.
    let trace = walk(&mut app, m, 200, Buttons::empty());
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
    let stopped = trace.iter().position(|t| !t.sprinting && t.stamina < 0.5).expect("never ran out");
    let seconds = stopped as f32 * DT;
    println!("light: out of stamina after {seconds:.2} s");
    assert!((seconds - tuning.light.sprint_time).abs() < 0.1, "{seconds}");
    assert!((horizontal_speed(&trace[stopped + 30]) - tuning.run_speed).abs() < 0.3);
    let again = stopped + trace[stopped..].iter().position(|t| t.sprinting).expect("never sprinted again");
    let wait = (again - stopped) as f32 * DT;
    println!("sprinting again after {wait:.2} s");
    assert!((wait - tuning.sprint_min_stamina * tuning.light.recover_time).abs() < 0.1, "{wait}");

    // Heavy kits run out sooner.
    let heavy = SoldierMotion { heavy: true, ..m };
    let trace = walk(&mut app, heavy, ticks(10.0), Buttons::SPRINT);
    let stopped = trace.iter().position(|t| !t.sprinting && t.stamina < 0.5).expect("never ran out");
    assert!((stopped as f32 * DT - tuning.heavy.sprint_time).abs() < 0.1);

    // Standing still recovers, but not right after a jump; jumping costs stamina.
    let tired = SoldierMotion { stamina: 0.5, ..m };
    let mut frames = vec![input(0.0, 0.0, Buttons::JUMP)];
    frames.extend(vec![input(0.0, 0.0, Buttons::empty()); ticks(2.0)]);
    let trace = simulate(&mut app, tired, frames);
    let cost = 0.5 - trace[0].stamina;
    // Less the stamina recovered in that tick before the jump.
    assert!((cost - tuning.light.jump_cost).abs() < 2e-3, "jump cost {cost}");
    let paused = ticks(tuning.stamina_delay_after_jump) - 2;
    assert_eq!(trace[paused].stamina, trace[0].stamina, "recovered during the jump delay");
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
    let up = trace.iter().position(|t| t.stance != Stance::Prone).unwrap();
    println!("got up after {:.2} s", up as f32 * DT);
    assert!((up as f32 * DT - tuning.prone_switch_delay).abs() < 0.05);
    assert!(!trace[up].can_fire() && trace.last().unwrap().can_fire());

    // Right after getting up, prone is blocked for the switch delay too.
    let got_up = trace[up];
    let trace = simulate(&mut app, got_up, vec![input(0.0, 0.0, Buttons::PRONE); ticks(1.5)]);
    let down = trace.iter().position(|t| t.stance == Stance::Prone).unwrap();
    assert!((down as f32 * DT - tuning.prone_switch_delay).abs() < 0.05);

    // Jumping: no firing for 0.7 s.
    let trace = simulate(&mut app, m, vec![input(0.0, 0.0, Buttons::JUMP); ticks(1.0)]);
    let ready = trace.iter().position(|t| t.can_fire()).unwrap();
    assert!((ready as f32 * DT - tuning.fire_delay_after_jump).abs() < 0.05);
}
