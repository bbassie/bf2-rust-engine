//! Grenades, charges and rockets against simple geometry: `projectile::step` headless with
//! avian's spatial queries, no level needed.

use avian3d::prelude::*;
use bevy::{ecs::system::RunSystemOnce, prelude::*, time::TimeUpdateStrategy};
use game_data::{Impact, ProjectileDesc};
use game_shared::{
    physics::GameLayer,
    projectile::{ProjectileMotion, Step, collision_layers, step, steer},
};

const DT: f32 = 1.0 / 60.0;

/// Flat ground at y = 0 and a wall facing -Z whose front is at z = -`wall` (if any).
fn world(wall: Option<f32>) -> App {
    let mut app = App::new();
    app.add_plugins((MinimalPlugins, TransformPlugin, AssetPlugin::default()))
        .init_asset::<Mesh>()
        .add_plugins(PhysicsPlugins::default())
        .insert_resource(TimeUpdateStrategy::FixedTimesteps(1));
    let layers = CollisionLayers::new(GameLayer::World, LayerMask::ALL);
    app.world_mut().spawn((
        RigidBody::Static,
        Collider::cuboid(400.0, 1.0, 400.0),
        Transform::from_xyz(0.0, -0.5, 0.0),
        layers,
    ));
    if let Some(z) = wall {
        app.world_mut().spawn((
            RigidBody::Static,
            Collider::cuboid(20.0, 10.0, 1.0),
            Transform::from_xyz(0.0, 5.0, -z - 0.5),
            layers,
        ));
    }
    app.finish();
    app.cleanup();
    for _ in 0..5 {
        app.update();
    }
    app
}

/// Flies `motion` for up to `seconds` and returns every step's outcome with the state after it.
fn fly(app: &mut App, desc: &ProjectileDesc, mut motion: ProjectileMotion, seconds: f32) -> Vec<(Step, ProjectileMotion)> {
    let desc = desc.clone();
    app.world_mut()
        .run_system_once(move |spatial: SpatialQuery| {
            let filter = SpatialQueryFilter::from_mask(collision_layers(&desc));
            let mut trace = Vec::new();
            let mut age = 0.0;
            while age < seconds {
                age += DT;
                let result = step(&spatial, &filter, &desc, &mut motion, age, DT);
                trace.push((result, motion));
                if result.hit.is_some() {
                    break;
                }
            }
            trace
        })
        .unwrap()
}

fn grenade() -> ProjectileDesc {
    ProjectileDesc {
        velocity: 25.0,
        gravity: 1.0,
        time_to_live: 2.8,
        explosion_damage: 140.0,
        explosion_radius: 9.0,
        impact: Impact::Bounce,
        ..Default::default()
    }
}

/// Thrown from 1.6 m up, 15 degrees above level, towards -Z.
fn throw(speed: f32) -> ProjectileMotion {
    let direction = Vec3::new(0.0, 15f32.to_radians().sin(), -15f32.to_radians().cos());
    ProjectileMotion::new(Vec3::new(0.0, 1.6, 0.0), direction * speed, 0.0)
}

#[test]
fn a_grenade_bounces_rolls_and_comes_to_rest() {
    let mut app = world(None);
    let trace = fly(&mut app, &grenade(), throw(25.0), 10.0);
    let bounces = trace.iter().filter(|(s, _)| s.bounced).count();
    let rest = trace.iter().position(|(s, _)| s.came_to_rest).expect("comes to rest");
    let (_, at_rest) = trace[rest];
    assert!(bounces >= 2, "{bounces} bounces");
    assert!(at_rest.resting && at_rest.velocity == Vec3::ZERO);
    assert!(at_rest.position.y > 0.0 && at_rest.position.y < 0.1, "lies on the ground: {}", at_rest.position);
    // Flies ~40 m, then bounces and rolls a bit further, but stops.
    let distance = -at_rest.position.z;
    assert!((30.0..70.0).contains(&distance), "rests {distance:.1} m away");
    let (_, last) = trace.last().unwrap();
    assert_eq!(last.position, at_rest.position, "stays put");
}

#[test]
fn a_grenade_bounces_back_off_a_wall() {
    let mut app = world(Some(8.0));
    let trace = fly(&mut app, &grenade(), throw(25.0), 6.0);
    let (_, last) = trace.last().unwrap();
    assert!(last.position.z > -8.0, "stays in front of the wall: {}", last.position);
    assert!(trace.iter().all(|(_, m)| m.position.z > -8.0));
    assert!(last.resting);
}

#[test]
fn charges_stick_where_they_land_and_claymores_only_to_level_ground() {
    let mut app = world(Some(3.0));
    let c4 = ProjectileDesc {
        velocity: 5.0,
        gravity: 1.5,
        time_to_live: 9999.0,
        impact: Impact::Stick { max_angle: 180.0 },
        ..Default::default()
    };
    // Thrown at the wall: sticks to it, facing out of it.
    let trace = fly(&mut app, &c4, ProjectileMotion::new(Vec3::new(0.0, 1.5, 0.0), Vec3::NEG_Z * 8.0, 0.0), 3.0);
    let stuck = trace.iter().find_map(|(s, m)| s.stuck.map(|c| (c, *m))).expect("sticks");
    assert!(stuck.0.normal.z > 0.9, "to the wall: {:?}", stuck.0.normal);
    assert!((stuck.1.position.z + 3.0).abs() < 0.05);
    assert!(stuck.1.resting && (stuck.1.rotation * Vec3::Y).z > 0.9, "tilted with the wall");

    // A claymore thrown at the wall bounces off it and sticks to the ground.
    let claymore = ProjectileDesc {
        impact: Impact::Stick { max_angle: 45.0 },
        ..c4
    };
    let trace = fly(&mut app, &claymore, ProjectileMotion::new(Vec3::new(0.0, 1.5, 0.0), Vec3::NEG_Z * 8.0, 0.0), 3.0);
    assert!(trace.iter().any(|(s, _)| s.bounced));
    let (contact, motion) = trace.iter().find_map(|(s, m)| s.stuck.map(|c| (c, *m))).expect("sticks");
    assert!(contact.normal.y > 0.9, "to the ground: {:?}", contact.normal);
    assert!(motion.position.y < 0.05);
}

#[test]
fn rifle_grenades_bounce_off_until_armed() {
    let mut app = world(Some(5.0));
    let shell = ProjectileDesc {
        velocity: 50.0,
        gravity: 1.0,
        time_to_live: 10.0,
        explosion_damage: 150.0,
        explosion_radius: 4.5,
        arming_delay: 0.3,
        ..Default::default()
    };
    let motion = ProjectileMotion::new(Vec3::new(0.0, 1.5, 0.0), Vec3::NEG_Z * 50.0, 0.0);
    let trace = fly(&mut app, &shell, motion, 5.0);
    assert!(trace.iter().any(|(s, _)| s.bounced), "the wall 5 m away is hit before arming");
    let (hit, _) = trace.last().unwrap();
    let hit = hit.hit.expect("goes off on the next impact");
    assert!(hit.normal.y > 0.9, "on the ground: {:?}", hit.normal);
}

#[test]
fn rockets_accelerate_to_their_top_speed() {
    let mut app = world(None);
    let rocket = ProjectileDesc {
        velocity: 30.0,
        gravity: 0.1,
        time_to_live: 10.0,
        acceleration: 175.0,
        max_speed: 45.0,
        motor_delay: 0.2,
        ..Default::default()
    };
    let motion = ProjectileMotion::new(Vec3::new(0.0, 20.0, 0.0), Vec3::NEG_Z * 30.0, 0.0);
    let trace = fly(&mut app, &rocket, motion, 1.0);
    assert!(trace[5].1.velocity.length() < 30.1, "coasts until the motor starts");
    assert!((trace[59].1.velocity.length() - 45.0).abs() < 0.1);
    // Steering turns it a limited amount per tick.
    let turned = steer(trace[59].1.velocity, Vec3::X, 0.5 * DT);
    assert!((turned.normalize().angle_between(trace[59].1.velocity.normalize()) - 0.5 * DT).abs() < 1e-4);
}
