//! Weapons on the server: firing and throwing, projectiles, explosions, damage and death.
//!
//! Projectiles are simulated here only. Clients are told about each shot (for tracers and
//! sounds), see grenades, rockets and charges through their replicated
//! [`ProjectileMotion`], and hear about hits, kills and detonations, but never decide them.
//! Hits on destroyable objects and explosions are passed on as messages (see
//! `destruction`).

use std::sync::Arc;

use avian3d::prelude::*;
use bevy::prelude::*;
use bevy_replicon::prelude::*;
use game_data::{FireKind, FireMode, Guidance, TriggerBy, WeaponDesc};
use game_shared::{
    conquest::RoundState,
    effects::PlayEffect,
    input::Buttons,
    physics::GameLayer,
    projectile::{
        self, Projectile, ProjectileMotion, SmokeCloud, collision_layers, launch_origin, launch_velocity, steer,
    },
    protocol::{ControlledBy, HitConfirmed, KillFeed, Player, Score, ShotFired, Team},
    soldier::{Health, Hitbox, Soldier, SoldierMotion, Stance, stance_height},
    statics::Destructible,
    weapons::{Armory, Fired, Inventory, Loadout, Trigger, WeaponState, damage_at, spread_direction},
};

use crate::{
    AppliedInput, Controls, HostPlayer, PlayerClient, RespawnTimer, ServerSettings,
    ServerSimSystems, destruction::Materials,
};

pub struct CombatPlugin;

impl Plugin for CombatPlugin {
    fn build(&self, app: &mut App) {
        app.add_message::<Died>()
            .add_message::<Explosion>()
            .add_message::<StaticHit>()
            .add_message::<SoldierHit>()
            .add_message::<Detonation>()
            .add_systems(
                FixedUpdate,
                (
                    clear_projectiles,
                    fire_weapons,
                    simulate_projectiles,
                    trigger_mines,
                    detonate,
                    age_smoke,
                    explode,
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
struct SoldierHit {
    victim: Entity,
    damage: f32,
    headshot: bool,
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

/// Damage multiplier for hits above the neck.
const HEADSHOT_MULTIPLIER: f32 = 2.5;
/// Soldiers' `armor.defaultMaterial` (Human_body): the damage table column for explosions.
const SOLDIER_ARMOR_MATERIAL: u32 = 24;
/// How far wire-guided missiles look for what the shooter aims at.
const GUIDANCE_RANGE: f32 = 2000.0;
/// Vehicle mines go off under bodies at least this heavy (kg).
const HEAVY: f32 = 1000.0;
/// Smoke clouds billow up around this far above the grenade.
const SMOKE_HEIGHT: f32 = 1.5;

/// A projectile in flight or lying around (server only). All have a [`ProjectileMotion`];
/// grenades, rockets and charges also the replicated [`Projectile`].
#[derive(Component)]
struct Live {
    weapon: Arc<WeaponDesc>,
    /// Its weapon in the shooter's loadout (a guided missile is steered while it's in hand).
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
    )>,
    placed: Query<(Entity, &Live, &ProjectileMotion)>,
    mut shots: MessageWriter<ToClients<ShotFired>>,
    mut detonations: MessageWriter<Detonation>,
) {
    let dt = time.delta_secs();
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

        // Hands are on the rungs while climbing.
        let trigger = Trigger {
            fire: input.pressed(Buttons::FIRE) && !motion.climbing,
            alt: input.pressed(Buttons::AIM) && !motion.climbing,
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

#[allow(clippy::too_many_arguments)]
fn simulate_projectiles(
    mut commands: Commands,
    time: Res<Time>,
    spatial: SpatialQuery,
    mut projectiles: Query<(Entity, &mut Live, &mut ProjectileMotion)>,
    shooters: Query<(&SoldierMotion, &Inventory)>,
    colliders: Query<&ColliderOf>,
    soldiers: Query<&SoldierMotion, With<Soldier>>,
    destructibles: Query<(), With<Destructible>>,
    mut soldier_hits: MessageWriter<SoldierHit>,
    mut static_hits: MessageWriter<StaticHit>,
    mut detonations: MessageWriter<Detonation>,
) {
    let dt = time.delta_secs();
    for (entity, mut live, mut motion) in &mut projectiles {
        live.age += dt;
        let weapon = live.weapon.clone();
        let desc = &weapon.projectile;
        let goes_off = desc.explodes() || desc.smoke.is_some();
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
            // hands and not being reloaded.
            let holding = shooters
                .get(live.shooter)
                .ok()
                .filter(|(_, inventory)| inventory.active == live.weapon_index && !inventory.reloading);
            match holding {
                Some((shooter, _)) => {
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
                    if to.length() > desc.guidance_min_distance && live.age >= desc.motor_delay {
                        next.velocity = steer(next.velocity, to, desc.turn_rate * dt);
                    }
                }
                None => {
                    live.guided = false;
                    info!("{} lost its guidance at {:.1}", weapon.name, next.position);
                }
            }
        }

        let mut filter = SpatialQueryFilter::from_mask(collision_layers(desc));
        if let Some(hitbox) = live.hitbox {
            filter = filter.with_excluded_entities([hitbox]);
        }
        let step = projectile::step(&spatial, &filter, desc, &mut next, live.age, dt);
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
        if let Ok(soldier) = soldiers.get(body) {
            let head = hit.point.y - soldier.position.y > stance_height(soldier.stance) - 0.3
                && soldier.stance != Stance::Prone;
            let damage = damage_at(desc, live.travelled);
            if damage > 0.0 {
                soldier_hits.write(SoldierHit {
                    victim: body,
                    damage: damage * if head { HEADSHOT_MULTIPLIER } else { 1.0 },
                    headshot: head,
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
                        let offset = target.position + Vec3::Y * 0.9 - motion.position;
                        let ahead = Vec3::new(offset.x, 0.0, offset.z).angle_between(motion.facing()).to_degrees();
                        *soldier != live.shooter
                            && (team.is_none() || team != owner_team)
                            && offset.length() <= trigger.radius
                            && target.velocity.length() >= trigger.min_speed
                            && (trigger.angle <= 0.0 || ahead <= trigger.angle)
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
                    duration: smoke.duration,
                    age: 0.0,
                },
                Replicated,
            ));
        }
        if let Some(effect) = &desc.detonation_effect {
            effects.write(ToClients {
                targets: SendTargets::All,
                message: PlayEffect {
                    up: detonation.normal,
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

/// Explosions hurt every soldier nearby, less with distance, by the blast's material
/// against the soldier's.
fn explode(
    mut explosions: MessageReader<Explosion>,
    materials: Option<Res<Materials>>,
    soldiers: Query<(Entity, &SoldierMotion), With<Soldier>>,
    mut hits: MessageWriter<SoldierHit>,
) {
    for explosion in explosions.read() {
        let factor = materials
            .as_ref()
            .map_or(1.0, |m| m.0.damage_mod(explosion.material, SOLDIER_ARMOR_MATERIAL));
        if factor <= 0.0 {
            continue;
        }
        for (soldier, motion) in &soldiers {
            let center = motion.position + Vec3::Y * 0.9;
            let distance = center.distance(explosion.position);
            if distance < explosion.radius && explosion.reaches(center) {
                hits.write(SoldierHit {
                    victim: soldier,
                    damage: explosion.damage * (1.0 - distance / explosion.radius) * factor,
                    headshot: false,
                    attacker: explosion.attacker.clone(),
                });
            }
        }
    }
}

#[allow(clippy::too_many_arguments)]
fn damage_soldiers(
    mut commands: Commands,
    settings: Res<ServerSettings>,
    host: Option<Res<HostPlayer>>,
    clients: Query<&PlayerClient>,
    mut hits: MessageReader<SoldierHit>,
    mut soldiers: Query<(&mut Health, &ControlledBy), With<Soldier>>,
    teams: Query<&Team>,
    mut players: Query<(&mut Score, &Player)>,
    mut confirmations: MessageWriter<ToClients<HitConfirmed>>,
    mut kills: MessageWriter<ToClients<KillFeed>>,
    mut died: MessageWriter<Died>,
) {
    for hit in hits.read() {
        let Ok((mut health, controlled_by)) = soldiers.get_mut(hit.victim) else {
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
        if health.current <= 0.0 {
            continue;
        }
        health.current -= hit.damage;
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
            kill(
                &mut commands,
                hit.victim,
                victim_player,
                attacker.player,
                &attacker.weapon,
                hit.headshot,
                settings.respawn_seconds,
                &mut players,
                &mut kills,
            );
            died.write(Died {
                player: victim_player,
                team: victim_team,
            });
        }
    }
}

/// Soldiers whose health ran out some other way (scripts, and later falls and crashes).
fn kill_the_dead(
    mut commands: Commands,
    settings: Res<ServerSettings>,
    soldiers: Query<(Entity, &Health, &ControlledBy), With<Soldier>>,
    teams: Query<&Team>,
    mut players: Query<(&mut Score, &Player)>,
    mut kills: MessageWriter<ToClients<KillFeed>>,
    mut died: MessageWriter<Died>,
) {
    for (soldier, health, controlled_by) in &soldiers {
        if health.current > 0.0 {
            continue;
        }
        let player = controlled_by.0;
        kill(
            &mut commands,
            soldier,
            player,
            None,
            "",
            false,
            settings.respawn_seconds,
            &mut players,
            &mut kills,
        );
        died.write(Died {
            player,
            team: teams.get(player).copied().unwrap_or_default(),
        });
    }
}

#[allow(clippy::too_many_arguments)]
fn kill(
    commands: &mut Commands,
    soldier: Entity,
    victim: Entity,
    killer: Option<Entity>,
    weapon: &str,
    headshot: bool,
    respawn_seconds: f32,
    players: &mut Query<(&mut Score, &Player)>,
    kills: &mut MessageWriter<ToClients<KillFeed>>,
) {
    let name = |player: Entity| {
        players
            .get(player)
            .map(|(_, p)| p.name.clone())
            .unwrap_or_else(|_| "?".into())
    };
    match killer.filter(|k| *k != victim) {
        Some(killer) => info!(
            "{} killed {} ({weapon}{})",
            name(killer),
            name(victim),
            if headshot { ", headshot" } else { "" }
        ),
        None => info!("{} died", name(victim)),
    }
    commands.entity(soldier).despawn();
    commands
        .entity(victim)
        .remove::<Controls>()
        .insert(RespawnTimer(Timer::from_seconds(respawn_seconds, TimerMode::Once)));
    if let Ok((mut score, _)) = players.get_mut(victim) {
        score.deaths += 1;
    }
    if let Some(killer) = killer.filter(|k| *k != victim)
        && let Ok((mut score, _)) = players.get_mut(killer)
    {
        score.kills += 1;
        score.score += 2;
    }
    kills.write(ToClients {
        targets: SendTargets::All,
        message: KillFeed {
            killer,
            victim,
            weapon: weapon.to_string(),
            headshot,
        },
    });
}
