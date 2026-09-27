//! Weapons on the server: firing, projectiles, damage and death.
//!
//! Projectiles are simulated here only; clients are told about each shot (for tracers and
//! sounds) and about hits and kills, but never decide them. Hits on destroyable objects and
//! explosions are passed on as messages (see `destruction`).

use std::sync::Arc;

use avian3d::prelude::*;
use bevy::prelude::*;
use bevy_replicon::prelude::*;
use game_data::{FireMode, WeaponDesc};
use game_shared::{
    input::Buttons,
    physics::GameLayer,
    protocol::{ControlledBy, HitConfirmed, KillFeed, Player, Score, ShotFired, Team},
    soldier::{Health, Hitbox, Soldier, SoldierMotion, Stance, stance_height},
    statics::Destructible,
    weapons::{Armory, Inventory, Loadout, WeaponState, damage_at, spread_direction},
};

use crate::{
    AppliedInput, Controls, HostPlayer, PlayerClient, RespawnTimer, ServerSettings,
    ServerSimSystems,
};

pub struct CombatPlugin;

impl Plugin for CombatPlugin {
    fn build(&self, app: &mut App) {
        app.add_message::<Died>()
            .add_message::<Explosion>()
            .add_message::<StaticHit>()
            .add_message::<SoldierHit>()
            .add_systems(
                FixedUpdate,
                (fire_weapons, simulate_projectiles, explode, damage_soldiers, kill_the_dead)
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
    /// The damage table row.
    pub material: u32,
    pub attacker: Attacker,
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

/// Damage multiplier for hits above the neck.
const HEADSHOT_MULTIPLIER: f32 = 2.5;
const GRAVITY: f32 = 9.81;

/// A bullet or rocket in flight (server only).
#[derive(Component)]
struct Projectile {
    weapon: Arc<WeaponDesc>,
    shooter: Entity,
    shooter_player: Entity,
    hitbox: Option<Entity>,
    velocity: Vec3,
    travelled: f32,
    age: f32,
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

#[allow(clippy::type_complexity)]
fn fire_weapons(
    mut commands: Commands,
    time: Res<Time>,
    armory: Res<Armory>,
    host: Option<Res<HostPlayer>>,
    clients: Query<&PlayerClient>,
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
    mut shots: MessageWriter<ToClients<ShotFired>>,
) {
    let dt = time.delta_secs();
    for (soldier, controlled_by, motion, applied, loadout, mut inventory, mut state, hitbox) in
        &mut soldiers
    {
        let input = applied.0;

        // Weapon switching.
        if input.weapon != inventory.active && (input.weapon as usize) < loadout.weapons.len() {
            inventory.active = input.weapon;
            inventory.fire_mode = 0;
            inventory.reloading = false;
            state.reload = 0.0;
            state.burst_left = 0;
            state.deploy = armory
                .weapon(&loadout.weapons[input.weapon as usize])
                .map_or(0.5, |w| w.deploy_time);
        }
        let active = inventory.active as usize;
        let Some(weapon) = loadout.weapons.get(active).and_then(|w| armory.weapon(w)).cloned()
        else {
            continue;
        };

        let local = Quat::from_rotation_y(-motion.yaw) * motion.velocity;
        state.tick(&weapon.deviation, dt, -local.z, local.x, !motion.grounded);

        // Fire mode cycling on the button's press.
        // Hands are on the rungs while climbing.
        let trigger = input.pressed(Buttons::FIRE) && !motion.climbing;
        if input.pressed(Buttons::FIRE_MODE) && !state.trigger_was_down && !trigger {
            inventory.fire_mode = ((inventory.fire_mode as usize + 1) % weapon.fire_modes.len().max(1)) as u8;
        }

        // Reloading.
        let [in_mag, spare] = inventory.ammo.get(active).copied().unwrap_or([0, 0]);
        if state.reload > 0.0 {
            state.reload -= dt;
            if state.reload <= 0.0 {
                let wanted = weapon.magazine_size as u16 - in_mag;
                let taken = wanted.min(spare);
                inventory.ammo[active] = [in_mag + taken, spare - taken];
                inventory.reloading = false;
            }
            state.trigger_was_down = trigger;
            continue;
        }
        let wants_reload = input.pressed(Buttons::RELOAD) && in_mag < weapon.magazine_size as u16;
        let empty_and_trying = in_mag == 0 && trigger;
        if (wants_reload || empty_and_trying) && spare > 0 {
            state.reload = weapon.reload_time;
            inventory.reloading = true;
            state.trigger_was_down = trigger;
            continue;
        }

        // Firing. Sprinting lowers the weapon.
        let sprinting = input.pressed(Buttons::SPRINT) && input.movement[1] > 64;
        let mode = weapon
            .fire_modes
            .get(inventory.fire_mode as usize)
            .copied()
            .unwrap_or(FireMode::Single);
        let pressed_now = trigger && !state.trigger_was_down;
        let wants_shot = match mode {
            FireMode::Auto => trigger,
            FireMode::Single => pressed_now,
            FireMode::Burst => pressed_now || state.burst_left > 0,
        };
        state.trigger_was_down = trigger;
        // Weapons without a magazine (the knife) never run out.
        let unlimited = weapon.magazine_size == 0;
        if !wants_shot
            || sprinting
            || state.cooldown > 0.0
            || state.deploy > 0.0
            || (in_mag == 0 && !unlimited)
            || weapon.projectile.velocity <= 0.0
        {
            if in_mag == 0 {
                state.burst_left = 0;
            }
            continue;
        }

        if mode == FireMode::Burst {
            state.burst_left = if pressed_now { 2 } else { state.burst_left.saturating_sub(1) };
        }
        if !unlimited {
            inventory.ammo[active][0] = in_mag - 1;
        }
        let zoomed = input.pressed(Buttons::AIM);
        let cone = state.deviation(&weapon.deviation, motion.stance, zoomed);
        state.on_shot(&weapon);

        let origin = motion.eye_position();
        let aim = motion.view_rotation() * Vec3::NEG_Z;
        for _ in 0..weapon.projectiles_per_shot.max(1) {
            let direction = spread_direction(aim, cone, (fastrand::f32(), fastrand::f32()));
            commands.spawn((
                Projectile {
                    weapon: weapon.clone(),
                    shooter: soldier,
                    shooter_player: controlled_by.0,
                    hitbox: hitbox.map(|h| h.entity),
                    velocity: direction * weapon.projectile.velocity,
                    travelled: 0.0,
                    age: 0.0,
                },
                Transform::from_translation(origin),
            ));
            // The shooter's client already showed its own shot.
            let targets = match player_client(controlled_by.0, &clients, host.as_deref()) {
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
    }
}

fn simulate_projectiles(
    mut commands: Commands,
    time: Res<Time>,
    spatial: SpatialQuery,
    mut projectiles: Query<(Entity, &mut Projectile, &mut Transform)>,
    colliders: Query<&ColliderOf>,
    soldiers: Query<&SoldierMotion, With<Soldier>>,
    destructibles: Query<(), With<Destructible>>,
    mut soldier_hits: MessageWriter<SoldierHit>,
    mut static_hits: MessageWriter<StaticHit>,
    mut explosions: MessageWriter<Explosion>,
) {
    let dt = time.delta_secs();
    for (entity, mut projectile, mut transform) in &mut projectiles {
        projectile.age += dt;
        if projectile.age > projectile.weapon.projectile.time_to_live {
            commands.entity(entity).despawn();
            continue;
        }
        let gravity = Vec3::NEG_Y * GRAVITY * projectile.weapon.projectile.gravity;
        let start_velocity = projectile.velocity;
        projectile.velocity += gravity * dt;
        let step = (start_velocity + projectile.velocity) * 0.5 * dt;
        let length = step.length();
        let Ok(direction) = Dir3::new(step) else {
            continue;
        };

        let mut filter = SpatialQueryFilter::from_mask([GameLayer::World, GameLayer::Soldier]);
        if let Some(hitbox) = projectile.hitbox {
            filter = filter.with_excluded_entities([hitbox]);
        }
        let Some(hit) = spatial.cast_ray(transform.translation, direction, length, true, &filter)
        else {
            transform.translation += step;
            projectile.travelled += length;
            continue;
        };
        let point = transform.translation + direction * hit.distance;
        let travelled = projectile.travelled + hit.distance;
        commands.entity(entity).despawn();

        let desc = &projectile.weapon.projectile;
        let attacker = Attacker {
            player: Some(projectile.shooter_player),
            soldier: Some(projectile.shooter),
            weapon: Arc::from(projectile.weapon.name.as_str()),
        };
        let body = colliders.get(hit.entity).map(|c| c.body).unwrap_or(hit.entity);
        if let Ok(motion) = soldiers.get(body) {
            let head = point.y - motion.position.y > stance_height(motion.stance) - 0.3
                && motion.stance != Stance::Prone;
            soldier_hits.write(SoldierHit {
                victim: body,
                damage: damage_at(desc, travelled) * if head { HEADSHOT_MULTIPLIER } else { 1.0 },
                headshot: head,
                attacker: attacker.clone(),
            });
        } else if destructibles.contains(hit.entity) {
            static_hits.write(StaticHit {
                part: hit.entity,
                damage: damage_at(desc, travelled),
                material: desc.material,
                attacker: attacker.clone(),
            });
        }
        if desc.explosion_damage > 0.0 && desc.explosion_radius > 0.0 {
            // The explosion's own material isn't imported; the projectile's stands in.
            explosions.write(Explosion {
                position: point,
                damage: desc.explosion_damage,
                radius: desc.explosion_radius,
                material: desc.material,
                attacker,
            });
        }
    }
}

/// Explosions hurt every soldier nearby, less with distance.
fn explode(
    mut explosions: MessageReader<Explosion>,
    soldiers: Query<(Entity, &SoldierMotion), With<Soldier>>,
    mut hits: MessageWriter<SoldierHit>,
) {
    for explosion in explosions.read() {
        for (soldier, motion) in &soldiers {
            let distance = (motion.position + Vec3::Y * 0.9).distance(explosion.position);
            if distance < explosion.radius {
                hits.write(SoldierHit {
                    victim: soldier,
                    damage: explosion.damage * (1.0 - distance / explosion.radius),
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
