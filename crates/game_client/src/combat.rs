//! Client side of weapons: weapon selection, locally predicted shots (tracer, recoil), zoom,
//! and effects for everyone else's shots. Hits are decided by the server. Weapon sounds are
//! in `audio`.

use std::{collections::VecDeque, sync::Arc};

use avian3d::prelude::*;
use bevy::{input::mouse::AccumulatedMouseScroll, prelude::*};
use bevy_replicon::{client::confirm_history::ConfirmHistory, prelude::*};
use game_data::{FireKind, FireMode, WeaponDesc};
use game_shared::{
    hitzones::{BodyPose, ServerClock, SoldierImpact, ZoneHit},
    revive::Downed,
    skeleton::{self, AnimState, HitRigs},
    soldier::Soldier,
    vehicle::{Seated, VehicleData},
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
            .add_systems(PostUpdate, record_drawn_anim.after(crate::prediction::RenderStateSystems))
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
                    (spawn_local_shots, receive_shots, receive_hits, receive_kills, receive_soldier_impacts),
                    (update_tracers, update_impacts, apply_zoom),
                )
                    .chain(),
            );
    }
}

/// The server tick of the world we see (other soldiers are shown
/// [`INTERPOLATION_DELAY`] behind the latest state received), sent with every input so the
/// server judges our hits against it. Hosting, the world drawn is between the last two ticks
/// simulated, and our input reaches the server a tick or two later: it is judged against the
/// tick drawn too. 0 without a server clock.
#[derive(Resource, Default)]
pub struct ViewTick {
    pub tick: u32,
    /// The same, with the fraction of a tick: seconds on the server clock (for the idle
    /// animations, which run on it).
    pub seconds: f64,
    /// The latest server tick we know of (the state just received, or simulated).
    pub latest_tick: u32,
    latest: u32,
    received: f64,
}

fn track_view_tick(
    real: Res<Time<Real>>,
    fixed: Res<Time<Fixed>>,
    state: Res<State<ClientState>>,
    local: Query<&ServerClock, Without<ConfirmHistory>>,
    remote: Query<&ServerClock, With<ConfirmHistory>>,
    mut view: ResMut<ViewTick>,
) {
    // Connected, the clock the server replicates: the idle in-process server may have made
    // one of its own before we connected (then every shot went out as seen in the present,
    // with no lag compensation at all).
    let connected = *state.get() == ClientState::Connected;
    let clock = if connected { remote.iter().next() } else { local.iter().next() };
    let Some(clock) = clock else {
        *view = ViewTick::default();
        return;
    };
    let tick = if connected {
        let now = real.elapsed_secs_f64();
        if clock.0 != view.latest {
            view.latest = clock.0;
            view.received = now;
        }
        // What is on screen is from a moment before the latest state (which the server sent a
        // little after the tick it counted).
        let seconds = (now - view.received).min(0.05) - INTERPOLATION_DELAY;
        clock.0 as f64 + seconds * game_shared::TICK_HZ
    } else {
        // Hosting: drawn between the last two ticks (see `prediction`).
        clock.0 as f64 - 1.0 + fixed.overstep_fraction() as f64
    };
    view.tick = (tick.round() as i64).max(1) as u32;
    view.seconds = tick / game_shared::TICK_HZ;
    view.latest_tick = clock.0;
}

/// On another soldier: the server tick he last fired a gun on, as far as we can tell (the tick
/// of the state that came with the `ShotFired`). His shot animation runs from it, and his hit
/// zones follow it (`game_shared::skeleton`).
#[derive(Component, Clone, Copy)]
pub(crate) struct ShotSeen(pub u32);

impl ShotSeen {
    /// Seconds since the shot at a moment on the server clock, once we draw it.
    pub fn since(&self, server_seconds: f64) -> Option<f32> {
        let since = server_seconds - self.0 as f64 / game_shared::TICK_HZ;
        (since >= 0.0).then_some(since as f32)
    }
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
    // --- Quick actions and the weapon list (quick_actions, weapon_list) ---
    /// The melee or grenade key is at work: sent as `Buttons::QUICK`.
    pub quick: bool,
    /// The quick action works the trigger: `Some(true)` holds `Buttons::FIRE` (the grenade key
    /// held, the knife swinging), `Some(false)` lets go of it (so the swing or throw starts
    /// with a fresh press once the knife or grenade is out), `None` leaves it to the player.
    pub quick_fire: Option<bool>,
    /// Counts the player's weapon switches (keys, wheel, quick actions), for the weapon
    /// list to show up.
    pub switches: u32,
    // --- end quick actions ---
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
    /// A released throw is on its way out of our hand (predicted; `quick_actions`).
    pub launching: bool,
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
            launching: false,
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
    /// Where the server's bullet starts: our eye (plus the weapon's start offset).
    pub origin: Vec3,
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
    /// Where the bullet is: on the server's path (from the eye), which is what it can hit.
    position: Vec3,
    /// Drawn this far off `position` at first (from the muzzle), closing in over
    /// [`TRACER_CONVERGE`] meters.
    offset: Vec3,
    travelled: f32,
    velocity: Vec3,
    gravity: f32,
    life: f32,
    /// The shooter's hitbox, which the tracer may start inside of.
    ignore: Option<Entity>,
    /// The shooting soldier, whose hit zones it can't hit.
    shooter: Option<Entity>,
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
    settings: Res<crate::settings::Settings>,
    scroll: Res<AccumulatedMouseScroll>,
    armory: Res<Armory>,
    soldier: Query<&Loadout, With<LocalSoldier>>,
    overlays: crate::weapon_list::Overlays,
    mut selection: ResMut<WeaponSelection>,
) {
    let Ok(loadout) = soldier.single() else {
        return;
    };
    // The wheel zooms the deploy map, number keys type in menus.
    if overlays.covered() {
        return;
    }
    // What can be taken in hand, in slot order (the weapon list's order): no worn gear (night
    // vision, gas mask: keys of their own), no parachute.
    let order = crate::weapon_list::weapon_order(loadout, &armory);
    if order.is_empty() {
        return;
    }
    let before = selection.index;
    // Number keys pick a BF2 inventory slot; pressing again cycles weapons in that slot.
    for slot in 1..=9 {
        if !actions.just_pressed(crate::settings::Action::WeaponSlot(slot as u8)) {
            continue;
        }
        let in_slot: Vec<u8> = order.iter().filter(|(_, w)| w.slot == slot).map(|(i, _)| *i).collect();
        if let Some(&first) = in_slot.first() {
            let current = in_slot.iter().position(|&w| w == selection.index);
            selection.index = match current {
                Some(pos) => in_slot[(pos + 1) % in_slot.len()],
                None => first,
            };
        }
    }
    // The wheel steps through the list; the knife and grenades have their quick keys unless
    // the setting puts them back in.
    let wheel: Vec<u8> = order
        .iter()
        .filter(|(i, w)| {
            settings.scroll_quick_weapons || *i == selection.index || !(w.is_melee() || w.is_hand_grenade())
        })
        .map(|(i, _)| *i)
        .collect();
    let step: isize = match scroll.delta.y {
        y if y < 0.0 => 1,
        y if y > 0.0 => -1,
        _ => 0,
    };
    if step != 0 && !wheel.is_empty() {
        let len = wheel.len() as isize;
        let next = match wheel.iter().position(|&i| i == selection.index) {
            Some(pos) => (pos as isize + step).rem_euclid(len),
            None => 0,
        };
        selection.index = wheel[next as usize];
    }
    if selection.index != before || step != 0 || (1..=9).any(|s| actions.just_pressed(crate::settings::Action::WeaponSlot(s))) {
        selection.switches = selection.switches.wrapping_add(1);
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
        local.state.switch_with(&weapon, input.pressed(Buttons::QUICK));
    }
    if inventory.is_changed() || local.ammo.len() != inventory.ammo.len() {
        local.ammo = inventory.ammo.clone();
    }
    let state = &mut local.state;
    let local_velocity = Quat::from_rotation_y(-input.yaw) * motion.velocity;
    state.tick(&weapon.deviation, dt, -local_velocity.z, local_velocity.x, motion.grounded);
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
    feedback.launching = launching;
    // The detonator's press animates like a shot.
    if released || launched || fired == Some(Fired::Detonate) {
        feedback.shots_fired = feedback.shots_fired.wrapping_add(1);
    }
    let Some(Fired::Launch { soft, .. }) = fired else {
        return;
    };
    let view = Quat::from_euler(EulerRot::YXZ, input.yaw, input.pitch, 0.0);
    let direction = spread_direction(view * Vec3::NEG_Z, cone, (fastrand::f32(), fastrand::f32()));
    let eye = predicted.map_or(motion, |p| p.motion()).eye_position();
    let origin = eye + view * Vec3::from(weapon.fire.start_offset);
    let pellets = weapon.projectiles_per_shot.max(1);
    for pellet in 0..pellets {
        let direction = match pellets {
            1 => direction,
            _ => spread_direction(direction, weapon.pellet_spread, (fastrand::f32(), fastrand::f32())),
        };
        shots.write(LocalShot {
            origin,
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
) -> Option<Entity> {
    spawn_tracer_from(commands, assets, origin, origin, direction, weapon, ignore, None)
}

/// Meters over which a tracer drawn from the muzzle closes in on the bullet's path.
const TRACER_CONVERGE: f32 = 15.0;

/// A tracer for a bullet from `origin`, drawn from `drawn` at first.
#[allow(clippy::too_many_arguments)]
fn spawn_tracer_from(
    commands: &mut Commands,
    assets: &EffectAssets,
    origin: Vec3,
    drawn: Vec3,
    direction: Vec3,
    weapon: &WeaponDesc,
    ignore: Option<Entity>,
    shooter: Option<Entity>,
) -> Option<Entity> {
    // The knife's "shot" is a short reach (`game_shared::weapons`), not a bullet to draw.
    if weapon.projectile.velocity <= 0.0 || weapon.is_melee() {
        return None;
    }
    let tracer = commands.spawn((
        Tracer {
            position: origin,
            offset: drawn - origin,
            travelled: 0.0,
            shooter,
            velocity: direction * weapon.projectile.velocity,
            gravity: weapon.projectile.gravity,
            life: weapon.projectile.time_to_live.min(3.0),
            ignore,
            material: weapon.projectile.material,
            detonates: weapon.projectile.goes_off(),
        },
        Mesh3d(assets.tracer.clone()),
        MeshMaterial3d(assets.tracer_material.clone()),
        Transform::from_translation(drawn).looking_to(direction, Vec3::Y),
        bevy::light::NotShadowCaster,
    ));
    Some(tracer.id())
}

/// A tracer of our own shot (for `BF2_HITREG_LOG`).
#[derive(Component)]
struct LocalTracer;

#[allow(clippy::too_many_arguments)]
fn spawn_local_shots(
    mut commands: Commands,
    mut shots: MessageReader<LocalShot>,
    assets: Res<EffectAssets>,
    camera: Single<&Transform, With<PlayerCamera>>,
    library: Option<Res<EffectLibrary>>,
    zoom: Res<Zoom>,
    third_person: Res<ThirdPerson>,
    soldier: Query<(Entity, &SoldierMotion, &Hitbox), With<LocalSoldier>>,
    weapon_parts: Query<(&WeaponPart, &GlobalTransform)>,
    mut effects: MessageWriter<SpawnEffect>,
) {
    let soldier = soldier.single().ok();
    let library = library.as_deref();
    for shot in shots.read() {
        // Drawn from roughly where the muzzle is, just below and right of the eye; the bullet
        // itself flies from the eye, as on the server.
        let origin = camera.translation + camera.rotation * Vec3::new(0.12, -0.1, -0.4);
        let hitbox = soldier.map(|(_, _, hitbox)| hitbox.entity);
        // Grenades, rockets and charges are drawn as themselves (`render::projectiles`).
        if !shot.weapon.projectile.is_object()
            && let Some(tracer) = spawn_tracer_from(
                &mut commands,
                &assets,
                shot.origin,
                origin,
                shot.direction,
                &shot.weapon,
                hitbox,
                soldier.map(|(entity, ..)| entity),
            )
        {
            commands.entity(tracer).insert(LocalTracer);
        }
        // Shotguns fire several pellets but flash once.
        if shot.pellet > 0 {
            continue;
        }
        let Some((muzzle, offset)) = library.and_then(|l| l.muzzle(&shot.weapon.name)) else {
            continue;
        };
        if third_person.0 {
            if let Some((_, motion, _)) = soldier {
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
    view: Res<ViewTick>,
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
        if weapon.fire.kind == FireKind::Gun {
            commands.entity(shot.soldier).try_insert(ShotSeen(view.latest_tick));
        }
        // Drawn from the gun rather than the eye (the bullet flies from the eye).
        let gun = shot.origin - Vec3::Y * 0.15;
        if !weapon.projectile.is_object() {
            spawn_tracer_from(
                &mut commands,
                &assets,
                shot.origin,
                gun,
                shot.direction,
                weapon,
                hitbox.map(|h| h.entity),
                Some(shot.soldier),
            );
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
    for kill in kills.read() {
        let victim = players.get(kill.victim).map_or_else(|_| "?".into(), |p| p.name.clone());
        let weapon = weapon_display_name(&kill.weapon);
        let headshot = if kill.headshot { " (headshot)" } else { "" };
        // Kills are announced when a soldier goes down; without a killer (falls, wrecks) he
        // may still be revived. The killer comes as a name (not an entity): it always gets
        // here, even for a killer this client hasn't been sent the entity of.
        let line = match &kill.killer_name {
            Some(killer) if *killer != victim => format!("{killer}  [{weapon}{headshot}]  {victim}"),
            Some(_) => format!("[{weapon}]  {victim}"),
            None => format!("{victim} is down"),
        };
        feedback.kills.push_back((line, time.elapsed_secs_f64()));
        while feedback.kills.len() > 6 {
            feedback.kills.pop_front();
        }
        if local.single().is_ok_and(|me| me == kill.victim) {
            feedback.killed_by = Some(kill.killer_name.clone().unwrap_or_else(|| "the environment".into()));
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

/// Where a soldier's hit zones are as drawn now: the pose the server judges the shots
/// against that were fired looking at this moment (see `ViewTick`).
pub(crate) fn drawn_body_pose(
    render: &SoldierRender,
    inventory: Option<&Inventory>,
    shot: Option<&ShotSeen>,
    seated: bool,
    view: &ViewTick,
) -> BodyPose {
    let on_foot = render.grounded && !render.climbing && !render.riding && !render.parachute && !render.swimming;
    let anim = (on_foot && !seated).then(|| AnimState {
        stance: render.stance,
        velocity: skeleton::local_velocity(render.yaw, render.velocity),
        stride: render.stride,
        clock: skeleton::clock(view.seconds),
        reload: inventory.and_then(|i| skeleton::reload_elapsed(i.reloading, i.reload_started, view.seconds)),
        fire: shot.and_then(|s| s.since(view.seconds)),
        weapon: inventory.map_or(0, |i| i.active),
    });
    BodyPose {
        position: render.position,
        yaw: render.yaw,
        stance: if seated { game_shared::soldier::Stance::Crouching } else { render.stance },
        anim,
    }
}

/// A soldier's animation states as drawn over the last frames (oldest first, each with the
/// seconds since the one before), to pose his hit zones mid-crossfade as the server does
/// (`game_shared::skeleton::Blend`).
#[derive(Component, Default)]
pub(crate) struct DrawnAnim(VecDeque<(AnimState, f32)>);

fn record_drawn_anim(
    mut commands: Commands,
    time: Res<Time>,
    view: Res<ViewTick>,
    mut soldiers: Query<
        (Entity, &SoldierRender, Option<&Inventory>, Option<&ShotSeen>, Has<Seated>, Option<&mut DrawnAnim>),
        With<Soldier>,
    >,
) {
    let dt = time.delta_secs();
    for (entity, render, inventory, shot, seated, history) in &mut soldiers {
        let Some(mut history) = history else {
            commands.entity(entity).insert(DrawnAnim::default());
            continue;
        };
        match drawn_body_pose(render, inventory, shot, seated, &view).anim {
            Some(anim) => {
                history.0.push_back((anim, dt));
                // As far back as the longest crossfade reaches.
                while history.0.iter().skip(1).map(|(_, dt)| dt).sum::<f32>() > skeleton::fade::LONGEST + 0.05 {
                    history.0.pop_front();
                }
            }
            None => history.0.clear(),
        }
    }
}

/// Soldiers bullets can hit, as drawn: for the tracers, the hit zone overlay and checks.
#[derive(bevy::ecs::system::SystemParam)]
pub(crate) struct DrawnTargets<'w, 's> {
    soldiers: Query<
        'w,
        's,
        (
            Entity,
            &'static SoldierRender,
            &'static Loadout,
            Option<&'static Inventory>,
            Option<&'static Seated>,
            Option<&'static DrawnAnim>,
            Option<&'static ShotSeen>,
        ),
        (With<Soldier>, Without<Downed>),
    >,
    vehicles: Query<'w, 's, &'static VehicleData>,
    armory: Res<'w, Armory>,
    rigs: Res<'w, HitRigs>,
    view: Res<'w, ViewTick>,
}

/// A soldier's hit zones as drawn this frame; its bones are posed once, for the first ray
/// passing near.
pub(crate) struct DrawnTarget<'a> {
    pub entity: Entity,
    pub pose: BodyPose,
    pub zones: &'a [game_data::HitZone],
    loadout: &'a Loadout,
    recent: Option<&'a DrawnAnim>,
    bones: std::cell::OnceCell<Option<skeleton::Pose>>,
}

impl DrawnTargets<'_, '_> {
    /// Every soldier the server lets bullets hit: not critically wounded, not inside a closed
    /// vehicle.
    pub(crate) fn collect(&self) -> Vec<DrawnTarget<'_>> {
        self.soldiers
            .iter()
            .filter(|(.., seated, _, _)| {
                seated.is_none_or(|s| {
                    self.vehicles
                        .get(s.vehicle)
                        .is_ok_and(|v| v.0.desc.seats.get(s.seat as usize).is_some_and(|seat| seat.open))
                })
            })
            .map(|(entity, render, loadout, inventory, seated, recent, shot)| DrawnTarget {
                entity,
                pose: drawn_body_pose(render, inventory, shot, seated.is_some(), &self.view),
                zones: self.armory.hit_zones(&loadout.kit),
                loadout,
                recent,
                bones: std::cell::OnceCell::new(),
            })
            .collect()
    }

    /// The bones of a target, posed (`None`: its zones keep their stance's pose).
    pub(crate) fn bones<'a>(&self, target: &'a DrawnTarget) -> Option<&'a skeleton::Pose> {
        target
            .bones
            .get_or_init(|| {
                let anim = target.pose.anim?;
                match target.recent.map(|r| r.0.iter().copied().collect::<Vec<_>>()).filter(|r| !r.is_empty()) {
                    Some(recent) => self.rigs.pose_recent(&target.loadout.kit, target.loadout, &self.armory, &recent),
                    None => self.rigs.pose(&target.loadout.kit, target.loadout, &self.armory, &anim),
                }
            })
            .as_ref()
    }

    /// A zone's capsule ends.
    pub(crate) fn capsule(&self, target: &DrawnTarget, zone: &game_data::HitZone) -> (Vec3, Vec3) {
        target.pose.posed_capsule(zone, self.bones(target))
    }

    /// The nearest zone of a target a ray meets.
    pub(crate) fn ray(&self, target: &DrawnTarget, origin: Vec3, direction: Vec3, max: f32) -> Option<ZoneHit> {
        if !target.pose.near(origin, direction, max) {
            return None;
        }
        let bones = self.bones(target).copied();
        target.pose.ray_posed(target.zones, || bones, origin, direction, max)
    }
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
    mut tracers: Query<(Entity, &mut Tracer, &mut Transform, Has<LocalTracer>)>,
    drawn: DrawnTargets,
) {
    let dt = time.delta_secs();
    // Bullets meet the world and vehicles as on the server, and soldiers in their hit zones
    // as drawn (the pose the server judges them against), not the soldiers' movement capsules.
    let layers = SpatialQueryFilter::from_mask([GameLayer::World, GameLayer::Vehicle]);
    let targets = if tracers.is_empty() { Vec::new() } else { drawn.collect() };
    for (entity, mut tracer, mut transform, local) in &mut tracers {
        let filter = match tracer.ignore {
            Some(hitbox) => layers.clone().with_excluded_entities([hitbox]),
            None => layers.clone(),
        };
        tracer.life -= dt;
        let start = tracer.velocity;
        let gravity = tracer.gravity;
        tracer.velocity += Vec3::NEG_Y * game_shared::physics::gravity(gravity) * dt;
        let step = (start + tracer.velocity) * 0.5 * dt;
        let from = tracer.position;
        let direction = step.normalize_or_zero();
        let world = Dir3::new(step)
            .ok()
            .and_then(|dir| spatial.cast_ray(from, dir, step.length(), true, &filter));
        let soldier = targets
            .iter()
            .filter(|t| Some(t.entity) != tracer.shooter)
            .filter_map(|t| Some((t, drawn.ray(t, from, direction, step.length())?)))
            .min_by(|a, b| a.1.distance.total_cmp(&b.1.distance))
            .filter(|(_, hit)| world.is_none_or(|w| hit.distance < w.distance));
        if let Some((target, hit)) = soldier {
            // A predicted hit: the tracer stops in the body. The blood comes from the server
            // (`SoldierImpact`), where it really hit.
            if local && game_shared::hitzones::hitreg_log() {
                info!(
                    "hitreg tracer: hit {:?} (material {}) zone {} at {:.3}",
                    target.entity,
                    hit.material,
                    target.zones.get(hit.zone).map_or("?", |z| z.bone.as_str()),
                    hit.point
                );
            }
            commands.entity(entity).despawn();
            continue;
        }
        if let Some(hit) = world {
            commands.entity(entity).despawn();
            if tracer.detonates {
                continue;
            }
            let point = from + direction * hit.distance;
            let surface = surfaces.material(hit.entity, point);
            if local && game_shared::hitzones::hitreg_log() {
                info!("hitreg tracer: hit {:?} (material {surface}) at {point:.3}", hit.entity);
            }
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
            spawn_impact(&mut commands, &assets, library.as_deref(), &mut effects, tracer.material, surface, point, hit.normal);
            continue;
        }
        if tracer.life <= 0.0 {
            if local && game_shared::hitzones::hitreg_log() {
                info!("hitreg tracer: nothing hit");
            }
            commands.entity(entity).despawn();
            continue;
        }
        tracer.position += step;
        tracer.travelled += step.length();
        let converge = (1.0 - tracer.travelled / TRACER_CONVERGE).max(0.0);
        transform.translation = tracer.position + tracer.offset * converge;
        transform.look_to(step, Vec3::Y);
    }
}

/// The impact effect of a projectile material on a surface material.
#[allow(clippy::too_many_arguments)]
fn spawn_impact(
    commands: &mut Commands,
    assets: &EffectAssets,
    library: Option<&EffectLibrary>,
    effects: &mut MessageWriter<SpawnEffect>,
    projectile: u32,
    surface: u32,
    point: Vec3,
    normal: Vec3,
) {
    match library.map(|l| (l.impact(projectile, surface), l.impacts.effects.is_empty())) {
        Some((Some(name), _)) => {
            effects.write(SpawnEffect::new(name, point).with_up(normal));
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
}

/// The blood where the server says a bullet hit a soldier.
fn receive_soldier_impacts(
    mut commands: Commands,
    mut impacts: MessageReader<SoldierImpact>,
    assets: Res<EffectAssets>,
    library: Option<Res<EffectLibrary>>,
    mut effects: MessageWriter<SpawnEffect>,
) {
    for impact in impacts.read() {
        spawn_impact(
            &mut commands,
            &assets,
            library.as_deref(),
            &mut effects,
            impact.projectile,
            impact.body_part,
            impact.point,
            impact.normal,
        );
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
    // Only when it changes: a changed projection makes Bevy rebuild the camera's frustum and
    // light clusters.
    let fov = settings.field_of_view.to_radians() * feedback.zoom;
    if let Projection::Perspective(perspective) = &**camera
        && perspective.fov != fov
        && let Projection::Perspective(perspective) = camera.as_mut()
    {
        perspective.fov = fov;
    }
    feedback.hit_marker = (feedback.hit_marker - time.delta_secs()).max(0.0);
}
