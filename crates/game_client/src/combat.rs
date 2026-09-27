//! Client side of weapons: weapon selection, locally predicted shots (tracer, sound,
//! recoil), zoom, and effects for everyone else's shots. Hits are decided by the server.

use std::{collections::VecDeque, sync::Arc};

use avian3d::prelude::*;
use bevy::{
    audio::Volume,
    input::mouse::AccumulatedMouseScroll,
    prelude::*,
};
use game_data::{FireMode, WeaponDesc};
use game_shared::{
    input::Buttons,
    physics::GameLayer,
    protocol::{HitConfirmed, KillFeed, Player, ShotFired},
    soldier::SoldierMotion,
    weapons::{Armory, Inventory, Loadout, WeaponState, spread_direction},
};

use crate::{
    camera::PlayerCamera,
    local_input::{InputHistory, LocalInputSystems, LookState},
    net::{LocalPlayer, LocalSoldier},
    prediction::SoldierRender,
};

pub struct ClientCombatPlugin;

impl Plugin for ClientCombatPlugin {
    fn build(&self, app: &mut App) {
        app.init_resource::<WeaponSelection>()
            .init_resource::<CombatFeedback>()
            .add_message::<LocalShot>()
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
        }
    }
}

/// Client-side weapon timers for our own soldier.
#[derive(Component, Default)]
struct LocalWeapon {
    state: WeaponState,
    selected: u8,
}

/// A shot we predicted this tick, spawned as visuals in `Update`.
#[derive(Message)]
struct LocalShot {
    direction: Vec3,
    weapon: Arc<WeaponDesc>,
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
    keys: Res<ButtonInput<KeyCode>>,
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
    const DIGITS: [KeyCode; 9] = [
        KeyCode::Digit1, KeyCode::Digit2, KeyCode::Digit3, KeyCode::Digit4, KeyCode::Digit5,
        KeyCode::Digit6, KeyCode::Digit7, KeyCode::Digit8, KeyCode::Digit9,
    ];
    for (i, key) in DIGITS.iter().enumerate() {
        if !keys.just_pressed(*key) {
            continue;
        }
        let slot = i as u32 + 1;
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

#[allow(clippy::type_complexity)]
fn predict_local_shots(
    time: Res<Time>,
    armory: Res<Armory>,
    history: Res<InputHistory>,
    mut look: ResMut<LookState>,
    mut feedback: ResMut<CombatFeedback>,
    mut soldier: Query<
        (&SoldierMotion, &Loadout, &Inventory, &mut LocalWeapon),
        (With<LocalSoldier>, Without<game_shared::vehicle::Seated>),
    >,
    mut shots: MessageWriter<LocalShot>,
) {
    let (Some(input), Ok((motion, loadout, inventory, mut local))) = (history.latest(), soldier.single_mut()) else {
        return;
    };
    let dt = time.delta_secs();
    let active = if (input.weapon as usize) < loadout.weapons.len() { input.weapon } else { inventory.active };
    let Some(weapon) = loadout.weapons.get(active as usize).and_then(|w| armory.weapon(w)).cloned() else {
        return;
    };
    if local.selected != active {
        local.selected = active;
        local.state.deploy = weapon.deploy_time;
        local.state.reload = 0.0;
        local.state.burst_left = 0;
    }
    let state = &mut local.state;
    let local_velocity = Quat::from_rotation_y(-input.yaw) * motion.velocity;
    state.tick(&weapon.deviation, dt, -local_velocity.z, local_velocity.x, !motion.grounded);
    let zoomed = input.pressed(Buttons::AIM);
    feedback.spread = state.deviation(&weapon.deviation, motion.stance, zoomed);

    let trigger = input.pressed(Buttons::FIRE);
    let [in_mag, spare] = inventory.ammo.get(active as usize).copied().unwrap_or([0, 0]);
    feedback.reloading = state.reload > 0.0;
    if state.reload > 0.0 {
        state.reload -= dt;
        state.trigger_was_down = trigger;
        return;
    }
    let wants_reload = input.pressed(Buttons::RELOAD) && in_mag < weapon.magazine_size as u16;
    if (wants_reload || (in_mag == 0 && trigger)) && spare > 0 {
        state.reload = weapon.reload_time;
        state.trigger_was_down = trigger;
        return;
    }

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
    let sprinting = input.pressed(Buttons::SPRINT) && input.movement[1] > 64;
    if !wants_shot || sprinting || state.cooldown > 0.0 || state.deploy > 0.0 || in_mag == 0 {
        return;
    }
    if mode == FireMode::Burst {
        state.burst_left = if pressed_now { 2 } else { state.burst_left.saturating_sub(1) };
    }
    let cone = state.deviation(&weapon.deviation, motion.stance, zoomed);
    state.on_shot(&weapon);
    feedback.shots_fired = feedback.shots_fired.wrapping_add(1);
    let aim = Quat::from_euler(EulerRot::YXZ, input.yaw, input.pitch, 0.0) * Vec3::NEG_Z;
    shots.write(LocalShot {
        direction: spread_direction(aim, cone, (fastrand::f32(), fastrand::f32())),
        weapon: weapon.clone(),
    });

    // Recoil kicks the view, which also moves the aim of the next shots.
    let recoil = &weapon.recoil;
    let scale = if zoomed { recoil.zoom_modifier } else { 1.0 };
    let range = |r: [f32; 2]| r[0] + (r[1] - r[0]) * fastrand::f32();
    look.pitch += range(recoil.up).to_radians() * scale;
    look.yaw -= range(recoil.left_right).to_radians() * scale;
}

pub(crate) fn play(commands: &mut Commands, asset_server: &AssetServer, path: Option<&String>, at: Option<Vec3>, volume: f32) {
    let Some(path) = path else {
        return;
    };
    let settings = PlaybackSettings::DESPAWN.with_volume(Volume::Linear(volume));
    let source = AudioPlayer::new(asset_server.load(format!("imported://{path}")));
    match at {
        Some(position) => {
            commands.spawn((source, settings.with_spatial(true), Transform::from_translation(position)));
        }
        None => {
            commands.spawn((source, settings));
        }
    }
}

pub(crate) fn spawn_tracer(commands: &mut Commands, assets: &EffectAssets, origin: Vec3, direction: Vec3, weapon: &WeaponDesc) {
    if weapon.projectile.velocity <= 0.0 {
        return;
    }
    commands.spawn((
        Tracer {
            velocity: direction * weapon.projectile.velocity,
            gravity: weapon.projectile.gravity,
            life: weapon.projectile.time_to_live.min(3.0),
        },
        Mesh3d(assets.tracer.clone()),
        MeshMaterial3d(assets.tracer_material.clone()),
        Transform::from_translation(origin).looking_to(direction, Vec3::Y),
        bevy::light::NotShadowCaster,
    ));
}

fn spawn_local_shots(
    mut commands: Commands,
    mut shots: MessageReader<LocalShot>,
    assets: Res<EffectAssets>,
    asset_server: Res<AssetServer>,
    camera: Single<&Transform, With<PlayerCamera>>,
) {
    for shot in shots.read() {
        // From roughly where the muzzle is, just below and right of the eye.
        let origin = camera.translation + camera.rotation * Vec3::new(0.12, -0.1, -0.4);
        spawn_tracer(&mut commands, &assets, origin, shot.direction, &shot.weapon);
        play(&mut commands, &asset_server, shot.weapon.sounds.fire_1p.as_ref(), None, 0.5);
    }
}

fn receive_shots(
    mut commands: Commands,
    mut shots: MessageReader<ShotFired>,
    assets: Res<EffectAssets>,
    asset_server: Res<AssetServer>,
    armory: Res<Armory>,
    loadouts: Query<&Loadout>,
) {
    for shot in shots.read() {
        let Some(weapon) = loadouts
            .get(shot.soldier)
            .ok()
            .and_then(|l| l.weapons.get(shot.weapon as usize))
            .and_then(|w| armory.weapon(w))
        else {
            continue;
        };
        // Tracer from the gun rather than the eye.
        spawn_tracer(&mut commands, &assets, shot.origin - Vec3::Y * 0.15, shot.direction, weapon);
        play(&mut commands, &asset_server, weapon.sounds.fire_3p.as_ref(), Some(shot.origin), 1.0);
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

fn update_tracers(
    mut commands: Commands,
    time: Res<Time>,
    spatial: SpatialQuery,
    assets: Res<EffectAssets>,
    mut tracers: Query<(Entity, &mut Tracer, &mut Transform)>,
) {
    let dt = time.delta_secs();
    let filter = SpatialQueryFilter::from_mask([GameLayer::World, GameLayer::Soldier]);
    for (entity, mut tracer, mut transform) in &mut tracers {
        tracer.life -= dt;
        let start = tracer.velocity;
        let gravity = tracer.gravity;
        tracer.velocity += Vec3::NEG_Y * 9.81 * gravity * dt;
        let step = (start + tracer.velocity) * 0.5 * dt;
        let hit = Dir3::new(step)
            .ok()
            .and_then(|dir| spatial.cast_ray(transform.translation, dir, step.length(), true, &filter));
        if let Some(hit) = hit {
            let point = transform.translation + step.normalize() * hit.distance;
            commands.spawn((
                Impact { age: 0.0 },
                Mesh3d(assets.impact.clone()),
                MeshMaterial3d(assets.impact_material.clone()),
                Transform::from_translation(point).with_scale(Vec3::splat(0.3)),
                bevy::light::NotShadowCaster,
            ));
            commands.entity(entity).despawn();
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

/// Right mouse zooms by the weapon's zoom factor (BF2 stores it as a field-of-view scale).
fn apply_zoom(
    time: Res<Time>,
    history: Res<InputHistory>,
    armory: Res<Armory>,
    soldier: Query<(&Loadout, &Inventory), (With<LocalSoldier>, With<SoldierRender>, Without<game_shared::vehicle::Seated>)>,
    mut look: ResMut<LookState>,
    mut feedback: ResMut<CombatFeedback>,
    mut camera: Single<&mut Projection, With<PlayerCamera>>,
) {
    let target = soldier
        .single()
        .ok()
        .filter(|_| history.latest().is_some_and(|input| input.pressed(Buttons::AIM)))
        .and_then(|(loadout, inventory)| loadout.weapons.get(inventory.active as usize))
        .and_then(|w| armory.weapon(w))
        .and_then(|w| w.zoom_factors.iter().copied().find(|&f| f > 0.0))
        .unwrap_or(1.0);
    let zoom = feedback.zoom;
    feedback.zoom = zoom + (target - zoom) * (1.0 - (-18.0 * time.delta_secs()).exp());
    look.zoom_scale = feedback.zoom;
    if let Projection::Perspective(perspective) = camera.as_mut() {
        perspective.fov = 75f32.to_radians() * feedback.zoom;
    }
    feedback.hit_marker = (feedback.hit_marker - time.delta_secs()).max(0.0);
}
