//! Vehicles against soldiers on foot. The physics solver doesn't see soldiers (they are
//! moved by [`step_soldier`](game_shared::soldier::step_soldier)), so after every physics
//! step this pushes soldiers out of vehicles that drove into them, runs over those hit fast
//! enough, and carries soldiers standing on a vehicle along with it.

use std::{collections::HashMap, sync::Arc};

use avian3d::prelude::*;
use bevy::prelude::*;
use bevy_replicon::prelude::*;
use game_shared::{
    physics::GameLayer,
    protocol::ControlledBy,
    soldier::{Soldier, SoldierMotion, SoldierShapes},
    vehicle::{Seated, VehicleData, VehicleSystems},
};

use crate::combat::{Attacker, SoldierHit};

pub struct RoadkillPlugin;

impl Plugin for RoadkillPlugin {
    fn build(&self, app: &mut App) {
        app.add_systems(
            FixedPostUpdate,
            vehicles_meet_soldiers
                .after(PhysicsSystems::Writeback)
                .after(VehicleSystems::Record)
                .run_if(in_state(ClientState::Disconnected)),
        );
    }
}

/// Hit faster than this (m/s, toward the soldier) is a run-over. BF2's
/// `phy-soldier-crush-minposspeedsqr` is 12 (3.5 m/s).
const ROADKILL_SPEED: f32 = 3.5;
/// Damage per m/s above that: a kill at 6 m/s (22 km/h). BF2's
/// `coll-soldier-overrun-impact-mod` is 2.5.
const ROADKILL_DAMAGE: f32 = 100.0 / 2.5;
/// Seconds before the same soldier can be run over again (one impact, not one per tick).
const ROADKILL_COOLDOWN: f32 = 1.0;
/// Contacts this steep or flatter are the vehicle's top: the soldier rides along.
const STANDING_NORMAL_Y: f32 = 0.7;

/// Server-side: a soldier was just run over.
#[derive(Component)]
struct RunOver(f32);

#[allow(clippy::type_complexity)]
fn vehicles_meet_soldiers(
    mut commands: Commands,
    time: Res<Time>,
    mover: MoveAndSlide,
    shapes: Res<SoldierShapes>,
    vehicles: Query<(
        &Position,
        &Rotation,
        &LinearVelocity,
        &AngularVelocity,
        &ComputedCenterOfMass,
        Option<&VehicleData>,
    )>,
    seated: Query<(Entity, &Seated, &ControlledBy)>,
    mut soldiers: Query<
        (Entity, &mut SoldierMotion, &mut Transform, Option<&mut RunOver>),
        (With<Soldier>, Without<Seated>),
    >,
    mut hits: MessageWriter<SoldierHit>,
) {
    let dt = time.delta_secs();
    let drivers: HashMap<Entity, (Entity, Entity)> = seated
        .iter()
        .filter(|(_, seat, _)| seat.seat == 0)
        .map(|(soldier, seat, controlled_by)| (seat.vehicle, (soldier, controlled_by.0)))
        .collect();
    let filter = SpatialQueryFilter::from_mask(GameLayer::Vehicle);

    for (soldier, mut motion, mut transform, run_over) in &mut soldiers {
        let mut can_run_over = match run_over {
            Some(mut run_over) => {
                run_over.0 -= dt;
                let over = run_over.0 <= 0.0;
                if over {
                    commands.entity(soldier).remove::<RunOver>();
                }
                over
            }
            None => true,
        };

        let shape = shapes.movement(motion.stance);
        let center = motion.position + motion.stance.collision_center();
        let mut contacts = Vec::new();
        mover.intersections(shape, center, Quat::IDENTITY, 0.05, &filter, |vehicle, contact, normal| {
            contacts.push((vehicle, contact.point, *normal, contact.penetration));
            true
        });
        if contacts.is_empty() {
            continue;
        }

        let mut next = *motion;
        let mut carried = false;
        for (vehicle, point, normal, penetration) in contacts {
            let Ok((position, rotation, linear, angular, com, data)) = vehicles.get(vehicle) else {
                continue;
            };
            let center_of_mass = position.0 + rotation.0 * com.0;
            let velocity = linear.0 + angular.0.cross(point - center_of_mass);

            if normal.y >= STANDING_NORMAL_Y {
                // Standing on it: move along with the spot we stand on.
                if !carried {
                    next.position += velocity * dt;
                    carried = true;
                }
                continue;
            }

            // Its side: out of the way, never pushed into the ground.
            let away = Vec3::new(normal.x, normal.y.max(0.0), normal.z).normalize_or(Vec3::X);
            if penetration > 0.0 {
                next.position += away * (penetration + 0.01);
            }
            let flat = Vec3::new(away.x, 0.0, away.z).normalize_or_zero();
            let approach = velocity.dot(flat);
            if approach <= 0.0 {
                continue;
            }
            // Shoved at the vehicle's speed.
            let own = next.velocity.dot(flat);
            if own < approach {
                next.velocity += flat * (approach - own);
            }
            if approach > ROADKILL_SPEED && can_run_over {
                let driver = drivers.get(&vehicle);
                let name = data.map_or("vehicle", |data| {
                    let desc = &data.0.desc;
                    if desc.display_name.is_empty() { &desc.name } else { &desc.display_name }
                });
                hits.write(SoldierHit {
                    victim: soldier,
                    damage: (approach - ROADKILL_SPEED) * ROADKILL_DAMAGE,
                    headshot: false,
                    attacker: Attacker {
                        player: driver.map(|d| d.1),
                        soldier: driver.map(|d| d.0),
                        weapon: Arc::from(name),
                    },
                });
                commands.entity(soldier).insert(RunOver(ROADKILL_COOLDOWN));
                can_run_over = false;
            }
        }

        if next != *motion {
            *motion = next;
            *transform = next.body_transform();
        }
    }
}

#[cfg(test)]
mod tests {
    use bevy::{state::app::StatesPlugin, time::TimeUpdateStrategy};
    use game_shared::soldier::SoldierPlugin;

    use super::*;

    #[derive(Resource, Default)]
    struct Hits(Vec<f32>);

    fn collect_hits(mut hits: MessageReader<SoldierHit>, mut all: ResMut<Hits>) {
        all.0.extend(hits.read().map(|hit| hit.damage));
    }

    /// A 4 m long box on the vehicle layer driving at `velocity`, a soldier at `soldier`.
    fn run(velocity: Vec3, soldier: Vec3, seconds: f32) -> (SoldierMotion, Vec<f32>) {
        let mut app = App::new();
        app.add_plugins((MinimalPlugins, TransformPlugin, AssetPlugin::default(), StatesPlugin))
            .init_asset::<Mesh>()
            .add_plugins((PhysicsPlugins::default(), SoldierPlugin, RoadkillPlugin))
            .insert_state(ClientState::Disconnected)
            .add_message::<SoldierHit>()
            .init_resource::<Hits>()
            .add_systems(FixedPostUpdate, collect_hits.after(vehicles_meet_soldiers))
            .insert_resource(TimeUpdateStrategy::FixedTimesteps(1));
        app.world_mut().spawn((
            RigidBody::Kinematic,
            Collider::cuboid(2.0, 1.5, 4.0),
            Transform::from_xyz(0.0, 0.75, 0.0),
            LinearVelocity(velocity),
            CollisionLayers::new(GameLayer::Vehicle, [GameLayer::World, GameLayer::Vehicle]),
        ));
        let player = app.world_mut().spawn_empty().id();
        let soldier = app
            .world_mut()
            .spawn((Soldier, SoldierMotion::at(soldier, 0.0), ControlledBy(player)))
            .id();
        app.finish();
        app.cleanup();
        for _ in 0..(seconds * 64.0) as usize {
            app.update();
        }
        let motion = *app.world().get::<SoldierMotion>(soldier).unwrap();
        (motion, app.world().resource::<Hits>().0.clone())
    }

    #[test]
    fn fast_vehicles_run_soldiers_over() {
        // 10 m/s towards a soldier 5 m ahead: one hit that kills, and shoved away.
        let (motion, hits) = run(Vec3::new(0.0, 0.0, -10.0), Vec3::new(0.0, 0.0, -5.0), 1.0);
        println!("{motion:?} {hits:?}");
        assert_eq!(hits.len(), 1);
        assert!(hits[0] >= 100.0);
        assert!(motion.velocity.z < -5.0);
    }

    #[test]
    fn slow_vehicles_push() {
        let (motion, hits) = run(Vec3::new(0.0, 0.0, -2.0), Vec3::new(0.0, 0.0, -3.0), 1.5);
        println!("{motion:?} {hits:?}");
        assert!(hits.is_empty());
        // Kept out of the hull (its front is at -2 - 3 m by now).
        assert!(motion.position.z < -2.0 - 2.0 * 1.5 - 0.2, "{motion:?}");
    }

    #[test]
    fn riders_are_carried() {
        let (motion, hits) = run(Vec3::new(3.0, 0.0, 0.0), Vec3::new(0.0, 1.51, 0.0), 1.0);
        println!("{motion:?}");
        assert!(hits.is_empty());
        assert!((motion.position.x - 3.0).abs() < 0.2, "{motion:?}");
    }
}
