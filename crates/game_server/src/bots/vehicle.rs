//! Bots and vehicles: getting in, driving, gunning, flying and getting out, all through the
//! same use button, seat keys and [`InputFrame`]s players use.
//!
//! **Getting in** is an [`Activity::Mount`] weighed like any other (BF2's `Change`
//! behaviour): squad leaders and bots on their own take a transport, APC, tank, boat or
//! aircraft near them when their objective is far away (tanks and APCs when the squad attacks),
//! squad members get into their leader's vehicle, bots join a teammate's vehicle as gunners,
//! and bots man stationary weapons when enemies are around. Seats they are on their way to
//! are claimed ([`VehicleClaims`]), so they don't all run for the same jeep. Getting in is
//! walking to an entry point and pressing use; the seat keys move them to the seat they
//! wanted.
//!
//! **Driving** follows a path on the vehicle grid ([`crate::nav::vehicle`]) with pure pursuit:
//! steering towards a point a speed-dependent distance ahead on the path, slowing for curves,
//! steep ground, the end of the path and teammates in the way, reversing out when stuck.
//! Drivers of squad transports wait for the squad to get in. Tanks and APCs slow down or stop
//! to fight. Boats sail the water grid to the shore nearest their objective.
//!
//! **Gunners** (and drivers with guns) aim through their view: main guns and missiles at
//! vehicles and groups, machine guns at soldiers, leading moving targets by the round's flight
//! time and drop, firing when the gun (which turns at its own speed) is on target. Heat
//! seekers wait for their lock.
//!
//! **Pilots** fly helicopters with the collective, cyclic and tail rotor: over the terrain and
//! statics (the air map) at a safe height, transports to a landing spot by the objective,
//! attack helicopters in circles around it, firing rockets when their nose is on a target.
//! Jets take off, climb and patrol over the objectives, making gun and rocket runs.
//!
//! **Getting out**: at the objective (riders of transports, everyone once a transport has
//! landed), when the vehicle is badly damaged, on its roof, stuck for good, or its driver left.

use std::f32::consts::FRAC_PI_2;

use game_shared::vehicle::{GunStatus, VehicleState};

use super::*;
use crate::{
    ai::vehicles::{GunInfo, GunKind, Mounted, Role, SeatWish, VehicleClaims, VehicleProfile, angles, lead},
    nav::vehicle::{Obstacle, VehicleNavGrid, VehiclePath},
};

/// How far bots look for a vehicle to take, meters.
const VEHICLE_SEARCH: f32 = 70.0;
/// How far squad members go to get into their leader's vehicle, meters.
const BOARD_DISTANCE: f32 = 90.0;
/// Objectives closer than this are walked to (per role), meters.
const MOUNT_DISTANCE: f32 = 160.0;
/// Seconds a driver waits for his squad to get in.
const BOARD_WAIT: f32 = 14.0;
/// Seconds a bot keeps away from vehicles after leaving one (and from that one for longer).
const VEHICLE_COOLDOWN: f32 = 8.0;
const ABANDON_COOLDOWN: f32 = 60.0;
/// Below this share of its hit points a vehicle is abandoned.
const BAIL_OUT: f32 = 0.22;
/// Stuck events (decaying) after which a driver gives up on his vehicle.
const GIVE_UP_STRIKES: f32 = 4.0;
/// Cruising heights above the highest obstacle around, meters: transport and attack
/// helicopters, jets.
const HELI_HEIGHT: f32 = 35.0;
const GUNSHIP_HEIGHT: f32 = 55.0;
const JET_HEIGHT: f32 = 180.0;
/// Attack helicopters circle their objective this far out, meters.
const ORBIT_RADIUS: f32 = 170.0;

/// A soldier in a vehicle seat.
#[derive(Clone, Copy, Debug)]
pub(super) struct Crew {
    pub seat: u8,
    pub player: Entity,
    pub team: Team,
}

/// Who sits where, per vehicle, this tick.
pub(super) type Crews = bevy::platform::collections::HashMap<Entity, Vec<Crew>>;

/// What vehicle decisions need beyond the senses.
pub(super) struct VehicleCx<'a> {
    pub claims: &'a mut VehicleClaims,
    pub crews: &'a Crews,
}

/// What a bot does in its seat.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum Purpose {
    Drive,
    Gun,
    Ride,
}

/// Flight phases of a bot pilot.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Flight {
    /// On the ground: spinning up, waiting for passengers.
    Ground,
    /// Straight up to a safe height.
    Climb,
    /// To the destination.
    Cruise,
    /// Down onto the landing spot.
    Land,
    /// Circling the objective (attack helicopters, jets).
    Orbit,
}

/// Something a vehicle's gun is aimed at.
#[derive(Clone, Copy, Debug)]
struct Aim {
    entity: Entity,
    vehicle: bool,
    position: Vec3,
    velocity: Vec3,
    gun: usize,
}

/// A bot's state while it sits in a vehicle.
pub(super) struct Ride {
    vehicle: Entity,
    seat: u8,
    purpose: Purpose,
    /// Seconds seated.
    time: f32,
    /// The seat it wants, if not the one it has.
    want_seat: Option<u8>,
    /// Pressing use to get out.
    leaving: bool,
    /// Why it gets out (for the log).
    reason: &'static str,
    use_down: bool,

    goal: Option<Vec3>,
    path: Option<VehiclePath>,
    path_goal: Option<Vec3>,
    path_task: Option<Task<Option<VehiclePath>>>,
    repath: bool,
    repath_cooldown: f32,
    waypoint: usize,
    stuck: f32,
    reverse: f32,
    reverse_steer: f32,
    strikes: f32,
    last_position: Vec3,
    /// Seconds spent waiting for riders, at the destination, without a driver, upside down.
    waited: f32,
    arrived: f32,
    no_driver: f32,
    flipped: f32,
    /// Seconds without a target (stationary gunners leave when nothing happens).
    idle: f32,

    aim: Option<Aim>,
    scan: f32,
    burst: f32,
    reaction: f32,
    /// Seconds a guided missile still needs the aim.
    guiding: f32,
    yaw: f32,
    pitch: f32,

    flight: Flight,
    orbit: f32,
    last_velocity: Vec3,
    airborne: bool,
}

impl Ride {
    fn new(vehicle: Entity, seat: u8, purpose: Purpose, position: Vec3, yaw: f32) -> Self {
        Self {
            vehicle,
            seat,
            purpose,
            time: 0.0,
            want_seat: None,
            leaving: false,
            reason: "",
            use_down: false,
            goal: None,
            path: None,
            path_goal: None,
            path_task: None,
            repath: true,
            repath_cooldown: 0.0,
            waypoint: 0,
            stuck: 0.0,
            reverse: 0.0,
            reverse_steer: 0.0,
            strikes: 0.0,
            last_position: position,
            waited: 0.0,
            arrived: 0.0,
            no_driver: 0.0,
            flipped: 0.0,
            idle: 0.0,
            aim: None,
            scan: 0.0,
            burst: 0.0,
            reaction: 0.5,
            guiding: 0.0,
            yaw,
            pitch: 0.0,
            flight: Flight::Ground,
            orbit: fastrand::f32() * TAU,
            last_velocity: Vec3::ZERO,
            airborne: false,
        }
    }

    /// The part of the path still ahead, for debug views.
    pub fn remaining_path(&self) -> &[Vec3] {
        self.path
            .as_ref()
            .map_or(&[], |p| &p.points[self.waypoint.min(p.points.len())..])
    }

    fn leave(&mut self, reason: &'static str) {
        if !self.leaving {
            self.leaving = true;
            self.reason = reason;
        }
    }
}

/// A vehicle as bots see it this tick.
pub(super) struct Seen<'a> {
    pub entity: Entity,
    pub template: &'a str,
    pub data: &'a game_shared::vehicle::VehicleData,
    pub motion: &'a VehicleMotion,
    pub state: &'a VehicleState,
    pub health: Option<&'a VehicleHealth>,
    pub weapons: Option<&'a game_shared::vehicle::VehicleWeapons>,
}

impl Seen<'_> {
    fn health_fraction(&self) -> f32 {
        self.health.map_or(1.0, |h| (h.current / h.max.max(1.0)).clamp(0.0, 1.0))
    }

    fn wrecked(&self) -> bool {
        self.health.is_some_and(|h| h.wrecked())
    }

    fn gun(&self, index: usize) -> Option<GunStatus> {
        self.weapons.and_then(|w| w.guns.get(index).copied())
    }
}

impl Senses<'_, '_> {
    pub(super) fn vehicle(&self, entity: Entity) -> Option<Seen<'_>> {
        let (entity, vehicle, data, motion, state, health, weapons) = self.rides.get(entity).ok()?;
        Some(Seen {
            entity,
            template: &vehicle.template,
            data,
            motion,
            state,
            health,
            weapons,
        })
    }

    /// A player's name, for the log.
    pub(super) fn name(&self, player: Entity) -> &str {
        self.players.get(player).map_or("?", |p| p.name.as_str())
    }

    pub(super) fn profile(&self, template: &str) -> Option<&VehicleProfile> {
        self.profiles.get(template)
    }

    fn vehicle_nav(&self) -> Option<&std::sync::Arc<VehicleNavGrid>> {
        self.vehicle_nav.as_deref().map(|n| &n.0)
    }

    /// Height above whatever is below (terrain, statics, water), up to `max`.
    fn height_above_ground(&self, at: Vec3, max: f32) -> f32 {
        let ground = self
            .spatial
            .cast_ray(at, Dir3::NEG_Y, max, true, &SpatialQueryFilter::from_mask(game_shared::physics::GameLayer::World))
            .map_or(max, |hit| hit.distance);
        match self.level.desc.water.as_ref() {
            Some(water) => ground.min((at.y - water.height).max(0.0)),
            None => ground,
        }
    }
}

/// The world yaw a direction points at (0 = -Z, positive turns left).
fn heading(v: Vec3) -> f32 {
    (-v.x).atan2(-v.z)
}

/// Signed angle from the vehicle's forward to `to` in the horizontal plane: positive to the
/// right.
fn bearing(rotation: Quat, to: Vec3) -> f32 {
    let local = rotation.inverse() * to;
    local.x.atan2(-local.z)
}

impl BotBrain {
    /// The vehicle the bot is in, for debug views.
    pub fn ride_path(&self) -> &[Vec3] {
        self.ride.as_ref().map_or(&[], |r| r.remaining_path())
    }

    /// Called on foot: a ride that ended (the bot got out, or its vehicle is gone).
    pub(super) fn end_ride(&mut self, w: &Senses, me: &Me, claims: &mut VehicleClaims, stats: &mut BotStats) {
        if let Some(ride) = self.ride.take() {
            let reason = if ride.leaving { ride.reason } else { "thrown out" };
            let template = w.vehicle(ride.vehicle).map_or("a wreck", |v| v.template);
            info!("{} got out of {template} ({reason}) after {:.0} s", w.name(me.player), ride.time);
            stats.vehicle_exits += 1;
            if ride.strikes >= GIVE_UP_STRIKES || reason == "stuck" || reason == "damaged" {
                self.left_vehicle = Some((ride.vehicle, ABANDON_COOLDOWN));
            } else {
                self.left_vehicle = Some((ride.vehicle, 20.0));
            }
            self.vehicle_cooldown = VEHICLE_COOLDOWN;
            // Walk on from here.
            self.path = None;
            self.path_task = None;
            self.path_goal = None;
            self.spot = None;
            self.via = None;
            self.last_position = me.motion.position;
            let _ = w;
        }
        self.mount_wish = None;
        claims.release(me.player);
    }

    /// Vehicle options for a bot on foot, weighed with the rest in `decide`.
    pub(super) fn vehicle_options(
        &mut self,
        w: &Senses,
        me: &Me,
        vcx: &mut VehicleCx,
        target: Option<SoldierMotion>,
        best: &mut (f32, Activity),
    ) {
        if self.vehicle_cooldown > 0.0 || me.motion.climbing || self.hurt_ago < 3.0 {
            return;
        }
        let position = me.motion.position;
        let squad = me.member.and_then(|m| w.snapshot.squads.get(&(me.team, m.squad)));
        let is_leader = me.member.is_some_and(|m| m.leader);
        let leader_player = squad.and_then(|s| s.leader).filter(|_| !is_leader);
        let squad_size = squad.map_or(1, |s| s.alive.len().max(1));
        let order = self.current_order(w, me);
        let objective = order.map(|(_, a)| w.map.areas[a].position);
        let far = objective.map_or(0.0, |o| o.xz().distance(position.xz()));
        let attacking = order.is_some_and(|(k, _)| k == OrderKind::Attack);
        let skill = self.personality.skill(w.settings.bot_skill).0;

        let offer = |best: &mut (f32, Activity), utility: f32, vehicle: Entity, wish: SeatWish| {
            if utility > best.0 {
                *best = (utility, Activity::Mount { vehicle, wish, time: 0.0 });
            }
        };
        for (entity, vehicle, _data, motion, _state, health, _) in &w.rides {
            let distance = motion.position.distance(position);
            let search = if leader_player.is_some() { BOARD_DISTANCE } else { VEHICLE_SEARCH };
            if distance > search.max(35.0) || health.is_some_and(|h| h.wrecked() || h.current < h.max * 0.5) {
                continue;
            }
            if self.left_vehicle.is_some_and(|(v, _)| v == entity) {
                continue;
            }
            let Some(profile) = w.profile(&vehicle.template) else {
                continue;
            };
            // Upside down, or sunk.
            if (motion.rotation * Vec3::Y).y < 0.5 {
                continue;
            }
            let crew = vcx.crews.get(&entity).map_or(&[][..], |c| &c[..]);
            if crew.iter().any(|c| c.team != me.team) {
                continue;
            }
            let taken = |seat: u8| crew.iter().any(|c| c.seat == seat);
            let free = (0..profile.seats as u8).filter(|s| !taken(*s)).count();
            let claimed: Vec<SeatWish> = vcx.claims.others(entity, me.player).collect();
            let driver_free = !taken(0) && !claimed.contains(&SeatWish::Driver);
            let driven = crew.iter().any(|c| c.seat == 0);
            let free_gunner = profile.gunner_seats.iter().filter(|s| !taken(**s)).count()
                > claimed.iter().filter(|c| **c == SeatWish::Gunner).count();
            let room = free > claimed.len();
            let speed = motion.velocity.length();

            // Into the leader's vehicle.
            if let Some(leader) = leader_player
                && crew.iter().any(|c| c.player == leader)
                && room
                && speed < 6.0
                && !profile.role.flies() | (motion.position.y - position.y < 4.0)
            {
                let wish = if free_gunner { SeatWish::Gunner } else { SeatWish::Passenger };
                offer(best, 6.0, entity, wish);
                continue;
            }
            if distance > VEHICLE_SEARCH {
                continue;
            }
            // A stationary weapon when enemies are about.
            if profile.role == Role::Stationary {
                if !driver_free || distance > 30.0 {
                    continue;
                }
                let range = profile.guns.iter().map(|g| g.range).fold(0.0, f32::max).min(500.0);
                let anti_air = profile.guns.iter().any(|g| g.kind == GunKind::AntiAir);
                let facing = motion.rotation * Vec3::NEG_Z;
                let threat = if anti_air {
                    w.rides.iter().any(|(e, v, _, m, ..)| {
                        w.profile(&v.template).is_some_and(|p| p.role.flies())
                            && m.position.distance(motion.position) < range
                            && equipment::crew_team(w, e).is_some_and(|t| t != me.team && t != Team::Spectator)
                    })
                } else {
                    let seen = target.map(|t| t.position).or(self.last_seen.filter(|(_, age)| *age < 8.0).map(|(at, _)| at));
                    seen.is_some_and(|at| {
                        let to = at - motion.position;
                        to.length() < range && to.length() > 15.0 && flat(to).normalize_or_zero().dot(flat(facing)) > -0.2
                    })
                };
                if threat {
                    offer(best, 5.5 + self.personality.teamwork, entity, SeatWish::Driver);
                }
                continue;
            }
            if target.is_some() {
                continue;
            }
            // A gunner's seat in a teammate's vehicle.
            if driven && free_gunner && !profile.role.flies() && distance < 40.0 && speed < 4.0 {
                offer(best, 4.0 + self.personality.teamwork, entity, SeatWish::Gunner);
                continue;
            }
            if driven || !driver_free || leader_player.is_some() {
                continue;
            }
            // Driving one ourselves, when the objective is far enough to be worth it.
            let (worth, utility) = match profile.role {
                Role::Transport => (far > MOUNT_DISTANCE && (squad_size >= 2 || is_leader || me.member.is_none()), 4.2),
                Role::Apc => (far > MOUNT_DISTANCE, 4.6),
                Role::Tank => (far > MOUNT_DISTANCE && (attacking || far > 300.0) && squad_size <= 3, 4.4),
                Role::AntiAir => (far > MOUNT_DISTANCE && hash01(self.seed, 11) < 0.5, 3.8),
                Role::Boat => (far > MOUNT_DISTANCE && self.stranded > 0.0 || far > 250.0 && distance < 40.0, 4.6),
                Role::TransportHeli => (far > 300.0 && is_leader && squad_size >= 3 && skill > 0.35, 4.0),
                Role::AttackHeli => (skill > 0.3 && hash01(self.seed, 12) < 0.6, 4.3),
                Role::Jet => (skill > 0.3 && hash01(self.seed, 13) < 0.6, 4.3),
                Role::Stationary => (false, 0.0),
            };
            // Stranded (a carrier, an island): anything that gets it off.
            let worth = worth || (self.stranded > 0.0 && profile.role != Role::Stationary);
            if !worth || (profile.role == Role::Boat && w.vehicle_nav().is_none_or(|n| n.water.is_none())) {
                continue;
            }
            // Nearer ones first; squads want room for everyone.
            let fits = if free >= squad_size { 0.3 } else { 0.0 };
            offer(best, utility + fits - distance / VEHICLE_SEARCH, entity, SeatWish::Driver);
        }
    }

    /// Walks to the vehicle's nearest entry point and presses use there.
    #[allow(clippy::too_many_arguments)]
    pub(super) fn mount(
        &mut self,
        w: &Senses,
        me: &Me,
        vcx: &mut VehicleCx,
        vehicle: Entity,
        wish: SeatWish,
        time: f32,
        intent: &mut Intent,
        dt: f32,
    ) {
        let seen = w.vehicle(vehicle).filter(|v| !v.wrecked());
        let crew = vcx.crews.get(&vehicle).map_or(&[][..], |c| &c[..]);
        let hostile = crew.iter().any(|c| c.team != me.team);
        let (Some(seen), false, true) = (seen, hostile, time < 30.0) else {
            vcx.claims.release(me.player);
            self.activity = Activity::Objective;
            self.vehicle_cooldown = VEHICLE_COOLDOWN;
            return;
        };
        // The driver's seat went to someone else meanwhile: go as a passenger or not at all.
        if wish == SeatWish::Driver && crew.iter().any(|c| c.seat == 0) {
            vcx.claims.release(me.player);
            self.activity = Activity::Objective;
            return;
        }
        vcx.claims.claim(vehicle, me.player, wish);
        self.mount_wish = Some((vehicle, wish));
        let transform = seen.motion.transform();
        let chest = me.motion.position + Vec3::Y;
        let entries = &seen.data.0.desc.entry_points;
        let (entry, radius) = entries
            .iter()
            .map(|e| (transform.transform_point(Vec3::from_array(e.position)), e.radius))
            .min_by(|a, b| a.0.distance(chest).total_cmp(&b.0.distance(chest)))
            .unwrap_or((seen.motion.position, 3.0));
        let reach = chest.distance(entry) - radius;
        if reach <= 0.4 {
            // Pressed and let go on alternate ticks: each press is a fresh one.
            self.use_toggle = !self.use_toggle;
            if self.use_toggle {
                intent.buttons |= Buttons::USE;
            }
            intent.look = Look::At(entry);
        } else {
            let distance = flat(entry - me.motion.position).length();
            intent.goal = Some(Goal {
                position: Vec3::new(entry.x, me.motion.position.y.min(entry.y), entry.z),
                tolerance: 1.0,
                sprint: distance > 8.0,
            });
        }
        self.activity = Activity::Mount { vehicle, wish, time: time + dt };
    }

    /// Everything a seated bot does: drive, gun, fly, ride along, get out.
    #[allow(clippy::too_many_arguments)]
    pub(super) fn tick_seated(
        &mut self,
        w: &Senses,
        me: &Me,
        seated: &Seated,
        vcx: &mut VehicleCx,
        intel: &mut TeamIntel,
        stats: &mut BotStats,
        team_stats: &mut TeamStats,
    ) -> InputFrame {
        let dt = w.time.delta_secs();
        self.seq = self.seq.wrapping_add(1);
        if self.soldier != Some(me.soldier) {
            // Spawned straight into a seat (it doesn't happen yet), or a new life.
            self.new_life(w, me);
        }
        let idle = InputFrame {
            seq: self.seq,
            yaw: self.yaw,
            pitch: self.pitch,
            ..default()
        };
        let Some(seen) = w.vehicle(seated.vehicle) else {
            return idle;
        };
        let Some(profile) = w.profile(seen.template) else {
            return idle;
        };
        let crew = vcx.crews.get(&seated.vehicle).map_or(&[][..], |c| &c[..]);

        // A new ride.
        if self.ride.as_ref().is_none_or(|r| r.vehicle != seated.vehicle) {
            let wish = self.mount_wish.filter(|(v, _)| *v == seated.vehicle).map(|(_, wish)| wish);
            let mut ride = Ride::new(seated.vehicle, seated.seat, Purpose::Ride, seen.motion.position, self.yaw);
            ride.want_seat = match (wish, seated.seat) {
                (Some(SeatWish::Gunner), seat) if !profile.gunner_seats.contains(&seat) => profile
                    .gunner_seats
                    .iter()
                    .copied()
                    .find(|s| !crew.iter().any(|c| c.seat == *s)),
                (Some(SeatWish::Passenger), 0) if profile.seats > 1 => (1..profile.seats as u8).find(|s| !crew.iter().any(|c| c.seat == *s)),
                (Some(SeatWish::Driver), seat) if seat != 0 && !crew.iter().any(|c| c.seat == 0) => Some(0),
                _ => None,
            };
            // What the seat it ends up in is for: someone in the driver's seat drives.
            ride.purpose = match ride.want_seat.unwrap_or(seated.seat) {
                _ if profile.role == Role::Stationary => Purpose::Gun,
                0 => Purpose::Drive,
                seat if profile.gunner_seats.contains(&seat) => Purpose::Gun,
                _ => Purpose::Ride,
            };
            info!(
                "{} got into {} ({}) seat {} to {:?}",
                w.name(me.player),
                seen.template,
                profile.role.name(),
                seated.seat,
                ride.purpose
            );
            stats.vehicle_entries += 1;
            team_stats.mounts += 1;
            if profile.role == Role::Stationary {
                team_stats.stationary += 1;
            }
            self.ride = Some(ride);
            self.activity = Activity::Objective;
            vcx.claims.release(me.player);
        }
        let mut ride = self.ride.take().unwrap();
        ride.time += dt;
        if ride.seat != seated.seat {
            ride.seat = seated.seat;
            ride.purpose = match seated.seat {
                0 if profile.role != Role::Stationary => Purpose::Drive,
                seat if profile.gunner_seats.contains(&seat) || profile.role == Role::Stationary => Purpose::Gun,
                _ => Purpose::Ride,
            };
        }
        if ride.want_seat == Some(seated.seat) || ride.time > 3.0 {
            ride.want_seat = None;
        }
        let mut frame = InputFrame {
            seq: self.seq,
            yaw: ride.yaw,
            pitch: ride.pitch,
            ..default()
        };
        if let Some(seat) = ride.want_seat
            && !crew.iter().any(|c| c.seat == seat)
        {
            frame.seat = seat + 1;
        }

        // Reasons to get out.
        let health = seen.health_fraction();
        if health < BAIL_OUT && profile.role != Role::Stationary {
            ride.leave("damaged");
        }
        let up = (seen.motion.rotation * Vec3::Y).y;
        ride.flipped = if up < 0.3 && profile.role != Role::Stationary { ride.flipped + dt } else { 0.0 };
        if ride.flipped > 2.0 {
            ride.leave("flipped");
        }
        let driver = crew.iter().find(|c| c.seat == 0);
        ride.no_driver = if driver.is_none() && profile.role != Role::Stationary { ride.no_driver + dt } else { 0.0 };
        if ride.purpose != Purpose::Drive && ride.no_driver > 4.0 && seen.motion.velocity.length() < 3.0 {
            // The driver left: nothing to wait for. Gunners of land vehicles with nobody else
            // to drive take the wheel if they can.
            if ride.purpose == Purpose::Gun && !profile.role.flies() && ride.no_driver < 4.5 && profile.role != Role::Boat {
                ride.want_seat = Some(0);
            } else {
                ride.leave("no driver");
            }
        }

        let skill = self.personality.skill(w.settings.bot_skill);
        let eye_seat = seated.seat as usize;
        let mounted = Mounted::new(&seen.data.0, seen.motion.transform(), &seen.state.joints);
        let order = self.current_order(w, me);

        match ride.purpose {
            Purpose::Drive => {
                let fighting = self.gun_seat(w, me, &mut ride, &seen, profile, &mounted, eye_seat, skill, intel, vcx, &mut frame, team_stats, dt);
                match profile.role {
                    Role::TransportHeli | Role::AttackHeli => {
                        self.fly_helicopter(w, me, &mut ride, &seen, profile, &mounted, order, vcx, &mut frame, stats, team_stats, dt)
                    }
                    Role::Jet => self.fly_jet(w, me, &mut ride, &seen, profile, &mounted, order, &mut frame, stats, team_stats, dt),
                    _ => self.drive(w, me, &mut ride, &seen, profile, order, fighting, vcx, &mut frame, stats, team_stats, dt),
                }
            }
            Purpose::Gun => {
                let fighting = self.gun_seat(w, me, &mut ride, &seen, profile, &mounted, eye_seat, skill, intel, vcx, &mut frame, team_stats, dt);
                ride.idle = if fighting { 0.0 } else { ride.idle + dt };
                if profile.role == Role::Stationary && ride.idle > 25.0 {
                    ride.leave("nothing to shoot");
                }
                if !fighting {
                    self.look_around(&mut ride, &seen, &mut frame, dt);
                }
                self.ride_along(w, me, &mut ride, &seen, profile, order, crew);
            }
            Purpose::Ride => {
                self.look_around(&mut ride, &seen, &mut frame, dt);
                self.ride_along(w, me, &mut ride, &seen, profile, order, crew);
            }
        }

        if ride.leaving {
            // Pressed and let go on alternate ticks until out.
            ride.use_down = !ride.use_down;
            if ride.use_down {
                frame.buttons |= Buttons::USE;
            }
            frame.seat = 0;
        }
        self.yaw = frame.yaw;
        self.pitch = frame.pitch;
        self.ride = Some(ride);
        frame
    }

    /// Riders and gunners get out where their vehicle has brought them.
    #[allow(clippy::too_many_arguments)]
    fn ride_along(
        &self,
        w: &Senses,
        me: &Me,
        ride: &mut Ride,
        seen: &Seen,
        profile: &VehicleProfile,
        order: Option<(OrderKind, usize)>,
        crew: &[Crew],
    ) {
        if profile.role == Role::Stationary {
            return;
        }
        let speed = seen.motion.velocity.length();
        let position = seen.motion.position;
        let height = if profile.role.flies() { w.height_above_ground(position, 50.0) } else { 0.0 };
        let landed = height < 2.5;
        // A human leader decides: out when he gets out.
        let squad = me.member.and_then(|m| w.snapshot.squads.get(&(me.team, m.squad)));
        if let Some(leader) = squad.and_then(|s| s.leader).filter(|_| !me.member.is_some_and(|m| m.leader))
            && squad.is_some_and(|s| s.leader_soldier.is_some())
            && !crew.iter().any(|c| c.player == leader)
            && speed < 4.0
            && landed
        {
            let leader_position = squad.and_then(|s| s.leader_soldier).map(|l| l.position).unwrap();
            if leader_position.distance(position) < 60.0 {
                ride.leave("leader got out");
            }
        }
        let Some((_, area)) = order else {
            return;
        };
        let area = &w.map.areas[area];
        let near = area.position.xz().distance(position.xz()) < area.radius + 45.0;
        let keeps_gunning = ride.purpose == Purpose::Gun && profile.role.fights();
        if near && speed < 4.0 && landed && !keeps_gunning {
            ride.leave("arrived");
        }
    }

    /// Passengers and idle gunners look about, mostly ahead.
    fn look_around(&mut self, ride: &mut Ride, seen: &Seen, frame: &mut InputFrame, dt: f32) {
        self.sweep += dt * 0.35;
        let forward = heading(seen.motion.rotation * Vec3::NEG_Z);
        let yaw = forward + self.sweep.sin() * 1.2;
        ride.yaw = turn_towards(ride.yaw, yaw, 1.5 * dt);
        ride.pitch += (0.0 - ride.pitch).clamp(-dt, dt);
        frame.yaw = ride.yaw;
        frame.pitch = ride.pitch;
    }

    /// Scans for targets for the seat's guns, aims (with lead) and fires. Returns whether it
    /// is fighting.
    #[allow(clippy::too_many_arguments)]
    fn gun_seat(
        &mut self,
        w: &Senses,
        me: &Me,
        ride: &mut Ride,
        seen: &Seen,
        profile: &VehicleProfile,
        mounted: &Mounted,
        seat: usize,
        skill: Skill,
        intel: &mut TeamIntel,
        vcx: &mut VehicleCx,
        frame: &mut InputFrame,
        team_stats: &mut TeamStats,
        dt: f32,
    ) -> bool {
        let guns: Vec<&GunInfo> = profile.seat_guns(seat as u8).collect();
        if guns.is_empty() {
            return false;
        }
        let eye = mounted.eye(seat);
        ride.scan -= dt;
        if ride.scan <= 0.0 {
            ride.scan = SCAN_INTERVAL + 0.05;
            let before = ride.aim.map(|a| a.entity);
            ride.aim = self.vehicle_targets(w, me, seen, &guns, eye, vcx.crews, skill, intel, before);
            if ride.aim.map(|a| a.entity) != before && ride.aim.is_some() {
                ride.reaction = skill.reaction_time() * 1.2;
                let angle = fastrand::f32() * TAU;
                self.aim_error = Vec2::new(angle.cos(), angle.sin()) * skill.aim_error() * 0.6;
            }
        }
        // Keep up with where it is between scans.
        let Some(mut aim) = ride.aim else {
            ride.guiding = (ride.guiding - dt).max(0.0);
            return false;
        };
        let current = if aim.vehicle {
            w.vehicle(aim.entity).filter(|v| !v.wrecked()).map(|v| (v.motion.position + Vec3::Y * 1.0, v.motion.velocity))
        } else {
            w.soldiers
                .get(aim.entity)
                .ok()
                .filter(|s| !s.8)
                .map(|s| (s.1.position + Vec3::Y * chest_height(s.1.stance), s.1.velocity))
        };
        let Some((position, velocity)) = current else {
            ride.aim = None;
            return false;
        };
        aim.position = position;
        aim.velocity = velocity;
        ride.aim = Some(aim);
        let Some(gun) = profile.guns.get(aim.gun) else {
            return false;
        };
        let muzzle = mounted.muzzle(gun.index);
        let own_velocity = seen.motion.velocity;
        let point = match gun.kind {
            // Guided: straight at it; the missile does the leading.
            GunKind::Missile | GunKind::AntiAir => position,
            _ => lead(muzzle.translation, position, velocity - own_velocity, gun.speed, gun.gravity),
        };
        let to = point - eye;
        let distance = to.length();
        // The error shrinks as it tracks.
        self.aim_error *= 1.0 - (skill.aim_settle() * dt).min(1.0);
        let wander = skill.aim_spread(distance) * 0.5 * (2.0 * skill.aim_settle() * dt).sqrt();
        self.aim_error += Vec2::new(gaussian(), gaussian()) * wander;
        let (yaw, pitch) = angles(to);
        let rate = skill.turn_rate() * 1.5;
        ride.yaw = turn_towards(ride.yaw, yaw + self.aim_error.x, rate * dt);
        ride.pitch += (pitch + self.aim_error.y - ride.pitch).clamp(-rate * dt, rate * dt);
        frame.yaw = ride.yaw;
        frame.pitch = ride.pitch;
        frame.weapon = gun.pick;
        ride.reaction -= dt;

        // Fire once the gun itself points there.
        let forward = muzzle.rotation * Vec3::NEG_Z;
        let wanted = (point - muzzle.translation).normalize_or_zero();
        let off = forward.angle_between(wanted);
        let tolerance = match gun.kind {
            GunKind::Cannon | GunKind::Missile => (2.0 / distance.max(1.0)).atan().max(0.012),
            GunKind::AntiAir => 0.2,
            _ => (3.0 / distance.max(1.0)).atan().clamp(0.02, 0.1),
        };
        let status = seen.gun(gun.index);
        let ready = status.is_none_or(|s| !s.reloading && s.rounds > 0 && s.heat < 220);
        let locked = gun.kind != GunKind::AntiAir || status.is_some_and(|s| s.lock == 255);
        let trigger = if gun.alt_fire { Buttons::AIM } else { Buttons::FIRE };
        ride.burst -= dt;
        if ride.guiding > 0.0 {
            // A guided missile in the air: keep the sight on the target.
            ride.guiding -= dt;
        } else if ride.reaction <= 0.0 && off < tolerance && ready && locked {
            match gun.kind {
                GunKind::MachineGun | GunKind::AutoCannon => {
                    if ride.burst < -0.25 - 0.3 * fastrand::f32() {
                        ride.burst = if distance < 60.0 { 0.6 + 0.5 * fastrand::f32() } else { 0.25 + 0.25 * fastrand::f32() };
                    }
                    if ride.burst > 0.0 {
                        frame.buttons |= trigger;
                    }
                }
                _ => {
                    if ride.burst <= 0.0 {
                        frame.buttons |= trigger;
                        ride.burst = if gun.single { 0.4 } else { 1.0 / gun.rate + 0.1 };
                        team_stats.vehicle_shots += 1;
                        if gun.kind == GunKind::Missile {
                            ride.guiding = (distance / gun.speed.max(1.0) + 0.5).min(8.0);
                        }
                    }
                }
            }
            if aim.vehicle && frame.buttons.intersects(Buttons::FIRE | Buttons::AIM) {
                vcx.claims.engage(aim.entity, me.team);
            }
        }
        true
    }

    /// The best target in reach of a seat's guns: enemy vehicles for guns against armour and
    /// aircraft for heat seekers, soldiers (on foot or in open seats) for the rest.
    #[allow(clippy::too_many_arguments)]
    fn vehicle_targets(
        &self,
        w: &Senses,
        me: &Me,
        seen: &Seen,
        guns: &[&GunInfo],
        eye: Vec3,
        crews: &Crews,
        skill: Skill,
        intel: &mut TeamIntel,
        current: Option<Entity>,
    ) -> Option<Aim> {
        let sight = skill.sight_range().max(250.0);
        let mut candidates: Vec<(f32, Aim)> = Vec::new();
        let enemy_crew = |vehicle: Entity| crews.get(&vehicle).is_some_and(|c| c.iter().any(|c| c.team != me.team && c.team != Team::Spectator));
        for (entity, vehicle, _, motion, _, health, _) in &w.rides {
            if entity == seen.entity || health.is_some_and(|h| h.wrecked()) || !enemy_crew(entity) {
                continue;
            }
            let target = w.profile(&vehicle.template);
            let flies = target.is_some_and(|p| p.role.flies());
            let armored = target.is_some_and(|p| matches!(p.role, Role::Tank | Role::Apc | Role::AntiAir));
            let at = motion.position + Vec3::Y;
            let distance = at.distance(eye);
            let gun = guns
                .iter()
                .filter(|g| distance <= g.range.min(sight * 1.5))
                .filter(|g| match g.kind {
                    GunKind::AntiAir => flies,
                    GunKind::MachineGun => !armored,
                    _ => !flies || g.kind != GunKind::Cannon,
                })
                .max_by_key(|g| match (g.kind, armored) {
                    (GunKind::Missile | GunKind::Cannon, true) => 4,
                    (GunKind::AntiAir, _) => 4,
                    (GunKind::AutoCannon | GunKind::Rockets, _) => 3,
                    (GunKind::Cannon | GunKind::Missile, false) => 2,
                    _ => 1,
                });
            if let Some(gun) = gun {
                let threat = if armored { 0.5 } else { 0.8 };
                candidates.push((
                    distance * threat,
                    Aim {
                        entity,
                        vehicle: true,
                        position: at,
                        velocity: motion.velocity,
                        gun: gun.index,
                    },
                ));
            }
        }
        let infantry_guns: Vec<&&GunInfo> = guns.iter().filter(|g| g.kind.anti_infantry()).collect();
        if !infantry_guns.is_empty() {
            for (entity, motion, controlled_by, _, _, _, _, seated, downed) in &w.soldiers {
                if downed || !w.is_enemy(controlled_by.0, me.team) {
                    continue;
                }
                // Crews only in open seats.
                if let Some(s) = seated {
                    let open = w
                        .vehicle(s.vehicle)
                        .and_then(|v| v.data.0.desc.seats.get(s.seat as usize).map(|d| d.open))
                        .unwrap_or(false);
                    if !open || s.vehicle == seen.entity {
                        continue;
                    }
                }
                let at = motion.position + Vec3::Y * chest_height(motion.stance);
                let distance = at.distance(eye);
                if distance > sight {
                    continue;
                }
                // Main guns only at close range or at groups.
                let gun = infantry_guns
                    .iter()
                    .filter(|g| distance <= g.range)
                    .filter(|g| {
                        !matches!(g.kind, GunKind::Cannon | GunKind::Rockets)
                            || distance < 60.0
                            || w.soldiers
                                .iter()
                                .filter(|o| o.0 != entity && w.is_enemy(o.2.0, me.team) && o.1.position.distance(motion.position) < 7.0)
                                .count()
                                >= 1
                    })
                    .min_by_key(|g| match g.kind {
                        GunKind::MachineGun => 0,
                        GunKind::AutoCannon => 1,
                        _ => 2,
                    });
                if let Some(gun) = gun {
                    candidates.push((
                        distance,
                        Aim {
                            entity,
                            vehicle: false,
                            position: at,
                            velocity: motion.velocity,
                            gun: gun.index,
                        },
                    ));
                }
            }
        }
        let keep = |aim: &Aim| if Some(aim.entity) == current { -20.0 } else { 0.0 };
        candidates.sort_by(|a, b| (a.0 + keep(&a.1)).total_cmp(&(b.0 + keep(&b.1))));
        for (_, aim) in candidates.into_iter().take(MAX_SIGHT_CHECKS) {
            if w.smoke.blocks(eye, aim.position) || !tactics::line_of_sight(&w.spatial, eye, aim.position) {
                continue;
            }
            intel.report(me.team, aim.entity, aim.position);
            return Some(aim);
        }
        None
    }

    /// Where the driver is going: the squad's objective. Transports stop short of the flag
    /// to drop their riders off, fighting vehicles drive into it, boats to the shore nearest
    /// it.
    fn drive_goal(
        &self,
        w: &Senses,
        me: &Me,
        seen: &Seen,
        profile: &VehicleProfile,
        order: Option<(OrderKind, usize)>,
    ) -> Option<(Vec3, f32)> {
        let position = seen.motion.position;
        let (kind, index) = order?;
        let area = &w.map.areas[index];
        let target = area.position;
        let from = flat(position - target).normalize_or(Vec3::X);
        match profile.role {
            Role::Boat => {
                let nav = w.vehicle_nav()?;
                let water = nav.water.as_ref()?;
                let landing = water.landing(target, 400.0, profile.spec.map_or(1.0, |s| s.depth))?;
                Some((landing, 12.0))
            }
            Role::Tank | Role::Apc | Role::AntiAir => {
                let spot = match kind {
                    OrderKind::Attack => target + from * area.radius * 0.35,
                    // Hold in front of the flag, towards the enemy.
                    OrderKind::Defend => {
                        let facing = self.facing(w, me.team, index);
                        target + Quat::from_rotation_y(facing) * Vec3::NEG_Z * (area.radius * 0.5 + 10.0)
                    }
                };
                Some((spot, area.radius * 0.5))
            }
            _ => Some((target + from * (area.radius * 0.6 + 10.0), area.radius * 0.5 + 10.0)),
        }
    }

    /// Drives land vehicles and boats: path, pure pursuit, speed, stuck recovery, waiting for
    /// riders, stopping to fight.
    #[allow(clippy::too_many_arguments)]
    fn drive(
        &mut self,
        w: &Senses,
        me: &Me,
        ride: &mut Ride,
        seen: &Seen,
        profile: &VehicleProfile,
        order: Option<(OrderKind, usize)>,
        fighting: bool,
        vcx: &mut VehicleCx,
        frame: &mut InputFrame,
        stats: &mut BotStats,
        team_stats: &mut TeamStats,
        dt: f32,
    ) {
        let motion = seen.motion;
        let position = motion.position;
        let forward = flat(motion.rotation * Vec3::NEG_Z).normalize_or(Vec3::NEG_Z);
        let speed = motion.velocity.dot(forward);
        if !fighting {
            // Look where it's going.
            let yaw = heading(forward);
            ride.yaw = turn_towards(ride.yaw, yaw, 2.0 * dt);
            ride.pitch += (0.0 - ride.pitch).clamp(-dt, dt);
            frame.yaw = ride.yaw;
            frame.pitch = ride.pitch;
        }
        let moved = flat(position - ride.last_position).length();
        ride.last_position = position;
        if moved < 5.0 {
            stats.driven += moved;
            team_stats.driven += moved;
        }
        ride.strikes = (ride.strikes - dt / 20.0).max(0.0);
        if ride.strikes >= GIVE_UP_STRIKES {
            ride.leave("stuck");
        }

        // Riders on their way in: wait for them.
        let pending = vcx.claims.pending(seen.entity);
        if pending > 0 && ride.waited < BOARD_WAIT && ride.time < BOARD_WAIT + 5.0 {
            ride.waited += dt;
            frame.buttons |= Buttons::JUMP;
            return;
        }

        let goal = self.drive_goal(w, me, seen, profile, order);
        let Some((goal, arrive)) = goal else {
            frame.buttons |= Buttons::JUMP;
            return;
        };
        let at_goal = flat(goal - position).length() < arrive.max(8.0);
        if at_goal {
            ride.arrived += dt;
            // Transports drop their riders and the driver gets out too.
            if !profile.role.fights() && speed.abs() < 3.0 {
                if ride.time < 3.0 {
                    info!(
                        "{} is at its goal {goal:.0} ({:?}) already, {:.0} m from it",
                        w.name(me.player),
                        order.map(|(k, a)| (k, &w.map.areas[a].name)),
                        flat(goal - position).length()
                    );
                }
                ride.leave("arrived");
            }
            frame.buttons |= Buttons::JUMP;
            frame.set_movement(Vec2::ZERO);
            return;
        }
        ride.arrived = 0.0;

        let Some(spec) = profile.spec else {
            return;
        };
        let Some(nav) = w.vehicle_nav() else {
            // No grid yet: wait.
            frame.buttons |= Buttons::JUMP;
            return;
        };
        stats.driving_seconds += dt;
        // Paths.
        if let Some(task) = &mut ride.path_task
            && let Some(result) = check_ready(task)
        {
            ride.path_task = None;
            stats.vehicle_paths += 1;
            match &result {
                Some(path) if !path.complete => stats.vehicle_partial_paths += 1,
                None => stats.vehicle_failed_paths += 1,
                _ => {}
            }
            ride.path = result;
            ride.waypoint = 0;
        }
        ride.repath_cooldown -= dt;
        if ride.path_goal.is_none_or(|g| flat(g - goal).length() > 12.0) {
            ride.repath = true;
        }
        if ride.repath && ride.path_task.is_none() && ride.repath_cooldown <= 0.0 {
            ride.repath = false;
            ride.repath_cooldown = 2.0;
            ride.path_goal = Some(goal);
            let grid = nav.clone();
            let from = position;
            // Other vehicles nearby (parked ones at spawns, wrecks) are driven around.
            let obstacles: Vec<Obstacle> = w
                .rides
                .iter()
                .filter(|(entity, _, _, other, ..)| *entity != seen.entity && other.position.distance(position) < 120.0)
                .map(|(_, _, data, other, ..)| {
                    let [min, max] = data.0.desc.physics.bounds.map(Vec3::from_array);
                    let grow = profile.half_width + 0.4;
                    let forward = flat(other.rotation * Vec3::NEG_Z).normalize_or(Vec3::NEG_Z).xz();
                    let middle = other.transform().transform_point((min + max) * 0.5).xz();
                    Obstacle {
                        center: middle,
                        forward,
                        half: Vec2::new((max.x - min.x) * 0.5 + grow, (max.z - min.z) * 0.5 + grow),
                    }
                })
                .collect();
            ride.path_task = Some(AsyncComputeTaskPool::get().spawn(async move {
                let started = Instant::now();
                let path = grid.find_path(&spec, from, goal, 30.0, &obstacles);
                let ms = started.elapsed().as_secs_f32() * 1000.0;
                if ms > 50.0 {
                    debug!("vehicle path took {ms:.0} ms");
                }
                path
            }));
        }
        ride.goal = Some(goal);

        // Where to steer: a point ahead on the path.
        let lookahead = (6.0 + speed.abs() * 0.9).clamp(6.0, 22.0) + profile.length * 0.3;
        let (target, remaining, corners) = match &ride.path {
            Some(path) if path.points.len() >= 2 => follow_polyline(&path.points, &mut ride.waypoint, position, lookahead),
            // No path yet: carefully straight at it.
            _ => (goal, flat(goal - position).length(), 5.0),
        };
        if let Some(path) = &ride.path
            && ride.waypoint + 1 >= path.points.len()
            && !path.complete
            && remaining < 8.0
        {
            // At the end of a path that doesn't get there: as close as it gets. Transports
            // drop their riders here; fighting vehicles hold and try again later.
            if !profile.role.fights() && speed.abs() < 3.0 {
                ride.leave("as close as it gets");
            }
            ride.repath = true;
        }
        let to = target - position;
        let alpha = bearing(motion.rotation, to);

        // Speed: top speed, less for curves ahead, steep headings, the end of the path.
        let top = profile.top_speed * if profile.role == Role::Boat { 0.9 } else { 0.8 };
        let mut wanted = top.min(corners);
        if alpha.abs() > 0.5 {
            wanted = wanted.min((top * (1.0 - alpha.abs() / FRAC_PI_2)).max(3.0));
        }
        wanted = wanted.min((2.0 * BRAKING * remaining).sqrt() + 1.0);
        // Fighting: tanks stop for vehicles, slow down for soldiers; others slow down.
        if fighting && let Some(aim) = ride.aim {
            let close = aim.position.distance(position);
            wanted = match (profile.role, aim.vehicle) {
                (Role::Tank, true) if close < 300.0 => 0.0,
                (Role::Tank | Role::Apc | Role::AntiAir, _) => wanted.min(5.0),
                _ => wanted,
            };
        }
        // Teammates and vehicles in the way.
        let right = Vec3::new(-forward.z, 0.0, forward.x);
        let lane = profile.half_width + 1.5;
        let nose = profile.length * 0.5;
        let mut blocked_at = f32::MAX;
        let mut check = |p: Vec3| {
            let d = p - position;
            let (ahead, side) = (d.dot(forward), d.dot(right).abs());
            if ahead > 0.0 && ahead < nose + 25.0 && side < lane && d.y.abs() < 4.0 {
                blocked_at = blocked_at.min(ahead - nose);
            }
        };
        if let Some(t) = me.team_index() {
            for s in &w.snapshot.soldiers[t] {
                if s.player != me.player && !vcx.crews.get(&seen.entity).is_some_and(|c| c.iter().any(|c| c.player == s.player)) {
                    check(s.position);
                }
            }
        }
        for (entity, _, _, other, ..) in &w.rides {
            if entity != seen.entity && other.position.distance(position) < 40.0 {
                check(other.position);
            }
        }
        let obstructed = blocked_at < f32::MAX;
        if obstructed {
            wanted = wanted.min(((blocked_at - 3.0) * 0.7).max(0.0));
        }

        // Stuck: pushing without moving. Back up, turning the other way, then find a new path.
        let pushing = wanted > 1.0 && !obstructed;
        if ride.reverse > 0.0 {
            ride.reverse -= dt;
            frame.set_movement(Vec2::new(ride.reverse_steer, -1.0));
            if ride.reverse <= 0.0 {
                ride.repath = true;
                ride.repath_cooldown = 0.0;
            }
            return;
        }
        if pushing && speed.abs() < 0.6 && ride.time > 3.0 {
            ride.stuck += dt;
        } else {
            ride.stuck = 0.0;
        }
        if ride.stuck > 2.0 {
            ride.stuck = 0.0;
            ride.reverse = 1.5 + fastrand::f32();
            ride.reverse_steer = if alpha > 0.0 { -1.0 } else { 1.0 };
            ride.strikes += 1.0;
            stats.vehicle_stuck += 1;
            team_stats.vehicle_stuck += 1;
            let square = ((position.x / 10.0).floor() as i32, (position.z / 10.0).floor() as i32);
            *stats.vehicle_stuck_spots.entry(square).or_default() += 1;
            return;
        }

        // Steering.
        let steer = if profile.tracked {
            // Skid steering: turn in place for sharp turns, else the pure pursuit curvature.
            let rate = 2.0 * speed.abs().max(2.0) * alpha.sin() / to.length().max(1.0);
            (rate / profile.turn_rate + alpha * 0.8).clamp(-1.0, 1.0)
        } else {
            let angle = (2.0 * profile.wheelbase * alpha.sin()).atan2(to.length().max(1.0));
            (angle / profile.max_steer * 1.3).clamp(-1.0, 1.0)
        };
        let mut throttle = ((wanted - speed) * 0.5 + wanted / top * 0.3).clamp(-1.0, 1.0);
        if speed > wanted + 2.0 && speed > 1.0 {
            // Pressing against the direction of travel brakes.
            throttle = -1.0;
        }
        if profile.tracked && alpha.abs() > 0.9 {
            throttle = throttle.min(0.25);
        }
        // Behind it and close: back up to it (wheels only; tracks turn in place).
        let (throttle, steer) = if !profile.tracked && alpha.abs() > 2.0 && to.length() < 15.0 && profile.role != Role::Boat {
            (-0.8, if alpha > 0.0 { -1.0 } else { 1.0 })
        } else {
            (throttle, steer)
        };
        if wanted < 0.5 && speed.abs() < 1.5 && profile.role != Role::Boat {
            frame.buttons |= Buttons::JUMP;
        }
        frame.set_movement(Vec2::new(steer, throttle));
    }

    /// Flies a helicopter: climb out, cruise over the air map to the objective, land there
    /// (transports) or circle it and fire (attack helicopters).
    #[allow(clippy::too_many_arguments)]
    fn fly_helicopter(
        &mut self,
        w: &Senses,
        me: &Me,
        ride: &mut Ride,
        seen: &Seen,
        profile: &VehicleProfile,
        mounted: &Mounted,
        order: Option<(OrderKind, usize)>,
        vcx: &mut VehicleCx,
        frame: &mut InputFrame,
        stats: &mut BotStats,
        team_stats: &mut TeamStats,
        dt: f32,
    ) {
        let motion = seen.motion;
        let position = motion.position;
        let Some(rotor) = seen.data.0.desc.rotor.as_ref() else {
            return;
        };
        let height = w.height_above_ground(position, 300.0);
        let crash = (motion.velocity - ride.last_velocity).length() > 9.0 && ride.time > 1.0;
        ride.last_velocity = motion.velocity;
        if crash {
            stats.crashes += 1;
            team_stats.crashes += 1;
        }
        if height > 8.0 && !ride.airborne {
            ride.airborne = true;
            stats.flights += 1;
            team_stats.flights += 1;
        }
        let attack = profile.role == Role::AttackHeli;
        let cruise = if attack { GUNSHIP_HEIGHT } else { HELI_HEIGHT };
        let nav = w.vehicle_nav();

        // Where to.
        let area = order.map(|(_, a)| &w.map.areas[a]);
        let destination = match (attack, area) {
            (true, Some(area)) => {
                // Round the objective.
                let d = flat(position - area.position).length();
                if d < ORBIT_RADIUS * 1.6 {
                    ride.flight = if ride.flight == Flight::Cruise { Flight::Orbit } else { ride.flight };
                }
                if ride.flight == Flight::Orbit {
                    ride.orbit += dt * 18.0 / ORBIT_RADIUS;
                }
                let angle = ride.orbit;
                Some(area.position + Vec3::new(angle.cos(), 0.0, angle.sin()) * ORBIT_RADIUS)
            }
            (false, Some(area)) => {
                // A landing spot beside the flag, found once.
                if ride.goal.is_none_or(|g| g.distance(area.position) > area.radius + 150.0) {
                    let from = flat(position - area.position).normalize_or(Vec3::X);
                    let near = area.position + from * (area.radius + 25.0);
                    ride.goal = nav
                        .and_then(|n| n.landing_spot(near, 90.0, 9.0))
                        .or(Some(near));
                }
                ride.goal
            }
            _ => None,
        };
        let pending = vcx.claims.pending(seen.entity);
        let spun_up = seen.state.engine > 0.95;
        match ride.flight {
            Flight::Ground => {
                let waiting = !attack && pending > 0 && ride.waited < BOARD_WAIT;
                if waiting {
                    ride.waited += dt;
                }
                if spun_up && !waiting && destination.is_some() {
                    ride.flight = Flight::Climb;
                }
            }
            Flight::Climb if height > 12.0 => ride.flight = Flight::Cruise,
            Flight::Cruise => {
                if !attack && let Some(goal) = destination
                    && flat(goal - position).length() < 60.0
                {
                    ride.flight = Flight::Land;
                }
            }
            _ => {}
        }
        if ride.flight == Flight::Land && height < 2.0 && motion.velocity.length() < 3.0 {
            // Down: everyone out, the pilot too once they are.
            ride.arrived += dt;
            let riders = vcx.crews.get(&seen.entity).map_or(0, |c| c.len());
            if riders <= 1 || ride.arrived > 6.0 {
                ride.leave("landed");
            }
        }

        // What the flight wants: a horizontal velocity, a heading, a height.
        let forward = motion.rotation * Vec3::NEG_Z;
        let flat_forward = flat(forward).normalize_or(Vec3::NEG_Z);
        let (mut wanted_velocity, mut face) = (Vec3::ZERO, heading(flat_forward));
        let wanted_height;
        match (ride.flight, destination) {
            (Flight::Ground, _) => {
                frame.set_movement(Vec2::new(0.0, if spun_up { -0.2 } else { 0.0 }));
                return;
            }
            (Flight::Climb, _) => wanted_height = 16.0,
            (Flight::Cruise | Flight::Orbit, Some(goal)) | (Flight::Land, Some(goal)) => {
                let to = flat(goal - position);
                let distance = to.length();
                let top = profile.top_speed.min(60.0) * if attack { 0.55 } else { 0.7 };
                let speed = match ride.flight {
                    Flight::Land => (distance * 0.3).min(8.0),
                    Flight::Orbit => 18.0,
                    _ => (distance * 0.25).clamp(4.0, top),
                };
                wanted_velocity = to.normalize_or_zero() * speed;
                face = if distance > 15.0 { heading(to) } else { face };
                wanted_height = match ride.flight {
                    Flight::Land => {
                        if distance < 15.0 {
                            (height - 3.0).max(0.0)
                        } else {
                            height.min(cruise * 0.6)
                        }
                    }
                    _ => cruise,
                };
            }
            _ => wanted_height = cruise,
        }
        // Keep above what's ahead: the air map around where it will be in a few seconds.
        let floor_now = position.y - height;
        let floor_ahead = nav
            .and_then(|n| n.flight_floor(position + motion.velocity * 4.0, 50.0))
            .map_or(floor_now, |f| f.max(floor_now));
        let mut wanted_altitude = floor_now + wanted_height;
        if ride.flight != Flight::Land {
            wanted_altitude = wanted_altitude.max(floor_ahead + 20.0);
        }
        // Attack: turn the nose onto a target in front and fire the rockets.
        if attack && ride.flight == Flight::Orbit
            && let Some(aim) = ride.aim.filter(|a| a.position.distance(position) < 450.0)
        {
            let to = flat(aim.position - position);
            face = heading(to);
            wanted_velocity = wanted_velocity * 0.4;
        }

        // Collective: the climb rate is proportional to it.
        let climb = ((wanted_altitude - position.y) * 0.5).clamp(-rotor.climb_speed[1].max(1.0), rotor.climb_speed[0].max(1.0));
        let collective = if climb >= 0.0 { climb / rotor.climb_speed[0].max(1.0) } else { climb / rotor.climb_speed[1].max(1.0) };
        let collective = if ride.flight == Flight::Climb && height < 4.0 { 1.0 } else { collective.clamp(-1.0, 1.0) };
        // Cyclic: tilt towards the velocity error, up to about 25 degrees.
        let error = wanted_velocity - flat(motion.velocity);
        let right = flat(motion.rotation * Vec3::X).normalize_or(Vec3::X);
        let max_tilt = 0.42;
        let tilt_forward = (error.dot(flat_forward) * 0.06).clamp(-max_tilt, max_tilt);
        let tilt_right = (error.dot(right) * 0.06).clamp(-max_tilt, max_tilt);
        let pitch = forward.y.clamp(-1.0, 1.0).asin();
        let roll = (-(motion.rotation * Vec3::X).y).clamp(-1.0, 1.0).asin();
        let omega = motion.rotation.inverse() * motion.angular_velocity;
        let stick_pitch = ((-tilt_forward - pitch) * 2.5 - omega.x * 0.6).clamp(-1.0, 1.0);
        let stick_roll = ((tilt_right - roll) * 2.5 + omega.z * 0.6).clamp(-1.0, 1.0);
        // Tail rotor: turn to the heading.
        let yaw_error = angle_delta(heading(flat_forward), face);
        let steer = (-yaw_error * 1.5 - omega.y * -0.4).clamp(-1.0, 1.0);
        frame.set_movement(Vec2::new(steer, collective));
        frame.set_stick(Vec2::new(stick_roll, stick_pitch));

        // The pilot's own guns (rocket pods) fire forward.
        if attack && let Some(aim) = ride.aim {
            let guns: Vec<&GunInfo> = profile.seat_guns(0).collect();
            if let Some(gun) = guns.first() {
                let muzzle = mounted.muzzle(gun.index);
                let wanted = (lead(muzzle.translation, aim.position, aim.velocity - motion.velocity, gun.speed, gun.gravity) - muzzle.translation)
                    .normalize_or_zero();
                let off = (muzzle.rotation * Vec3::NEG_Z).angle_between(wanted);
                let distance = aim.position.distance(position);
                if off < (6.0 / distance.max(1.0)).atan().max(0.03) && distance < gun.range && ride.burst <= 0.0 {
                    frame.buttons |= if gun.alt_fire { Buttons::AIM } else { Buttons::FIRE };
                    frame.weapon = gun.pick;
                    ride.burst = 0.35;
                    team_stats.vehicle_shots += 1;
                }
            }
        }
        let _ = me;
    }

    /// Flies a jet: full throttle down the runway, rotate, climb out, then circle over the
    /// objective at height, diving on vehicles with the guns.
    #[allow(clippy::too_many_arguments)]
    fn fly_jet(
        &mut self,
        w: &Senses,
        me: &Me,
        ride: &mut Ride,
        seen: &Seen,
        profile: &VehicleProfile,
        mounted: &Mounted,
        order: Option<(OrderKind, usize)>,
        frame: &mut InputFrame,
        stats: &mut BotStats,
        team_stats: &mut TeamStats,
        dt: f32,
    ) {
        let motion = seen.motion;
        let position = motion.position;
        let height = w.height_above_ground(position, 400.0);
        let speed = motion.velocity.length();
        let forward = motion.rotation * Vec3::NEG_Z;
        let crash = (motion.velocity - ride.last_velocity).length() > 9.0 && ride.time > 1.0;
        ride.last_velocity = motion.velocity;
        if crash {
            stats.crashes += 1;
            team_stats.crashes += 1;
        }
        if height > 15.0 && !ride.airborne {
            ride.airborne = true;
            stats.flights += 1;
            team_stats.flights += 1;
        }
        let stall = seen.data.0.desc.aero.as_ref().map_or(15.0, |a| a.stall_angle);
        let _ = stall;
        let nav = w.vehicle_nav();
        // Takeoff roll: full throttle, wings level, straight on, then pull up.
        if ride.flight == Flight::Ground {
            let rotate = profile.top_speed * 0.45;
            let pull = if speed > rotate { 0.7 } else { 0.0 };
            let yaw_rate = (motion.rotation.inverse() * motion.angular_velocity).y;
            frame.set_movement(Vec2::new((yaw_rate * 2.0).clamp(-1.0, 1.0), 1.0));
            frame.set_stick(Vec2::new(0.0, pull));
            if height > 25.0 {
                ride.flight = Flight::Climb;
            }
            if ride.time > 40.0 && height < 5.0 {
                ride.leave("can't take off");
            }
            return;
        }
        let area = order.map(|(_, a)| &w.map.areas[a]);
        let center = area.map_or(position, |a| a.position);
        if ride.flight == Flight::Climb && height > JET_HEIGHT * 0.6 {
            ride.flight = Flight::Orbit;
        }
        // Circle the objective; dive on a target in front when there is one.
        let radius = 450.0;
        let to_center = flat(center - position);
        let tangent = Vec3::new(-to_center.z, 0.0, to_center.x).normalize_or(Vec3::X);
        let mut goal_dir = (to_center.normalize_or_zero() * ((to_center.length() - radius) / radius).clamp(-1.0, 1.0) + tangent).normalize_or(Vec3::NEG_Z);
        let floor = nav.and_then(|n| n.flight_floor(position + motion.velocity * 5.0, 120.0)).unwrap_or(position.y - height);
        let mut wanted_altitude = floor.max(position.y - height) + JET_HEIGHT;
        let mut firing = None;
        if ride.flight == Flight::Orbit
            && let Some(aim) = ride.aim
        {
            let to = aim.position - position;
            if to.length() < 900.0 && to.normalize_or_zero().dot(forward) > 0.5 {
                goal_dir = flat(to).normalize_or(goal_dir);
                wanted_altitude = (aim.position.y + to.length() * 0.35).max(floor + 60.0);
                firing = Some(aim);
            }
        }
        // Bank to turn: roll towards the heading error, pull to hold the climb angle.
        let heading_error = bearing(motion.rotation, goal_dir);
        let wanted_roll = (heading_error * 1.2).clamp(-1.1, 1.1);
        let roll = (-(motion.rotation * Vec3::X).y).clamp(-1.0, 1.0).asin();
        let omega = motion.rotation.inverse() * motion.angular_velocity;
        let stick_roll = ((wanted_roll - roll) * 2.0 + omega.z * 0.5).clamp(-1.0, 1.0);
        let climb_angle = ((wanted_altitude - position.y) / 300.0).clamp(-0.35, 0.4);
        let path_angle = (motion.velocity.y / speed.max(1.0)).clamp(-1.0, 1.0).asin();
        let bank_pull = (1.0 / roll.cos().abs().max(0.3) - 1.0) * 0.5;
        let stick_pitch = ((climb_angle - path_angle) * 3.0 + bank_pull - omega.x * 0.4).clamp(-1.0, 1.0);
        let throttle = if speed < profile.top_speed * 0.6 { 1.0 } else { 0.0 };
        frame.set_movement(Vec2::new(0.0, throttle));
        frame.set_stick(Vec2::new(stick_roll, stick_pitch));
        if let Some(aim) = firing {
            for gun in profile.seat_guns(0) {
                let muzzle = mounted.muzzle(gun.index);
                let wanted = (lead(muzzle.translation, aim.position, aim.velocity - motion.velocity, gun.speed, gun.gravity) - muzzle.translation)
                    .normalize_or_zero();
                let distance = aim.position.distance(position);
                if (muzzle.rotation * Vec3::NEG_Z).angle_between(wanted) < 0.05 && distance < gun.range {
                    frame.buttons |= if gun.alt_fire { Buttons::AIM } else { Buttons::FIRE };
                    frame.weapon = gun.pick;
                    break;
                }
            }
        }
        let _ = (me, dt);
    }
}

/// Braking deceleration drivers plan with, m/s².
const BRAKING: f32 = 3.5;

/// How fast to take a corner turning by `angle` radians, m/s.
fn corner_speed(angle: f32) -> f32 {
    4.0 + 20.0 * (1.0 - angle / FRAC_PI_2).max(0.0)
}

/// Pure pursuit along a polyline: advances `index` past the corners already reached, and
/// returns the point `lookahead` meters ahead along the path, the length left, and the speed
/// the corners coming up allow (braking for them in time).
fn follow_polyline(points: &[Vec3], index: &mut usize, position: Vec3, lookahead: f32) -> (Vec3, f32, f32) {
    let p = flat(position);
    // Advance while the next corner is reached or passed.
    while *index + 1 < points.len() {
        let (a, b) = (flat(points[*index]), flat(points[*index + 1]));
        let ab = b - a;
        let t = (p - a).dot(ab) / ab.length_squared().max(1e-6);
        if t >= 1.0 || p.distance(b) < 3.0 {
            *index += 1;
        } else {
            break;
        }
    }
    if *index + 1 >= points.len() {
        let last = *points.last().unwrap();
        return (last, flat(last - position).length(), f32::MAX);
    }
    // Project onto the current segment, then walk along.
    let (a, b) = (flat(points[*index]), flat(points[*index + 1]));
    let ab = b - a;
    let t = ((p - a).dot(ab) / ab.length_squared().max(1e-6)).clamp(0.0, 1.0);
    let mut here = a + ab * t;
    let mut left = lookahead;
    let mut target = None;
    let mut remaining = 0.0;
    let mut limit = f32::MAX;
    let mut travelled = 0.0;
    for i in *index..points.len() - 1 {
        let (from, to) = (if i == *index { here } else { flat(points[i]) }, flat(points[i + 1]));
        let length = from.distance(to);
        remaining += length;
        if target.is_none() {
            // A sharp corner ahead: steer for the corner itself rather than cut it, until
            // close to it.
            let sharp = i + 2 < points.len() && (to - from).angle_between(flat(points[i + 2]) - to) > 0.6;
            let beyond_corner = sharp && lookahead - left + length > 4.0;
            if length >= left {
                let point = from + (to - from) * (left / length.max(1e-6));
                target = Some(Vec3::new(point.x, points[i + 1].y, point.z));
            } else if beyond_corner {
                target = Some(points[i + 1]);
            } else {
                left -= length;
            }
        }
        travelled += length;
        if i + 2 < points.len() && travelled < 80.0 {
            let next = flat(points[i + 2]) - to;
            let turn = (to - from).angle_between(next);
            if turn.is_finite() {
                let v = corner_speed(turn);
                limit = limit.min((v * v + 2.0 * BRAKING * travelled).sqrt());
            }
        }
        here = to;
    }
    let target = target.unwrap_or(*points.last().unwrap());
    (target, remaining, limit)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pursues_points_ahead_on_the_path() {
        let points = [Vec3::ZERO, Vec3::new(0.0, 0.0, -20.0), Vec3::new(20.0, 0.0, -20.0)];
        let mut index = 0;
        let (target, remaining, limit) = follow_polyline(&points, &mut index, Vec3::new(0.5, 0.0, -5.0), 10.0);
        assert_eq!(index, 0);
        assert!((target - Vec3::new(0.0, 0.0, -15.0)).length() < 0.1, "{target}");
        assert!((remaining - 35.0).abs() < 0.1, "{remaining}");
        // A right angle 15 m ahead: slow enough to brake down to about 4 m/s by then.
        assert!((limit - (16.0f32 + 2.0 * BRAKING * 15.0).sqrt()).abs() < 0.1, "{limit}");
        // Past the corner: on to the next segment.
        let (target, ..) = follow_polyline(&points, &mut index, Vec3::new(2.0, 0.0, -19.5), 5.0);
        assert_eq!(index, 1);
        assert!((target - Vec3::new(7.0, 0.0, -20.0)).length() < 0.6, "{target}");
        // Steering: a point ahead to the right has a positive bearing.
        assert!(bearing(Quat::IDENTITY, Vec3::new(1.0, 0.0, -1.0)) > 0.7);
    }
}
