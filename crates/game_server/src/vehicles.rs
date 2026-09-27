//! Vehicles on the server: spawners, getting in and out, switching seats, and feeding the
//! occupants' input to the simulation in `game_shared::vehicle`.
//!
//! A seated soldier stays alive but doesn't move by itself: `apply_inputs` skips it, its input
//! goes to the vehicle instead, and it is carried along at its seat every tick.

use std::collections::HashMap;

use avian3d::prelude::*;
use bevy::prelude::*;
use bevy_replicon::prelude::*;
use game_data::VehicleSpawnerDesc;
use game_shared::{
    config::GamePaths,
    conquest::{FlagState, RoundState},
    input::{Buttons, InputFrame},
    level::{LevelEntity, LoadedLevel},
    physics::GameLayer,
    protocol::{ControlledBy, MatchInfo, Team},
    soldier::{Hitbox, InputAck, SOLDIER_CENTER, Soldier, SoldierMotion, SoldierShapes, Stance},
    vehicle::{SeatInputs, Seated, Vehicle, VehicleData, VehicleLibrary, VehicleMotion, VehicleModel, VehicleState, VehicleSystems},
};

use crate::{AppliedInput, InputBuffer, ServerSimSystems, conquest::ControlPointRules};

pub struct VehiclesPlugin;

impl Plugin for VehiclesPlugin {
    fn build(&self, app: &mut App) {
        app.configure_sets(
            FixedUpdate,
            VehicleSystems::Simulate.after(ServerSimSystems::ApplyInputs),
        )
        .add_systems(
            Update,
            (
                create_spawners.run_if(resource_exists_and_changed::<LoadedLevel>),
                run_spawners,
            )
                .chain()
                .run_if(resource_exists::<LoadedLevel>)
                .run_if(in_state(ClientState::Disconnected)),
        )
        .add_systems(
            FixedUpdate,
            (
                ride_vehicles.in_set(ServerSimSystems::ApplyInputs),
                enter_vehicles
                    .after(ServerSimSystems::ApplyInputs)
                    .before(VehicleSystems::Simulate),
            )
                .run_if(in_state(ClientState::Disconnected)),
        )
        .add_systems(
            FixedPostUpdate,
            carry_occupants
                .after(VehicleSystems::Record)
                .run_if(in_state(ClientState::Disconnected)),
        );
    }
}

/// Seconds an empty vehicle may stand away from its spawner (or on its roof) before it is
/// taken back and respawned.
const ABANDON_SECONDS: f32 = 60.0;
const FLIPPED_SECONDS: f32 = 8.0;
/// Respawn delay when the spawner doesn't set one.
const DEFAULT_RESPAWN_SECONDS: f32 = 10.0;

/// Server-side: one of the layout's vehicle spawners.
#[derive(Component)]
struct VehicleSpawner {
    desc: VehicleSpawnerDesc,
    vehicle: Option<Entity>,
    /// Seconds until the next vehicle appears.
    timer: f32,
    /// Seconds the current vehicle has been abandoned.
    idle: f32,
}

impl VehicleSpawner {
    fn respawn_delay(&self) -> f32 {
        let (min, max) = (self.desc.min_respawn_seconds, self.desc.max_respawn_seconds.max(self.desc.min_respawn_seconds));
        let delay = min + (max - min) * fastrand::f32();
        if delay > 0.0 { delay } else { DEFAULT_RESPAWN_SECONDS }
    }
}

/// Server-side: whether the use button was down last tick, to act once per press.
#[derive(Component, Default)]
struct UseLatch(bool);

fn create_spawners(
    mut commands: Commands,
    level: Res<LoadedLevel>,
    paths: Res<GamePaths>,
    mut library: ResMut<VehicleLibrary>,
    match_info: Single<&MatchInfo>,
) {
    let Some(layout) = level.game_mode(&match_info.mode, match_info.size) else {
        return;
    };
    let mut count = 0;
    for desc in &layout.vehicle_spawners {
        if !desc.templates.iter().flatten().any(|t| library.exists(t, &paths)) {
            continue;
        }
        commands.spawn((
            LevelEntity,
            VehicleSpawner {
                desc: desc.clone(),
                vehicle: None,
                timer: 0.0,
                idle: 0.0,
            },
        ));
        count += 1;
    }
    info!("{count} vehicle spawners");
}

#[allow(clippy::too_many_arguments, clippy::type_complexity)]
fn run_spawners(
    mut commands: Commands,
    time: Res<Time>,
    paths: Res<GamePaths>,
    mut library: ResMut<VehicleLibrary>,
    round: Single<Ref<RoundState>>,
    mut spawners: Query<&mut VehicleSpawner>,
    vehicles: Query<(Entity, &Position, &Rotation), With<Vehicle>>,
    seated: Query<&Seated>,
    control_points: Query<(&FlagState, &ControlPointRules)>,
    spatial: SpatialQuery,
) {
    let dt = time.delta_secs();
    // A new round starts with every vehicle back at its spawner.
    if round.is_changed() && **round == RoundState::Playing {
        for (vehicle, _, _) in &vehicles {
            commands.entity(vehicle).despawn();
        }
        for mut spawner in &mut spawners {
            spawner.vehicle = None;
            spawner.timer = 0.0;
        }
        return;
    }
    for mut spawner in &mut spawners {
        let home = Vec3::from_array(spawner.desc.placement.position);
        if let Some(vehicle) = spawner.vehicle {
            match vehicles.get(vehicle) {
                Ok((_, position, rotation)) => {
                    let occupied = seated.iter().any(|s| s.vehicle == vehicle);
                    let flipped = (rotation.0 * Vec3::Y).y < 0.2;
                    let away = position.0.distance(home) > 15.0;
                    spawner.idle = if !occupied && (away || flipped) { spawner.idle + dt } else { 0.0 };
                    let limit = if flipped { FLIPPED_SECONDS } else { ABANDON_SECONDS };
                    if spawner.idle > limit {
                        commands.entity(vehicle).despawn();
                        spawner.vehicle = None;
                        spawner.timer = spawner.respawn_delay();
                    }
                }
                Err(_) => {
                    spawner.vehicle = None;
                    spawner.timer = spawner.respawn_delay();
                }
            }
            continue;
        }

        // The vehicle belongs to whoever holds the spawner's control point.
        let team = match &spawner.desc.control_point {
            Some(id) => control_points
                .iter()
                .find(|(_, rules)| rules.id == *id)
                .map_or(Team::Spectator, |(state, _)| state.owner),
            None => Team::One,
        };
        let template = match team {
            Team::One => spawner.desc.templates[0].clone(),
            Team::Two => spawner.desc.templates[1].clone(),
            Team::Spectator => None,
        };
        let Some(template) = template else {
            continue;
        };
        spawner.timer -= dt;
        if spawner.timer > 0.0 {
            continue;
        }
        let Some(model) = library.get(&template, &paths) else {
            continue;
        };
        let rotation = Quat::from_array(spawner.desc.placement.rotation);
        let position = resting_position(&model, home, &spatial);
        let entity = commands
            .spawn((
                Vehicle { template },
                Transform::from_translation(position).with_rotation(rotation),
                VehicleMotion {
                    position,
                    rotation,
                    velocity: Vec3::ZERO,
                },
                Replicated,
                LevelEntity,
            ))
            .id();
        spawner.vehicle = Some(entity);
        spawner.idle = 0.0;
    }
}

/// Puts a vehicle's wheels on the ground below a spawner.
fn resting_position(model: &VehicleModel, spawner: Vec3, spatial: &SpatialQuery) -> Vec3 {
    let desc = &model.desc;
    // How far the lowest wheel (or the hull) reaches below the origin.
    let reach = desc
        .wheels
        .iter()
        .map(|w| w.radius - w.position[1])
        .fold(-desc.physics.bounds[0][1], f32::max);
    let filter = SpatialQueryFilter::from_mask([GameLayer::World, GameLayer::Vehicle]);
    let ground = spatial
        .cast_ray(spawner + Vec3::Y * 3.0, Dir3::NEG_Y, 12.0, true, &filter)
        .map_or(spawner.y, |hit| spawner.y + 3.0 - hit.distance);
    Vec3::new(spawner.x, ground + reach + 0.1, spawner.z)
}

/// The occupant of every seat of every vehicle.
fn occupancy<'a>(seated: impl Iterator<Item = (Entity, &'a Seated)>) -> HashMap<Entity, HashMap<u8, Entity>> {
    let mut map: HashMap<Entity, HashMap<u8, Entity>> = HashMap::new();
    for (soldier, seat) in seated {
        map.entry(seat.vehicle).or_default().insert(seat.seat, soldier);
    }
    map
}

/// Seated soldiers: their input drives the vehicle; the use button gets out, the seat keys
/// change seats.
#[allow(clippy::type_complexity, clippy::too_many_arguments)]
fn ride_vehicles(
    mut commands: Commands,
    spatial: SpatialQuery,
    shapes: Res<SoldierShapes>,
    mut soldiers: Query<
        (
            Entity,
            &ControlledBy,
            &Seated,
            &mut SoldierMotion,
            &mut InputAck,
            &mut AppliedInput,
            Option<&mut UseLatch>,
            &Hitbox,
        ),
        With<Soldier>,
    >,
    mut buffers: Query<&mut InputBuffer>,
    mut vehicles: Query<(&VehicleData, &VehicleState, &Position, &Rotation, &LinearVelocity, &mut SeatInputs)>,
) {
    for (.., mut inputs) in &mut vehicles {
        inputs.0.iter_mut().for_each(|seat| *seat = None);
    }
    let mut taken = occupancy(soldiers.iter().map(|(e, _, s, ..)| (e, s)));
    for (soldier, controlled_by, seated, mut motion, mut ack, mut applied, latch, hitbox) in &mut soldiers {
        let Ok(mut buffer) = buffers.get_mut(controlled_by.0) else {
            continue;
        };
        let input = buffer.next();
        ack.set_if_neq(InputAck(input.seq));
        // Hands off the soldier's own weapons while seated. The use button stays, so a press
        // that got us out isn't seen as a new press to get back in.
        applied.0 = InputFrame {
            seq: input.seq,
            yaw: input.yaw,
            pitch: input.pitch,
            weapon: input.weapon,
            buttons: input.buttons & Buttons::USE,
            ..default()
        };
        let pressed = input.pressed(Buttons::USE);
        let use_pressed = match latch {
            Some(mut latch) => {
                let edge = pressed && !latch.0;
                latch.0 = pressed;
                edge
            }
            None => {
                commands.entity(soldier).insert(UseLatch(pressed));
                false
            }
        };
        let Ok((data, state, position, rotation, velocity, mut inputs)) = vehicles.get_mut(seated.vehicle) else {
            // The vehicle is gone.
            commands.entity(soldier).remove::<Seated>();
            set_hittable(&mut commands, hitbox, true);
            continue;
        };
        let model = &data.0;
        let vehicle_transform = Transform::from_translation(position.0).with_rotation(rotation.0);

        if use_pressed {
            let transforms = model.part_transforms(&state.joints);
            if let Some(exit) = exit_position(model, &transforms, vehicle_transform, seated.seat as usize, &spatial, &shapes) {
                motion.position = exit;
                motion.velocity = velocity.0 * 0.5;
                motion.stance = Stance::Standing;
                motion.grounded = false;
                commands.entity(soldier).remove::<Seated>();
                set_hittable(&mut commands, hitbox, true);
                if let Some(seats) = taken.get_mut(&seated.vehicle) {
                    seats.remove(&seated.seat);
                }
                continue;
            }
        }

        let wanted = input.seat.wrapping_sub(1);
        if input.seat > 0
            && wanted != seated.seat
            && (wanted as usize) < model.desc.seats.len()
            && !taken.get(&seated.vehicle).is_some_and(|s| s.contains_key(&wanted))
        {
            let seats = taken.entry(seated.vehicle).or_default();
            seats.remove(&seated.seat);
            seats.insert(wanted, soldier);
            commands.entity(soldier).insert(Seated {
                vehicle: seated.vehicle,
                seat: wanted,
            });
            set_hittable(&mut commands, hitbox, model.desc.seats[wanted as usize].open);
            continue;
        }

        if let Some(slot) = inputs.0.get_mut(seated.seat as usize) {
            *slot = Some(input);
        }
    }
}

/// Soldiers inside a closed hull can't be shot until the hull can be (vehicle damage).
fn set_hittable(commands: &mut Commands, hitbox: &Hitbox, hittable: bool) {
    let layers = if hittable {
        CollisionLayers::new(GameLayer::Soldier, LayerMask::NONE)
    } else {
        CollisionLayers::NONE
    };
    commands.entity(hitbox.entity).try_insert(layers);
}

/// Where a soldier leaving a seat can stand: the seat's exit, else the other side, behind,
/// in front or on top of the vehicle.
fn exit_position(
    model: &VehicleModel,
    transforms: &[Transform],
    vehicle: Transform,
    seat: usize,
    spatial: &SpatialQuery,
    shapes: &SoldierShapes,
) -> Option<Vec3> {
    let desc = &model.desc;
    let seat_desc = desc.seats.get(seat)?;
    let seat_part = vehicle * transforms.get(seat_desc.part as usize).copied().unwrap_or_default();
    let exit = Vec3::from_array(seat_desc.exit);
    let [min, max] = desc.physics.bounds.map(Vec3::from_array);
    let candidates = [
        seat_part.transform_point(exit),
        seat_part.transform_point(exit * Vec3::new(-1.0, 1.0, 1.0)),
        vehicle.transform_point(Vec3::new(0.0, 0.0, max.z + 1.0)),
        vehicle.transform_point(Vec3::new(0.0, 0.0, min.z - 1.0)),
        vehicle.transform_point(Vec3::new(0.0, max.y + 0.1, 0.0)),
    ];
    let filter = SpatialQueryFilter::from_mask([GameLayer::World, GameLayer::Vehicle]);
    candidates.into_iter().find_map(|point| {
        // Stand on whatever is below.
        let feet = spatial
            .cast_ray(point + Vec3::Y * 1.5, Dir3::NEG_Y, 6.0, true, &filter)
            .map_or(point, |hit| point + Vec3::Y * (1.5 - hit.distance + 0.05));
        let blocked = !spatial
            .shape_intersections(&shapes.standing, feet + SOLDIER_CENTER + Vec3::Y * 0.05, Quat::IDENTITY, &filter)
            .is_empty();
        (!blocked).then_some(feet)
    })
}

/// Soldiers on foot: the use button near an entry point gets into the first free seat.
#[allow(clippy::type_complexity)]
fn enter_vehicles(
    mut commands: Commands,
    mut soldiers: Query<
        (Entity, &ControlledBy, &SoldierMotion, &AppliedInput, Option<&mut UseLatch>, &Hitbox),
        (With<Soldier>, Without<Seated>),
    >,
    seated: Query<(Entity, &Seated, &ControlledBy)>,
    vehicles: Query<(Entity, &VehicleData, &Position, &Rotation)>,
    teams: Query<&Team>,
) {
    let mut taken = occupancy(seated.iter().map(|(e, s, _)| (e, s)));
    for (soldier, controlled_by, motion, applied, latch, hitbox) in &mut soldiers {
        let pressed = applied.0.pressed(Buttons::USE);
        let use_pressed = match latch {
            Some(mut latch) => {
                let edge = pressed && !latch.0;
                latch.0 = pressed;
                edge
            }
            None => {
                commands.entity(soldier).insert(UseLatch(pressed));
                false
            }
        };
        if !use_pressed {
            continue;
        }
        let team = teams.get(controlled_by.0).copied().unwrap_or_default();
        let chest = motion.position + Vec3::Y * 1.0;
        let nearest = vehicles
            .iter()
            .filter_map(|(entity, data, position, rotation)| {
                let transform = Transform::from_translation(position.0).with_rotation(rotation.0);
                let entries = &data.0.desc.entry_points;
                let distance = if entries.is_empty() {
                    chest.distance(position.0) - 3.0
                } else {
                    entries
                        .iter()
                        .map(|e| chest.distance(transform.transform_point(Vec3::from_array(e.position))) - e.radius)
                        .fold(f32::MAX, f32::min)
                };
                (distance <= 0.5).then_some((entity, data, distance))
            })
            .min_by(|a, b| a.2.total_cmp(&b.2));
        let Some((vehicle, data, _)) = nearest else {
            continue;
        };
        // Enemies can't get into a vehicle our team is using, and vice versa.
        let enemy_inside = seated
            .iter()
            .filter(|(_, s, _)| s.vehicle == vehicle)
            .any(|(_, _, c)| teams.get(c.0).is_ok_and(|t| *t != team));
        if enemy_inside {
            continue;
        }
        let seats = taken.entry(vehicle).or_default();
        let Some(seat) = (0..data.0.desc.seats.len() as u8).find(|s| !seats.contains_key(s)) else {
            continue;
        };
        seats.insert(seat, soldier);
        commands.entity(soldier).insert(Seated { vehicle, seat });
        set_hittable(&mut commands, hitbox, data.0.desc.seats[seat as usize].open);
    }
}

/// Keeps seated soldiers at their seats (for their hitbox, capturing flags, and what clients
/// see).
fn carry_occupants(
    mut soldiers: Query<(&Seated, &AppliedInput, &mut SoldierMotion, &mut Transform), With<Soldier>>,
    vehicles: Query<(&VehicleData, &VehicleState, &Position, &Rotation, &LinearVelocity), Without<Soldier>>,
) {
    for (seated, applied, mut motion, mut transform) in &mut soldiers {
        let Ok((data, state, position, rotation, velocity)) = vehicles.get(seated.vehicle) else {
            continue;
        };
        let model = &data.0;
        let transforms = model.part_transforms(&state.joints);
        let seat = Transform::from_translation(position.0).with_rotation(rotation.0)
            * model.seat_transform(&transforms, seated.seat as usize);
        // Seated is about crouching height; the seat point is the hips.
        let next = SoldierMotion {
            position: seat.translation - Vec3::Y * 0.45,
            velocity: velocity.0,
            yaw: applied.0.yaw,
            pitch: applied.0.pitch,
            grounded: true,
            stance: Stance::Crouching,
        };
        motion.set_if_neq(next);
        let body = next.body_transform();
        if transform.translation != body.translation || transform.rotation != body.rotation {
            *transform = body;
        }
    }
}
