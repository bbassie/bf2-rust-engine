//! Client side of weapons: weapon selection, locally predicted shots (tracer, recoil), zoom,
//! and effects for everyone else's shots. Hits are decided by the server. Weapon sounds are
//! in `audio`.

use std::{collections::VecDeque, sync::Arc};

use avian3d::prelude::*;
use bevy::{input::mouse::AccumulatedMouseScroll, prelude::*};
use bevy_replicon::{
    client::{ServerUpdateTick, server_mutate_ticks::ServerMutateTicks},
    prelude::*,
};
use game_data::{FireKind, FireMode, WeaponDesc};
use game_shared::{
    input::{Buttons, InputPacket},
    physics::GameLayer,
    protocol::{HitConfirmed, KillFeed, Player, ShotFired},
    soldier::{Hitbox, SoldierMotion},
    projectile::{launch_origin, launch_velocity},
    weapons::{Armory, Fired, Inventory, Loadout, Trigger, WeaponState, cooks, spread_direction},
};

use crate::{
    camera::{PlayerCamera, ThirdPerson},
    effects::{EffectLibrary, SpawnDecal, SpawnEffect, SurfaceQuery, decals::Decals},
    local_input::{InputHistory, LocalInputSystems, LookState},
    net::{LocalPlayer, LocalSoldier},
    prediction::{INTERPOLATION_DELAY, Predicted, SoldierRender},
    render::scope::{WeaponPart, Zoom},
};

pub struct ClientCombatPlugin;

impl Plugin for ClientCombatPlugin {
    fn build(&self, app: &mut App) {
        app.init_resource::<WeaponSelection>()
            .init_resource::<CombatFeedback>()
            .init_resource::<ViewTick>()
            .add_systems(Startup, simulated_input_delay)
            .add_systems(PreUpdate, track_view_tick.after(ClientSystems::Receive))
            .add_systems(PostUpdate, delay_inputs.before(ClientSystems::Send))
            .add_message::<LocalShot>()
            .add_message::<LocalLaunch>()
            .add_systems(Startup, load_effect_assets)
            .add_observer(init_local_weapon)
            .add_systems(FixedUpdate, predict_local_shots.after(LocalInputSystems))
            .add_systems(
                Update,
                (
                    select_weapon,
                    (spawn_local_shots, receive_shots, receive_hits, receive_kills),
                    (update_tracers, update_impacts, apply_zoom),
                )
                    .chain(),
            );
    }
}

/// The server tick of the world we see (other soldiers are shown
/// [`INTERPOLATION_DELAY`] behind the latest state received), sent with every input so the
/// server judges our hits against it. 0 when not connected to a remote server.
#[derive(Resource, Default)]
pub struct ViewTick {
    pub tick: u32,
    latest: u32,
    received: f64,
}

fn track_view_tick(
    real: Res<Time<Real>>,
    state: Res<State<ClientState>>,
    updates: Option<Res<ServerUpdateTick>>,
    mutations: Option<Res<ServerMutateTicks>>,
    mut view: ResMut<ViewTick>,
) {
    if *state.get() != ClientState::Connected {
        *view = ViewTick::default();
        return;
    }
    let latest = updates
        .map_or(0, |t| t.get())
        .max(mutations.map_or(0, |t| t.last_tick().get()));
    let now = real.elapsed_secs_f64();
    if latest != view.latest {
        view.latest = latest;
        view.received = now;
    }
    // The server has gone on ticking since (it only sends what changed); what is on screen
    // is from a moment before.
    let seconds = now - view.received - INTERPOLATION_DELAY;
    view.tick = (latest as i64 + (seconds * game_shared::TICK_HZ).round() as i64).max(1) as u32;
}

/// `BF2_SIM_INPUT_DELAY_MS`: our inputs are held back this long before they go to the
/// server, to test lag compensation against a real server as if the network were slow.
#[derive(Resource)]
struct InputDelay {
    seconds: f64,
    queue: VecDeque<(f64, InputPacket)>,
}

fn simulated_input_delay(mut commands: Commands) {
    let delay = std::env::var("BF2_SIM_INPUT_DELAY_MS").ok().and_then(|ms| ms.parse::<f64>().ok());
    if let Some(ms) = delay.filter(|ms| *ms > 0.0) {
        warn!("holding inputs back {ms} ms (BF2_SIM_INPUT_DELAY_MS)");
        commands.insert_resource(InputDelay {
            seconds: ms / 1000.0,
            queue: VecDeque::new(),
        });
    }
}

fn delay_inputs(real: Res<Time<Real>>, delay: Option<ResMut<InputDelay>>, mut packets: ResMut<Messages<InputPacket>>) {
    let Some(mut delay) = delay else {
        return;
    };
    let now = real.elapsed_secs_f64();
    let due = now + delay.seconds;
    for packet in packets.drain() {
        delay.queue.push_back((due, packet));
    }
    while delay.queue.front().is_some_and(|(at, _)| *at <= now) {
        if let Some((_, packet)) = delay.queue.pop_front() {
            packets.write(packet);
        }
    }
}

/// The weapon the player wants in hand (index into the loadout), sent with every input.
#[derive(Resource, Default)]
pub struct WeaponSelection {
    pub index: u8,
}

/// Recent combat events for the HUD.
#[derive(Resource)]
pub struct CombatFeedback {
    /// Seconds left to show the hit marker.
    pub hit_marker: f32,
    pub hit_killed: bool,
    /// Kill feed lines with the time they arrived.
    pub kills: VecDeque<(String, f64)>,
    /// Who killed us last, while we are dead.
    pub killed_by: Option<String>,
    /// Current spread cone (degrees) of our weapon, for the crosshair.
    pub spread: f32,
    /// Current field of view multiplier from zooming (1 = not zoomed).
    pub zoom: f32,
    /// Shots our weapon fired (predicted), for the first-person animations.
    pub shots_fired: u32,
    /// Our weapon is reloading (predicted).
    pub reloading: bool,
    /// We are winding up a throw (and cooking a grenade) (predicted).
    pub cooking: bool,
    /// Seconds left on the fuse of the grenade in our hand, while winding up (predicted).
    pub fuse: Option<f32>,
    /// The C4 detonator is in our hand instead of the charges (predicted).
    pub detonator: bool,
}

impl Default for CombatFeedback {
    fn default() -> Self {
        Self {
            hit_marker: 0.0,
            hit_killed: false,
            kills: VecDeque::new(),
            killed_by: None,
            spread: 0.0,
            zoom: 1.0,
            shots_fired: 0,
            reloading: false,
            cooking: false,
            fuse: None,
            detonator: false,
        }
    }
}

/// Client-side weapon timers for our own soldier.
#[derive(Component, Default)]
struct LocalWeapon {
    state: WeaponState,
    selected: u8,
    /// Predicted ammo per weapon, reset whenever the server's arrives.
    ammo: Vec<[u16; 2]>,
    /// A throw was on its way out of the hand last tick.
    launching: bool,
    /// Seconds the grenade or mine in hand has been used up.
    used_up_for: f32,
}

/// Seconds after the last grenade or mine is thrown until the primary weapon comes out.
const TOGGLE_WHEN_USED_UP: f32 = 0.8;

/// A shot we predicted this tick, spawned as visuals in `Update` (and heard, see `audio`).
#[derive(Message)]
pub(crate) struct LocalShot {
    pub direction: Vec3,
    pub weapon: Arc<WeaponDesc>,
    /// Which of a shotgun's pellets this is (0 for everything else).
    pub pellet: u32,
}

/// A grenade, rocket or charge we predicted leaving our hands this tick (see
/// `render::projectiles`).
#[derive(Message)]
pub(crate) struct LocalLaunch {
    pub origin: Vec3,
    pub velocity: Vec3,
    pub weapon: Arc<WeaponDesc>,
    /// Our facing, which placed mines keep.
    pub yaw: f32,
}

#[derive(Resource)]
pub(crate) struct EffectAssets {
    tracer: Handle<Mesh>,
    tracer_material: Handle<StandardMaterial>,
    impact: Handle<Mesh>,
    impact_material: Handle<StandardMaterial>,
}

#[derive(Component)]
struct Tracer {
    velocity: Vec3,
    gravity: f32,
    life: f32,
    /// The shooter's hitbox, which the tracer may start inside of.
    ignore: Option<Entity>,
    /// Projectile material, for the impact effect and the mark it leaves.
    material: u32,
    /// Shells that go off where they hit: the server plays the detonation (`PlayEffect`),
    /// the tracer shows no impact.
    detonates: bool,
}

#[derive(Component)]
struct Impact {
    age: f32,
}

fn load_effect_assets(
    mut commands: Commands,
    mut meshes: ResMut<Assets<Mesh>>,
    mut materials: ResMut<Assets<StandardMaterial>>,
) {
    commands.insert_resource(EffectAssets {
        tracer: meshes.add(Cuboid::new(0.025, 0.025, 2.5)),
        tracer_material: materials.add(StandardMaterial {
            base_color: Color::srgb(1.0, 0.8, 0.35),
            emissive: LinearRgba::rgb(6.0, 3.5, 1.0),
            unlit: true,
            ..default()
        }),
        impact: meshes.add(Sphere::new(0.12)),
        impact_material: materials.add(StandardMaterial {
            base_color: Color::srgba(0.55, 0.5, 0.42, 0.7),
            alpha_mode: AlphaMode::Blend,
            unlit: true,
            ..default()
        }),
    });
}

fn init_local_weapon(
    add: On<Add, LocalSoldier>,
    mut commands: Commands,
    inventories: Query<&Inventory>,
    mut selection: ResMut<WeaponSelection>,
    mut feedback: ResMut<CombatFeedback>,
) {
    let active = inventories.get(add.entity).map_or(0, |i| i.active);
    selection.index = active;
    feedback.killed_by = None;
    commands.entity(add.entity).insert(LocalWeapon {
        selected: active,
        ..default()
    });
}

fn select_weapon(
    actions: crate::settings::Actions,
    scroll: Res<AccumulatedMouseScroll>,
    armory: Res<Armory>,
    soldier: Query<&Loadout, With<LocalSoldier>>,
    mut selection: ResMut<WeaponSelection>,
) {
    let Ok(loadout) = soldier.single() else {
        return;
    };
    let count = loadout.weapons.len() as u8;
    if count == 0 {
        return;
    }
    let slot_of = |index: u8| {
        loadout
            .weapons
            .get(index as usize)
            .and_then(|w| armory.weapon(w))
            .map_or(0, |w| w.slot)
    };
    // Number keys pick a BF2 inventory slot; pressing again cycles weapons in that slot.
    for slot in 1..=9 {
        if !actions.just_pressed(crate::settings::Action::WeaponSlot(slot as u8)) {
            continue;
        }
        let in_slot: Vec<u8> = (0..count).filter(|&w| slot_of(w) == slot).collect();
        if let Some(&first) = in_slot.first() {
            let current = in_slot.iter().position(|&w| w == selection.index);
            selection.index = match current {
                Some(pos) => in_slot[(pos + 1) % in_slot.len()],
                None => first,
            };
        }
    }
    if scroll.delta.y < 0.0 {
        selection.index = (selection.index + 1) % count;
    } else if scroll.delta.y > 0.0 {
        selection.index = (selection.index + count - 1) % count;
    }
}

/// Runs the same weapon rules as the server for our soldier, so shots, throws and reloads
/// show at once.
#[allow(clippy::type_complexity, clippy::too_many_arguments)]
fn predict_local_shots(
    time: Res<Time>,
    armory: Res<Armory>,
    history: Res<InputHistory>,
    spatial: SpatialQuery,
    mut look: ResMut<LookState>,
    mut feedback: ResMut<CombatFeedback>,
    mut selection: ResMut<WeaponSelection>,
    mut soldier: Query<
        (&SoldierMotion, &Loadout, Ref<Inventory>, &mut LocalWeapon, Option<&Predicted>),
        (With<LocalSoldier>, Without<game_shared::vehicle::Seated>),
    >,
    mut shots: MessageWriter<LocalShot>,
    mut launches: MessageWriter<LocalLaunch>,
) {
    let (Some(input), Ok((motion, loadout, inventory, mut local, predicted))) = (history.latest(), soldier.single_mut())
    else {
        return;
    };
    let dt = time.delta_secs();
    let active = if (input.weapon as usize) < loadout.weapons.len() { input.weapon } else { inventory.active };
    let Some(weapon) = loadout.weapons.get(active as usize).and_then(|w| armory.weapon(w)).cloned() else {
        return;
    };
    let local = &mut *local;
    if local.selected != active {
        local.selected = active;
        local.state.switch_to(&weapon);
    }
    if inventory.is_changed() || local.ammo.len() != inventory.ammo.len() {
        local.ammo = inventory.ammo.clone();
    }
    let state = &mut local.state;
    let local_velocity = Quat::from_rotation_y(-input.yaw) * motion.velocity;
    state.tick(&weapon.deviation, dt, -local_velocity.z, local_velocity.x, !motion.grounded);
    let zoomed = input.pressed(Buttons::AIM);
    let cone = state.deviation(&weapon.deviation, motion.stance, zoomed);
    feedback.spread = cone;

    // Not on ladders, nor just after a jump or getting up (as predicted).
    let can_fire = predicted.map_or(motion, |p| p.motion()).can_fire();
    let trigger = Trigger {
        fire: input.pressed(Buttons::FIRE) && can_fire,
        alt: input.pressed(Buttons::AIM) && can_fire,
        reload: input.pressed(Buttons::RELOAD),
        lowered: input.pressed(Buttons::SPRINT) && input.movement[1] > 64,
    };
    let mode = weapon
        .fire_modes
        .get(inventory.fire_mode as usize)
        .copied()
        .unwrap_or(FireMode::Single);
    let mut ammo = local.ammo.get(active as usize).copied().unwrap_or([0, 0]);
    let fired = state.trigger(&weapon, mode, &mut ammo, trigger, dt);
    if let Some(slot) = local.ammo.get_mut(active as usize) {
        *slot = ammo;
    }
    feedback.reloading = state.reload > 0.0;
    feedback.cooking = state.wind_up.is_some();
    feedback.fuse = state
        .wind_up
        .filter(|_| cooks(&weapon.projectile))
        .map(|held| (weapon.projectile.time_to_live - (held - weapon.fire.pull_back).max(0.0)).max(0.0));
    feedback.detonator = state.detonator && weapon.fire.kind == FireKind::Explosives;
    // Out of grenades or mines: back to the primary weapon once the throw is over (BF2
    // `ammo.toggleWhenNoAmmo`).
    let used_up = weapon.fire.kind == FireKind::Thrown && ammo == [0, 0] && state.launch.is_none();
    local.used_up_for = if used_up { local.used_up_for + dt } else { 0.0 };
    if local.used_up_for > TOGGLE_WHEN_USED_UP && selection.index == active {
        let primary = loadout.weapons.iter().position(|w| armory.weapon(w).is_some_and(|w| w.slot == 3));
        if let Some(primary) = primary {
            selection.index = primary as u8;
        }
    }
    // A throw animates from letting go, before the grenade leaves the hand.
    let launching = state.launch.is_some();
    let released = launching && !local.launching;
    let launched = matches!(fired, Some(Fired::Launch { .. })) && !local.launching;
    local.launching = launching;
    // The detonator's press animates like a shot.
    if released || launched || fired == Some(Fired::Detonate) {
        feedback.shots_fired = feedback.shots_fired.wrapping_add(1);
    }
    let Some(Fired::Launch { soft, .. }) = fired else {
        return;
    };
    let view = Quat::from_euler(EulerRot::YXZ, input.yaw, input.pitch, 0.0);
    let direction = spread_direction(view * Vec3::NEG_Z, cone, (fastrand::f32(), fastrand::f32()));
    let pellets = weapon.projectiles_per_shot.max(1);
    for pellet in 0..pellets {
        let direction = match pellets {
            1 => direction,
            _ => spread_direction(direction, weapon.pellet_spread, (fastrand::f32(), fastrand::f32())),
        };
        shots.write(LocalShot {
            direction,
            weapon: weapon.clone(),
            pellet,
        });
    }
    if weapon.projectile.is_object() {
        launches.write(LocalLaunch {
            origin: launch_origin(&spatial, motion.eye_position(), view, Vec3::from(weapon.fire.start_offset)),
            velocity: launch_velocity(&weapon, direction, soft, motion.velocity),
            weapon: weapon.clone(),
            yaw: input.yaw,
        });
    }

    // Recoil kicks the view, which also moves the aim of the next shots.
    let recoil = &weapon.recoil;
    let scale = if zoomed { recoil.zoom_modifier } else { 1.0 };
    let range = |r: [f32; 2]| r[0] + (r[1] - r[0]) * fastrand::f32();
    look.pitch += range(recoil.up).to_radians() * scale;
    look.yaw -= range(recoil.left_right).to_radians() * scale;
}

pub(crate) fn spawn_tracer(
    commands: &mut Commands,
    assets: &EffectAssets,
    origin: Vec3,
    direction: Vec3,
    weapon: &WeaponDesc,
    ignore: Option<Entity>,
) {
    if weapon.projectile.velocity <= 0.0 {
        return;
    }
    commands.spawn((
        Tracer {
            velocity: direction * weapon.projectile.velocity,
            gravity: weapon.projectile.gravity,
            life: weapon.projectile.time_to_live.min(3.0),
            ignore,
            material: weapon.projectile.material,
            detonates: weapon.projectile.goes_off(),
        },
        Mesh3d(assets.tracer.clone()),
        MeshMaterial3d(assets.tracer_material.clone()),
        Transform::from_translation(origin).looking_to(direction, Vec3::Y),
        bevy::light::NotShadowCaster,
    ));
}

#[allow(clippy::too_many_arguments)]
fn spawn_local_shots(
    mut commands: Commands,
    mut shots: MessageReader<LocalShot>,
    assets: Res<EffectAssets>,
    camera: Single<&Transform, With<PlayerCamera>>,
    library: Option<Res<EffectLibrary>>,
    zoom: Res<Zoom>,
    third_person: Res<ThirdPerson>,
    soldier: Query<(&SoldierMotion, &Hitbox), With<LocalSoldier>>,
    weapon_parts: Query<(&WeaponPart, &GlobalTransform)>,
    mut effects: MessageWriter<SpawnEffect>,
) {
    let soldier = soldier.single().ok();
    let library = library.as_deref();
    for shot in shots.read() {
        // From roughly where the muzzle is, just below and right of the eye.
        let origin = camera.translation + camera.rotation * Vec3::new(0.12, -0.1, -0.4);
        let hitbox = soldier.map(|(_, hitbox)| hitbox.entity);
        // Grenades, rockets and charges are drawn as themselves (`render::projectiles`).
        if !shot.weapon.projectile.is_object() {
            spawn_tracer(&mut commands, &assets, origin, shot.direction, &shot.weapon, hitbox);
        }
        // Shotguns fire several pellets but flash once.
        if shot.pellet > 0 {
            continue;
        }
        let Some((muzzle, offset)) = library.and_then(|l| l.muzzle(&shot.weapon.name)) else {
            continue;
        };
        if third_person.0 {
            if let Some((motion, _)) = soldier {
                let at = motion.eye_position() - Vec3::Y * 0.15 + shot.direction * 0.8;
                effects.write(SpawnEffect::new(muzzle, at).with_forward(shot.direction));
            }
        } else if !zoom.scoped {
            // At the muzzle of the view model's weapon.
            let at = weapon_parts
                .iter()
                .find(|(part, _)| part.0 == 0)
                .map_or(origin, |(_, transform)| transform.transform_point(offset));
            effects.write(SpawnEffect::new(muzzle, at).with_forward(camera.forward().as_vec3()).first_person());
        }
    }
}

#[allow(clippy::too_many_arguments)]
fn receive_shots(
    mut commands: Commands,
    mut shots: MessageReader<ShotFired>,
    assets: Res<EffectAssets>,
    armory: Res<Armory>,
    loadouts: Query<(&Loadout, Option<&Hitbox>)>,
    library: Option<Res<EffectLibrary>>,
    mut effects: MessageWriter<SpawnEffect>,
    mut flashed: Local<Vec<Entity>>,
) {
    flashed.clear();
    let library = library.as_deref();
    for shot in shots.read() {
        let Ok((loadout, hitbox)) = loadouts.get(shot.soldier) else {
            continue;
        };
        let Some(weapon) = loadout.weapons.get(shot.weapon as usize).and_then(|w| armory.weapon(w)) else {
            continue;
        };
        // Tracer from the gun rather than the eye.
        let gun = shot.origin - Vec3::Y * 0.15;
        if !weapon.projectile.is_object() {
            spawn_tracer(&mut commands, &assets, gun, shot.direction, weapon, hitbox.map(|h| h.entity));
        }
        // Shotguns fire several pellets but flash once.
        if flashed.contains(&shot.soldier) {
            continue;
        }
        flashed.push(shot.soldier);
        if let Some((muzzle, _)) = library.and_then(|l| l.muzzle(&weapon.name)) {
            effects.write(SpawnEffect::new(muzzle, gun + shot.direction * 0.8).with_forward(shot.direction));
        }
    }
}

fn receive_hits(mut hits: MessageReader<HitConfirmed>, mut feedback: ResMut<CombatFeedback>) {
    for hit in hits.read() {
        feedback.hit_marker = 0.2;
        feedback.hit_killed = hit.killed;
    }
}

fn receive_kills(
    time: Res<Time<Real>>,
    mut kills: MessageReader<KillFeed>,
    players: Query<&Player>,
    local: Query<Entity, With<LocalPlayer>>,
    mut feedback: ResMut<CombatFeedback>,
) {
    let name = |e: Option<Entity>| e.and_then(|e| players.get(e).ok()).map(|p| p.name.clone());
    for kill in kills.read() {
        let victim = name(Some(kill.victim)).unwrap_or_else(|| "?".into());
        let weapon = weapon_display_name(&kill.weapon);
        let headshot = if kill.headshot { " (headshot)" } else { "" };
        let line = match name(kill.killer) {
            Some(killer) if kill.killer != Some(kill.victim) => format!("{killer}  [{weapon}{headshot}]  {victim}"),
            _ => format!("{victim} died"),
        };
        feedback.kills.push_back((line, time.elapsed_secs_f64()));
        while feedback.kills.len() > 6 {
            feedback.kills.pop_front();
        }
        if local.single().is_ok_and(|me| me == kill.victim) {
            feedback.killed_by = Some(name(kill.killer).unwrap_or_else(|| "the environment".into()));
        }
    }
}

/// `usrif_m16a2` / `KILLMESSAGE_WEAPON_m16a2` → `M16A2`.
pub fn weapon_display_name(name: &str) -> String {
    let name = name.trim_start_matches("KILLMESSAGE_WEAPON_");
    let name = match name.split_once('_') {
        Some((prefix, rest)) if prefix.len() <= 6 && !rest.is_empty() => rest,
        _ => name,
    };
    name.replace('_', " ").to_uppercase()
}

#[allow(clippy::too_many_arguments)]
fn update_tracers(
    mut commands: Commands,
    time: Res<Time>,
    spatial: SpatialQuery,
    assets: Res<EffectAssets>,
    library: Option<Res<EffectLibrary>>,
    decals: Option<Res<Decals>>,
    surfaces: SurfaceQuery,
    mut effects: MessageWriter<SpawnEffect>,
    mut marks: MessageWriter<SpawnDecal>,
    mut tracers: Query<(Entity, &mut Tracer, &mut Transform)>,
) {
    let dt = time.delta_secs();
    let layers = SpatialQueryFilter::from_mask([GameLayer::World, GameLayer::Soldier]);
    for (entity, mut tracer, mut transform) in &mut tracers {
        let filter = match tracer.ignore {
            Some(hitbox) => layers.clone().with_excluded_entities([hitbox]),
            None => layers.clone(),
        };
        tracer.life -= dt;
        let start = tracer.velocity;
        let gravity = tracer.gravity;
        tracer.velocity += Vec3::NEG_Y * 9.81 * gravity * dt;
        let step = (start + tracer.velocity) * 0.5 * dt;
        let hit = Dir3::new(step)
            .ok()
            .and_then(|dir| spatial.cast_ray(transform.translation, dir, step.length(), true, &filter));
        if let Some(hit) = hit {
            commands.entity(entity).despawn();
            if tracer.detonates {
                continue;
            }
            let point = transform.translation + step.normalize() * hit.distance;
            let surface = surfaces.material(hit.entity, point);
            if let (Some(parent), Some(name)) = (
                surfaces.decal_surface(hit.entity),
                decals.as_ref().and_then(|d| d.decal(tracer.material, surface)),
            ) {
                marks.write(SpawnDecal {
                    name: name.to_string(),
                    position: point,
                    normal: hit.normal,
                    parent,
                });
            }
            match library.as_ref().map(|l| (l.impact(tracer.material, surface), l.impacts.effects.is_empty())) {
                Some((Some(name), _)) => {
                    effects.write(SpawnEffect::new(name, point).with_up(hit.normal));
                }
                // BF2 has no effect for this projectile on this surface.
                Some((None, false)) => {}
                // Without imported effects: a puff.
                _ => {
                    commands.spawn((
                        Impact { age: 0.0 },
                        Mesh3d(assets.impact.clone()),
                        MeshMaterial3d(assets.impact_material.clone()),
                        Transform::from_translation(point).with_scale(Vec3::splat(0.3)),
                        bevy::light::NotShadowCaster,
                    ));
                }
            }
            continue;
        }
        if tracer.life <= 0.0 {
            commands.entity(entity).despawn();
            continue;
        }
        transform.translation += step;
        transform.look_to(step, Vec3::Y);
    }
}

fn update_impacts(mut commands: Commands, time: Res<Time>, mut impacts: Query<(Entity, &mut Impact, &mut Transform)>) {
    for (entity, mut impact, mut transform) in &mut impacts {
        impact.age += time.delta_secs();
        if impact.age > 0.35 {
            commands.entity(entity).despawn();
            continue;
        }
        transform.scale = Vec3::splat(0.3 + impact.age * 3.0);
        transform.translation.y += time.delta_secs() * 0.5;
    }
}

/// Right mouse zooms by the weapon's zoom factor (BF2 stores it as a field-of-view scale)
/// after its field-of-view delay. The world snaps in and out with the zoom model (a scope
/// view must not show the zoom easing in, nor the hip weapon a magnified world).
#[allow(clippy::too_many_arguments)]
pub(crate) fn apply_zoom(
    time: Res<Time>,
    zoom: Res<Zoom>,
    armory: Res<Armory>,
    soldier: Query<(&Loadout, &Inventory), (With<LocalSoldier>, With<SoldierRender>, Without<game_shared::vehicle::Seated>)>,
    mut was_scoped: Local<bool>,
    mut look: ResMut<LookState>,
    mut feedback: ResMut<CombatFeedback>,
    mut camera: Single<&mut Projection, With<PlayerCamera>>,
    settings: Res<crate::settings::Settings>,
) {
    let target = soldier
        .single()
        .ok()
        .and_then(|(loadout, inventory)| loadout.weapons.get(inventory.active as usize))
        .and_then(|w| armory.weapon(w))
        .filter(|w| zoom.held > 0.0 && zoom.held >= w.zoom.fov_delay)
        .and_then(|w| w.zoom_factors.iter().copied().find(|&f| f > 0.0))
        .unwrap_or(1.0);
    let current = feedback.zoom;
    feedback.zoom = if zoom.scoped || *was_scoped {
        target
    } else {
        current + (target - current) * (1.0 - (-18.0 * time.delta_secs()).exp())
    };
    *was_scoped = zoom.scoped;
    look.zoom_scale = feedback.zoom;
    if let Projection::Perspective(perspective) = camera.as_mut() {
        perspective.fov = settings.field_of_view.to_radians() * feedback.zoom;
    }
    feedback.hit_marker = (feedback.hit_marker - time.delta_secs()).max(0.0);
}
