//! What bots know about vehicles: what each one is for ([`Role`]), how its guns are used
//! ([`GunInfo`]), who is on the way to which seat ([`VehicleClaims`], so bots don't all run
//! for the same jeep), and aiming with lead.
//!
//! Like BF2's `Unit` and `Armament` AI plugins, a vehicle's role follows from its data: jets,
//! helicopters with rockets (attack) or without (transport), boats, stationary weapons, and
//! on land tanks (a tracked hull with a main gun), APCs (a gun on the driver's seat and room
//! for a squad), mobile anti-aircraft (heat seekers on the driver's seat) and transports.

use bevy::{platform::collections::HashMap, prelude::*};
use game_data::{DriveKind, Guidance, VehicleCategory, VehicleDesc, WeaponDesc};
use game_shared::vehicle::{Vehicle, VehicleData, VehicleModel};

use super::AiData;
use crate::nav::vehicle::{DriveSpec, NavClass};

/// What a vehicle is for, as bots see it.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Role {
    /// Jeeps and other cars: a driver, maybe a gunner, passengers.
    Transport,
    /// A gun on the driver's seat and room for a squad.
    Apc,
    /// A tracked hull with a main gun.
    Tank,
    /// Heat seekers on the driver's seat (Tunguska, M6).
    AntiAir,
    Boat,
    TransportHeli,
    AttackHeli,
    Jet,
    /// Guns and launchers fixed to the ground.
    Stationary,
}

impl Role {
    pub fn flies(self) -> bool {
        matches!(self, Role::TransportHeli | Role::AttackHeli | Role::Jet)
    }

    /// Stays mounted and fights at the objective instead of dropping its riders off.
    pub fn fights(self) -> bool {
        matches!(self, Role::Tank | Role::Apc | Role::AntiAir | Role::AttackHeli | Role::Jet)
    }

    pub fn name(self) -> &'static str {
        match self {
            Role::Transport => "transport",
            Role::Apc => "APC",
            Role::Tank => "tank",
            Role::AntiAir => "AA",
            Role::Boat => "boat",
            Role::TransportHeli => "transport helicopter",
            Role::AttackHeli => "attack helicopter",
            Role::Jet => "jet",
            Role::Stationary => "stationary",
        }
    }
}

/// How a gun is used.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum GunKind {
    /// Tank guns: against vehicles and groups.
    Cannon,
    /// APC guns: vehicles and infantry.
    AutoCannon,
    /// Against infantry and light vehicles.
    MachineGun,
    /// Wire or TV guided missiles: kept on the target until they hit.
    Missile,
    /// Unguided rocket pods.
    Rockets,
    /// Heat seekers: locked on aircraft first.
    AntiAir,
}

impl GunKind {
    /// Worth firing at armour.
    pub fn anti_armor(self) -> bool {
        matches!(self, GunKind::Cannon | GunKind::AutoCannon | GunKind::Missile | GunKind::Rockets)
    }

    /// Worth firing at soldiers.
    pub fn anti_infantry(self) -> bool {
        matches!(self, GunKind::MachineGun | GunKind::AutoCannon | GunKind::Cannon | GunKind::Rockets)
    }
}

/// One of a vehicle's guns.
#[derive(Clone, Debug)]
pub struct GunInfo {
    /// Index into the vehicle's guns.
    pub index: usize,
    pub seat: u8,
    /// Fired with the secondary button.
    pub alt_fire: bool,
    /// The weapon key that selects it among the guns on its seat's trigger.
    pub pick: u8,
    pub kind: GunKind,
    /// Muzzle speed (m/s) and the gravity its projectile falls with (m/s²).
    pub speed: f32,
    pub gravity: f32,
    /// Meters.
    pub range: f32,
    /// Rounds per second.
    pub rate: f32,
    /// A magazine of one: a fresh pull per shot.
    pub single: bool,
}

/// What bots need to know about a vehicle template.
#[derive(Clone, Debug)]
pub struct VehicleProfile {
    pub role: Role,
    pub category: VehicleCategory,
    /// How it is driven for path planning; `None` for aircraft and stationary weapons.
    pub spec: Option<DriveSpec>,
    pub seats: usize,
    /// Seats with a gun or a turret of their own, the driver's excluded.
    pub gunner_seats: Vec<u8>,
    pub guns: Vec<GunInfo>,
    pub half_width: f32,
    pub length: f32,
    pub top_speed: f32,
    pub reverse_speed: f32,
    pub tracked: bool,
    /// Wheeled: the steering joint's reach, radians; and the distance between the axles.
    pub max_steer: f32,
    pub wheelbase: f32,
    /// Tracked: turn rate at full steering, radians per second.
    pub turn_rate: f32,
    pub hit_points: f32,
}

impl VehicleProfile {
    pub fn new(desc: &VehicleDesc, data: &AiData) -> Self {
        let [min, max] = desc.physics.bounds.map(Vec3::from_array);
        let half_width = ((max.x - min.x) * 0.5).max(0.5);
        let length = (max.z - min.z).max(1.0);
        let guns = guns(desc, data);
        let gunner_seats: Vec<u8> = (1..desc.seats.len() as u8)
            .filter(|&seat| guns.iter().any(|g| g.seat == seat) || seat_aims(desc, seat as usize))
            .collect();
        let driver_gun = |kind: fn(GunKind) -> bool| guns.iter().any(|g| g.seat == 0 && kind(g.kind));
        let floats = !desc.floaters.is_empty();
        let tracked = desc.drive == DriveKind::Tracked;
        let role = match desc.category {
            VehicleCategory::Air => Role::Jet,
            VehicleCategory::Helicopter => {
                let rockets = guns.iter().any(|g| matches!(g.kind, GunKind::Rockets | GunKind::Missile));
                if rockets && desc.seats.len() <= 3 { Role::AttackHeli } else { Role::TransportHeli }
            }
            VehicleCategory::Sea => Role::Boat,
            VehicleCategory::Stationary => Role::Stationary,
            VehicleCategory::Land => {
                if driver_gun(|k| k == GunKind::AntiAir) && !driver_gun(|k| k == GunKind::Cannon) {
                    Role::AntiAir
                } else if tracked && driver_gun(|k| k == GunKind::Cannon) {
                    Role::Tank
                } else if driver_gun(|k| k.anti_armor()) {
                    if desc.seats.len() >= 3 { Role::Apc } else { Role::Tank }
                } else {
                    Role::Transport
                }
            }
        };
        let spec = match desc.category {
            VehicleCategory::Land => Some(DriveSpec {
                class: match (floats, tracked) {
                    (true, _) => NavClass::Amphibious,
                    (false, true) => NavClass::Tracked,
                    (false, false) => NavClass::Wheeled,
                },
                half_width,
                depth: if tracked { 1.1 } else { 0.7 },
            }),
            VehicleCategory::Sea => Some(DriveSpec {
                class: NavClass::Boat,
                half_width,
                depth: 1.0,
            }),
            _ => None,
        };
        // Steering: the widest steering joint of a wheel.
        let max_steer = desc
            .parts
            .iter()
            .filter_map(|p| p.joint.as_ref())
            .filter_map(|j| j.axes.iter().find(|a| a.input == Some(game_data::JointInput::Steer)))
            .map(|a| a.max.abs().max(a.min.abs()).to_radians())
            .fold(0.0f32, f32::max);
        let axles: Vec<f32> = desc.wheels.iter().filter(|w| w.contact).map(|w| w.position[2]).collect();
        let wheelbase = match (
            axles.iter().copied().reduce(f32::min),
            axles.iter().copied().reduce(f32::max),
        ) {
            (Some(a), Some(b)) if b - a > 0.5 => b - a,
            _ => length * 0.6,
        };
        Self {
            role,
            category: desc.category,
            spec,
            seats: desc.seats.len(),
            gunner_seats,
            guns,
            half_width,
            length,
            top_speed: desc.engine.top_speed.max(1.0),
            reverse_speed: desc.engine.reverse_speed.max(1.0),
            tracked,
            max_steer: if max_steer > 0.05 { max_steer } else { 0.5 },
            wheelbase,
            turn_rate: desc.engine.turn_rate.max(0.3),
            hit_points: desc.hit_points,
        }
    }

    /// The guns a seat fires.
    pub fn seat_guns(&self, seat: u8) -> impl Iterator<Item = &GunInfo> + '_ {
        self.guns.iter().filter(move |g| g.seat == seat)
    }
}

/// Whether a seat turns a joint towards its aim.
fn seat_aims(desc: &VehicleDesc, seat: usize) -> bool {
    use game_data::JointInput;
    desc.parts.iter().filter_map(|p| p.joint.as_ref()).any(|j| {
        j.seat as usize == seat && j.axes.iter().any(|a| matches!(a.input, Some(JointInput::AimYaw | JointInput::AimPitch)))
    })
}

/// A vehicle's guns, classified.
fn guns(desc: &VehicleDesc, data: &AiData) -> Vec<GunInfo> {
    let mut out = Vec::with_capacity(desc.weapons.len());
    for (index, gun) in desc.weapons.iter().enumerate() {
        let w: &WeaponDesc = &gun.weapon;
        let p = &w.projectile;
        let kind = if w.fire.lock.is_some() || w.fire.guidance == Guidance::Heat {
            GunKind::AntiAir
        } else if w.fire.guidance == Guidance::Wire {
            GunKind::Missile
        } else if p.explodes() && w.rounds_per_minute <= 90.0 {
            GunKind::Cannon
        } else if p.explodes() && w.magazine_size > 0 && w.magazine_size <= 40 {
            GunKind::Rockets
        } else if p.explodes() || p.damage >= 60.0 {
            GunKind::AutoCannon
        } else {
            GunKind::MachineGun
        };
        let template = data.weapons.weapons.get(&w.name).map(|t| t.max_range);
        let speed = p.velocity.max(p.max_speed).max(1.0);
        let reach = if p.time_to_live > 0.0 { speed * p.time_to_live } else { f32::MAX };
        let default = match kind {
            GunKind::Cannon => 450.0,
            GunKind::AutoCannon => 350.0,
            GunKind::MachineGun => 250.0,
            GunKind::Missile => 600.0,
            GunKind::Rockets => 350.0,
            GunKind::AntiAir => w.fire.lock.map_or(800.0, |l| l.range),
        };
        let range = template.filter(|r| *r > 20.0).unwrap_or(default).min(reach);
        let pick = desc.weapons[..index]
            .iter()
            .filter(|o| o.seat == gun.seat && o.alt_fire == gun.alt_fire)
            .count() as u8;
        out.push(GunInfo {
            index,
            seat: gun.seat as u8,
            alt_fire: gun.alt_fire,
            pick,
            kind,
            speed: p.velocity.max(1.0),
            gravity: game_shared::projectile::GRAVITY * p.gravity,
            range,
            rate: w.rounds_per_minute.max(1.0) / 60.0,
            single: w.magazine_size == 1,
        });
    }
    out
}

/// Profiles by template, filled in as vehicles appear.
#[derive(Resource, Default)]
pub struct VehicleProfiles(pub HashMap<String, std::sync::Arc<VehicleProfile>>);

impl VehicleProfiles {
    pub fn get(&self, template: &str) -> Option<&VehicleProfile> {
        self.0.get(template).map(|p| &**p)
    }
}

/// Profiles every new vehicle template.
pub fn update_profiles(
    mut profiles: ResMut<VehicleProfiles>,
    data: Res<AiData>,
    vehicles: Query<(&Vehicle, &VehicleData), Added<VehicleData>>,
) {
    if data.is_changed() {
        profiles.0.clear();
    }
    for (vehicle, model) in &vehicles {
        if !profiles.0.contains_key(&vehicle.template) {
            let profile = VehicleProfile::new(&model.0.desc, &data);
            info!(
                "ai: {} is a {} ({} seats, gunners {:?}, guns {:?})",
                vehicle.template,
                profile.role.name(),
                profile.seats,
                profile.gunner_seats,
                profile.guns.iter().map(|g| (g.seat, g.kind, g.range as u32)).collect::<Vec<_>>()
            );
            profiles.0.insert(vehicle.template.clone(), std::sync::Arc::new(profile));
        }
    }
}

/// Which seat a bot is going for.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SeatWish {
    Driver,
    Gunner,
    Passenger,
}

/// Bots on their way to a vehicle, so others go elsewhere.
#[derive(Resource, Default)]
pub struct VehicleClaims {
    /// Per vehicle: who, for which seat, until when (seconds of [`Self::clock`]).
    claims: HashMap<Entity, Vec<(Entity, SeatWish, f32)>>,
    clock: f32,
    /// Vehicles bots fired at, by the firing team, and when: a vehicle wrecked soon after
    /// counts as their kill.
    pub engaged: HashMap<Entity, (game_shared::protocol::Team, f32)>,
}

/// How long a claim holds without being renewed, seconds.
const CLAIM_SECONDS: f32 = 3.0;

impl VehicleClaims {
    pub fn tick(&mut self, dt: f32) {
        self.clock += dt;
        let clock = self.clock;
        self.claims.retain(|_, list| {
            list.retain(|(_, _, until)| *until > clock);
            !list.is_empty()
        });
        self.engaged.retain(|_, (_, at)| clock - *at < 10.0);
    }

    pub fn now(&self) -> f32 {
        self.clock
    }

    /// Claims (or renews the claim of) a seat.
    pub fn claim(&mut self, vehicle: Entity, player: Entity, wish: SeatWish) {
        self.release(player);
        self.claims
            .entry(vehicle)
            .or_default()
            .push((player, wish, self.clock + CLAIM_SECONDS));
    }

    pub fn release(&mut self, player: Entity) {
        for list in self.claims.values_mut() {
            list.retain(|(p, ..)| *p != player);
        }
    }

    /// Claims others hold on a vehicle.
    pub fn others(&self, vehicle: Entity, player: Entity) -> impl Iterator<Item = SeatWish> + '_ {
        self.claims
            .get(&vehicle)
            .into_iter()
            .flatten()
            .filter(move |(p, ..)| *p != player)
            .map(|(_, wish, _)| *wish)
    }

    /// Bots on their way into a vehicle.
    pub fn pending(&self, vehicle: Entity) -> usize {
        self.claims.get(&vehicle).map_or(0, |l| l.len())
    }

    /// A bot of `team` fired at a vehicle.
    pub fn engage(&mut self, vehicle: Entity, team: game_shared::protocol::Team) {
        self.engaged.insert(vehicle, (team, self.clock));
    }
}

/// Where to aim to hit something at `target` moving at `velocity` with a projectile of
/// `speed` falling with `gravity`, from `from`: ahead of it by its flight time, and above
/// it by its drop.
pub fn lead(from: Vec3, target: Vec3, velocity: Vec3, speed: f32, gravity: f32) -> Vec3 {
    let mut point = target;
    for _ in 0..3 {
        let time = from.distance(point) / speed.max(1.0);
        point = target + velocity * time;
    }
    let time = from.distance(point) / speed.max(1.0);
    point + Vec3::Y * (0.5 * gravity * time * time)
}

/// View yaw and pitch looking along `v` (yaw 0 = -Z, positive turns left).
pub fn angles(v: Vec3) -> (f32, f32) {
    ((-v.x).atan2(-v.z), v.y.atan2(Vec2::new(v.x, v.z).length()))
}

/// Where a vehicle's seats look from and its guns point.
pub struct Mounted<'a> {
    pub model: &'a VehicleModel,
    pub transform: Transform,
}

impl<'a> Mounted<'a> {
    pub fn new(model: &'a VehicleModel, transform: Transform) -> Self {
        Self { model, transform }
    }

    /// Where a seat looks from, world space (with turrets at rest: close enough to look
    /// around from).
    pub fn eye(&self, seat: usize) -> Vec3 {
        self.transform.transform_point(self.model.eye(&self.model.rest_hull, seat))
    }

    /// A gun's muzzle, world space, facing where it fires (-Z), at the current joint
    /// angles.
    pub fn muzzle(&self, joints: &[[f32; 3]], gun: usize) -> Transform {
        let parts = self.model.part_transforms(joints);
        self.transform * self.model.muzzle(&parts, gun)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn leads_moving_targets() {
        // 100 m away crossing at 10 m/s, a 500 m/s round: 0.2 s ahead, 2 m.
        let aim = lead(Vec3::ZERO, Vec3::new(0.0, 0.0, -100.0), Vec3::new(10.0, 0.0, 0.0), 500.0, 0.0);
        assert!((aim.x - 2.0).abs() < 0.05, "{aim}");
        // Falling rounds aim high.
        let aim = lead(Vec3::ZERO, Vec3::new(0.0, 0.0, -100.0), Vec3::ZERO, 100.0, 9.81);
        assert!((aim.y - 4.9).abs() < 0.1, "{aim}");
        let (yaw, pitch) = angles(Vec3::new(-1.0, 0.0, 0.0));
        assert!((yaw - std::f32::consts::FRAC_PI_2).abs() < 1e-5 && pitch.abs() < 1e-6);
    }

    #[test]
    fn claims_expire() {
        let mut claims = VehicleClaims::default();
        let mut world = World::new();
        let (jeep, a, b) = (world.spawn_empty().id(), world.spawn_empty().id(), world.spawn_empty().id());
        claims.claim(jeep, a, SeatWish::Driver);
        assert_eq!(claims.others(jeep, b).collect::<Vec<_>>(), [SeatWish::Driver]);
        assert_eq!(claims.others(jeep, a).count(), 0);
        claims.tick(CLAIM_SECONDS + 0.1);
        assert_eq!(claims.pending(jeep), 0);
    }
}
