//! Weapon and soldier sounds.
//!
//! Our own shots play the weapon's first-person fire sound, everyone else's the third-person
//! one where they were fired, crossfading into the importer's muffled far version between
//! [`FAR_FIRE`] meters (BF2 has no far versions for handheld weapons). Every shot's path is
//! traced once when it is fired, like its tracer: where it hits schedules the impact sound for
//! that surface material (BF2's material manager cells, `sounds.ron`), and other soldiers'
//! bullets passing within [`FLYBY_RADIUS`] of the listener schedule a crack there. Reloads,
//! weapon switches, zoom, fire mode and dry fire come from our input and prediction, other
//! soldiers' reloads and switches from their replicated inventory. Soldiers cry out when
//! killed (and their body hits the ground); we hear ourselves getting hurt.

use std::collections::HashMap;

use avian3d::prelude::*;
use bevy::{ecs::system::SystemParam, prelude::*};
use game_data::{SoundDesc, WeaponDesc, WeaponSounds};
use game_shared::{
    input::Buttons,
    level::LoadedLevel,
    physics::GameLayer,
    protocol::{ControlledBy, KillFeed, ShotFired},
    soldier::{Health, Hitbox, Soldier},
    weapons::{Armory, Inventory, Loadout},
};

use super::{
    AudioSystems, Sounds,
    voices::{PlaySound, Sound, SoundCache},
};
use crate::{
    combat::{CombatFeedback, LocalShot, WeaponSelection},
    effects::SurfaceQuery,
    local_input::InputHistory,
    net::LocalSoldier,
    prediction::SoldierRender,
};

pub struct WeaponAudioPlugin;

impl Plugin for WeaponAudioPlugin {
    fn build(&self, app: &mut App) {
        app.init_resource::<Pending>().add_systems(
            PostUpdate,
            (
                preload,
                (local_shots, remote_shots, local_weapon, remote_weapons, soldier_voices),
                play_pending,
            )
                .chain()
                .in_set(AudioSystems::Trigger),
        );
    }
}

/// The close and the far fire sound crossfade between these distances [our choice].
const FAR_FIRE: [f32; 2] = [40.0, 120.0];
/// Bullets passing closer than this to the listener crack past (BF2's `Bullet_Flyby`
/// material; its size is our guess).
const FLYBY_RADIUS: f32 = 4.0;
/// ... unless it is that close to where they were fired: that's the shooter's own noise.
const FLYBY_MIN_TRAVEL: f32 = 3.0;
/// Seconds per segment of a traced bullet path.
const SEGMENT: f32 = 0.05;
/// Traced paths end after this long, however long the bullet lives.
const MAX_FLIGHT: f32 = 3.0;
const GRAVITY: f32 = 9.81;
/// Glancing hits (less than about 20° off the surface) may ricochet.
const RICOCHET_COSINE: f32 = 0.35;
/// Material ids (`materials.ron`).
const WATER: u32 = 1;
const BULLET: u32 = 38;

/// Sounds due later: impacts and flybys when the bullet gets there, bodies hitting the
/// ground.
#[derive(Resource, Default)]
struct Pending(Vec<(f32, PlaySound)>);

fn smoothstep(edge0: f32, edge1: f32, x: f32) -> f32 {
    let t = ((x - edge0) / (edge1 - edge0)).clamp(0.0, 1.0);
    t * t * (3.0 - 2.0 * t)
}

fn weapon_sounds(sounds: &WeaponSounds) -> impl Iterator<Item = &SoundDesc> {
    [
        &sounds.fire_1p,
        &sounds.fire_3p,
        &sounds.fire_3p_distant,
        &sounds.reload_1p,
        &sounds.reload_3p,
        &sounds.deploy_1p,
        &sounds.deploy_3p,
        &sounds.dry_fire,
        &sounds.bolt,
        &sounds.switch_fire_mode,
        &sounds.zoom,
    ]
    .into_iter()
    .flatten()
}

/// Loads the level's weapon sounds and the library before they are needed.
fn preload(armory: Res<Armory>, library: Res<Sounds>, mut cache: ResMut<SoundCache>, assets: Res<AssetServer>) {
    if !armory.is_changed() && !library.is_changed() {
        return;
    }
    for desc in library.0.sounds.values() {
        cache.preload(&assets, desc);
    }
    for weapon in armory.weapons.values() {
        for desc in weapon_sounds(&weapon.sounds) {
            cache.preload(&assets, desc);
        }
    }
}

/// Traces bullet paths for their impact and flyby sounds.
#[derive(SystemParam)]
struct BulletTracer<'w, 's> {
    time: Res<'w, Time<Real>>,
    spatial: SpatialQuery<'w, 's>,
    surfaces: SurfaceQuery<'w, 's>,
    library: Res<'w, Sounds>,
    level: Option<Res<'w, LoadedLevel>>,
    listener: Query<'w, 's, &'static Transform, With<SpatialListener>>,
    pending: ResMut<'w, Pending>,
}

impl BulletTracer<'_, '_> {
    fn listener(&self) -> Option<Vec3> {
        self.listener.iter().next().map(|t| t.translation)
    }

    /// Follows a bullet from `origin` (gravity included) until it hits something, the water
    /// or its time runs out.
    fn trace(&mut self, origin: Vec3, direction: Vec3, weapon: &WeaponDesc, shooter: Option<Entity>, flyby: bool) {
        let projectile = &weapon.projectile;
        // Grenades and rockets sound through their detonation effect.
        if projectile.velocity <= 0.0 || projectile.explosion_radius > 0.0 {
            return;
        }
        let now = self.time.elapsed_secs();
        let listener = self.listener().filter(|_| flyby);
        let water = self.level.as_ref().and_then(|l| l.desc.water.as_ref()).map(|w| w.height);
        let mut filter = SpatialQueryFilter::from_mask([GameLayer::World, GameLayer::Soldier]);
        if let Some(hitbox) = shooter {
            filter = filter.with_excluded_entities([hitbox]);
        }
        let (mut position, mut velocity) = (origin, direction.normalize_or(Vec3::NEG_Z) * projectile.velocity);
        let (mut t, mut travelled, mut cracked) = (0.0, 0.0, listener.is_none());
        let flight = projectile.time_to_live.min(MAX_FLIGHT);
        while t < flight {
            let dt = SEGMENT.min(flight - t);
            let next = velocity + Vec3::NEG_Y * GRAVITY * projectile.gravity * dt;
            let step = (velocity + next) * 0.5 * dt;
            let length = step.length();
            let Ok(dir) = Dir3::new(step) else { break };
            let mut hit = self
                .spatial
                .cast_ray(position, dir, length, true, &filter)
                .map(|hit| (hit.distance, Some(hit.entity), hit.normal));
            if let Some(height) = water
                && position.y > height
                && position.y + step.y <= height
            {
                let distance = (position.y - height) / -step.y * length;
                if hit.is_none_or(|(d, _, _)| distance < d) {
                    hit = Some((distance, None, Vec3::Y));
                }
            }
            let reach = hit.map_or(length, |(d, _, _)| d);
            if let (false, Some(ear)) = (cracked, listener) {
                let along = (ear - position).dot(*dir).clamp(0.0, reach);
                let closest = position + dir * along;
                if closest.distance(ear) < FLYBY_RADIUS && travelled + along > FLYBY_MIN_TRAVEL {
                    cracked = true;
                    self.flyby(projectile.material, closest, now + t + along / length * dt);
                }
            }
            if let Some((distance, entity, normal)) = hit {
                let point = position + dir * distance;
                let surface = entity.map_or(WATER, |e| self.surfaces.material(e, point));
                self.impact(projectile.material, surface, point, normal, *dir, now + t + distance / length * dt);
                return;
            }
            position += step;
            velocity = next;
            travelled += length;
            t += dt;
        }
    }

    fn flyby(&mut self, projectile: u32, at: Vec3, when: f32) {
        let flybys = &self.library.0.flybys;
        if let Some(name) = flybys.get(&projectile).or_else(|| flybys.get(&BULLET)) {
            let sound = PlaySound::at(Sound::Named(name.clone()), at).reason("flyby");
            self.pending.0.push((when, sound));
        }
    }

    fn impact(&mut self, projectile: u32, surface: u32, at: Vec3, normal: Vec3, direction: Vec3, when: f32) {
        let impacts = &self.library.0.impacts;
        let Some(sounds) = impacts
            .get(&projectile)
            .or_else(|| impacts.get(&BULLET))
            .and_then(|by_surface| by_surface.get(&surface))
        else {
            debug!(target: "audio", "no impact sound for materials {projectile} on {surface}");
            return;
        };
        if let Some(name) = &sounds.impact {
            let sound = PlaySound::at(Sound::Named(name.clone()), at).reason("impact");
            self.pending.0.push((when, sound));
        }
        if let Some(name) = &sounds.ricochet
            && normal.dot(-direction) < RICOCHET_COSINE
            && fastrand::bool()
        {
            let sound = PlaySound::at(Sound::Named(name.clone()), at).reason("ricochet");
            self.pending.0.push((when, sound));
        }
    }
}

fn local_shots(
    mut shots: MessageReader<LocalShot>,
    soldier: Query<(Entity, &SoldierRender, &Hitbox), With<LocalSoldier>>,
    mut tracer: BulletTracer,
    mut sounds: MessageWriter<PlaySound>,
) {
    let mut fired = false;
    for shot in shots.read() {
        let Ok((entity, render, hitbox)) = soldier.single() else {
            continue;
        };
        // Shotgun pellets sound once.
        if let (false, Some(fire)) = (fired, &shot.weapon.sounds.fire_1p) {
            sounds.write(PlaySound::local(fire).emitter(entity).reason("fire 1p"));
            fired = true;
        }
        tracer.trace(render.eye_position(), shot.direction, &shot.weapon, Some(hitbox.entity), false);
    }
}

fn remote_shots(
    mut shots: MessageReader<ShotFired>,
    armory: Res<Armory>,
    loadouts: Query<(&Loadout, Option<&Hitbox>)>,
    mut tracer: BulletTracer,
    mut sounds: MessageWriter<PlaySound>,
    mut heard: Local<Vec<Entity>>,
) {
    heard.clear();
    for shot in shots.read() {
        let Ok((loadout, hitbox)) = loadouts.get(shot.soldier) else {
            continue;
        };
        let Some(weapon) = loadout.weapons.get(shot.weapon as usize).and_then(|w| armory.weapon(w)) else {
            continue;
        };
        tracer.trace(shot.origin, shot.direction, weapon, hitbox.map(|h| h.entity), true);
        if heard.contains(&shot.soldier) {
            continue;
        }
        heard.push(shot.soldier);
        let far = tracer
            .listener()
            .zip(weapon.sounds.fire_3p_distant.as_ref())
            .map_or(0.0, |(ear, _)| smoothstep(FAR_FIRE[0], FAR_FIRE[1], ear.distance(shot.origin)));
        if let Some(near) = weapon.sounds.fire_3p.as_ref().filter(|_| far < 0.99) {
            let sound = PlaySound::at(near, shot.origin).volume(1.0 - far).emitter(shot.soldier);
            sounds.write(sound.reason("fire 3p"));
        }
        if let Some(distant) = weapon.sounds.fire_3p_distant.as_ref().filter(|_| far > 0.01) {
            let sound = PlaySound::at(distant, shot.origin).volume(far).emitter(shot.soldier);
            sounds.write(sound.reason("fire 3p far"));
        }
    }
}

#[derive(Default)]
struct LocalWeaponState {
    soldier: Option<Entity>,
    buttons: Buttons,
    reloading: bool,
    selected: u8,
    /// Active weapon and rounds in its magazine.
    magazine: (u8, u16),
}

/// Our weapon's reload, switch, zoom, fire mode and dry fire sounds.
#[allow(clippy::too_many_arguments)]
fn local_weapon(
    history: Res<InputHistory>,
    feedback: Res<CombatFeedback>,
    selection: Res<WeaponSelection>,
    armory: Res<Armory>,
    soldier: Query<(Entity, &Loadout, &Inventory), With<LocalSoldier>>,
    mut state: Local<LocalWeaponState>,
    mut sounds: MessageWriter<PlaySound>,
) {
    let Ok((entity, loadout, inventory)) = soldier.single() else {
        state.soldier = None;
        return;
    };
    let buttons = history.latest().map_or(Buttons::empty(), |input| input.buttons);
    let selected = selection.index;
    let magazine = (
        inventory.active,
        inventory.ammo.get(inventory.active as usize).map_or(0, |a| a[0]),
    );
    let previous = std::mem::replace(
        &mut *state,
        LocalWeaponState {
            soldier: Some(entity),
            buttons,
            reloading: feedback.reloading,
            selected,
            magazine,
        },
    );
    let Some(weapon) = loadout.weapons.get(selected as usize).and_then(|w| armory.weapon(w)) else {
        return;
    };
    let mut play = |sound: &Option<SoundDesc>, reason: &'static str| {
        if let Some(sound) = sound {
            sounds.write(PlaySound::local(sound).emitter(entity).reason(reason));
        }
    };
    // Spawning takes the weapon out too.
    if previous.soldier != Some(entity) || previous.selected != selected {
        play(&weapon.sounds.deploy_1p, "deploy 1p");
        return;
    }
    let pressed = |button: Buttons| buttons.contains(button) && !previous.buttons.contains(button);
    if feedback.reloading && !previous.reloading {
        play(&weapon.sounds.reload_1p, "reload 1p");
    }
    if pressed(Buttons::AIM) && weapon.zoom_factors.iter().any(|&f| f > 0.0) {
        play(&weapon.sounds.zoom, "zoom");
    }
    if pressed(Buttons::FIRE_MODE) && weapon.fire_modes.len() > 1 {
        play(&weapon.sounds.switch_fire_mode, "fire mode");
    }
    let [rounds, spare] = inventory.ammo.get(selected as usize).copied().unwrap_or([0, 0]);
    if pressed(Buttons::FIRE) && weapon.magazine_size > 0 && rounds == 0 && spare == 0 {
        play(&weapon.sounds.dry_fire, "dry fire");
    }
    // The server's count: the bolt catches once it confirms the last round.
    if magazine.0 == previous.magazine.0 && previous.magazine.1 > 0 && magazine.1 == 0 && !inventory.reloading {
        play(&weapon.sounds.bolt, "bolt");
    }
}

/// Other soldiers' reloads and weapon switches, from their replicated inventory.
fn remote_weapons(
    armory: Res<Armory>,
    soldiers: Query<(Entity, &Loadout, &Inventory, &SoldierRender), (With<Soldier>, Without<LocalSoldier>)>,
    mut known: Local<HashMap<Entity, (u8, bool)>>,
    mut sounds: MessageWriter<PlaySound>,
) {
    for (entity, loadout, inventory, render) in &soldiers {
        let Some((active, reloading)) = known.insert(entity, (inventory.active, inventory.reloading)) else {
            continue;
        };
        let Some(weapon) = loadout.weapons.get(inventory.active as usize).and_then(|w| armory.weapon(w)) else {
            continue;
        };
        let hands = render.position + Vec3::Y * (render.eye_height - 0.35).max(0.2);
        let mut play = |sound: &Option<SoundDesc>, reason: &'static str| {
            if let Some(sound) = sound {
                sounds.write(PlaySound::at(sound, hands).emitter(entity).reason(reason));
            }
        };
        if inventory.active != active {
            play(&weapon.sounds.deploy_3p, "deploy 3p");
        } else if inventory.reloading && !reloading {
            play(&weapon.sounds.reload_3p, "reload 3p");
        }
    }
    known.retain(|entity, _| soldiers.contains(*entity));
}

/// Deaths (the soldier is gone by the time the kill feed arrives: its last position is
/// remembered for a while), and our soldier getting hurt.
#[allow(clippy::too_many_arguments)]
fn soldier_voices(
    time: Res<Time<Real>>,
    library: Res<Sounds>,
    mut kills: MessageReader<KillFeed>,
    soldiers: Query<(&SoldierRender, &ControlledBy), With<Soldier>>,
    local: Query<(Entity, &Health), With<LocalSoldier>>,
    mut last_seen: Local<HashMap<Entity, (Vec3, f32)>>,
    mut health: Local<Option<(Entity, f32)>>,
    mut pending: ResMut<Pending>,
    mut sounds: MessageWriter<PlaySound>,
) {
    let now = time.elapsed_secs();
    for (render, controlled_by) in &soldiers {
        last_seen.insert(controlled_by.0, (render.position, now));
    }
    let soldier = &library.0.soldier;
    for kill in kills.read() {
        let Some(&(position, _)) = last_seen.get(&kill.victim) else {
            continue;
        };
        if let Some(death) = &soldier.death {
            sounds.write(PlaySound::at(Sound::Named(death.clone()), position + Vec3::Y * 0.8).reason("death"));
        }
        if let Some(fall) = &soldier.body_fall {
            let sound = PlaySound::at(Sound::Named(fall.clone()), position).reason("body fall");
            pending.0.push((now + 0.7, sound));
        }
    }
    last_seen.retain(|_, (_, seen)| now - *seen < 2.0);

    let current = local.single().ok().map(|(entity, health)| (entity, health.current));
    if let (Some((entity, now_health)), Some((was_entity, was_health))) = (current, *health)
        && entity == was_entity
        && now_health < was_health
        && now_health > 0.0
        && let Some(injury) = &soldier.injury
    {
        sounds.write(PlaySound::local(Sound::Named(injury.clone())).reason("hurt"));
    }
    *health = current;
}

fn play_pending(time: Res<Time<Real>>, mut pending: ResMut<Pending>, mut sounds: MessageWriter<PlaySound>) {
    let now = time.elapsed_secs();
    let mut index = 0;
    while index < pending.0.len() {
        if pending.0[index].0 <= now {
            sounds.write(pending.0.swap_remove(index).1);
        } else {
            index += 1;
        }
    }
}
