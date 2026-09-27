//! Weapons on the server: firing, projectiles, damage and death.
//!
//! Projectiles are simulated here only; clients are told about each shot (for tracers and
//! sounds) and about hits and kills, but never decide them.

use std::sync::Arc;

use avian3d::prelude::*;
use bevy::prelude::*;
use bevy_replicon::prelude::*;
use bevy::ecs::system::SystemParam;
use game_data::{FireMode, ProjectileDesc, WeaponDesc};
use game_shared::{
    input::Buttons,
    physics::GameLayer,
    protocol::{ControlledBy, HitConfirmed, KillFeed, Player, Score, ShotFired, Team},
    soldier::{Health, Hitbox, Soldier, SoldierMotion, Stance, stance_height},
    vehicle::{BLAST_MATERIAL, Seated, VehicleData, VehicleHealth, armor_damage_modifier},
    weapons::{Armory, Inventory, Loadout, WeaponState, damage_at, spread_direction},
};

use crate::{
    AppliedInput, Controls, HostPlayer, PlayerClient, RespawnTimer, ServerSettings,
    ServerSimSystems,
};

pub struct CombatPlugin;

impl Plugin for CombatPlugin {
    fn build(&self, app: &mut App) {
        app.add_message::<Died>().add_systems(
            FixedUpdate,
            (fire_weapons, simulate_projectiles, kill_the_dead)
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

/// Damage multiplier for hits above the neck.
const HEADSHOT_MULTIPLIER: f32 = 2.5;
const GRAVITY: f32 = 9.81;

/// A bullet or rocket in flight (server only).
#[derive(Component)]
struct Projectile {
    weapon: Arc<WeaponDesc>,
    shooter: Entity,
    shooter_player: Entity,
    /// Collider the projectile starts inside and must not hit (the shooter's hitbox or
    /// vehicle).
    ignore: Option<Entity>,
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
        let trigger = input.pressed(Buttons::FIRE);
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
        if !wants_shot
            || sprinting
            || state.cooldown > 0.0
            || state.deploy > 0.0
            || in_mag == 0
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
        inventory.ammo[active][0] = in_mag - 1;
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
                    ignore: hitbox.map(|h| h.entity),
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

#[allow(clippy::too_many_arguments, clippy::type_complexity)]
fn simulate_projectiles(
    mut commands: Commands,
    time: Res<Time>,
    settings: Res<ServerSettings>,
    spatial: SpatialQuery,
    host: Option<Res<HostPlayer>>,
    clients: Query<&PlayerClient>,
    mut projectiles: Query<(Entity, &mut Projectile, &mut Transform)>,
    colliders: Query<&ColliderOf>,
    mut soldiers: Query<(Entity, &mut Health, &SoldierMotion, &ControlledBy, Has<Seated>), With<Soldier>>,
    teams: Query<&Team>,
    mut players: Query<(&mut Score, &Player)>,
    mut hits: MessageWriter<ToClients<HitConfirmed>>,
    mut kills: MessageWriter<ToClients<KillFeed>>,
    mut died: MessageWriter<Died>,
    mut vehicles: VehicleTargets,
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

        let mut filter =
            SpatialQueryFilter::from_mask([GameLayer::World, GameLayer::Soldier, GameLayer::Vehicle]);
        if let Some(ignore) = projectile.ignore {
            filter = filter.with_excluded_entities([ignore]);
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
        let shooter_team = teams.get(projectile.shooter_player).copied().unwrap_or_default();
        let mut victims: Vec<(Entity, f32, bool)> = Vec::new();

        // Direct hit on a soldier.
        let body = colliders.get(hit.entity).map(|c| c.body).unwrap_or(hit.entity);
        if let Ok((_, _, motion, ..)) = soldiers.get(body) {
            let head = point.y - motion.position.y > stance_height(motion.stance) - 0.3
                && motion.stance != Stance::Prone;
            let damage = damage_at(desc, travelled) * if head { HEADSHOT_MULTIPLIER } else { 1.0 };
            victims.push((body, damage, head));
        }
        // Explosions hurt everyone nearby, less with distance (the hull shields those inside).
        if desc.explosion_damage > 0.0 && desc.explosion_radius > 0.0 {
            for (soldier, _, motion, _, seated) in &soldiers {
                if seated {
                    continue;
                }
                let distance = (motion.position + Vec3::Y * 0.9).distance(point);
                if distance < desc.explosion_radius {
                    let falloff = 1.0 - distance / desc.explosion_radius;
                    victims.push((soldier, desc.explosion_damage * falloff, false));
                }
            }
        }

        for (victim, damage, destroyed) in
            vehicles.damage(body, point, travelled, desc, shooter_team, settings.friendly_fire, &teams)
        {
            if let Some(client) = player_client(projectile.shooter_player, &clients, host.as_deref()) {
                hits.write(ToClients {
                    targets: SendTargets::Single(client),
                    message: HitConfirmed {
                        victim,
                        damage,
                        headshot: false,
                        killed: destroyed,
                    },
                });
            }
        }

        for (victim, damage, head) in victims {
            let Ok((_, mut health, _, controlled_by, _)) = soldiers.get_mut(victim) else {
                continue;
            };
            let victim_player = controlled_by.0;
            let victim_team = teams.get(victim_player).copied().unwrap_or_default();
            if victim_team == shooter_team && !settings.friendly_fire && victim != projectile.shooter {
                continue;
            }
            if health.current <= 0.0 {
                continue;
            }
            health.current -= damage;
            let killed = health.current <= 0.0;
            if let Some(client) = player_client(projectile.shooter_player, &clients, host.as_deref()) {
                hits.write(ToClients {
                    targets: SendTargets::Single(client),
                    message: HitConfirmed {
                        victim,
                        damage,
                        headshot: head,
                        killed,
                    },
                });
            }
            if killed {
                kill(
                    &mut commands,
                    victim,
                    victim_player,
                    Some(projectile.shooter_player),
                    &projectile.weapon.name,
                    head,
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
}

/// Vehicles projectiles can damage.
#[derive(SystemParam)]
struct VehicleTargets<'w, 's> {
    vehicles: Query<'w, 's, (Entity, &'static Position, &'static VehicleData, &'static mut VehicleHealth)>,
    seated: Query<'w, 's, (&'static Seated, &'static ControlledBy)>,
}

impl VehicleTargets<'_, '_> {
    /// Applies a projectile's direct hit on `body` (if it is a vehicle) and its blast to
    /// vehicles, through the armour damage table. Returns (vehicle, damage, destroyed).
    #[allow(clippy::too_many_arguments)]
    fn damage(
        &mut self,
        body: Entity,
        point: Vec3,
        travelled: f32,
        projectile: &ProjectileDesc,
        shooter_team: Team,
        friendly_fire: bool,
        teams: &Query<&Team>,
    ) -> Vec<(Entity, f32, bool)> {
        let mut damaged = Vec::new();
        for (vehicle, position, data, mut health) in &mut self.vehicles {
            if health.wrecked() {
                continue;
            }
            let desc = &data.0.desc;
            let mut damage = 0.0;
            if vehicle == body {
                damage += damage_at(projectile, travelled) * armor_damage_modifier(projectile.material, desc.armor_material);
            }
            if projectile.explosion_damage > 0.0 && projectile.explosion_radius > 0.0 {
                // Measured to the hull's surface, roughly.
                let reach = Vec3::from_array(desc.physics.bounds[1]).length().min(4.0);
                let distance = (position.0.distance(point) - reach).max(0.0);
                if distance < projectile.explosion_radius {
                    let falloff = 1.0 - distance / projectile.explosion_radius;
                    damage += projectile.explosion_damage
                        * falloff
                        * armor_damage_modifier(BLAST_MATERIAL, desc.blast_material);
                }
            }
            if damage <= 0.0 {
                continue;
            }
            let friendly = self
                .seated
                .iter()
                .filter(|(s, _)| s.vehicle == vehicle)
                .any(|(_, c)| teams.get(c.0).is_ok_and(|t| *t == shooter_team));
            if friendly && !friendly_fire {
                continue;
            }
            health.current = (health.current - damage).max(0.0);
            damaged.push((vehicle, damage, health.wrecked()));
        }
        damaged
    }
}

/// Spawns a projectile fired by `shooter` (a soldier, possibly manning a vehicle gun).
pub fn spawn_projectile(
    commands: &mut Commands,
    weapon: Arc<WeaponDesc>,
    shooter: Entity,
    shooter_player: Entity,
    ignore: Option<Entity>,
    origin: Vec3,
    direction: Vec3,
) {
    let velocity = direction * weapon.projectile.velocity;
    commands.spawn((
        Projectile {
            weapon,
            shooter,
            shooter_player,
            ignore,
            velocity,
            travelled: 0.0,
            age: 0.0,
        },
        Transform::from_translation(origin),
    ));
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
