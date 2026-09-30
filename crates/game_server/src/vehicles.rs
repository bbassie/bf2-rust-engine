//! Vehicles on the server: spawners, getting in and out, switching seats, and feeding the
//! occupants' input to the simulation in `game_shared::vehicle`.
//!
//! A seated soldier stays alive but doesn't move by itself: `apply_inputs` skips it, its input
//! goes to the vehicle instead, and it is carried along at its seat every tick.

use std::collections::HashMap;

use avian3d::prelude::*;
use bevy::prelude::*;
use bevy_replicon::prelude::*;
use game_data::{VehicleCategory, VehicleSpawnerDesc};
use game_shared::{
    config::GamePaths,
    conquest::{FlagState, RoundState},
    input::{Buttons, InputFrame},
    level::{LevelEntity, LoadedLevel},
    physics::GameLayer,
    protocol::{ControlledBy, MatchInfo, Team},
    soldier::{Hitbox, InputAck, SOLDIER_CENTER, Soldier, SoldierMotion, SoldierShapes, Stance},
    soldier::Health,
    vehicle::{
        SeatInputs, Seated, Vehicle, VehicleData, VehicleHealth, VehicleLibrary, VehicleModel, VehicleMotion,
        GunStatus, VehicleShot, VehicleState, VehicleSystems, VehicleWeapons, water_height,
    },
    weapons::spread_direction,
};

use crate::{AppliedInput, InputBuffer, ServerSimSystems, chat::Notices, combat, conquest::ControlPointRules};

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
                (enter_vehicles, crash_damage, wreck_vehicles)
                    .chain()
                    .after(ServerSimSystems::ApplyInputs)
                    .before(VehicleSystems::Simulate),
                fire_vehicle_guns.after(VehicleSystems::Simulate),
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
/// How long a destroyed vehicle stays (BF2's `armor.timeToStayAsWreck`). Its hit points
/// burn down from 0 to minus the maximum meanwhile (the wreck's armor effects, down to its
/// last explosion, follow them); it is removed a moment later.
const WRECK_SECONDS: f32 = 10.0;
const WRECK_LINGER_SECONDS: f32 = 0.5;

/// Server-side: per gun, seconds until it may fire again, rounds left in the magazine,
/// seconds of reloading left, heat, and what a heat seeker is locking on to (for how long).
#[derive(Component)]
struct VehicleGuns(Vec<GunState>);

#[derive(Clone, Copy, Default)]
struct GunState {
    cooldown: f32,
    rounds: u32,
    reload: f32,
    heat: f32,
    /// Seconds it can't fire for having overheated.
    overheated: f32,
    lock: Option<(Entity, f32)>,
    /// Countermeasures: rounds left of the burst the key started, and whether the key was
    /// down last tick.
    burst: u32,
    pressed: bool,
}

/// Server-side: until when (elapsed seconds) its decoy flares draw heat seekers off the
/// vehicle.
#[derive(Component)]
pub struct Decoy(pub f32);

impl Decoy {
    pub fn active(&self, now: f32) -> bool {
        now < self.0
    }
}

/// Leaving an aircraft further than this above the ground (m) opens a parachute.
const BAIL_OUT_HEIGHT: f32 = 8.0;

/// How long decoy flares keep heat seekers off the aircraft that dropped them.
const DECOY_TIME: f32 = 3.0;
/// Share of the aircraft's velocity decoy flares keep (they fall behind in arcs).
const FLARE_INHERITED: f32 = 0.6;

/// Server-side: a destroyed vehicle, removed when the timer runs out.
#[derive(Component)]
struct Wreck(f32);

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
    level: Res<LoadedLevel>,
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
        let position = resting_position(&model, home, &spatial, water_height(Some(&level)));
        let guns = model
            .guns
            .iter()
            .map(|w| GunState {
                rounds: w.magazine_size,
                ..default()
            })
            .collect();
        let entity = commands
            .spawn((
                Vehicle { template },
                VehicleGuns(guns),
                VehicleWeapons::default(),
                Transform::from_translation(position).with_rotation(rotation),
                VehicleMotion {
                    position,
                    rotation,
                    ..default()
                },
                Replicated,
                LevelEntity,
            ))
            .id();
        spawner.vehicle = Some(entity);
        spawner.idle = 0.0;
    }
}

/// Puts a vehicle's wheels on the ground below a spawner, a boat on the water; stationary
/// weapons stand where the spawner is.
fn resting_position(model: &VehicleModel, spawner: Vec3, spatial: &SpatialQuery, water: Option<f32>) -> Vec3 {
    let desc = &model.desc;
    if desc.category == VehicleCategory::Stationary {
        return spawner;
    }
    // How far the lowest wheel (or the hull) reaches below the origin.
    let reach = desc
        .wheels
        .iter()
        .map(|w| w.radius - w.position[1])
        .fold(-desc.physics.bounds[0][1], f32::max);
    // Rest it on what actually holds its wheels up, not a plant BF2 gives no vehicle
    // collision (see `GameLayer::VehicleGround`).
    let filter = SpatialQueryFilter::from_mask(GameLayer::vehicle_movement_mask());
    let ground = spatial
        .cast_ray(spawner + Vec3::Y * 3.0, Dir3::NEG_Y, 12.0, true, &filter)
        .map_or(spawner.y, |hit| spawner.y + 3.0 - hit.distance);
    // Boats float with their floaters about half under.
    let surface = water
        .filter(|w| !desc.floaters.is_empty() && *w > ground)
        .map(|w| {
            let floater = desc.floaters.iter().map(|f| f.position[1] - f.depth * 0.5).fold(f32::MAX, f32::min);
            w - floater
        });
    match surface {
        Some(height) => Vec3::new(spawner.x, height.max(ground + reach + 0.1), spawner.z),
        None => Vec3::new(spawner.x, ground + reach + 0.1, spawner.z),
    }
}

/// The occupant of every seat of every vehicle.
fn occupancy<'a>(seated: impl Iterator<Item = (Entity, &'a Seated)>) -> HashMap<Entity, HashMap<u8, Entity>> {
    let mut map: HashMap<Entity, HashMap<u8, Entity>> = HashMap::new();
    for (soldier, seat) in seated {
        map.entry(seat.vehicle).or_default().insert(seat.seat, soldier);
    }
    map
}

/// Who sits in a seat, as far as taking it goes: players take seats from bots, never from
/// other players.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Occupant {
    Free,
    Bot,
    Player,
}

/// How a soldier getting in takes his seat.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Boarding {
    /// A free seat.
    Free(u8),
    /// A bot's seat; the bot moves on to a free one.
    Displace { seat: u8, to: u8 },
    /// A bot's seat; the bot gets out (there is no free seat for it).
    Evict(u8),
}

/// The seat a soldier getting in takes. Bots take the first free seat. Players take the
/// driver's seat even from a bot (as they would an empty vehicle's), else the first free seat,
/// else a bot's; the bot moves on to a free seat (a gunner's first, so a driver stays useful)
/// or, with none left, gets out.
fn boarding(seats: &[Occupant], gunner: impl Fn(u8) -> bool, player: bool) -> Option<Boarding> {
    let free = |seat: &u8| seats[*seat as usize] == Occupant::Free;
    let all = 0..seats.len() as u8;
    let first_free = all.clone().find(free);
    if !player {
        return first_free.map(Boarding::Free);
    }
    let seat = match seats.first()? {
        Occupant::Free => return Some(Boarding::Free(0)),
        Occupant::Bot => 0,
        Occupant::Player => match first_free {
            Some(seat) => return Some(Boarding::Free(seat)),
            None => all.clone().find(|s| seats[*s as usize] == Occupant::Bot)?,
        },
    };
    let to = all.filter(free).find(|s| gunner(*s)).or(first_free);
    Some(match to {
        Some(to) => Boarding::Displace { seat, to },
        None => Boarding::Evict(seat),
    })
}

/// An aircraft's origin this close above the ground (m) has landed (the bots' pilots use the
/// same, `bots::vehicle::LANDED_HEIGHT`).
const LANDED_HEIGHT: f32 = 3.5;
/// Beyond [`BAIL_OUT_HEIGHT`], this much more (m) before a bot is put out of an aircraft
/// under its parachute.
const EJECT_MARGIN: f32 = 4.0;

/// Whether a bot may be put out of a vehicle whose origin is `height` m above the ground to
/// make room for a player: always out of land vehicles and boats; out of aircraft only landed
/// or high enough up for his parachute to open, never in between.
fn may_evict(flies: bool, height: f32) -> bool {
    !flies || height < LANDED_HEIGHT || height > BAIL_OUT_HEIGHT + EJECT_MARGIN
}

/// Seats with guns of their own (not just countermeasures).
fn gunner_seat(desc: &game_data::VehicleDesc, seat: u8) -> bool {
    desc.weapons.iter().any(|w| w.seat == seat as u32 && w.countermeasure.is_none())
}

/// What a seat is called in notices: "the driver seat", "the gunner seat", "seat 3".
fn seat_name(desc: &game_data::VehicleDesc, seat: u8) -> String {
    match seat {
        0 if desc.category.flies() => "the pilot seat".into(),
        0 if desc.category == VehicleCategory::Stationary => "the gun".into(),
        0 => "the driver seat".into(),
        seat if gunner_seat(desc, seat) => "the gunner seat".into(),
        seat => format!("seat {}", seat + 1),
    }
}

/// A soldier getting out at `exit`, with (some of) the vehicle's speed; bailing out of an
/// aircraft high up opens his parachute.
fn step_out(motion: &mut SoldierMotion, exit: Vec3, vehicle: Entity, velocity: Vec3, flies: bool, spatial: &SpatialQuery) {
    motion.position = exit;
    // Out of an aircraft, with its speed: anything slower is run over by it.
    motion.velocity = velocity * if flies { 1.0 } else { 0.5 };
    motion.stance = Stance::Standing;
    motion.grounded = false;
    let filter = SpatialQueryFilter::from_mask([GameLayer::World, GameLayer::Vehicle]).with_excluded_entities([vehicle]);
    let ground = spatial.cast_ray(exit + Vec3::Y * 0.5, Dir3::NEG_Y, 2000.0, true, &filter);
    motion.parachute = flies && ground.is_none_or(|hit| hit.distance > BAIL_OUT_HEIGHT);
}

/// Seated soldiers: their input drives the vehicle; the use button gets out, the seat keys
/// change seats. A player asking for a bot's seat swaps seats with it.
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
    mut vehicles: Query<(&Vehicle, &VehicleData, &VehicleState, &Position, &Rotation, &LinearVelocity, &mut SeatInputs)>,
    bots: Query<(), With<crate::bots::BotBrain>>,
    mut notices: Notices,
) {
    for (.., mut inputs) in &mut vehicles {
        inputs.0.iter_mut().for_each(|seat| *seat = None);
    }
    let mut taken = occupancy(soldiers.iter().map(|(e, _, s, ..)| (e, s)));
    // Seated bots' soldiers and their players, and bots a player swapped seats with this tick
    // (their `Seated` changes after the loop; until then this is where they sit).
    let bot_soldiers: HashMap<Entity, Entity> =
        soldiers.iter().filter(|(_, c, ..)| bots.contains(c.0)).map(|(e, c, ..)| (e, c.0)).collect();
    let mut swapped: HashMap<Entity, (Entity, u8)> = HashMap::new();
    for (soldier, controlled_by, seated, mut motion, mut ack, mut applied, latch, hitbox) in &mut soldiers {
        let seat = swapped.get(&soldier).map_or(seated.seat, |(_, seat)| *seat);
        let Ok(mut buffer) = buffers.get_mut(controlled_by.0) else {
            continue;
        };
        let mut input = buffer.next();
        // Only players predict from the acks (theirs and their vehicle's); a bot's input number
        // goes up every tick and would replicate both every tick for nothing.
        if bots.contains(controlled_by.0) {
            input.seq = 0;
        }
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
        let Ok((vehicle, data, state, position, rotation, velocity, mut inputs)) = vehicles.get_mut(seated.vehicle) else {
            // The vehicle is gone.
            commands.entity(soldier).remove::<Seated>();
            set_hittable(&mut commands, hitbox, true);
            swapped.remove(&soldier);
            continue;
        };
        let model = &data.0;
        let vehicle_transform = Transform::from_translation(position.0).with_rotation(rotation.0);

        if use_pressed {
            let transforms = model.part_transforms(&state.joints);
            if let Some(exit) = exit_position(model, &transforms, vehicle_transform, seat as usize, &spatial, &shapes) {
                step_out(&mut motion, exit, seated.vehicle, velocity.0, model.desc.category.flies(), &spatial);
                commands.entity(soldier).remove::<Seated>();
                set_hittable(&mut commands, hitbox, true);
                swapped.remove(&soldier);
                if let Some(seats) = taken.get_mut(&seated.vehicle) {
                    seats.remove(&seat);
                }
                continue;
            }
        }

        let wanted = input.seat.wrapping_sub(1);
        if input.seat > 0 && wanted != seat && (wanted as usize) < model.desc.seats.len() {
            let occupant = taken.get(&seated.vehicle).and_then(|s| s.get(&wanted)).copied();
            match occupant {
                None => {
                    let seats = taken.entry(seated.vehicle).or_default();
                    seats.remove(&seat);
                    seats.insert(wanted, soldier);
                    commands.entity(soldier).insert(Seated {
                        vehicle: seated.vehicle,
                        seat: wanted,
                    });
                    set_hittable(&mut commands, hitbox, model.desc.seats[wanted as usize].open);
                    swapped.remove(&soldier);
                    continue;
                }
                // A player asking for a bot's seat swaps with it; nobody takes a player's.
                Some(other) if !bots.contains(controlled_by.0) && bot_soldiers.contains_key(&other) => {
                    let seats = taken.entry(seated.vehicle).or_default();
                    seats.insert(wanted, soldier);
                    seats.insert(seat, other);
                    commands.entity(soldier).insert(Seated {
                        vehicle: seated.vehicle,
                        seat: wanted,
                    });
                    set_hittable(&mut commands, hitbox, model.desc.seats[wanted as usize].open);
                    swapped.insert(other, (seated.vehicle, seat));
                    // Neither drives from the other's seat this tick.
                    for slot in [wanted, seat] {
                        if let Some(input) = inputs.0.get_mut(slot as usize) {
                            *input = None;
                        }
                    }
                    let bot = notices.name(bot_soldiers[&other]);
                    info!(
                        "seats: {} swapped seats with {bot} in {}: {} <-> {}",
                        notices.name(controlled_by.0),
                        vehicle.template,
                        seat,
                        wanted
                    );
                    notices.tell(
                        controlled_by.0,
                        format!("You took {} from {bot}, who moved to {}.", seat_name(&model.desc, wanted), seat_name(&model.desc, seat)),
                    );
                    continue;
                }
                // Taken by a player: stay put until it's free.
                Some(_) => {}
            }
        }

        if let Some(slot) = inputs.0.get_mut(seat as usize) {
            *slot = Some(input);
        }
    }
    // Bots a player swapped seats with move to his old seat.
    for (bot, (vehicle, seat)) in swapped {
        commands.entity(bot).insert(Seated { vehicle, seat });
        let open = vehicles
            .get(vehicle)
            .ok()
            .and_then(|(_, data, ..)| data.0.desc.seats.get(seat as usize).map(|s| s.open))
            .unwrap_or(true);
        if let Ok((.., hitbox)) = soldiers.get(bot) {
            set_hittable(&mut commands, hitbox, open);
        }
    }
}

/// Puts a soldier straight into a seat, as getting in there would (the scenarios' `SeatBot`).
pub fn seat_soldier(commands: &mut Commands, soldier: Entity, hitbox: &Hitbox, vehicle: Entity, seat: u8, open: bool) {
    commands.entity(soldier).insert(Seated { vehicle, seat });
    set_hittable(commands, hitbox, open);
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
            .shape_intersections(shapes.movement(Stance::Standing), feet + SOLDIER_CENTER + Vec3::Y * 0.05, Quat::IDENTITY, &filter)
            .is_empty();
        (!blocked).then_some(feet)
    })
}

/// Soldiers on foot: the use button near an entry point gets into the first free seat; a
/// player takes a bot's seat (the driver's first), see [`boarding`].
#[allow(clippy::type_complexity, clippy::too_many_arguments)]
fn enter_vehicles(
    mut commands: Commands,
    spatial: SpatialQuery,
    shapes: Res<SoldierShapes>,
    mut soldiers: Query<
        (Entity, &ControlledBy, &SoldierMotion, &AppliedInput, Option<&mut UseLatch>, &Hitbox),
        (With<Soldier>, Without<Seated>),
    >,
    mut seated: Query<(Entity, &Seated, &ControlledBy, &Hitbox, &mut SoldierMotion), With<Soldier>>,
    mut vehicles: Query<(
        Entity,
        &Vehicle,
        &VehicleData,
        &VehicleState,
        (&Position, &Rotation, &LinearVelocity),
        &VehicleHealth,
        &mut SeatInputs,
    )>,
    teams: Query<&Team>,
    bots: Query<(), With<crate::bots::BotBrain>>,
    mut notices: Notices,
    mut claims: Option<ResMut<crate::ai::vehicles::VehicleClaims>>,
) {
    let mut taken = occupancy(seated.iter().map(|(e, s, ..)| (e, s)));
    // Bots' soldiers in (or getting into) seats, and their players.
    let mut bot_soldiers: HashMap<Entity, Entity> =
        seated.iter().filter(|(_, _, c, ..)| bots.contains(c.0)).map(|(e, _, c, ..)| (e, c.0)).collect();
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
            .filter(|(.., health, _)| !health.wrecked())
            .filter_map(|(entity, _, data, _, (position, rotation, _), ..)| {
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
        let Some((vehicle, ..)) = nearest else {
            continue;
        };
        // Enemies can't get into a vehicle our team is using, and vice versa.
        let enemy_inside = seated
            .iter()
            .filter(|(_, s, ..)| s.vehicle == vehicle)
            .any(|(_, _, c, ..)| teams.get(c.0).is_ok_and(|t| *t != team));
        if enemy_inside {
            continue;
        }
        let Ok((_, kind, data, state, (position, rotation, velocity), _, mut inputs)) = vehicles.get_mut(vehicle) else {
            continue;
        };
        let model = &data.0;
        let player = !bots.contains(controlled_by.0);
        let seats = taken.entry(vehicle).or_default();
        let occupants: Vec<Occupant> = (0..model.desc.seats.len() as u8)
            .map(|seat| match seats.get(&seat) {
                None => Occupant::Free,
                Some(other) if bot_soldiers.contains_key(other) => Occupant::Bot,
                Some(_) => Occupant::Player,
            })
            .collect();
        let Some(plan) = boarding(&occupants, |seat| gunner_seat(&model.desc, seat), player) else {
            continue;
        };
        let seat = match plan {
            Boarding::Free(seat) => seat,
            Boarding::Displace { seat, to } => {
                let bot = seats[&seat];
                seats.insert(to, bot);
                commands.entity(bot).insert(Seated { vehicle, seat: to });
                if let Ok((.., bot_hitbox, _)) = seated.get(bot) {
                    set_hittable(&mut commands, bot_hitbox, model.desc.seats[to as usize].open);
                }
                let name = notices.name(bot_soldiers[&bot]);
                info!(
                    "seats: {} took seat {seat} of {} from bot {name}, who moved to seat {to}",
                    notices.name(controlled_by.0),
                    kind.template
                );
                notices.tell(controlled_by.0, format!("You took {} from {name}.", seat_name(&model.desc, seat)));
                seat
            }
            Boarding::Evict(seat) => {
                let bot = seats[&seat];
                let name = notices.name(bot_soldiers[&bot]);
                let flies = model.desc.category.flies();
                // Out of an aircraft only on the ground or under a parachute.
                let filter = SpatialQueryFilter::from_mask([GameLayer::World, GameLayer::Vehicle]).with_excluded_entities([vehicle]);
                let height = spatial.cast_ray(position.0, Dir3::NEG_Y, 2000.0, true, &filter).map_or(f32::MAX, |hit| hit.distance);
                let transform = Transform::from_translation(position.0).with_rotation(rotation.0);
                let exit = may_evict(flies, height)
                    .then(|| exit_position(model, &model.part_transforms(&state.joints), transform, seat as usize, &spatial, &shapes))
                    .flatten();
                let (Some(exit), Ok((.., bot_hitbox, mut bot_motion))) = (exit, seated.get_mut(bot)) else {
                    info!(
                        "seats: {} can't get into {}: bot {name} can't get out here ({height:.1} m up)",
                        notices.name(controlled_by.0),
                        kind.template
                    );
                    notices.tell(controlled_by.0, format!("No room: {name} can't get out here."));
                    continue;
                };
                step_out(&mut bot_motion, exit, vehicle, velocity.0, flies, &spatial);
                // Its squad mates aboard don't follow it out.
                if let Some(claims) = claims.as_mut() {
                    claims.displaced(bot_soldiers[&bot], vehicle);
                }
                commands.entity(bot).remove::<Seated>();
                set_hittable(&mut commands, bot_hitbox, true);
                seats.remove(&seat);
                info!(
                    "seats: {} took seat {seat} of {} from bot {name}, who got out at {exit:.1}{}",
                    notices.name(controlled_by.0),
                    kind.template,
                    if bot_motion.parachute { " under a parachute" } else { "" }
                );
                notices.tell(controlled_by.0, format!("You took {} from {name}, who got out.", seat_name(&model.desc, seat)));
                seat
            }
        };
        // What the bot put in this tick doesn't drive or fire for the new occupant.
        if let Some(input) = inputs.0.get_mut(seat as usize) {
            *input = None;
        }
        seats.insert(seat, soldier);
        if !player {
            bot_soldiers.insert(soldier, controlled_by.0);
        }
        commands.entity(soldier).insert(Seated { vehicle, seat });
        set_hittable(&mut commands, hitbox, model.desc.seats[seat as usize].open);
    }
}

/// Keeps seated soldiers at their seats (for their hitbox, capturing flags, and what clients
/// see).
fn carry_occupants(
    mut soldiers: Query<(Entity, &Seated, &AppliedInput, &mut SoldierMotion, &mut Transform), With<Soldier>>,
    vehicles: Query<(&VehicleData, &VehicleState, &Position, &Rotation, &LinearVelocity), Without<Soldier>>,
) {
    for (soldier, seated, applied, mut motion, mut transform) in &mut soldiers {
        // Getting into a vehicle under a parachute leaves the parachute behind (for everyone
        // who draws him, and for his own prediction once he gets out again).
        if motion.parachute {
            info!("parachute of {soldier} ends: in a vehicle");
        }
        let Ok((data, state, position, rotation, velocity)) = vehicles.get(seated.vehicle) else {
            continue;
        };
        let model = &data.0;
        let transforms = model.part_transforms(&state.joints);
        let seat = Transform::from_translation(position.0).with_rotation(rotation.0)
            * model.seat_transform(&transforms, seated.seat as usize);
        // Seated is about crouching height; the seat point is the feet in the seat's pose (or
        // the hips, for seats inside the hull).
        let feet = if model.desc.seats.get(seated.seat as usize).is_some_and(|s| s.soldier.is_some()) { 0.0 } else { 0.45 };
        let next = SoldierMotion {
            position: seat.translation - Vec3::Y * feet,
            velocity: velocity.0,
            yaw: applied.0.yaw,
            pitch: applied.0.pitch,
            grounded: true,
            stance: Stance::Crouching,
            parachute: false,
            ..*motion
        };
        motion.set_if_neq(next);
        let body = next.body_transform();
        if transform.translation != body.translation || transform.rotation != body.rotation {
            *transform = body;
        }
    }
}

/// Guns fire while their seat holds the trigger (the secondary button for secondary guns),
/// at their rate of fire, reloading when the magazine is empty. Where a seat has several
/// guns on one trigger, its weapon keys choose which. Machine guns overheat, heat seekers
/// lock on to enemy aircraft in their sight and hand the target to their missiles.
/// Countermeasures fire a burst per press of their key, out of their barrels in turn;
/// flares keep heat seekers off the vehicle for a while. Shells, missiles and bombs leave
/// with the vehicle's velocity (bombs are dropped at none of their own), flares with some.
#[allow(clippy::type_complexity)]
fn fire_vehicle_guns(
    mut commands: Commands,
    time: Res<Time>,
    mut vehicles: Query<(
        Entity,
        &VehicleData,
        &VehicleState,
        (&Position, &Rotation, &LinearVelocity),
        &SeatInputs,
        &VehicleHealth,
        &mut VehicleGuns,
        &mut VehicleWeapons,
    )>,
    targets: Query<(Entity, &VehicleData, &Position, &VehicleHealth, Option<&Decoy>)>,
    seated: Query<(Entity, &Seated, &ControlledBy)>,
    teams: Query<&Team>,
    mut shots: MessageWriter<ToClients<VehicleShot>>,
) {
    let dt = time.delta_secs();
    let now = time.elapsed_secs();
    let occupants: HashMap<(Entity, u8), (Entity, Entity)> = seated
        .iter()
        .map(|(soldier, s, c)| ((s.vehicle, s.seat), (soldier, c.0)))
        .collect();
    let team_of = |player: Entity| teams.get(player).copied().unwrap_or_default();
    // Aircraft someone flies, and the pilot's team.
    // Flares in the air hide them.
    let aircraft: Vec<(Entity, Vec3, Team)> = targets
        .iter()
        .filter(|(_, data, _, health, decoy)| {
            data.0.desc.category.flies() && !health.wrecked() && !decoy.is_some_and(|d| d.active(now))
        })
        .filter_map(|(entity, _, position, ..)| Some((entity, position.0, team_of(occupants.get(&(entity, 0))?.1))))
        .collect();
    for (vehicle, data, state, (position, rotation, velocity), inputs, health, mut guns, mut status) in &mut vehicles {
        if health.wrecked() {
            continue;
        }
        let model = &data.0;
        let mut transforms = None;
        let mut next = VehicleWeapons {
            guns: Vec::with_capacity(model.desc.weapons.len()),
        };
        for (index, weapon) in model.desc.weapons.iter().enumerate() {
            let (Some(gun), Some(desc)) = (guns.0.get_mut(index), model.guns.get(index)) else {
                continue;
            };
            let input = inputs.0.get(weapon.seat as usize).copied().flatten();
            let occupant = occupants.get(&(vehicle, weapon.seat as u8)).copied();
            // Among the seat's guns on the same trigger, the weapon keys pick one.
            let group: Vec<usize> = model
                .desc
                .weapons
                .iter()
                .enumerate()
                .filter(|(_, w)| w.seat == weapon.seat && w.alt_fire == weapon.alt_fire && w.countermeasure.is_none())
                .map(|(i, _)| i)
                .collect();
            let pick = input.map_or(0, |i| i.weapon as usize) % group.len().max(1);
            // Countermeasures have a key of their own.
            let selected = weapon.countermeasure.is_some() || group.get(pick) == Some(&index);
            if let (Some(countermeasure), Some(input)) = (&weapon.countermeasure, input) {
                let down = input.pressed(Buttons::COUNTERMEASURE);
                if down && !gun.pressed && gun.burst == 0 && gun.reload <= 0.0 {
                    gun.burst = countermeasure.burst;
                }
                gun.pressed = down;
            }

            gun.cooldown = (gun.cooldown - dt).max(0.0);
            if let Some(overheat) = &desc.fire.overheat {
                gun.overheated = (gun.overheated - dt).max(0.0);
                gun.heat = (gun.heat - overheat.cooling * dt).max(0.0);
            }
            let world = Transform::from_translation(position.0).with_rotation(rotation.0);
            let muzzle = |transforms: &mut Option<Vec<Transform>>| {
                let transforms = transforms.get_or_insert_with(|| model.part_transforms(&state.joints));
                world * model.muzzle(transforms, index)
            };

            // Heat seekers lock on to the enemy aircraft nearest their sight line.
            if let (Some(lock), Some((_, player))) = (&desc.fire.lock, occupant.filter(|_| selected)) {
                let sight = muzzle(&mut transforms);
                let forward = sight.rotation * Vec3::NEG_Z;
                let team = team_of(player);
                let candidate = aircraft
                    .iter()
                    .filter(|(entity, _, owner)| *entity != vehicle && *owner != team)
                    .filter_map(|(entity, target, _)| {
                        let to = *target - sight.translation;
                        let angle = to.angle_between(forward).to_degrees();
                        (to.length() <= lock.range && angle <= lock.angle).then_some((*entity, angle))
                    })
                    .min_by(|a, b| a.1.total_cmp(&b.1))
                    .map(|(entity, _)| entity);
                gun.lock = match (candidate, gun.lock) {
                    (Some(target), Some((locked, time))) if target == locked => Some((target, time + dt)),
                    (Some(target), _) => Some((target, 0.0)),
                    (None, _) => None,
                };
            } else {
                gun.lock = None;
            }

            if gun.reload > 0.0 {
                gun.reload -= dt;
                if gun.reload <= 0.0 {
                    gun.rounds = desc.magazine_size;
                }
            } else if let (Some(input), Some((soldier, player))) = (input, occupant) {
                let trigger = match weapon.countermeasure {
                    Some(_) => gun.burst > 0,
                    None => input.pressed(if weapon.alt_fire { Buttons::AIM } else { Buttons::FIRE }),
                };
                let fires = desc.projectile.velocity > 0.0 || desc.projectile.is_object();
                if trigger && selected && gun.cooldown <= 0.0 && gun.overheated <= 0.0 && fires {
                    gun.cooldown = 60.0 / desc.rounds_per_minute.max(1.0);
                    // Rounds fired from this magazine so far pick the barrel.
                    let fired = desc.magazine_size.saturating_sub(gun.rounds) as usize;
                    gun.burst = gun.burst.saturating_sub(1);
                    // A magazine size of 0 never runs dry.
                    if desc.magazine_size > 0 {
                        gun.rounds = gun.rounds.saturating_sub(1);
                        if gun.rounds == 0 {
                            gun.reload = desc.reload_time.max(0.5);
                            gun.burst = 0;
                        }
                    }
                    if let Some(overheat) = &desc.fire.overheat {
                        gun.heat += overheat.per_shot;
                        if gun.heat >= 1.0 {
                            gun.heat = 1.0;
                            gun.overheated = overheat.penalty.max(0.5);
                        }
                    }
                    let barrels = weapon.countermeasure.as_ref().map_or(&[][..], |c| c.barrels.as_slice());
                    let muzzle = match barrels.get(fired % barrels.len().max(1)) {
                        Some(barrel) => {
                            let transforms = transforms.get_or_insert_with(|| model.part_transforms(&state.joints));
                            world * transforms[weapon.part as usize] * game_shared::level::placement_transform(barrel)
                        }
                        None => muzzle(&mut transforms),
                    };
                    let forward = muzzle.rotation * Vec3::NEG_Z;
                    let direction = spread_direction(forward, desc.deviation.min, (fastrand::f32(), fastrand::f32()));
                    let target = desc
                        .fire
                        .lock
                        .and_then(|lock| gun.lock.filter(|(_, time)| *time >= lock.time))
                        .map(|(target, _)| target);
                    // Shells, missiles, bombs and smoke grenades fly on with the vehicle; flares
                    // fall behind.
                    let inherited = match &weapon.countermeasure {
                        _ if !desc.projectile.is_object() => Vec3::ZERO,
                        Some(countermeasure) if countermeasure.decoy => velocity.0 * FLARE_INHERITED,
                        _ => velocity.0,
                    };
                    combat::spawn_projectile(
                        &mut commands,
                        desc.clone(),
                        soldier,
                        player,
                        Some(vehicle),
                        muzzle.translation,
                        direction,
                        inherited,
                        target,
                    );
                    if weapon.countermeasure.as_ref().is_some_and(|c| c.decoy) {
                        commands.entity(vehicle).insert(Decoy(now + DECOY_TIME));
                    }
                    shots.write(ToClients {
                        targets: SendTargets::All,
                        message: VehicleShot {
                            vehicle,
                            gun: index as u8,
                            origin: muzzle.translation,
                            direction,
                        },
                    });
                }
            }

            let lock = match (&desc.fire.lock, gun.lock) {
                (Some(lock), Some((_, time))) => ((time / lock.time.max(0.01)).min(1.0) * 255.0) as u8,
                _ => 0,
            };
            next.guns.push(GunStatus {
                rounds: if desc.magazine_size == 0 { u16::MAX } else { gun.rounds.min(u16::MAX as u32 - 1) as u16 },
                reloading: gun.reload > 0.0,
                heat: if gun.overheated > 0.0 { 255 } else { (gun.heat * 254.0) as u8 },
                selected,
                lock,
            });
        }
        status.set_if_neq(next);
    }
}

/// Server-side: a vehicle's velocity last tick, to notice crashes. Removing it (moving a
/// vehicle by hand, like the scenarios' `PlaceVehicle`) skips the next tick's check.
#[derive(Component, Default)]
pub struct LastVelocity(Vec3);

/// Speed lost in one tick beyond which a crash does damage (m/s), and hit points per m/s
/// beyond it; aircraft are more fragile.
const CRASH_SPEED: [f32; 2] = [18.0, 9.0];
const CRASH_DAMAGE: [f32; 2] = [25.0, 60.0];

/// Hitting something hard hurts, and vehicles that can't float drown in deep water (BF2
/// `hpLostWhileInDeepWater`).
#[allow(clippy::type_complexity)]
fn crash_damage(
    mut commands: Commands,
    time: Res<Time>,
    level: Option<Res<LoadedLevel>>,
    mut vehicles: Query<(
        Entity,
        &Vehicle,
        &VehicleData,
        &Position,
        &LinearVelocity,
        &mut VehicleHealth,
        Option<&mut LastVelocity>,
    )>,
) {
    let water = water_height(level.as_deref());
    for (entity, vehicle, data, position, velocity, mut health, last) in &mut vehicles {
        let Some(mut last) = last else {
            commands.entity(entity).insert(LastVelocity(velocity.0));
            continue;
        };
        if health.wrecked() {
            continue;
        }
        let desc = &data.0.desc;
        let fragile = desc.category.flies() as usize;
        let change = (velocity.0 - last.0).length();
        last.0 = velocity.0;
        let mut damage = (change - CRASH_SPEED[fragile]).max(0.0) * CRASH_DAMAGE[fragile];
        if damage > 0.0 {
            info!("{} crashed ({change:.1} m/s)", vehicle.template);
        }
        if let Some(water) = water
            && desc.floaters.is_empty()
            && desc.category != VehicleCategory::Stationary
            && position.0.y < water - 1.0
        {
            damage += if desc.category.flies() { 400.0 } else { 100.0 } * time.delta_secs();
        }
        if damage > 0.0 {
            health.current -= damage;
        }
    }
}

/// A vehicle out of hit points kills everyone inside and stays as a wreck for a while. The
/// commander's assets stay wrecks until repaired (see `commander`); a wreck brought back
/// above 0 hit points is a vehicle again.
#[allow(clippy::type_complexity)]
fn wreck_vehicles(
    mut commands: Commands,
    time: Res<Time>,
    mut vehicles: Query<(
        Entity,
        &Vehicle,
        &VehicleData,
        &Position,
        &mut VehicleHealth,
        Option<&mut Wreck>,
        &mut SeatInputs,
    )>,
    mut soldiers: Query<(&Seated, &mut Health), With<Soldier>>,
) {
    for (vehicle, desc, data, position, mut health, wreck, mut inputs) in &mut vehicles {
        if !health.wrecked() {
            if wreck.is_some() {
                info!("{} repaired at {:.1}", desc.template, position.0);
                commands.entity(vehicle).remove::<Wreck>();
            }
            continue;
        }
        match wreck {
            None => {
                info!("{} destroyed at {:.1}", desc.template, position.0);
                commands.entity(vehicle).insert(Wreck(WRECK_SECONDS));
                for (seated, mut soldier) in &mut soldiers {
                    if seated.vehicle == vehicle {
                        soldier.current = 0.0;
                    }
                }
            }
            // It stays as it is until repaired.
            Some(_) if data.0.desc.repairable_wreck => {}
            Some(mut wreck) => {
                wreck.0 -= time.delta_secs();
                let burnt = -health.max * (1.0 - wreck.0 / WRECK_SECONDS).min(1.0);
                if burnt < health.current {
                    health.current = burnt;
                }
                if wreck.0 <= -WRECK_LINGER_SECONDS {
                    commands.entity(vehicle).despawn();
                }
            }
        }
        inputs.0.iter_mut().for_each(|seat| *seat = None);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use Occupant::{Bot, Free, Player};

    /// A jeep: driver, gunner (seat 1 has the gun), passenger.
    fn jeep_gunner(seat: u8) -> bool {
        seat == 1
    }

    #[test]
    fn bots_take_the_first_free_seat_and_never_displace() {
        assert_eq!(boarding(&[Bot, Free, Free], jeep_gunner, false), Some(Boarding::Free(1)));
        assert_eq!(boarding(&[Bot, Bot, Bot], jeep_gunner, false), None);
        assert_eq!(boarding(&[Player, Free, Free], jeep_gunner, false), Some(Boarding::Free(1)));
    }

    #[test]
    fn a_player_takes_the_driver_seat_from_a_bot_who_moves_to_the_gun() {
        assert_eq!(boarding(&[Free, Bot, Free], jeep_gunner, true), Some(Boarding::Free(0)));
        assert_eq!(boarding(&[Bot, Free, Free], jeep_gunner, true), Some(Boarding::Displace { seat: 0, to: 1 }));
        // The gun is manned: the driver moves to the passenger seat.
        assert_eq!(boarding(&[Bot, Bot, Free], jeep_gunner, true), Some(Boarding::Displace { seat: 0, to: 2 }));
        // Full of bots: the driver gets out.
        assert_eq!(boarding(&[Bot, Bot, Bot], jeep_gunner, true), Some(Boarding::Evict(0)));
    }

    #[test]
    fn players_are_never_displaced() {
        assert_eq!(boarding(&[Player, Bot, Free], jeep_gunner, true), Some(Boarding::Free(2)));
        // A player drives, the rest are bots: a bot's seat, and that bot gets out.
        assert_eq!(boarding(&[Player, Bot, Bot], jeep_gunner, true), Some(Boarding::Evict(1)));
        assert_eq!(boarding(&[Player, Player, Player], jeep_gunner, true), None);
        assert_eq!(boarding(&[Player, Player], jeep_gunner, true), None);
    }

    #[test]
    fn bots_leave_aircraft_only_landed_or_under_a_parachute() {
        assert!(may_evict(false, 50.0), "land vehicles and boats: anywhere");
        assert!(may_evict(true, 1.5), "landed");
        assert!(!may_evict(true, 5.0), "too low for a parachute");
        assert!(!may_evict(true, BAIL_OUT_HEIGHT + 1.0), "barely high enough");
        assert!(may_evict(true, 80.0), "high up, under a parachute");
    }
}
