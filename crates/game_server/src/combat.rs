//! Weapons on the server: firing and throwing, projectiles, explosions, damage and death.
//!
//! Projectiles are simulated here only. Clients are told about each shot (for tracers and
//! sounds), see grenades, rockets and charges through their replicated
//! [`ProjectileMotion`], and hear about hits, kills and detonations, but never decide them.
//! Hits on destroyable objects and explosions are passed on as messages (see
//! `destruction`).
//!
//! Bullets and rockets hit soldiers in BF2's per-bone hit zones (`game_shared::hitzones`),
//! each body part through its own damage table column. They are judged against where the
//! soldiers were in the world the shooter saw: clients show other soldiers a little in the
//! past, and say which server tick they were looking at (`InputFrame::view_tick`), so the
//! server keeps a short history of every soldier's pose and rewinds up to [`MAX_REWIND`].

use std::{collections::VecDeque, sync::Arc};

use avian3d::prelude::*;
use bevy::{ecs::system::SystemParam, prelude::*};
use bevy_replicon::{prelude::*, server::server_tick::ServerTick};
use game_data::{FireKind, FireMode, Guidance, HitZone, TriggerBy, WeaponDesc};
use game_shared::{
    conquest::RoundState,
    effects::PlayEffect,
    hitzones::{BodyPose, HEAD, ServerClock},
    input::Buttons,
    physics::GameLayer,
    projectile::{
        self, Projectile, ProjectileMotion, SmokeCloud, collision_layers, in_trigger, launch_origin, launch_velocity,
        steer,
    },
    protocol::{ControlledBy, HitConfirmed, Player, ShotFired, Team},
    revive::{Downed, WRECK_HIT_POINTS},
    soldier::{Health, Hitbox, Soldier, SoldierMotion},
    statics::Destructible,
    vehicle::{BLAST_MATERIAL, Seated, VehicleData, VehicleHealth, VehicleState, armor_damage_modifier},
    weapons::{Armory, Fired, Inventory, Loadout, Trigger, WeaponState, damage_at, spread_direction},
};

use crate::{
    AppliedInput, HostPlayer, PlayerClient, ServerSettings, ServerSimSystems,
    abilities::{BleedOut, Deaths, hurts_downed},
    destruction::Materials,
    vehicles::Decoy,
};

pub struct CombatPlugin;

impl Plugin for CombatPlugin {
    fn build(&self, app: &mut App) {
        app.add_message::<Died>()
            .add_message::<Explosion>()
            .add_message::<StaticHit>()
            .add_message::<SoldierHit>()
            .add_message::<VehicleHit>()
            .add_message::<Detonation>()
            .add_systems(
                FixedUpdate,
                (
                    clear_projectiles,
                    record_poses,
                    fire_weapons,
                    simulate_projectiles,
                    trigger_mines,
                    detonate,
                    age_smoke,
                    explode,
                    damage_vehicles,
                    damage_soldiers,
                    kill_the_dead,
                )
                    .chain()
                    .in_set(CombatSystems)
                    .after(ServerSimSystems::ApplyInputs)
                    .run_if(in_state(ClientState::Disconnected)),
            );
    }
}

/// Weapons, projectiles and deaths, after inputs are applied.
#[derive(SystemSet, Debug, Clone, PartialEq, Eq, Hash)]
pub struct CombatSystems;

/// Server-side: a player's soldier died (for tickets and other rules).
#[derive(Message, Clone, Copy, Debug)]
pub struct Died {
    pub player: Entity,
    pub team: Team,
}

/// Who caused damage: for friendly fire, hit markers and the kill feed.
#[derive(Clone, Debug)]
pub struct Attacker {
    pub player: Option<Entity>,
    /// The soldier that fired, so it can hurt itself even without friendly fire.
    pub soldier: Option<Entity>,
    /// Weapon name for the kill feed.
    pub weapon: Arc<str>,
}

/// Server-side: a blast (grenades, rockets, exploding objects). Hurts soldiers here and
/// destroyable objects in `destruction`.
#[derive(Message, Clone, Debug)]
pub struct Explosion {
    pub position: Vec3,
    /// Damage at the center, falling off linearly to 0 at `radius`.
    pub damage: f32,
    pub radius: f32,
    /// The damage table row, against the armor material of what is caught in it.
    pub material: u32,
    pub attacker: Attacker,
    /// Directional charges (claymores): the blast's forward axis and how many degrees off it
    /// the blast reaches.
    pub cone: Option<(Vec3, f32)>,
}

impl Explosion {
    /// Whether `point` is in the blast's direction (always, unless it is directional).
    pub fn reaches(&self, point: Vec3) -> bool {
        match self.cone {
            Some((forward, degrees)) => (point - self.position).angle_between(forward).to_degrees() <= degrees,
            None => true,
        }
    }
}

/// Server-side: a projectile hit a part of a destroyable object.
#[derive(Message, Clone, Debug)]
pub struct StaticHit {
    pub part: Entity,
    pub damage: f32,
    /// The projectile's material (the damage table row).
    pub material: u32,
    pub attacker: Attacker,
}

/// Server-side: damage for a soldier this tick.
#[derive(Message, Clone, Debug)]
pub(crate) struct SoldierHit {
    pub victim: Entity,
    pub damage: f32,
    pub headshot: bool,
    pub attacker: Attacker,
}

/// Server-side: damage for a vehicle this tick, through its armor already.
#[derive(Message, Clone, Debug)]
struct VehicleHit {
    vehicle: Entity,
    damage: f32,
    attacker: Attacker,
}

/// Server-side: a grenade, rocket or charge goes off.
#[derive(Message, Clone, Debug)]
struct Detonation {
    /// The projectile, unless it went off in the hand.
    entity: Option<Entity>,
    weapon: Arc<WeaponDesc>,
    position: Vec3,
    /// Surface normal where it lies, or up.
    normal: Vec3,
    /// Its forward axis, for directional charges.
    facing: Vec3,
    attacker: Attacker,
}

/// Damage multiplier for hits on the head when the damage table isn't imported.
const HEADSHOT_MULTIPLIER: f32 = 2.5;
/// Hits are judged at most this many ticks in the past (250 ms).
pub const MAX_REWIND: u32 = 15;
/// Ticks of pose history kept per soldier.
const POSE_HISTORY: usize = MAX_REWIND as usize + 4;
/// Soldiers' `armor.defaultMaterial` (Human_body): the damage table column for explosions.
const SOLDIER_ARMOR_MATERIAL: u32 = 24;
/// How far wire-guided missiles look for what the shooter aims at.
const GUIDANCE_RANGE: f32 = 2000.0;
/// Guided missiles only follow an aim point at most this far off their nose (radians; BF2's
/// `seek.maxAngleLock 90` of every wire-guided missile).
const SEEKER_ANGLE: f32 = std::f32::consts::FRAC_PI_2;
/// Heat seekers turn this many times their `follow.maxYaw` in degrees per second, within
/// these radians per second, and go off this close to their target (meters).
const HEAT_TURN: f32 = 5.0;
const HEAT_TURN_RANGE: [f32; 2] = [1.0, 3.0];
const HEAT_PROXIMITY: f32 = 4.0;
/// Vehicle mines go off under bodies at least this heavy (kg).
const HEAVY: f32 = 1000.0;
/// Smoke clouds billow up around this far above the grenade.
const SMOKE_HEIGHT: f32 = 1.5;
/// Seconds a smoke cloud stays after the grenade stops smoking: the effect's particles
/// live about this long.
const SMOKE_LINGER: f32 = 8.0;

/// A projectile in flight or lying around (server only). All have a [`ProjectileMotion`];
/// grenades, rockets and charges also the replicated [`Projectile`].
#[derive(Component)]
struct Live {
    weapon: Arc<WeaponDesc>,
    /// Its weapon in the shooter's loadout (a guided missile is steered while it's in hand);
    /// `u8::MAX` for vehicle guns (steered while the shooter stays seated).
    weapon_index: u8,
    shooter: Entity,
    shooter_player: Entity,
    hitbox: Option<Entity>,
    travelled: f32,
    age: f32,
    /// Seconds after launch it goes off by itself (its lifetime, minus cooking).
    fuse: f32,
    /// Steered by the shooter's aim until he lets go of the launcher.
    guided: bool,
    /// Heat seekers: the aircraft it was locked on to.
    target: Option<Entity>,
    /// Ticks the world the shooter saw lagged behind the server's: soldiers are hit where
    /// they were that long ago.
    rewind: u32,
}

impl Live {
    fn attacker(&self) -> Attacker {
        Attacker {
            player: Some(self.shooter_player),
            soldier: Some(self.shooter),
            weapon: Arc::from(self.weapon.name.as_str()),
        }
    }
}

/// Where a soldier was over the last ticks, by server tick (server only).
#[derive(Component, Default)]
struct PoseHistory(VecDeque<(u32, BodyPose)>);

impl PoseHistory {
    /// The pose at `tick`, or the oldest one kept if that is further back.
    fn at(&self, tick: u32) -> Option<BodyPose> {
        self.0
            .iter()
            .rev()
            .find(|(t, _)| *t <= tick)
            .or(self.0.front())
            .map(|(_, pose)| *pose)
    }
}

/// The server tick this tick's state goes out with (replicon counts it up after the
/// simulation).
fn current_tick(tick: &ServerTick) -> u32 {
    tick.get().wrapping_add(1)
}

/// Remembers where every soldier is this tick, and tells clients which tick it is.
#[allow(clippy::type_complexity)]
fn record_poses(
    mut commands: Commands,
    tick: Res<ServerTick>,
    mut clock: Query<&mut ServerClock>,
    mut soldiers: Query<(Entity, &SoldierMotion, Has<Seated>, Option<&mut PoseHistory>), With<Soldier>>,
) {
    let tick = current_tick(&tick);
    match clock.single_mut() {
        Ok(mut clock) => clock.0 = tick,
        Err(_) => {
            commands.spawn((ServerClock(tick), Replicated));
        }
    }
    for (entity, motion, seated, history) in &mut soldiers {
        let pose = BodyPose::of(motion, seated);
        match history {
            Some(mut history) => {
                history.0.push_back((tick, pose));
                while history.0.len() > POSE_HISTORY {
                    history.0.pop_front();
                }
            }
            None => {
                commands.entity(entity).insert(PoseHistory(VecDeque::from([(tick, pose)])));
            }
        }
    }
}

/// Ticks to rewind for an input made looking at `view_tick` (0: the present).
/// `BF2_NO_LAG_COMPENSATION` turns rewinding off, for comparison.
fn rewind_for(view_tick: u32, now: u32) -> u32 {
    static OFF: std::sync::LazyLock<bool> =
        std::sync::LazyLock::new(|| std::env::var_os("BF2_NO_LAG_COMPENSATION").is_some());
    match view_tick {
        0 => 0,
        _ if *OFF => 0,
        view => now.checked_sub(view).unwrap_or(0).min(MAX_REWIND),
    }
}

/// Where to send messages meant for a player's human, if it has one.
fn player_client(
    player: Entity,
    clients: &Query<&PlayerClient>,
    host: Option<&HostPlayer>,
) -> Option<ClientId> {
    if host.is_some_and(|h| h.0 == player) {
        return Some(ClientId::Server);
    }
    clients.get(player).ok().map(|c| ClientId::Client(c.0))
}

/// A new round starts with no charges or smoke lying around, and those of players who
/// left go with them.
fn clear_projectiles(
    mut commands: Commands,
    rounds: Query<Ref<RoundState>>,
    projectiles: Query<(Entity, &Live)>,
    clouds: Query<Entity, With<SmokeCloud>>,
    players: Query<(), With<Player>>,
    mut round_over: Local<bool>,
) {
    let new_round = rounds.single().is_ok_and(|round| {
        let over = matches!(*round, RoundState::Ended { .. });
        let new_round = round.is_changed() && !over && *round_over;
        *round_over = over;
        new_round
    });
    for (entity, live) in &projectiles {
        if new_round || !players.contains(live.shooter_player) {
            commands.entity(entity).despawn();
        }
    }
    if new_round {
        for entity in &clouds {
            commands.entity(entity).despawn();
        }
    }
}

#[allow(clippy::type_complexity, clippy::too_many_arguments)]
fn fire_weapons(
    mut commands: Commands,
    time: Res<Time>,
    armory: Res<Armory>,
    spatial: SpatialQuery,
    host: Option<Res<HostPlayer>>,
    clients: Query<&PlayerClient>,
    players: Query<&Player>,
    mut soldiers: Query<(
        Entity,
        &ControlledBy,
        &SoldierMotion,
        &AppliedInput,
        &Loadout,
        &mut Inventory,
        &mut WeaponState,
        Option<&Hitbox>,
    ), Without<Seated>>,
    placed: Query<(Entity, &Live, &ProjectileMotion)>,
    tick: Res<ServerTick>,
    mut shots: MessageWriter<ToClients<ShotFired>>,
    mut detonations: MessageWriter<Detonation>,
) {
    let dt = time.delta_secs();
    let now = current_tick(&tick);
    for (soldier, controlled_by, motion, applied, loadout, mut inventory, mut state, hitbox) in
        &mut soldiers
    {
        let input = applied.0;
        let player = controlled_by.0;

        // Weapon switching.
        if input.weapon != inventory.active && (input.weapon as usize) < loadout.weapons.len() {
            inventory.active = input.weapon;
            inventory.fire_mode = 0;
            inventory.reloading = false;
            match armory.weapon(&loadout.weapons[input.weapon as usize]) {
                Some(weapon) => state.switch_to(weapon),
                None => state.deploy = 0.5,
            }
        }
        let active = inventory.active as usize;
        let Some(weapon) = loadout.weapons.get(active).and_then(|w| armory.weapon(w)).cloned()
        else {
            continue;
        };

        let local = Quat::from_rotation_y(-motion.yaw) * motion.velocity;
        state.tick(&weapon.deviation, dt, -local.z, local.x, !motion.grounded);

        // Not on ladders, nor just after a jump or getting up.
        let trigger = Trigger {
            fire: input.pressed(Buttons::FIRE) && motion.can_fire(),
            alt: input.pressed(Buttons::AIM) && motion.can_fire(),
            reload: input.pressed(Buttons::RELOAD),
            lowered: input.pressed(Buttons::SPRINT) && input.movement[1] > 64,
        };
        // Fire mode cycling on the button's press.
        let mode_pressed = input.pressed(Buttons::FIRE_MODE);
        if mode_pressed && !state.mode_was_down && !trigger.fire {
            inventory.fire_mode = ((inventory.fire_mode as usize + 1) % weapon.fire_modes.len().max(1)) as u8;
        }
        state.mode_was_down = mode_pressed;
        let mode = weapon
            .fire_modes
            .get(inventory.fire_mode as usize)
            .copied()
            .unwrap_or(FireMode::Single);

        let zoomed = input.pressed(Buttons::AIM);
        let cone = state.deviation(&weapon.deviation, motion.stance, zoomed);
        let mut ammo = inventory.ammo.get(active).copied().unwrap_or([0, 0]);
        let fired = state.trigger(&weapon, mode, &mut ammo, trigger, dt);
        if inventory.ammo.get(active).is_some_and(|a| *a != ammo) {
            inventory.ammo[active] = ammo;
        }
        let reloading = state.reload > 0.0;
        if inventory.reloading != reloading {
            inventory.reloading = reloading;
        }
        let Some(fired) = fired else {
            continue;
        };

        let name = players.get(player).map_or("?", |p| p.name.as_str());
        let attacker = Attacker {
            player: Some(player),
            soldier: Some(soldier),
            weapon: Arc::from(weapon.name.as_str()),
        };
        let eye = motion.eye_position();
        let view = motion.view_rotation();
        let aim = view * Vec3::NEG_Z;
        let desc = &weapon.projectile;
        match fired {
            Fired::Detonate => {
                let charges: Vec<_> = placed
                    .iter()
                    .filter(|(_, live, _)| live.shooter_player == player && live.weapon.name == weapon.name)
                    .collect();
                info!("{name} sets off {} {}", charges.len(), weapon.name);
                for (entity, live, charge) in charges {
                    detonations.write(Detonation {
                        entity: Some(entity),
                        weapon: live.weapon.clone(),
                        position: charge.position,
                        normal: charge.rotation * Vec3::Y,
                        facing: charge.facing(),
                        attacker: attacker.clone(),
                    });
                }
            }
            Fired::InHand => {
                info!("{name} held {} too long", weapon.name);
                detonations.write(Detonation {
                    entity: None,
                    weapon: weapon.clone(),
                    position: eye + view * Vec3::new(0.2, -0.3, -0.3),
                    normal: Vec3::Y,
                    facing: aim,
                    attacker,
                });
            }
            Fired::Launch { cooked, soft } => {
                let origin = launch_origin(&spatial, eye, view, Vec3::from(weapon.fire.start_offset));
                // Beyond the weapon's limit of charges or mines, the oldest goes.
                let limit = weapon.fire.max_in_world as usize;
                if limit > 0 {
                    let mut mine: Vec<(f32, Entity)> = placed
                        .iter()
                        .filter(|(_, live, _)| live.shooter_player == player && live.weapon.name == weapon.name)
                        .map(|(entity, live, _)| (live.age, entity))
                        .collect();
                    mine.sort_by(|a, b| b.0.total_cmp(&a.0));
                    for (_, entity) in mine.iter().take((mine.len() + 1).saturating_sub(limit)) {
                        commands.entity(*entity).despawn();
                    }
                }
                let direction = spread_direction(aim, cone, (fastrand::f32(), fastrand::f32()));
                let pellets = weapon.projectiles_per_shot.max(1);
                for _ in 0..pellets {
                    let direction = match pellets {
                        1 => direction,
                        _ => spread_direction(direction, weapon.pellet_spread, (fastrand::f32(), fastrand::f32())),
                    };
                    let mut projectile = commands.spawn((
                        Live {
                            weapon: weapon.clone(),
                            weapon_index: active as u8,
                            shooter: soldier,
                            shooter_player: player,
                            hitbox: hitbox.map(|h| h.entity),
                            travelled: 0.0,
                            age: 0.0,
                            fuse: desc.time_to_live - cooked,
                            guided: weapon.fire.guidance == Guidance::Wire,
                            target: None,
                            rewind: rewind_for(input.view_tick, now),
                        },
                        ProjectileMotion::new(origin, launch_velocity(&weapon, direction, soft, motion.velocity), motion.yaw),
                    ));
                    if desc.is_object() {
                        projectile.insert((
                            Projectile {
                                player,
                                weapon: weapon.name.clone(),
                            },
                            Replicated,
                        ));
                    }
                    // The shooter's client already showed its own shot.
                    let targets = match player_client(player, &clients, host.as_deref()) {
                        Some(client) => SendTargets::AllExcept(client),
                        None => SendTargets::All,
                    };
                    shots.write(ToClients {
                        targets,
                        message: ShotFired {
                            soldier,
                            origin,
                            direction,
                            weapon: active as u8,
                        },
                    });
                }
                debug!(
                    "{name} ({player:?}) fires {} ({} ticks back, saw tick {} at {now})",
                    weapon.name,
                    rewind_for(input.view_tick, now),
                    input.view_tick
                );
                if desc.is_object() {
                    let how = match (weapon.fire.kind, soft) {
                        (FireKind::Gun, _) => "fires",
                        (_, true) => "rolls",
                        _ => "throws",
                    };
                    info!(
                        "{name} {how} {} from {origin:.1} at {:.0} m/s{}",
                        weapon.name,
                        launch_velocity(&weapon, aim, soft, motion.velocity).length(),
                        if cooked > 0.0 { format!(", cooked {cooked:.2} s") } else { String::new() }
                    );
                }
            }
        }
    }
}

/// A soldier bullets can hit, as the projectiles see it this tick.
struct Target<'a> {
    entity: Entity,
    pose: BodyPose,
    history: Option<&'a PoseHistory>,
    zones: &'a [HitZone],
}

impl Target<'_> {
    /// Its pose `rewind` ticks ago.
    fn pose(&self, now: u32, rewind: u32) -> BodyPose {
        match (rewind, self.history) {
            (1.., Some(history)) => history.at(now.wrapping_sub(rewind)).unwrap_or(self.pose),
            _ => self.pose,
        }
    }
}

/// The damage table factor for a projectile hitting a soldier's body part.
fn body_part_factor(materials: Option<&Materials>, projectile: u32, part: u32) -> f32 {
    match materials {
        Some(materials) if !materials.0.damage.is_empty() => materials.0.damage_mod(projectile, part),
        _ if part == HEAD => HEADSHOT_MULTIPLIER,
        _ => 1.0,
    }
}

#[allow(clippy::too_many_arguments, clippy::type_complexity)]
fn simulate_projectiles(
    mut commands: Commands,
    time: Res<Time>,
    spatial: SpatialQuery,
    tick: Res<ServerTick>,
    armory: Res<Armory>,
    mut projectiles: Query<(Entity, &mut Live, &mut ProjectileMotion)>,
    shooters: Query<(&SoldierMotion, &Inventory, Has<Seated>)>,
    colliders: Query<&ColliderOf>,
    soldiers: Query<
        (Entity, &SoldierMotion, &Loadout, Option<&PoseHistory>, Option<&Seated>),
        (With<Soldier>, Without<Downed>),
    >,
    vehicles: Query<(&VehicleData, &Position, &LinearVelocity, &Rotation, Option<&VehicleState>, Option<&Decoy>)>,
    materials: Option<Res<Materials>>,
    destructibles: Query<(), With<Destructible>>,
    mut soldier_hits: MessageWriter<SoldierHit>,
    mut vehicle_hits: MessageWriter<VehicleHit>,
    mut static_hits: MessageWriter<StaticHit>,
    mut detonations: MessageWriter<Detonation>,
) {
    let dt = time.delta_secs();
    let now = current_tick(&tick);
    // Soldiers inside closed vehicles can't be hit, those on open seats can; bullets pass
    // over the critically wounded (only blasts and shock paddles reach them).
    let targets: Vec<Target> = soldiers
        .iter()
        .filter(|(.., seated)| {
            seated.is_none_or(|s| {
                vehicles
                    .get(s.vehicle)
                    .is_ok_and(|(v, ..)| v.0.desc.seats.get(s.seat as usize).is_some_and(|seat| seat.open))
            })
        })
        .map(|(entity, motion, loadout, history, seated)| Target {
            entity,
            pose: BodyPose::of(motion, seated.is_some()),
            history,
            zones: armory.hit_zones(&loadout.kit),
        })
        .collect();
    for (entity, mut live, mut motion) in &mut projectiles {
        live.age += dt;
        let weapon = live.weapon.clone();
        let desc = &weapon.projectile;
        let goes_off = desc.goes_off();
        if live.age >= live.fuse {
            if goes_off {
                detonations.write(Detonation {
                    entity: Some(entity),
                    weapon: weapon.clone(),
                    position: motion.position,
                    normal: if motion.resting { motion.rotation * Vec3::Y } else { Vec3::Y },
                    facing: motion.facing(),
                    attacker: live.attacker(),
                });
            } else {
                commands.entity(entity).despawn();
            }
            continue;
        }

        let mut next = *motion;
        if live.guided {
            // Wire guided: towards whatever the shooter aims at, while the launcher is in his
            // hands and not being reloaded (or he stays at the vehicle's sight).
            let holding = shooters.get(live.shooter).ok().filter(|(_, inventory, seated)| match live.weapon_index {
                u8::MAX => *seated,
                index => inventory.active == index && !inventory.reloading,
            });
            match holding {
                Some((shooter, ..)) => {
                    let eye = shooter.eye_position();
                    let aim = shooter.view_rotation() * Vec3::NEG_Z;
                    let mut filter = SpatialQueryFilter::from_mask([GameLayer::World, GameLayer::Vehicle, GameLayer::Soldier]);
                    if let Some(hitbox) = live.hitbox {
                        filter = filter.with_excluded_entities([hitbox]);
                    }
                    let distance = Dir3::new(aim)
                        .ok()
                        .and_then(|dir| spatial.cast_ray(eye, dir, GUIDANCE_RANGE, true, &filter))
                        .map_or(GUIDANCE_RANGE, |hit| hit.distance);
                    let to = eye + aim * distance - next.position;
                    let in_view = to.angle_between(next.velocity) <= SEEKER_ANGLE;
                    if in_view && to.length() > desc.guidance_min_distance && live.age >= desc.motor_delay {
                        next.velocity = steer(next.velocity, to, desc.turn_rate * dt);
                    }
                    debug!(
                        "{} at {:.1} flying {:.2}, steering towards {:.1}",
                        weapon.name,
                        next.position,
                        next.velocity.normalize_or_zero(),
                        eye + aim * distance
                    );
                }
                None => {
                    live.guided = false;
                    info!("{} lost its guidance at {:.1}", weapon.name, next.position);
                }
            }
        }

        if let Some(target) = live.target {
            // Heat seeking: towards where the aircraft will be, until it slips out of view.
            match vehicles.get(target) {
                Ok((.., Some(decoy))) if decoy.active(time.elapsed_secs()) => {
                    info!("{} lost its target to decoy flares", weapon.name);
                    live.target = None;
                }
                Ok((_, position, velocity, ..)) => {
                    let to = position.0 - next.position;
                    if to.length() < HEAT_PROXIMITY {
                        detonations.write(Detonation {
                            entity: Some(entity),
                            weapon: weapon.clone(),
                            position: next.position,
                            normal: Vec3::Y,
                            facing: next.velocity.normalize_or(Vec3::NEG_Z),
                            attacker: live.attacker(),
                        });
                        continue;
                    }
                    let closing = next.velocity.length().max(desc.max_speed).max(50.0);
                    let lead = to + velocity.0 * (to.length() / closing).min(3.0);
                    if lead.angle_between(next.velocity) > SEEKER_ANGLE {
                        live.target = None;
                    } else if live.age >= desc.motor_delay {
                        let [min, max] = HEAT_TURN_RANGE;
                        let turn = (desc.turn_rate.to_radians() * HEAT_TURN).clamp(min, max);
                        next.velocity = steer(next.velocity, lead, turn * dt);
                    }
                }
                Err(_) => live.target = None,
            }
        }

        let mut filter = SpatialQueryFilter::from_mask(collision_layers(desc));
        if let Some(hitbox) = live.hitbox {
            filter = filter.with_excluded_entities([hitbox]);
        }
        let (shooter, rewind) = (live.shooter, live.rewind);
        let soldier_along = |origin: Vec3, direction: Dir3, length: f32| {
            targets
                .iter()
                .filter(|target| target.entity != shooter)
                .filter_map(|target| {
                    let hit = target.pose(now, rewind).ray(target.zones, origin, *direction, length)?;
                    Some((target, hit))
                })
                .min_by(|a, b| a.1.distance.total_cmp(&b.1.distance))
                .map(|(target, hit)| {
                    if rewind > 0 {
                        let now_too = target.pose.ray(target.zones, origin, *direction, length).is_some();
                        debug!(
                            "rewound hit on {:?}: {}",
                            target.entity,
                            if now_too { "a hit without rewinding too" } else { "a miss without rewinding" }
                        );
                    }
                    let entity = target.entity;
                    let contact = projectile::Contact {
                        entity,
                        point: hit.point,
                        normal: hit.normal,
                        body_part: Some(hit.material),
                    };
                    (hit.distance, contact)
                })
        };
        let incoming = next.velocity;
        let step = projectile::step(&spatial, &filter, desc, &mut next, live.age, dt, soldier_along);
        if next != *motion {
            *motion = next;
        }
        live.travelled += step.distance;
        if step.bounced {
            debug!("{} bounced at {:.2}, now {:.1} m/s", weapon.name, motion.position, motion.velocity.length());
        }
        if step.came_to_rest {
            info!("{} came to rest at {:.2} after {:.2} s", weapon.name, motion.position, live.age);
        }
        if let Some(contact) = step.stuck {
            info!("{} stuck at {:.2} (surface normal {:.2})", weapon.name, contact.point, contact.normal);
        }
        let Some(hit) = step.hit else {
            continue;
        };

        let attacker = live.attacker();
        let body = colliders.get(hit.entity).map(|c| c.body).unwrap_or(hit.entity);
        if let Some(part) = hit.body_part {
            let factor = body_part_factor(materials.as_deref(), desc.material, part);
            let damage = damage_at(desc, live.travelled) * factor;
            if let Some(target) = targets.iter().find(|t| t.entity == hit.entity) {
                let moved = target.pose(now, live.rewind).position.distance(target.pose.position);
                debug!(
                    "{} of {:?} hit {:?} in material {part} (x{factor}) for {damage:.1}, judged {} ticks back (moved {moved:.2} m since)",
                    weapon.name, live.shooter_player, hit.entity, live.rewind
                );
            }
            if damage > 0.0 {
                soldier_hits.write(SoldierHit {
                    victim: hit.entity,
                    damage,
                    headshot: part == HEAD,
                    attacker: attacker.clone(),
                });
            }
        } else if let Ok((vehicle, position, _, rotation, state, _)) = vehicles.get(body) {
            // The face it hits: front, side or rear armour, tracks, glass.
            let inverse = rotation.0.inverse();
            let joints = state.map_or(&[][..], |s| s.joints.as_slice());
            let face = vehicle
                .0
                .armor_material_at(joints, inverse * (hit.point - position.0), inverse * incoming);
            let material = face.unwrap_or(vehicle.0.desc.armor_material);
            let armor = damage_mod(materials.as_deref(), desc.material, material);
            let damage = damage_at(desc, live.travelled) * armor;
            debug!(
                "{} hit {} on material {material}{} (x{armor}) for {damage:.1}",
                weapon.name,
                vehicle.0.desc.name,
                if face.is_none() { " (no armour face)" } else { "" }
            );
            if damage > 0.0 {
                vehicle_hits.write(VehicleHit {
                    vehicle: body,
                    damage,
                    attacker: attacker.clone(),
                });
            }
        } else if destructibles.contains(hit.entity) {
            static_hits.write(StaticHit {
                part: hit.entity,
                damage: damage_at(desc, live.travelled),
                material: desc.material,
                attacker: attacker.clone(),
            });
        }
        if goes_off {
            detonations.write(Detonation {
                entity: Some(entity),
                weapon: weapon.clone(),
                position: hit.point,
                normal: hit.normal,
                facing: motion.velocity.normalize_or(Vec3::NEG_Z),
                attacker,
            });
        } else {
            commands.entity(entity).despawn();
        }
    }
}

/// Claymores go off when an enemy walks into their cone, vehicle mines under anything heavy
/// that moves.
#[allow(clippy::too_many_arguments)]
fn trigger_mines(
    spatial: SpatialQuery,
    projectiles: Query<(Entity, &Live, &ProjectileMotion)>,
    soldiers: Query<(Entity, &SoldierMotion, &ControlledBy), With<Soldier>>,
    teams: Query<&Team>,
    players: Query<&Player>,
    colliders: Query<&ColliderOf>,
    bodies: Query<(&ComputedMass, &LinearVelocity)>,
    mut detonations: MessageWriter<Detonation>,
) {
    for (entity, live, motion) in &projectiles {
        let desc = &live.weapon.projectile;
        let Some(trigger) = &desc.trigger else {
            continue;
        };
        if live.age < desc.arming_delay {
            continue;
        }
        let culprit = match trigger.by {
            TriggerBy::Soldiers => {
                let owner_team = teams.get(live.shooter_player).ok().copied();
                soldiers
                    .iter()
                    .find(|(soldier, target, controlled_by)| {
                        let team = teams.get(controlled_by.0).ok().copied();
                        *soldier != live.shooter
                            && (team.is_none() || team != owner_team)
                            && in_trigger(trigger, motion, target.position + Vec3::Y * 0.9, target.velocity.length())
                    })
                    .map(|(_, _, controlled_by)| {
                        players.get(controlled_by.0).map_or("a soldier".to_string(), |p| p.name.clone())
                    })
            }
            TriggerBy::Vehicles => {
                let filter = SpatialQueryFilter::from_mask(GameLayer::Vehicle);
                spatial
                    .shape_intersections(&Collider::sphere(trigger.radius), motion.position, Quat::IDENTITY, &filter)
                    .into_iter()
                    .find_map(|collider| {
                        let body = colliders.get(collider).map_or(collider, |c| c.body);
                        let (mass, velocity) = bodies.get(body).ok()?;
                        (mass.value() >= HEAVY && velocity.length() >= trigger.min_speed).then(|| format!("vehicle {body}"))
                    })
            }
        };
        if let Some(culprit) = culprit {
            info!("{} at {:.1} set off by {culprit}", live.weapon.name, motion.position);
            detonations.write(Detonation {
                entity: Some(entity),
                weapon: live.weapon.clone(),
                position: motion.position,
                normal: motion.rotation * Vec3::Y,
                facing: motion.facing(),
                attacker: live.attacker(),
            });
        }
    }
}

/// Grenades, rockets and charges going off: the blast, smoke, and the effect for clients.
fn detonate(
    mut commands: Commands,
    mut detonations: MessageReader<Detonation>,
    mut explosions: MessageWriter<Explosion>,
    mut effects: MessageWriter<ToClients<PlayEffect>>,
    mut done: Local<Vec<Entity>>,
) {
    done.clear();
    for detonation in detonations.read() {
        if let Some(entity) = detonation.entity {
            // Set off twice in one tick (fuse and detonator, say): once is enough.
            if done.contains(&entity) {
                continue;
            }
            done.push(entity);
            match commands.get_entity(entity) {
                Ok(mut projectile) => projectile.despawn(),
                Err(_) => continue,
            }
        }
        let desc = &detonation.weapon.projectile;
        info!("{} went off at {:.1}", detonation.weapon.name, detonation.position);
        if desc.explodes() {
            explosions.write(Explosion {
                position: detonation.position + detonation.normal * 0.1,
                damage: desc.explosion_damage,
                radius: desc.explosion_radius,
                material: desc.explosion_material,
                attacker: detonation.attacker.clone(),
                cone: (desc.explosion_cone > 0.0).then_some((detonation.facing, desc.explosion_cone)),
            });
        }
        if let Some(smoke) = &desc.smoke {
            commands.spawn((
                SmokeCloud {
                    position: detonation.position + Vec3::Y * SMOKE_HEIGHT,
                    radius: smoke.radius,
                    duration: smoke.duration + SMOKE_LINGER,
                    age: 0.0,
                    gas_damage: smoke.gas_damage,
                },
                Replicated,
            ));
        }
        if let Some(effect) = &desc.detonation_effect {
            effects.write(ToClients {
                targets: SendTargets::All,
                message: PlayEffect {
                    up: detonation.normal,
                    // Smoke keeps billowing out for the cloud's time.
                    duration: desc.smoke.as_ref().map_or(0.0, |smoke| smoke.duration),
                    material: desc.explosion_material,
                    ..PlayEffect::new(effect.clone(), detonation.position)
                },
            });
        }
    }
}

fn age_smoke(mut commands: Commands, time: Res<Time>, mut clouds: Query<(Entity, &mut SmokeCloud)>) {
    for (entity, mut cloud) in &mut clouds {
        cloud.age += time.delta_secs();
        if cloud.age >= cloud.duration {
            commands.entity(entity).despawn();
        }
    }
}

/// Explosions hurt every soldier and vehicle nearby, less with distance, by the blast's
/// material against the soldier's or the vehicle's blast sensitivity. Hulls shield those
/// inside closed seats.
fn explode(
    mut explosions: MessageReader<Explosion>,
    materials: Option<Res<Materials>>,
    soldiers: Query<(Entity, &SoldierMotion, Option<&Seated>), With<Soldier>>,
    vehicles: Query<(Entity, &Position, &VehicleData, &VehicleHealth)>,
    mut hits: MessageWriter<SoldierHit>,
    mut vehicle_hits: MessageWriter<VehicleHit>,
) {
    for explosion in explosions.read() {
        let factor = materials
            .as_ref()
            .map_or(1.0, |m| m.0.damage_mod(explosion.material, SOLDIER_ARMOR_MATERIAL));
        for (soldier, motion, seated) in &soldiers {
            let exposed = seated.is_none_or(|s| {
                vehicles
                    .get(s.vehicle)
                    .is_ok_and(|(_, _, data, _)| data.0.desc.seats.get(s.seat as usize).is_some_and(|seat| seat.open))
            });
            let center = motion.position + Vec3::Y * 0.9;
            let distance = center.distance(explosion.position);
            if factor > 0.0 && exposed && distance < explosion.radius && explosion.reaches(center) {
                hits.write(SoldierHit {
                    victim: soldier,
                    damage: explosion.damage * (1.0 - distance / explosion.radius) * factor,
                    headshot: false,
                    attacker: explosion.attacker.clone(),
                });
            }
        }
        // Blasts without a material of their own count as BF2's blast wave.
        let material = if explosion.material == 0 { BLAST_MATERIAL } else { explosion.material };
        for (vehicle, position, data, health) in &vehicles {
            if health.wrecked() {
                continue;
            }
            let desc = &data.0.desc;
            // Measured to the hull's surface, roughly.
            let reach = Vec3::from_array(desc.physics.bounds[1]).length().min(4.0);
            let distance = (position.0.distance(explosion.position) - reach).max(0.0);
            let sensitivity = damage_mod(materials.as_deref(), material, desc.blast_material);
            if distance < explosion.radius && sensitivity > 0.0 && explosion.reaches(position.0) {
                vehicle_hits.write(VehicleHit {
                    vehicle,
                    damage: explosion.damage * (1.0 - distance / explosion.radius) * sensitivity,
                    attacker: explosion.attacker.clone(),
                });
            }
        }
    }
}

/// The damage table's factor for `attacker` against `target`: BF2's whole table when it is
/// imported (pairs it leaves out deal full damage, which is what lets AT mines and C4 wreck
/// vehicles), else the vehicle armor stopgap.
fn damage_mod(materials: Option<&Materials>, attacker: u32, target: u32) -> f32 {
    match materials {
        Some(materials) if !materials.0.damage.is_empty() => materials.0.damage_mod(attacker, target),
        _ => armor_damage_modifier(attacker, target),
    }
}

/// Vehicles that can be hurt, with their crews (who decide whose side a vehicle is on).
#[derive(SystemParam)]
struct VehicleTargets<'w, 's> {
    vehicles: Query<'w, 's, (&'static VehicleData, &'static mut VehicleHealth)>,
    seated: Query<'w, 's, (&'static Seated, &'static ControlledBy)>,
}

/// Damage to vehicles, unless their crew is on the attacker's side (without friendly fire).
/// A vehicle out of hit points is wrecked by `vehicles`, which kills its crew.
fn damage_vehicles(
    settings: Res<ServerSettings>,
    host: Option<Res<HostPlayer>>,
    clients: Query<&PlayerClient>,
    teams: Query<&Team>,
    mut hits: MessageReader<VehicleHit>,
    mut targets: VehicleTargets,
    mut confirmations: MessageWriter<ToClients<HitConfirmed>>,
) {
    for hit in hits.read() {
        let attacker_team = hit.attacker.player.and_then(|p| teams.get(p).ok()).copied();
        let friendly = targets
            .seated
            .iter()
            .filter(|(seated, _)| seated.vehicle == hit.vehicle)
            .any(|(_, crew)| attacker_team.is_some() && teams.get(crew.0).ok().copied() == attacker_team);
        if friendly && !settings.friendly_fire {
            continue;
        }
        let Ok((data, mut health)) = targets.vehicles.get_mut(hit.vehicle) else {
            continue;
        };
        if health.wrecked() {
            continue;
        }
        health.current = (health.current - hit.damage).max(0.0);
        info!(
            "{} took {:.1} damage from {}, {:.0} left",
            data.0.desc.name, hit.damage, hit.attacker.weapon, health.current
        );
        if let Some(client) = hit.attacker.player.and_then(|p| player_client(p, &clients, host.as_deref())) {
            confirmations.write(ToClients {
                targets: SendTargets::Single(client),
                message: HitConfirmed {
                    victim: hit.vehicle,
                    damage: hit.damage,
                    headshot: false,
                    killed: health.wrecked(),
                },
            });
        }
    }
}

/// Damage to soldiers. At 0 hit points a soldier is critically wounded, or dead if the blow
/// took him beyond [`WRECK_HIT_POINTS`] below zero or he sat in a vehicle (see
/// `abilities`). Downed soldiers only take blasts and enemy shock paddles.
#[allow(clippy::too_many_arguments, clippy::type_complexity)]
fn damage_soldiers(
    settings: Res<ServerSettings>,
    host: Option<Res<HostPlayer>>,
    clients: Query<&PlayerClient>,
    armory: Res<Armory>,
    mut hits: MessageReader<SoldierHit>,
    mut soldiers: Query<(&mut Health, &ControlledBy, &SoldierMotion, Option<&BleedOut>, Has<Seated>), With<Soldier>>,
    teams: Query<&Team>,
    mut confirmations: MessageWriter<ToClients<HitConfirmed>>,
    mut deaths: Deaths,
) {
    for hit in hits.read() {
        let Ok((mut health, controlled_by, motion, bleeding, seated)) = soldiers.get_mut(hit.victim) else {
            continue;
        };
        let attacker = &hit.attacker;
        let attacker_team = attacker.player.and_then(|p| teams.get(p).ok()).copied().unwrap_or_default();
        let victim_player = controlled_by.0;
        let victim_team = teams.get(victim_player).copied().unwrap_or_default();
        if victim_team == attacker_team
            && !settings.friendly_fire
            && Some(hit.victim) != attacker.soldier
        {
            continue;
        }
        if let Some(bleeding) = bleeding {
            let enemy = victim_team != attacker_team;
            if health.current <= -WRECK_HIT_POINTS || !hurts_downed(&armory, &attacker.weapon, enemy) {
                continue;
            }
            health.current -= hit.damage;
            if health.current <= -WRECK_HIT_POINTS {
                deaths.die(hit.victim, victim_player, bleeding.down_for);
            }
            continue;
        }
        if health.current <= 0.0 {
            continue;
        }
        health.current -= hit.damage;
        deaths.record_damage(hit.victim, attacker.player, hit.damage);
        let killed = health.current <= 0.0;
        if let Some(client) = attacker.player.and_then(|p| player_client(p, &clients, host.as_deref())) {
            confirmations.write(ToClients {
                targets: SendTargets::Single(client),
                message: HitConfirmed {
                    victim: hit.victim,
                    damage: hit.damage,
                    headshot: hit.headshot,
                    killed,
                },
            });
        }
        if killed {
            deaths.kill(hit.victim, victim_player, attacker.player, &attacker.weapon, hit.headshot);
            if health.current <= -WRECK_HIT_POINTS || seated {
                deaths.die(hit.victim, victim_player, 0.0);
            } else {
                deaths.wound(hit.victim, victim_player, motion.yaw);
            }
        }
    }
}

/// A projectile fired by something other than a soldier's own weapon handling (vehicle guns).
/// `ignore` is the collider it starts inside and must not hit (the firing vehicle). Shells
/// and missiles are replicated like grenades (vehicle guns are in the armory too).
pub fn spawn_projectile(
    commands: &mut Commands,
    weapon: Arc<WeaponDesc>,
    shooter: Entity,
    shooter_player: Entity,
    ignore: Option<Entity>,
    origin: Vec3,
    direction: Vec3,
    inherited: Vec3,
    target: Option<Entity>,
) {
    let fuse = weapon.projectile.time_to_live;
    let velocity = direction * weapon.projectile.velocity + inherited;
    let guided = weapon.fire.guidance == Guidance::Wire;
    let object = weapon.projectile.is_object();
    let name = weapon.name.clone();
    let yaw = (-direction.x).atan2(-direction.z);
    let mut projectile = commands.spawn((
        Live {
            weapon,
            weapon_index: u8::MAX,
            shooter,
            shooter_player,
            hitbox: ignore,
            travelled: 0.0,
            age: 0.0,
            fuse,
            guided,
            target,
            rewind: 0,
        },
        ProjectileMotion::new(origin, velocity, yaw),
    ));
    if object {
        projectile.insert((
            Projectile {
                player: shooter_player,
                weapon: name,
            },
            Replicated,
        ));
    }
}

/// Soldiers whose health ran out some other way (scripts, wrecked vehicles, and later
/// falls): critically wounded, or dead if far below zero or in a vehicle. Downed soldiers
/// taken beyond [`WRECK_HIT_POINTS`] below zero die.
#[allow(clippy::type_complexity)]
fn kill_the_dead(
    soldiers: Query<(Entity, &Health, &ControlledBy, &SoldierMotion, Option<&BleedOut>, Has<Seated>), With<Soldier>>,
    mut deaths: Deaths,
) {
    for (soldier, health, controlled_by, motion, bleeding, seated) in &soldiers {
        let player = controlled_by.0;
        match bleeding {
            Some(bleeding) if health.current <= -WRECK_HIT_POINTS => deaths.die(soldier, player, bleeding.down_for),
            Some(_) => {}
            None if health.current > 0.0 => {}
            None => {
                deaths.kill(soldier, player, None, "", false);
                if health.current <= -WRECK_HIT_POINTS || seated {
                    deaths.die(soldier, player, 0.0);
                } else {
                    deaths.wound(soldier, player, motion.yaw);
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use game_shared::{hitzones, soldier::Stance};

    use super::*;

    /// A soldier running along +X at 6 m/s, one pose per tick up to tick 100.
    fn runner() -> PoseHistory {
        PoseHistory(
            (100 - POSE_HISTORY as u32 + 1..=100)
                .map(|tick| {
                    let pose = BodyPose {
                        position: Vec3::new(tick as f32 * 0.1, 0.0, -20.0),
                        yaw: 0.0,
                        stance: Stance::Standing,
                    };
                    (tick, pose)
                })
                .collect(),
        )
    }

    #[test]
    fn rewinding_is_capped_and_off_for_the_present() {
        assert_eq!(rewind_for(0, 500), 0);
        assert_eq!(rewind_for(494, 500), 6);
        assert_eq!(rewind_for(400, 500), MAX_REWIND);
        assert_eq!(rewind_for(510, 500), 0, "a view from the future is the present");
    }

    #[test]
    fn shots_are_judged_where_the_target_was_when_the_shooter_saw_it() {
        let history = runner();
        let target = Target {
            entity: Entity::PLACEHOLDER,
            pose: history.at(100).unwrap(),
            history: Some(&history),
            zones: hitzones::fallback(),
        };
        // The shooter saw tick 90 (10 ticks back): the target a meter behind where it is now.
        let seen = target.pose(100, 10);
        assert!((seen.position.x - 9.0).abs() < 1e-4);
        let aim = seen.position + Vec3::Y * 1.2;
        let direction = aim.normalize();
        let rewound = seen.ray(target.zones, Vec3::ZERO, direction, 100.0);
        let present = target.pose.ray(target.zones, Vec3::ZERO, direction, 100.0);
        assert!(rewound.is_some_and(|hit| hit.material == hitzones::BODY));
        assert!(present.is_none(), "a meter ahead by now");
        // Further back than the history keeps: the oldest pose.
        assert_eq!(target.pose(100, 60).position, history.0.front().unwrap().1.position);
    }
}
