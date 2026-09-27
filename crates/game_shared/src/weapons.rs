//! Kits, weapons and the rules for firing them, shared by server (authority) and client
//! (prediction of timing, spread for the crosshair, recoil).

use std::{collections::HashMap, sync::Arc};

use bevy::prelude::*;
use game_data::{
    DeviationDesc, FireKind, FireMode, Impact, KitDesc, LevelDesc, ProjectileDesc, RecoilDesc,
    WeaponDesc, WeaponSounds,
};
use serde::{Deserialize, Serialize};

use crate::{config::GamePaths, level::LoadedLevel, soldier::Stance};

pub struct WeaponsPlugin;

impl Plugin for WeaponsPlugin {
    fn build(&self, app: &mut App) {
        app.init_resource::<Armory>().add_systems(
            Update,
            load_armory.run_if(resource_exists_and_changed::<LoadedLevel>),
        );
    }
}

/// Kits and weapons available in the current level, by lowercase name.
#[derive(Resource, Default, Clone)]
pub struct Armory {
    pub kits: HashMap<String, KitDesc>,
    pub weapons: HashMap<String, Arc<WeaponDesc>>,
    /// Kit names of team 1 and team 2, by slot.
    pub team_kits: [Vec<String>; 2],
}

impl Armory {
    pub fn weapon(&self, name: &str) -> Option<&Arc<WeaponDesc>> {
        self.weapons.get(name)
    }

    /// The kit a team's slot uses, falling back to the first kit, then the test kit.
    pub fn kit_for(&self, team: usize, slot: usize) -> Option<&KitDesc> {
        let kits = self.team_kits.get(team)?;
        let name = kits.get(slot).or_else(|| kits.first())?;
        self.kits.get(name)
    }
}

fn load_armory(level: Res<LoadedLevel>, paths: Res<GamePaths>, mut armory: ResMut<Armory>) {
    *armory = build_armory(&level.desc, &paths);
    info!(
        "armory: {} kits, {} weapons",
        armory.kits.len(),
        armory.weapons.len()
    );
}

fn build_armory(level: &LevelDesc, paths: &GamePaths) -> Armory {
    let mut armory = Armory::default();
    for (team, desc) in level.teams.iter().take(2).enumerate() {
        for slot in &desc.kits {
            armory.team_kits[team].push(slot.kit.clone());
            if armory.kits.contains_key(&slot.kit) {
                continue;
            }
            let path = paths.imported.join("kits").join(format!("{}.ron", slot.kit));
            match game_data::read_ron::<KitDesc>(&path) {
                Ok(kit) => {
                    for weapon in &kit.weapons {
                        if armory.weapons.contains_key(weapon) {
                            continue;
                        }
                        let path = paths.imported.join("weapons").join(format!("{weapon}.ron"));
                        match game_data::read_ron::<WeaponDesc>(&path) {
                            Ok(desc) => {
                                armory.weapons.insert(weapon.clone(), Arc::new(desc));
                            }
                            Err(err) => warn!("{err}"),
                        }
                    }
                    armory.kits.insert(slot.kit.clone(), kit);
                }
                Err(err) => warn!("{err}"),
            }
        }
    }
    // Levels without imported kits (the test range) get a generic rifle kit.
    if armory.kits.is_empty() {
        let rifle = test_rifle();
        let kit = KitDesc {
            name: "test_kit".into(),
            kind: "Assault".into(),
            weapons: vec![rifle.name.clone()],
            ability_restore: 0.0,
        };
        armory.weapons.insert(rifle.name.clone(), Arc::new(rifle));
        armory.team_kits = [vec![kit.name.clone()], vec![kit.name.clone()]];
        armory.kits.insert(kit.name.clone(), kit);
    }
    armory
}

fn test_rifle() -> WeaponDesc {
    WeaponDesc {
        name: "test_rifle".into(),
        display_name: "Rifle".into(),
        slot: 3,
        mesh_1p: None,
        mesh_3p: None,
        animations_3p: None,
        animations_1p: None,
        rounds_per_minute: 700.0,
        fire_modes: vec![FireMode::Auto, FireMode::Single],
        magazine_size: 30,
        magazines: 6,
        reload_time: 3.0,
        deploy_time: 0.8,
        projectiles_per_shot: 1,
        pellet_spread: 0.0,
        shift_delay: 0.0,
        reload_amount: 0,
        fire: Default::default(),
        detonator: None,
        projectile: ProjectileDesc {
            velocity: 900.0,
            damage: 30.0,
            min_damage: 10.0,
            falloff_start: 100.0,
            falloff_end: 300.0,
            gravity: 1.0,
            time_to_live: 1.0,
            material: 38,
            ..Default::default()
        },
        deviation: DeviationDesc {
            min: 0.35,
            stand: 1.5,
            crouch: 1.3,
            prone: 1.0,
            zoom: 0.8,
            fire: [2.0, 0.2, 0.05],
            speed: [1.0, 0.2, 0.2, 0.1],
            misc: [3.0, 1.5, 0.05],
        },
        recoil: RecoilDesc {
            up: [0.2, 0.5],
            left_right: [-0.3, 0.3],
            zoom_modifier: 0.8,
        },
        zoom_factors: vec![0.0, 0.6],
        zoom: Default::default(),
        sounds: WeaponSounds::default(),
        replenish: None,
    }
}

/// What a soldier carries. Replicated once when the soldier spawns.
#[derive(Component, Serialize, Deserialize, Clone, Debug, Default)]
pub struct Loadout {
    pub kit: String,
    /// Weapon names in kit order.
    pub weapons: Vec<String>,
}

/// The weapon in hand and ammo of every weapon. Replicated.
#[derive(Component, Serialize, Deserialize, Clone, Debug, Default, PartialEq)]
pub struct Inventory {
    /// Index into [`Loadout::weapons`].
    pub active: u8,
    /// `[in magazine, spare rounds]` per weapon.
    pub ammo: Vec<[u16; 2]>,
    /// Index into the active weapon's fire modes.
    pub fire_mode: u8,
    /// Currently reloading (for animations and the HUD).
    pub reloading: bool,
}

impl Inventory {
    pub fn full(loadout: &Loadout, armory: &Armory) -> Self {
        let ammo = loadout
            .weapons
            .iter()
            .map(|name| match armory.weapon(name) {
                Some(w) if w.magazine_size > 0 => [
                    w.magazine_size as u16,
                    (w.magazine_size * w.magazines.saturating_sub(1)) as u16,
                ],
                _ => [0, 0],
            })
            .collect();
        // Start with the primary weapon (BF2 slot 3) if there is one.
        let active = loadout
            .weapons
            .iter()
            .position(|w| armory.weapon(w).is_some_and(|w| w.slot == 3))
            .unwrap_or(0) as u8;
        Self {
            active,
            ammo,
            fire_mode: 0,
            reloading: false,
        }
    }
}

/// Per-soldier weapon timers and spread accumulators. Not replicated; the server keeps the
/// authoritative copy and clients run their own for their soldier.
#[derive(Component, Clone, Debug, Default)]
pub struct WeaponState {
    /// Seconds until the next shot may fire.
    pub cooldown: f32,
    /// Seconds left in the current reload (0 = not reloading).
    pub reload: f32,
    /// Seconds left before a just-selected weapon can be used.
    pub deploy: f32,
    pub burst_left: u8,
    pub trigger_was_down: bool,
    pub alt_was_down: bool,
    pub mode_was_down: bool,
    /// Spread accumulators in degrees.
    pub fire_dev: f32,
    pub speed_dev: f32,
    pub misc_dev: f32,
    /// Thrown weapons: seconds the trigger has been held for the throw being wound up.
    pub wind_up: Option<f32>,
    /// The throw being wound up or launched is underhand (the alternative fire button).
    pub soft: bool,
    /// A released throw or a placed charge on its way out of the hand: seconds until it
    /// leaves, and seconds of its fuse already burnt.
    pub launch: Option<(f32, f32)>,
    /// C4: the detonator is in hand instead of the charges.
    pub detonator: bool,
}

/// What a weapon did this tick (see [`WeaponState::trigger`]).
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Fired {
    /// Projectiles leave now. `cooked` seconds of a grenade's fuse burnt in the hand; `soft`
    /// is the underhand throw.
    Launch { cooked: f32, soft: bool },
    /// The grenade's fuse ran out in the hand.
    InHand,
    /// The detonator was pressed: the soldier's charges go off.
    Detonate,
}

/// The buttons that work a weapon, for one tick.
#[derive(Clone, Copy, Debug, Default)]
pub struct Trigger {
    pub fire: bool,
    /// The alternative fire button (BF2 `PIAltFire`, which is also zoom): underhand throws,
    /// taking out the C4 detonator.
    pub alt: bool,
    pub reload: bool,
    /// Sprinting lowers the weapon.
    pub lowered: bool,
}

/// Seconds the C4 detonator needs between presses (its `fire.roundsPerMinute 60`).
const DETONATOR_INTERVAL: f32 = 1.0;

/// BF2 tunes spread decay per 30 Hz server frame.
const BF2_FRAME: f32 = 1.0 / 30.0;

impl WeaponState {
    /// Current spread cone in degrees.
    pub fn deviation(&self, desc: &DeviationDesc, stance: Stance, zoomed: bool) -> f32 {
        let stance_mod = match stance {
            Stance::Standing => desc.stand,
            Stance::Crouching => desc.crouch,
            Stance::Prone => desc.prone,
        };
        let zoom_mod = if zoomed { desc.zoom } else { 1.0 };
        desc.min * stance_mod * zoom_mod + self.fire_dev + self.speed_dev + self.misc_dev
    }

    /// Advances timers and decays spread. `forward`/`strafe` are speeds in m/s.
    pub fn tick(&mut self, desc: &DeviationDesc, dt: f32, forward: f32, strafe: f32, jumped: bool) {
        let frames = dt / BF2_FRAME;
        self.cooldown = (self.cooldown - dt).max(0.0);
        self.deploy = (self.deploy - dt).max(0.0);
        self.fire_dev = (self.fire_dev - desc.fire[2] * frames).max(0.0);
        let [speed_max, per_forward, per_strafe, speed_decay] = desc.speed;
        let target = (forward.abs() * per_forward + strafe.abs() * per_strafe).min(speed_max);
        self.speed_dev = if target > self.speed_dev {
            target
        } else {
            (self.speed_dev - speed_decay * frames).max(target)
        };
        if jumped {
            self.misc_dev = (self.misc_dev + desc.misc[1]).min(desc.misc[0]);
        }
        self.misc_dev = (self.misc_dev - desc.misc[2] * frames).max(0.0);
    }

    pub fn on_shot(&mut self, weapon: &WeaponDesc) {
        self.cooldown = shot_interval(weapon);
        let [max, add, _] = weapon.deviation.fire;
        self.fire_dev = (self.fire_dev + add).min(max);
    }

    /// Takes `weapon` in hand: whatever the previous one was doing stops.
    pub fn switch_to(&mut self, weapon: &WeaponDesc) {
        self.deploy = weapon.deploy_time;
        self.reload = 0.0;
        self.burst_left = 0;
        self.wind_up = None;
        self.launch = None;
        self.detonator = false;
    }

    /// Works the trigger for one tick, after [`Self::tick`]: reloading, fire modes, winding
    /// up and throwing, placing charges and the detonator. `ammo` (`[in magazine, spare]`)
    /// is used up and refilled here.
    pub fn trigger(
        &mut self,
        weapon: &WeaponDesc,
        mode: FireMode,
        ammo: &mut [u16; 2],
        input: Trigger,
        dt: f32,
    ) -> Option<Fired> {
        let pressed = input.fire && !self.trigger_was_down;
        let alt_pressed = input.alt && !self.alt_was_down;
        self.trigger_was_down = input.fire;
        self.alt_was_down = input.alt;
        let [in_mag, spare] = *ammo;
        let magazine = weapon.magazine_size as u16;
        let unlimited = magazine == 0;

        // A throw or charge on its way out leaves whatever else happens.
        if let Some((left, cooked)) = &mut self.launch {
            *left -= dt;
            if *left > 0.0 {
                return None;
            }
            let cooked = *cooked;
            return Some(self.release(weapon, ammo, cooked));
        }

        if self.reload > 0.0 {
            self.reload -= dt;
            if self.reload <= 0.0 {
                let step = if weapon.reload_amount > 0 { weapon.reload_amount as u16 } else { u16::MAX };
                let taken = magazine.saturating_sub(in_mag).min(spare).min(step);
                *ammo = [in_mag + taken, spare - taken];
                // Shell by shell until full, unless the trigger wants to fire.
                if weapon.reload_amount > 0 && ammo[0] < magazine && ammo[1] > 0 && !input.fire {
                    self.reload = weapon.reload_time;
                }
            }
            return None;
        }
        // Grenades and charges come out of the pouch by themselves; an empty gun reloads on
        // a fresh pull of the trigger (not while it is still held after the last shot, which
        // would cut a guided missile's wire).
        let auto_reload = weapon.fire.kind != FireKind::Gun;
        let wants_reload = input.reload && in_mag < magazine;
        let empty = in_mag == 0 && !unlimited && (pressed || auto_reload);
        if (wants_reload || empty) && spare > 0 && self.wind_up.is_none() {
            self.reload = weapon.reload_time;
            self.burst_left = 0;
            return None;
        }

        let ready = !input.lowered
            && self.cooldown <= 0.0
            && self.deploy <= 0.0
            && (in_mag > 0 || unlimited)
            && weapon.projectile.velocity > 0.0;
        match weapon.fire.kind {
            FireKind::Gun => {
                let wants_shot = match mode {
                    FireMode::Auto => input.fire,
                    FireMode::Single => pressed,
                    FireMode::Burst => pressed || self.burst_left > 0,
                };
                if !wants_shot || !ready {
                    if in_mag == 0 {
                        self.burst_left = 0;
                    }
                    return None;
                }
                if mode == FireMode::Burst {
                    self.burst_left = if pressed { 2 } else { self.burst_left.saturating_sub(1) };
                }
                if !unlimited {
                    ammo[0] = in_mag - 1;
                }
                self.on_shot(weapon);
                Some(Fired::Launch { cooked: 0.0, soft: false })
            }
            FireKind::Thrown => self.throw(weapon, ammo, input, pressed || alt_pressed, ready, dt),
            FireKind::Explosives => {
                // With nothing left to place, the detonator is all there is.
                let used_up = in_mag == 0 && spare == 0;
                if alt_pressed || used_up {
                    self.detonator = !self.detonator || used_up;
                }
                if self.detonator {
                    if pressed && self.cooldown <= 0.0 && self.deploy <= 0.0 {
                        self.cooldown = DETONATOR_INTERVAL;
                        return Some(Fired::Detonate);
                    }
                    return None;
                }
                if !(pressed && ready) {
                    return None;
                }
                self.soft = false;
                self.launch = Some((weapon.fire.launch_delay, 0.0));
                (weapon.fire.launch_delay <= 0.0).then(|| self.release(weapon, ammo, 0.0))
            }
        }
    }

    /// Winding up while the trigger is held (a grenade's fuse starts once wound up),
    /// throwing when it is let go.
    fn throw(
        &mut self,
        weapon: &WeaponDesc,
        ammo: &mut [u16; 2],
        input: Trigger,
        pressed: bool,
        ready: bool,
        dt: f32,
    ) -> Option<Fired> {
        let fire = &weapon.fire;
        let Some(held) = self.wind_up else {
            if pressed && ready {
                self.wind_up = Some(0.0);
                self.soft = !input.fire;
            }
            return None;
        };
        let held = held + dt;
        let cooks = cooks(&weapon.projectile);
        let cooked = if cooks { (held - fire.pull_back).max(0.0) } else { 0.0 };
        if cooks && cooked >= weapon.projectile.time_to_live {
            self.wind_up = None;
            ammo[0] = ammo[0].saturating_sub(1);
            self.cooldown = shot_interval(weapon);
            return Some(Fired::InHand);
        }
        let holding = if self.soft { input.alt } else { input.fire };
        if holding || held < fire.pull_back {
            self.wind_up = Some(held);
            return None;
        }
        self.wind_up = None;
        let delay = if self.soft { fire.launch_delay_soft } else { fire.launch_delay };
        self.launch = Some((delay, cooked));
        (delay <= 0.0).then(|| self.release(weapon, ammo, cooked))
    }

    fn release(&mut self, weapon: &WeaponDesc, ammo: &mut [u16; 2], cooked: f32) -> Fired {
        self.launch = None;
        if weapon.magazine_size > 0 {
            ammo[0] = ammo[0].saturating_sub(1);
        }
        self.cooldown = shot_interval(weapon);
        Fired::Launch { cooked, soft: self.soft }
    }
}

/// Seconds from one shot until the next can fire.
pub fn shot_interval(weapon: &WeaponDesc) -> f32 {
    (60.0 / weapon.rounds_per_minute.max(1.0)).max(weapon.shift_delay)
}

/// Grenades with a fuse cook in the hand once wound up; mines and charges don't.
pub fn cooks(projectile: &ProjectileDesc) -> bool {
    projectile.impact == Impact::Bounce && projectile.goes_off() && projectile.time_to_live < 100.0
}

/// Damage of a projectile after travelling `distance` meters.
pub fn damage_at(projectile: &ProjectileDesc, distance: f32) -> f32 {
    let (start, end) = (projectile.falloff_start, projectile.falloff_end);
    if end <= start || distance <= start {
        return projectile.damage;
    }
    let t = ((distance - start) / (end - start)).clamp(0.0, 1.0);
    projectile.damage + (projectile.min_damage.min(projectile.damage) - projectile.damage) * t
}

/// Rotates `forward` by a random offset inside a cone of `cone_degrees`.
pub fn spread_direction(forward: Vec3, cone_degrees: f32, random: (f32, f32)) -> Vec3 {
    let forward = forward.normalize_or(Vec3::NEG_Z);
    // Uniform over the cone's disc (BF2's exact distribution is unknown).
    let radius = (cone_degrees.to_radians() * 0.5) * random.0.sqrt();
    let angle = random.1 * std::f32::consts::TAU;
    let side = forward.any_orthonormal_vector();
    let up = forward.cross(side);
    (forward + (side * angle.cos() + up * angle.sin()) * radius.tan()).normalize()
}
