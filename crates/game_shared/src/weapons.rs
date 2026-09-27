//! Kits, weapons and the rules for firing them, shared by server (authority) and client
//! (prediction of timing, spread for the crosshair, recoil).

use std::{collections::HashMap, sync::Arc};

use bevy::prelude::*;
use game_data::{
    DeviationDesc, FireMode, KitDesc, LevelDesc, ProjectileDesc, RecoilDesc, WeaponDesc,
    WeaponSounds,
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
        rounds_per_minute: 700.0,
        fire_modes: vec![FireMode::Auto, FireMode::Single],
        magazine_size: 30,
        magazines: 6,
        reload_time: 3.0,
        deploy_time: 0.8,
        projectiles_per_shot: 1,
        projectile: ProjectileDesc {
            velocity: 900.0,
            damage: 30.0,
            min_damage: 10.0,
            falloff_start: 100.0,
            falloff_end: 300.0,
            gravity: 1.0,
            time_to_live: 1.0,
            material: 38,
            explosion_damage: 0.0,
            explosion_radius: 0.0,
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
        sounds: WeaponSounds::default(),
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
    /// Spread accumulators in degrees.
    pub fire_dev: f32,
    pub speed_dev: f32,
    pub misc_dev: f32,
}

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
        self.cooldown = 60.0 / weapon.rounds_per_minute.max(1.0);
        let [max, add, _] = weapon.deviation.fire;
        self.fire_dev = (self.fire_dev + add).min(max);
    }
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
