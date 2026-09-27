//! Bots' kit beyond the rifle: grenade launchers at enemies behind cover or bunched up,
//! rocket launchers at enemy vehicles (wire-guided missiles steered by keeping the aim on
//! the target until they hit), engineers repairing the team's vehicles and commander assets
//! with the wrench, bags thrown to teammates a few meters away, and what flashbangs and tear
//! gas do to bots.

use game_data::Guidance;
use game_shared::gear::gas_exposure;

use super::*;
use crate::ai::gadgets::{flash_effect, flash_strength};

/// What a launcher is fired at.
#[derive(Clone, Copy, Debug, PartialEq)]
pub(super) enum LaunchTarget {
    Point(Vec3),
    Vehicle(Entity),
}

/// What an engineer repairs.
#[derive(Clone, Copy, Debug, PartialEq)]
pub(super) enum RepairTarget {
    Vehicle(Entity),
    /// A commander asset's level object: its instance and where it stands.
    Object(u32, Vec3),
}

/// Rocket launchers are for vehicles within this range, meters.
const ROCKET_RANGE: f32 = 250.0;
/// Grenade launchers are used between these distances, meters.
const LAUNCHER_RANGE: (f32, f32) = (25.0, 150.0);
/// Engineers go this far to repair, meters.
const REPAIR_DISTANCE: f32 = 50.0;
/// A vehicle below this share of its health is worth repairing.
const REPAIR_BELOW: f32 = 0.8;
/// Bags are thrown to teammates up to this far, meters (they fly about 10 m).
const BAG_THROW: f32 = 10.0;

impl BotBrain {
    /// Finds the launchers and the wrench in a new soldier's loadout.
    pub(super) fn pick_equipment(&mut self, w: &Senses, me: &Me) {
        self.launcher = None;
        self.rocket = None;
        self.gas_launcher = None;
        self.flashbang = None;
        self.wrench = me.loadout.and_then(|l| gadget(l, &w.armory, Gadget::Wrench));
        let Some(loadout) = me.loadout else {
            return;
        };
        let anti_tank = w.armory.kits.get(&loadout.kit).is_some_and(|k| k.kind.eq_ignore_ascii_case("AT"));
        for i in 0..loadout.weapons.len() as u8 {
            let Some(desc) = w.weapon(Some(loadout), i) else {
                continue;
            };
            let flash = desc
                .projectile
                .detonation_effect
                .as_ref()
                .is_some_and(|e| w.data.gadgets.flashbangs.contains_key(e));
            if desc.fire.kind == FireKind::Thrown && flash {
                self.flashbang = Some(i);
            }
            if desc.fire.kind == FireKind::Gun && desc.projectile.smoke.is_some() {
                self.gas_launcher = Some(i);
            }
            let explosive_gun = desc.fire.kind == FireKind::Gun
                && desc.projectile.explodes()
                && desc.projectile.trigger.is_none()
                && i != self.primary;
            if !explosive_gun {
                continue;
            }
            if anti_tank || desc.fire.guidance == Guidance::Wire {
                self.rocket = self.rocket.or(Some(i));
            } else {
                self.launcher = self.launcher.or(Some(i));
            }
        }
    }

    /// Flashbangs going off and tear gas; true while blinded.
    pub(super) fn feel_gadgets(&mut self, w: &Senses, me: &Me, team_stats: &mut TeamStats, dt: f32) -> bool {
        let eye = me.motion.eye_position();
        let forward = Quat::from_euler(EulerRot::YXZ, self.yaw, self.pitch, 0.0) * Vec3::NEG_Z;
        for &(at, desc) in &w.flashes.0 {
            let strength = flash_strength(&desc, eye, forward, at, tactics::line_of_sight(&w.spatial, eye, at + Vec3::Y * 0.2));
            let stronger = self
                .flash
                .is_none_or(|(old, s, age)| !flash_effect(&old, s, age).1 || strength > s);
            if strength > 0.15 && stronger {
                self.flash = Some((desc, strength, 0.0));
                team_stats.flashed += 1;
            }
        }
        (self.blind, self.dazed) = match &mut self.flash {
            Some((desc, strength, age)) => {
                *age += dt;
                flash_effect(desc, *strength, *age)
            }
            None => (false, false),
        };
        if !self.blind && !self.dazed {
            self.flash = None;
        }

        let masked = w.gear.get(me.soldier).is_ok_and(|g| g.gas_mask);
        self.gassed = if masked || w.gas.is_empty() { 0.0 } else { gas_exposure(eye, w.gas.iter()) };
        if self.gassed > 0.1 {
            team_stats.gassed += dt;
        }
        self.blind
    }

    /// How much worse it aims and reacts: dazed by a flashbang, or tear gas in the eyes.
    pub(super) fn impairment(&self) -> f32 {
        1.0 + if self.dazed { 3.0 } else { 0.0 } + 3.0 * self.gassed
    }

    /// A spot out of the tear gas around it.
    pub(super) fn out_of_gas(&self, w: &Senses, me: &Me) -> Option<Vec3> {
        let position = me.motion.position;
        let (cloud, _) = w
            .gas
            .iter()
            .min_by(|a, b| a.0.position.distance(position).total_cmp(&b.0.position.distance(position)))?;
        let away = flat(position - cloud.position).try_normalize().unwrap_or(Vec3::X);
        let spot = cloud.position.with_y(position.y) + away * (cloud.current_radius() + 6.0);
        let nav = w.nav()?;
        let region = nav.cell(nav.locate(position, 2.0, None)?).region;
        nav.locate(spot, 5.0, Some(region)).map(|c| nav.position(c))
    }

    /// Rocket bots: the nearest enemy vehicle it can see.
    pub(super) fn scan_armor(&mut self, w: &Senses, me: &Me) {
        self.armor = None;
        if self.rocket.is_none() {
            return;
        }
        let eye = me.motion.eye_position();
        let mut best: Option<(f32, Entity, Vec3)> = None;
        for (vehicle, motion, health) in &w.vehicles {
            let at = motion.position + Vec3::Y;
            let distance = at.distance(eye);
            if distance > ROCKET_RANGE
                || health.is_some_and(|h| h.wrecked())
                || crew_team(w, vehicle).is_none_or(|t| t == me.team || t == Team::Spectator)
                || best.is_some_and(|(d, ..)| distance > d)
                || w.smoke.blocks(eye, at)
                || !tactics::line_of_sight(&w.spatial, eye, at)
            {
                continue;
            }
            best = Some((distance, vehicle, at));
        }
        self.armor = best.map(|(_, vehicle, at)| (vehicle, at));
    }

    /// Launchers, repairs and thrown bags, weighed against the other options.
    pub(super) fn equipment_options(
        &mut self,
        w: &Senses,
        me: &Me,
        target: Option<SoldierMotion>,
        best: &mut (f32, Activity),
    ) {
        let position = me.motion.position;
        let offer = |best: &mut (f32, Activity), utility: f32, activity: Activity| {
            if utility > best.0 {
                *best = (utility, activity);
            }
        };

        // Rockets at enemy vehicles, before anything else.
        if let (Some(rocket), Some((vehicle, at))) = (self.rocket, self.armor)
            && self.launch_cooldown <= 0.0
            && ammo_left(me, rocket) > 0
            && at.distance(position) > 15.0
        {
            let target = LaunchTarget::Vehicle(vehicle);
            let shots = ammo_left(me, rocket);
            offer(best, 9.0, Activity::Launch { target, time: 0.0, weapon: rocket, shots, fired: -1.0 });
        }

        // Grenade launchers at enemies bunched up or down behind something, or where one went
        // behind cover.
        if let Some(launcher) = self.launcher
            && self.launch_cooldown <= 0.0
            && ammo_left(me, launcher) > 0
        {
            let at = match (target, self.last_seen) {
                (Some(t), _) => {
                    let bunched = w
                        .soldiers
                        .iter()
                        .filter(|(_, m, c, ..)| w.is_enemy(c.0, me.team) && m.position.distance(t.position) < 6.0)
                        .count()
                        >= 2;
                    (bunched || t.stance != Stance::Standing).then_some((t.position, 7.8))
                }
                (None, Some((at, age))) if (1.0..6.0).contains(&age) => Some((at - Vec3::Y, 5.5)),
                _ => None,
            };
            if let Some((at, utility)) = at
                && (LAUNCHER_RANGE.0..LAUNCHER_RANGE.1).contains(&flat(at - position).length())
                && fastrand::f32() < 0.3
                && friends_clear(w, me, at, 10.0)
            {
                let shots = ammo_left(me, launcher);
                let target = LaunchTarget::Point(at);
                offer(best, utility, Activity::Launch { target, time: 0.0, weapon: launcher, shots, fired: -1.0 });
            }
        }

        // Special forces: a flashbang at an enemy close by, tear gas at enemies further off.
        if let Some(at) = target.map(|t| t.position).or(self.last_seen.filter(|(_, age)| *age < 3.0).map(|(at, _)| at)) {
            let distance = flat(at - position).length();
            let eye = me.motion.eye_position();
            let arc_clear = || {
                let early = eye + flat(at - eye).normalize_or_zero() * 5.0 + Vec3::Y * 1.5;
                tactics::line_of_sight(&w.spatial, eye, early)
            };
            if let Some(flashbang) = self.flashbang
                && self.grenade_cooldown <= 0.0
                && ammo_left(me, flashbang) > 0
                && (8.0..22.0).contains(&distance)
                && fastrand::f32() < 0.3
                && friends_clear(w, me, at, 15.0)
                && arc_clear()
            {
                offer(best, 6.5, Activity::Throw { at, time: 0.0, weapon: flashbang });
            }
            if let Some(gas) = self.gas_launcher
                && self.launch_cooldown <= 0.0
                && ammo_left(me, gas) > 0
                && (20.0..70.0).contains(&distance)
                && fastrand::f32() < 0.2
                && friends_clear(w, me, at, 15.0)
            {
                let shots = ammo_left(me, gas);
                let target = LaunchTarget::Point(at);
                offer(best, 6.0, Activity::Launch { target, time: 0.0, weapon: gas, shots, fired: -1.0 });
            }
        }

        if target.is_some() || self.hurt_ago < 5.0 {
            return;
        }

        // Engineers mend the team's vehicles and assets.
        if self.wrench.is_some()
            && self.repair_cooldown <= 0.0
            && !matches!(self.activity, Activity::Repair { .. })
            && let Some(target) = self.find_repair(w, me)
        {
            // A destroyed asset takes about half a minute of wrench work.
            offer(best, 4.0, Activity::Repair { target, time: 60.0 });
        }

        // Bags to teammates a few meters away (those closer get them held out).
        if self.bag_cooldown <= 0.0 {
            let throw_to = |bag: Option<u8>, need: &dyn Fn(Entity, Vec3) -> bool| -> Option<(u8, Vec3)> {
                let bag = bag.filter(|&b| ammo_left(me, b) > 0)?;
                w.soldiers
                    .iter()
                    .filter(|(entity, m, c, ..)| {
                        *entity != me.soldier
                            && w.teams.get(c.0).is_ok_and(|t| *t == me.team)
                            && (BAG_REACH..BAG_THROW).contains(&flat(m.position - position).length())
                    })
                    .find(|(entity, m, ..)| need(*entity, m.position))
                    .map(|(_, m, ..)| (bag, m.position))
            };
            let hurt = |soldier: Entity, _: Vec3| {
                w.soldiers
                    .get(soldier)
                    .ok()
                    .and_then(|s| s.4)
                    .is_some_and(|h| h.current < h.max * 0.6 && !w.wounded.is_downed(soldier))
            };
            let empty = |soldier: Entity, _: Vec3| {
                w.soldiers
                    .get(soldier)
                    .ok()
                    .and_then(|s| s.3.zip(s.6))
                    .is_some_and(|(i, l)| low_on_ammo(i, l, &w.armory))
            };
            if let Some((bag, at)) = throw_to(self.medic_bag, &hurt).or_else(|| throw_to(self.ammo_bag, &empty)) {
                offer(best, 3.5, Activity::Throw { at, time: 0.0, weapon: bag });
            }
        }
    }

    /// Something of the team's to repair: a damaged vehicle crewed by teammates (or empty and
    /// close), or a damaged or destroyed commander asset.
    fn find_repair(&self, w: &Senses, me: &Me) -> Option<RepairTarget> {
        let position = me.motion.position;
        let mut best: Option<(f32, RepairTarget)> = None;
        let mut consider = |distance: f32, target: RepairTarget| {
            if distance < REPAIR_DISTANCE && best.is_none_or(|(d, _)| distance < d) {
                best = Some((distance, target));
            }
        };
        for (vehicle, motion, health) in &w.vehicles {
            if health.is_none_or(|h| h.wrecked() || h.current >= h.max * REPAIR_BELOW) {
                continue;
            }
            let distance = motion.position.distance(position);
            let ours = match crew_team(w, vehicle) {
                Some(team) => team == me.team,
                None => distance < 25.0,
            };
            if ours {
                consider(distance, RepairTarget::Vehicle(vehicle));
            }
        }
        let destroyed = w.destroyed.single().ok();
        for asset in w.assets.instances.iter().filter(|a| a.team == me.team) {
            let broken = destroyed.is_some_and(|d| d.0.contains(&asset.instance))
                || w.object_health.0.contains_key(&asset.instance);
            if broken {
                let at = Vec3::from_array(asset.placement.position);
                consider(at.distance(position), RepairTarget::Object(asset.instance, at));
            }
        }
        best.map(|(_, target)| target)
    }

    /// Switches to the launcher, aims (along an arc for grenades, straight for rockets) and
    /// fires; keeps a wire-guided missile on target until it gets there; then back to the
    /// main weapon.
    #[allow(clippy::too_many_arguments)]
    pub(super) fn launch(
        &mut self,
        w: &Senses,
        me: &Me,
        skill: Skill,
        target: LaunchTarget,
        time: f32,
        weapon: u8,
        shots: u16,
        fired: f32,
        intent: &mut Intent,
        dt: f32,
    ) {
        let point = match target {
            LaunchTarget::Point(point) => Some(point),
            LaunchTarget::Vehicle(vehicle) => w
                .vehicles
                .get(vehicle)
                .ok()
                .filter(|(_, _, health)| health.is_none_or(|h| !h.wrecked()))
                .map(|(_, motion, _)| motion.position + Vec3::Y),
        };
        let (Some(desc), Some(point)) = (w.weapon(me.loadout, weapon), point) else {
            self.end_launch(intent);
            return;
        };
        intent.weapon = Some(weapon);
        intent.buttons |= Buttons::CROUCH;
        let eye = me.motion.eye_position();
        let guided = desc.fire.guidance == Guidance::Wire;
        let gravity = GRAVITY * desc.projectile.gravity;
        let to = point - eye;
        let pitch = match guided || desc.projectile.gravity < 0.2 {
            true => to.y.atan2(flat(to).length()),
            false => tactics::throw_pitch(eye, point, desc.projectile.velocity, gravity).unwrap_or(0.6),
        };
        let yaw = yaw_to(to) + self.aim_error.x * 0.3;
        let rate = skill.turn_rate();
        self.yaw = turn_towards(self.yaw, yaw, rate * dt);
        self.pitch += (pitch - self.pitch).clamp(-rate * dt, rate * dt);
        intent.look = Look::Aimed;
        let aimed = angle_delta(self.yaw, yaw).abs() + (self.pitch - pitch).abs() < 0.03 * self.impairment();

        let time = time + dt;
        let mut fired = fired;
        if fired < 0.0 {
            if ammo_left(me, weapon) < shots {
                fired = time;
            } else if time > desc.deploy_time + 0.2 && aimed {
                // A fresh pull for the shot (an empty launcher reloads on it first).
                self.burst -= dt;
                if self.burst <= 0.0 {
                    intent.buttons |= Buttons::FIRE;
                    self.burst = 0.3;
                }
            }
            if time > desc.deploy_time + desc.reload_time + 4.0 {
                self.end_launch(intent);
                return;
            }
        } else {
            // Guided: aim at it until the missile gets there; otherwise done.
            let flight = to.length() / desc.projectile.velocity.max(1.0) + 1.0;
            if !guided || time - fired > flight.min(desc.projectile.time_to_live) {
                self.end_launch(intent);
                return;
            }
        }
        self.activity = Activity::Launch { target, time, weapon, shots, fired };
    }

    fn end_launch(&mut self, intent: &mut Intent) {
        self.activity = Activity::Objective;
        self.launch_cooldown = if self.launcher.is_some() { 6.0 + 6.0 * fastrand::f32() } else { 2.0 };
        intent.weapon = Some(self.primary);
    }

    /// To the damaged thing, and the wrench at it while it needs mending.
    pub(super) fn repair(&mut self, w: &Senses, me: &Me, target: RepairTarget, time: f32, intent: &mut Intent, dt: f32) {
        let destroyed = w.destroyed.single().ok();
        let (at, done, reach) = match target {
            RepairTarget::Vehicle(vehicle) => match w.vehicles.get(vehicle) {
                Ok((_, motion, health)) => {
                    let done = health.is_none_or(|h| h.wrecked() || h.current >= h.max * 0.99);
                    (motion.position, done, 3.5)
                }
                Err(_) => (me.motion.position, true, 0.0),
            },
            RepairTarget::Object(instance, at) => {
                let broken = destroyed.is_some_and(|d| d.0.contains(&instance)) || w.object_health.0.contains_key(&instance);
                (at, !broken, 3.5)
            }
        };
        let (Some(wrench), false, true) = (self.wrench, done, time > 0.0) else {
            self.activity = Activity::Objective;
            self.repair_cooldown = 5.0;
            intent.weapon = Some(self.primary);
            return;
        };
        let distance = flat(at - me.motion.position).length();
        if distance > reach {
            intent.goal = Some(Goal {
                position: at,
                tolerance: 2.0,
                sprint: distance > 15.0,
            });
        } else {
            intent.weapon = Some(wrench);
            intent.buttons |= Buttons::FIRE;
            intent.look = Look::At(at + Vec3::Y * 0.8);
        }
        self.activity = Activity::Repair { target, time: time - dt };
    }
}

/// Rounds (or uses) left of a weapon: in the magazine and spare.
fn ammo_left(me: &Me, weapon: u8) -> u16 {
    me.inventory
        .and_then(|i| i.ammo.get(weapon as usize))
        .map_or(0, |[mag, spare]| mag + spare)
}

/// The team of a vehicle's crew, if anyone is in it.
pub(super) fn crew_team(w: &Senses, vehicle: Entity) -> Option<Team> {
    w.soldiers
        .iter()
        .find(|(.., seated, _)| seated.is_some_and(|s| s.vehicle == vehicle))
        .and_then(|(_, _, controlled_by, ..)| w.teams.get(controlled_by.0).ok().copied())
}

/// No teammate within `radius` of `at`.
fn friends_clear(w: &Senses, me: &Me, at: Vec3, radius: f32) -> bool {
    me.team_index().is_none_or(|t| {
        w.snapshot.soldiers[t]
            .iter()
            .all(|s| s.player == me.player || s.position.distance(at) > radius)
    })
}
