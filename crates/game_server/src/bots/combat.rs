//! Infantry tactics on top of the utility behaviours of [`super`]: fighting from cover,
//! suppression, hearing and memory, spotting for the team, and the squad's fire teams
//! (bounding overwatch, flanking when pinned; see [`crate::ai::squad`]).
//!
//! - **Cover**: in a firefight a bot rolls (by difficulty and courage) whether it fights from
//!   cover this time. If so, and it isn't in cover from the threat, it looks for some
//!   ([`crate::ai::cover`]: grid candidates checked with rays, rationed per tick), runs
//!   there and fights from it: up to shoot (standing over low cover, a step out past a
//!   corner), down to reload, when the fire gets close or when hurt. Attackers move on from
//!   cover to cover towards their flag. With the enemy out of sight it watches from cover.
//! - **Suppression**: enemy fire passing within [`NEAR_MISS`] (the client's flyby crack
//!   radius) raises it; it shakes the aim, slows the reaction, makes the bot hide or get
//!   low, and tells it where the shooter is.
//! - **Awareness**: what it sees, hears (gunfire, footsteps), what the team saw or spotted,
//!   and where shots came from go into its [`Memory`], which fades.
//! - **Spotting**: it calls out enemies it sees with the commo rose's spot, so the team
//!   (humans included) sees them.
//! - **Squads**: fire team 0 suppresses and team 1 flanks when the squad is pinned down;
//!   medics go to downed squad mates first and medics and support run their bags to squad
//!   mates who need them.

use bevy::prelude::*;
use game_data::{FireKind, FireMode};
use game_shared::{input::Buttons, protocol::Team, soldier::Stance};

use super::{
    Activity, BAG_REACH, BotBrain, Goal, Intent, Look, Me, Senses, angle_delta, chest_height, flat, gaussian,
    low_on_ammo, turn_towards, yaw_to,
};
use crate::ai::{
    awareness::{NEAR_MISS, Source, suppression_rate},
    cover::{self, CoverQuery, CoverSpot},
    skill::Skill,
    squad::{self, SquadReports},
    stats::TeamStats,
    strategy::{OrderKind, TeamIntel, hash01},
    tactics,
};

/// A trigger held this tick: where the shot goes.
pub(super) struct Shot {
    pub soldier: Entity,
    pub team: Team,
    pub eye: Vec3,
    pub dir: Vec3,
    pub range: f32,
    pub feet: Vec3,
}

/// An enemy a bot calls out on the radio (see [`crate::radio::Spot`]).
pub(super) struct SpotCall {
    pub player: Entity,
    pub target: Entity,
    pub team: Team,
    pub position: Vec3,
    pub sniper: bool,
}

/// What bots share within one tick of `think`.
pub(super) struct Cx<'a> {
    /// Rays left this tick for cover searches and fire lanes, and for near-miss checks.
    pub rays: &'a mut u32,
    pub near_rays: &'a mut u32,
    pub shots: &'a [Shot],
    /// Cover spots bots hold or run to (last tick's), kept apart.
    pub taken: &'a [Vec3],
    pub spots: &'a mut Vec<SpotCall>,
    pub reports: &'a mut SquadReports,
}

/// Rays per tick for cover searches and fire lanes, and for near-miss checks.
pub(super) const COVER_RAYS: u32 = 32;
pub(super) const NEAR_RAYS: u32 = 24;

impl BotBrain {
    /// Whether this bot plays with the tactics of this module.
    pub fn tactical(&self) -> bool {
        self.tactical
    }

    /// What it is doing, in a word or two (debug views, scenarios).
    pub fn doing(&self) -> &'static str {
        self.idle_reason()
    }

    /// At its cover in a firefight (hiding or up shooting), and whether up.
    pub fn fighting_from_cover(&self) -> Option<bool> {
        let cover = self.cover?;
        (self.fighting > 0.0 && cover.peek.is_some()).then_some(self.exposed)
    }

    /// An enemy in sight, and how suppressed it is (0..1.5).
    pub fn combat_state(&self) -> (bool, f32) {
        (self.target.is_some(), self.suppression)
    }

    /// Enemy fire passing close: suppression, and where it came from.
    pub(super) fn feel_fire(&mut self, w: &Senses, me: &Me, skill: Skill, team_stats: &mut TeamStats, cx: &mut Cx, dt: f32) {
        self.shot_at_ago += dt;
        if self.shot_at_ago > 0.4 {
            let recovery = 0.4 + 0.3 * self.personality.courage + 0.2 * skill.0;
            self.suppression = (self.suppression - dt * recovery).max(0.0);
        }
        let chest = me.motion.position + Vec3::Y * chest_height(me.motion.stance);
        let mut closest: Option<(f32, &Shot)> = None;
        for shot in cx.shots.iter().filter(|s| s.team != me.team && s.soldier != me.soldier) {
            let to = chest - shot.eye;
            let along = to.dot(shot.dir);
            if !(3.0..shot.range).contains(&along) {
                continue;
            }
            let miss = (to - shot.dir * along).length();
            if miss < NEAR_MISS && closest.is_none_or(|(m, _)| miss < m) {
                closest = Some((miss, shot));
            }
        }
        let Some((miss, shot)) = closest else {
            return;
        };
        // The bullets have to get here: nothing in the way well before us (checked once per
        // burst from a shooter).
        let known = self.target == Some(shot.soldier) || (self.near_shooter == Some(shot.soldier) && self.shot_at_ago < 0.5);
        if !known {
            if *cx.near_rays == 0 {
                return;
            }
            *cx.near_rays -= 1;
            let along = (chest - shot.eye).dot(shot.dir);
            let point = shot.eye + shot.dir * (along - 2.0).max(0.5);
            if !tactics::line_of_sight(&w.spatial, shot.eye, point) {
                return;
            }
        }
        if self.shot_at_ago > 2.0 {
            team_stats.suppressed += 1;
        }
        self.near_shooter = Some(shot.soldier);
        self.shot_at_ago = 0.0;
        let nerve = 1.0 - 0.35 * self.personality.courage;
        self.suppression = (self.suppression + suppression_rate(miss) * dt * nerve).min(1.5);
        self.memory.learn(shot.soldier, shot.eye - Vec3::Y * 0.4, Source::Shot);
        if self.target.is_none() {
            // Turn towards the shooter.
            self.threat = Some((shot.soldier, shot.feet));
            self.alert = self.alert.max(1.5);
        }
    }

    /// An enemy it doesn't see (yet): heard firing or walking, or spotted for the team.
    #[allow(clippy::too_many_arguments)]
    pub(super) fn notice(
        &mut self,
        w: &Senses,
        me: &Me,
        enemy: Entity,
        motion: &game_shared::soldier::SoldierMotion,
        firing: bool,
        distance: f32,
        hearing: f32,
    ) {
        let chest = motion.position + Vec3::Y * chest_height(motion.stance);
        if w.spotted.get(enemy).is_ok_and(|s| s.by == me.team) {
            self.memory.learn(enemy, chest, Source::Team);
            return;
        }
        let speed = flat(motion.velocity).length();
        let footsteps = match motion.stance {
            Stance::Standing if speed > 5.0 => hearing * 0.3,
            Stance::Standing if speed > 2.5 => hearing * 0.16,
            _ => 0.0,
        };
        if !(firing && distance < hearing) && distance >= footsteps {
            return;
        }
        // Roughly where: off by up to about a tenth of the distance.
        let error = distance * 0.08;
        let at = chest + Vec3::new(gaussian() * error, 0.0, gaussian() * error);
        self.memory.learn(enemy, at, Source::Heard);
    }

    /// What teammates saw lately around here.
    pub(super) fn team_knowledge(&mut self, me: &Me, intel: &TeamIntel, range: f32) {
        let seen: Vec<(Entity, Vec3, f32)> = intel.sightings_near(me.team, me.motion.position, range * 0.6, 3.0).collect();
        for (enemy, at, age) in seen {
            if age < 3.0 && self.target != Some(enemy) {
                self.memory.learn(enemy, at, Source::Team);
            }
        }
    }

    /// Calls out a newly seen enemy on the radio (the commo rose's spot), now and then.
    pub(super) fn call_spot(&mut self, w: &Senses, me: &Me, skill: Skill, cx: &mut Cx) {
        let Some(target) = self.target else {
            return;
        };
        if self.spot_cooldown > 0.0 || w.spotted.get(target).is_ok_and(|s| s.by == me.team) {
            return;
        }
        if fastrand::f32() > 0.3 + 0.5 * skill.tactics() {
            self.spot_cooldown = 5.0;
            return;
        }
        self.spot_cooldown = 12.0 + 12.0 * fastrand::f32();
        let sniper = w
            .soldiers
            .get(target)
            .ok()
            .and_then(|s| s.6)
            .is_some_and(|l| l.kit.to_ascii_lowercase().contains("sniper"));
        cx.spots.push(super::combat::SpotCall {
            player: me.player,
            target,
            team: me.team,
            position: me.motion.position,
            sniper,
        });
    }

    /// Whether `soldier` is in this bot's squad.
    pub(super) fn is_squad_mate(&self, w: &Senses, me: &Me, soldier: Entity) -> bool {
        let Some(member) = me.member else {
            return false;
        };
        let Some(player) = w.soldiers.get(soldier).ok().map(|s| s.2.0) else {
            return false;
        };
        w.snapshot
            .squads
            .get(&(me.team, member.squad))
            .is_some_and(|s| s.leader == Some(player) || s.members.contains(&player))
    }

    /// The fire team this bot is in (see [`squad::fire_team`]).
    fn fire_team(&self, w: &Senses, me: &Me) -> u8 {
        let slot = me
            .member
            .filter(|m| !m.leader)
            .and_then(|m| w.snapshot.squads.get(&(me.team, m.squad)).and_then(|s| s.slot(me.player)));
        squad::fire_team(slot)
    }

    /// Where the danger it fights is: the target's eye, the most pressing contact, or the
    /// threat point.
    fn threat_eye(&self, w: &Senses, me: &Me) -> Option<Vec3> {
        self.target
            .and_then(|t| w.soldiers.get(t).ok())
            .map(|s| s.1.eye_position())
            .or_else(|| {
                self.memory
                    .most_pressing(me.motion.position, 5.0)
                    .filter(|c| c.source >= Source::Shot)
                    .map(|c| c.position + Vec3::Y * 0.4)
            })
            .or_else(|| self.threat_point())
    }

    /// Whether the main weapon's magazine is below this fraction (with spare ammo).
    fn magazine_low(&self, w: &Senses, me: &Me, fraction: f32) -> bool {
        let Some(weapon) = w.weapon(me.loadout, self.primary).filter(|w| w.magazine_size > 0) else {
            return false;
        };
        me.inventory
            .and_then(|i| i.ammo.get(self.primary as usize))
            .is_some_and(|a| (a[0] as f32) < weapon.magazine_size as f32 * fraction && a[1] > 0)
    }

    /// At its cover (hiding or peeking) from a threat there.
    fn at_cover(&self, position: Vec3, threat: Option<Vec3>) -> bool {
        self.cover.is_some_and(|c| {
            let near = flat(c.spot - position).length() < 2.0 || c.peek.is_some_and(|p| flat(p - position).length() < 1.2);
            near && threat.is_none_or(|t| c.still_good(t))
        })
    }

    /// The tactical options, weighed against the others in `decide`.
    pub(super) fn tactical_options(
        &mut self,
        w: &Senses,
        me: &Me,
        skill: Skill,
        target: Option<game_shared::soldier::SoldierMotion>,
        best: &mut (f32, Activity),
        cx: &mut Cx,
    ) {
        let consider = |best: &mut (f32, Activity), utility: f32, activity: Activity| {
            if utility > best.0 {
                *best = (utility, activity);
            }
        };
        let position = me.motion.position;
        let eye = me.motion.eye_position();
        let hp = me.health_fraction;
        let aggression = self.personality.aggression;
        let depth = skill.tactics();
        let busy = matches!(
            self.activity,
            Activity::Charge { .. } | Activity::Mount { .. } | Activity::Demolish { .. } | Activity::Revive { .. }
        );
        let threat = self.threat_eye(w, me);
        let fighting = self.fighting > 0.0;
        let key = me.member.map(|m| (me.team, m.squad));
        let tactic = key.and_then(|k| w.tactics.squads.get(&k)).copied();
        let team_no = self.fire_team(w, me);
        let target_distance = target.map(|t| t.position.distance(position));
        let covered = self.at_cover(position, threat);

        // Cover to fight from.
        if let (Some(threat), Some(nav)) = (threat, w.nav())
            && fighting
            && self.wants_cover
            && !busy
            && self.cover_cooldown <= 0.0
            && !covered
            && !matches!(self.activity, Activity::TakeCover { .. })
            // Up close, a healthy bot fights it out.
            && flat(threat - position).length() > if self.suppression > 0.5 || hp < 0.6 { 10.0 } else { 18.0 }
            && *cx.rays >= 12
        {
            let leader = me.member.is_some_and(|m| m.leader);
            let urgency = 2.5 * self.suppression.min(1.0)
                + 2.0 * (1.0 - hp)
                + if self.magazine_low(w, me, 0.25) { 1.5 } else { 0.0 }
                + if leader { 0.8 } else { 0.0 };
            if 8.8 + urgency > best.0 {
                let query = CoverQuery {
                    from: position,
                    threat,
                    radius: 14.0,
                    toward: None,
                    taken: cx.taken,
                    fire: true,
                };
                // With an enemy in sight, running for cover means not shooting back: only for
                // cover close by, unless pinned down or hurt.
                let pressed = self.suppression > 0.5 || hp < 0.6 || self.magazine_low(w, me, 0.15);
                let reach = if target.is_some() && !pressed { 6.0 } else { 14.0 };
                let query = CoverQuery { radius: reach, ..query };
                match cover::find(nav, &w.spatial, &query, cx.rays) {
                    Some(cover) => {
                        let d = flat(cover.spot - position).length();
                        let utility = 8.8 - 0.18 * d + urgency;
                        if utility > best.0 {
                            consider(best, utility, Activity::TakeCover { cover, time: 2.0 + d / 3.0 });
                        } else {
                            self.cover_cooldown = 1.5;
                        }
                    }
                    None => self.cover_cooldown = 2.5,
                }
            }
        }

        // Attackers work forward from cover to cover (the squad's moving fire team, if it
        // bounds).
        let holding_bound = tactic.is_some_and(|t| t.bounding && t.moving != team_no);
        if let Some((OrderKind::Attack, index)) = self.order
            && let (Some(area), Some(threat), Some(nav)) = (w.map.areas.get(index), threat, w.nav())
            && covered
            && fighting
            && !holding_bound
            && hp > 0.45
            && self.suppression < 0.5
            && self.in_cover_time > 4.0 + 5.0 * (1.0 - aggression)
            // Not while trading shots with someone.
            && target_distance.is_none_or(|d| d > 45.0)
            && area.position.distance(position) > area.radius
            && *cx.rays >= 12
        {
            self.in_cover_time = 0.0;
            let toward = (area.position - position).with_y(0.0).normalize_or_zero();
            let query = CoverQuery {
                from: position,
                threat,
                radius: 20.0,
                toward: Some(area.position),
                taken: cx.taken,
                fire: true,
            };
            if let Some(cover) = cover::find(nav, &w.spatial, &query, cx.rays)
                && (cover.spot - position).with_y(0.0).dot(toward) > 5.0
            {
                let d = flat(cover.spot - position).length();
                consider(best, 8.0, Activity::TakeCover { cover, time: 2.0 + d / 3.0 });
            }
        }

        // The enemy went out of sight: watch (and peek) from cover.
        if target.is_none()
            && covered
            && let Some(contact) = self.memory.most_pressing(position, 8.0).filter(|c| c.source >= Source::Shot)
        {
            consider(best, 4.8, Activity::Watch { at: contact.position, time: 3.0 + 3.0 * (1.0 - aggression) });
        }

        // Suppressive fire: fire team 0 of a pinned squad, or now and then from cover, at
        // where an enemy ducked out of sight.
        let pinned = tactic.and_then(|t| t.pinned);
        if target.is_none()
            && self.suppress_cooldown <= 0.0
            && !busy
            && *cx.rays > 0
            && self.has_spare_magazine(w, me)
            && let Some(contact) = self
                .memory
                .most_pressing(position, 6.0)
                .filter(|c| c.source == Source::Seen && c.age > 0.4)
                .copied()
            && (12.0..130.0).contains(&flat(contact.position - position).length())
        {
            let role = pinned.is_some() && team_no == 0;
            let utility = if role {
                6.2
            } else if covered {
                3.5 * (0.5 + aggression)
            } else {
                0.0
            };
            if utility > best.0 && fastrand::f32() < 0.3 + 0.7 * depth {
                *cx.rays -= 1;
                if cover::fire_lane(&w.spatial, eye, contact.position) {
                    consider(best, utility, Activity::Suppress { at: contact.position, time: 2.5 + 2.0 * fastrand::f32() });
                } else {
                    self.suppress_cooldown = 2.0;
                }
            }
        }

        // Pinned down: fire team 1 goes round the enemy (those who roll for it this time).
        if let (Some(tactic), Some(at), Some(nav)) = (tactic, pinned, w.nav())
            && team_no == 1
            && !busy
            && self.flanked_episode != tactic.episode
            && !matches!(self.activity, Activity::Flank { .. })
            && 6.8 > best.0
        {
            // Once per episode, those who roll for it.
            self.flanked_episode = tactic.episode;
            if hash01(self.seed, tactic.episode) < 0.4 + 0.6 * depth
                && let Some(spot) = tactics::flank_spot(nav, position, at, tactic.flank_side)
            {
                consider(best, 6.8, Activity::Flank { spot, time: 25.0 });
            }
        }

        // Medics and support run their bags to squad mates who need them.
        if target.is_none()
            && self.hurt_ago > 4.0
            && self.bag_cooldown <= 0.0
            && 3.8 > best.0
            && !busy
            && !matches!(self.activity, Activity::Supply { .. })
            && let Some(soldier) = self.squad_mate_in_need(w, me)
        {
            consider(best, 3.8, Activity::Supply { soldier, time: 20.0 });
        }
    }

    /// Spare ammunition for at least a magazine of the main weapon.
    fn has_spare_magazine(&self, w: &Senses, me: &Me) -> bool {
        let Some(weapon) = w.weapon(me.loadout, self.primary).filter(|w| w.fire.kind == FireKind::Gun) else {
            return false;
        };
        me.inventory
            .and_then(|i| i.ammo.get(self.primary as usize))
            .is_some_and(|a| a[0] > 0 && a[1] as u32 >= weapon.magazine_size.max(1))
    }

    /// A squad mate within reach who is hurt (medics) or low on ammo (support).
    fn squad_mate_in_need(&self, w: &Senses, me: &Me) -> Option<Entity> {
        let member = me.member?;
        let squad = w.snapshot.squads.get(&(me.team, member.squad))?;
        let position = me.motion.position;
        w.soldiers
            .iter()
            .filter(|s| s.0 != me.soldier && !s.8 && s.7.is_none())
            .filter(|s| squad.leader == Some(s.2.0) || squad.members.contains(&s.2.0))
            .filter(|s| (BAG_REACH..45.0).contains(&flat(s.1.position - position).length()))
            .filter(|s| {
                let hurt = self.medic_bag.is_some() && s.4.is_some_and(|h| h.current < h.max * 0.6);
                let empty = self.ammo_bag.is_some() && s.3.zip(s.6).is_some_and(|(i, l)| low_on_ammo(i, l, &w.armory));
                hurt || empty
            })
            .min_by(|a, b| a.1.position.distance(position).total_cmp(&b.1.position.distance(position)))
            .map(|s| s.0)
    }

    /// Fighting from cover: up to shoot, down to reload, when the fire gets close or when
    /// just hurt. Sets where to stand and the stance; `Some(up)` while at its cover.
    pub(super) fn fight_from_cover(&mut self, me: &Me, intent: &mut Intent, dt: f32) -> Option<bool> {
        let cover = self.cover?;
        let peek = cover.peek?;
        let position = me.motion.position;
        let near = flat(cover.spot - position).length() < 2.0 || flat(peek - position).length() < 1.2;
        if !near {
            return None;
        }
        // Up with the enemy in sight and nobody shooting back: stays up longer.
        let winning = self.exposed && self.target.is_some() && self.suppression < 0.4 && self.hurt_ago > 2.0;
        self.peek_timer -= if winning { dt * 0.4 } else { dt };
        let reloading = me.inventory.is_some_and(|i| i.reloading);
        let hide = self.suppression > 0.75 || self.hurt_ago < 0.4 || reloading;
        if self.exposed && (self.peek_timer <= 0.0 || hide) {
            self.exposed = false;
            self.peek_timer = 0.7 + 1.0 * fastrand::f32() + 0.8 * self.suppression.min(1.0);
        } else if !self.exposed && self.peek_timer <= 0.0 && !hide {
            self.exposed = true;
            self.peek_timer = 1.5 + 2.0 * fastrand::f32() * (0.5 + self.personality.aggression);
        }
        let at = if self.exposed { peek } else { cover.spot };
        if flat(at - position).length() > 0.6 {
            intent.goal = Some(Goal {
                position: at,
                tolerance: 0.5,
                sprint: false,
            });
        }
        if cover.low && !self.exposed {
            intent.buttons |= Buttons::CROUCH;
        }
        if !self.exposed
            && me.inventory.is_some_and(|i| !i.reloading)
            && self.primary_magazine_below(me, 0.6)
        {
            intent.buttons |= Buttons::RELOAD;
        }
        Some(self.exposed)
    }

    /// Whether the main weapon's magazine is below this fraction of its size (with ammo to
    /// reload), from the inventory alone.
    fn primary_magazine_below(&self, me: &Me, fraction: f32) -> bool {
        let Some(inventory) = me.inventory else {
            return false;
        };
        inventory
            .ammo
            .get(self.primary as usize)
            .is_some_and(|a| a[1] > 0 && (a[0] as f32) < self.magazine_size as f32 * fraction)
    }

    /// Runs to cover, then fights from it (or watches from it).
    pub(super) fn take_cover(&mut self, w: &Senses, me: &Me, cover: CoverSpot, time: f32, intent: &mut Intent, dt: f32) {
        let position = me.motion.position;
        let d = flat(cover.spot - position).length();
        if d < 1.0 || time <= 0.0 {
            if d < 2.0 {
                self.cover = Some(cover);
                self.in_cover_time = 0.0;
                self.exposed = false;
                self.peek_timer = 0.3 + 0.5 * fastrand::f32();
            } else {
                self.cover_cooldown = 3.0;
            }
            self.activity = if self.target.is_some() {
                Activity::Engage
            } else if let Some(contact) = self.memory.most_pressing(position, 8.0) {
                Activity::Watch { at: contact.position, time: 3.0 }
            } else {
                Activity::Objective
            };
            return;
        }
        intent.goal = Some(Goal {
            position: cover.spot,
            tolerance: 0.5,
            sprint: d > 4.0,
        });
        if d < 3.0
            && let Some(threat) = self.threat_eye(w, me)
        {
            intent.look = Look::At(threat);
        }
        self.activity = Activity::TakeCover { cover, time: time - dt };
    }

    /// In cover with the enemy out of sight: looks at where he was, peeking now and then.
    pub(super) fn watch(&mut self, _w: &Senses, me: &Me, at: Vec3, time: f32, intent: &mut Intent, dt: f32) {
        if time <= 0.0 {
            self.activity = Activity::Objective;
            return;
        }
        intent.look = Look::At(at);
        if self.fight_from_cover(me, intent, dt).is_none() {
            intent.buttons |= Buttons::CROUCH;
            if self.primary_magazine_below(me, 0.6) {
                intent.buttons |= Buttons::RELOAD;
            }
        }
        self.activity = Activity::Watch { at, time: time - dt };
    }

    /// Suppressive fire at `at`: bursts into where the enemy hides, to keep his head down.
    #[allow(clippy::too_many_arguments)]
    pub(super) fn suppress(&mut self, w: &Senses, me: &Me, skill: Skill, at: Vec3, time: f32, intent: &mut Intent, dt: f32) {
        if time <= 0.0 || self.target.is_some() {
            self.activity = if self.target.is_some() { Activity::Engage } else { Activity::Objective };
            self.suppress_cooldown = 6.0 + 6.0 * fastrand::f32();
            return;
        }
        let eye = me.motion.eye_position();
        let to = at - eye;
        let distance = to.length();
        // Spray around the spot, not at a man.
        let spread = 1.2 / distance.max(1.0);
        let desired_yaw = yaw_to(to) + self.aim_error.x;
        let desired_pitch = to.y.atan2(Vec2::new(to.x, to.z).length()) + self.aim_error.y;
        self.aim_error = (self.aim_error * (1.0 - dt) + Vec2::new(gaussian(), gaussian()) * spread * dt * 3.0)
            .clamp_length_max(spread * 2.0);
        self.yaw = turn_towards(self.yaw, desired_yaw, skill.turn_rate() * dt);
        self.pitch += (desired_pitch - self.pitch).clamp(-3.0 * dt, 3.0 * dt);
        intent.look = Look::Aimed;
        intent.weapon = Some(self.primary);
        let on_target = angle_delta(self.yaw, desired_yaw).abs() < 0.1 && (self.pitch - desired_pitch).abs() < 0.1;
        let mode = w
            .weapon(me.loadout, self.primary)
            .and_then(|weapon| weapon.fire_modes.get(me.inventory.map_or(0, |i| i.fire_mode) as usize).copied())
            .unwrap_or(FireMode::Auto);
        if on_target {
            self.burst -= dt;
            match mode {
                FireMode::Auto => {
                    if self.burst < -0.35 - fastrand::f32() * 0.3 {
                        self.burst = 0.25 + 0.3 * fastrand::f32();
                    }
                    if self.burst > 0.0 {
                        intent.buttons |= Buttons::FIRE;
                    }
                }
                FireMode::Single | FireMode::Burst => {
                    if self.burst <= 0.0 {
                        intent.buttons |= Buttons::FIRE;
                        self.burst = 0.25 + 0.25 * fastrand::f32();
                    }
                }
            }
        }
        match self.cover {
            Some(cover) if cover.low && flat(cover.spot - me.motion.position).length() < 2.0 => {}
            _ => intent.buttons |= Buttons::CROUCH,
        }
        self.activity = Activity::Suppress { at, time: time - dt };
    }

    /// To a squad mate who needs a bag, holding it out once close.
    pub(super) fn supply(&mut self, w: &Senses, me: &Me, soldier: Entity, time: f32, intent: &mut Intent, dt: f32) {
        let mate = w.soldiers.get(soldier).ok().filter(|s| !s.8 && s.7.is_none()).map(|s| s.1.position);
        let (Some(at), true) = (mate, time > 0.0) else {
            self.activity = Activity::Objective;
            return;
        };
        let d = flat(at - me.motion.position).length();
        if d < BAG_REACH * 0.6 {
            // Close enough: the bag held out in `decide` does the rest.
            self.activity = Activity::Objective;
            self.bag_cooldown = 6.0;
            return;
        }
        intent.goal = Some(Goal {
            position: at,
            tolerance: 1.5,
            sprint: d > 12.0,
        });
        if d < 10.0 {
            intent.weapon = self.medic_bag.or(self.ammo_bag);
        }
        self.activity = Activity::Supply { soldier, time: time - dt };
    }

    /// Bookkeeping after every tick on foot: memory, cover, the firefight, statistics and
    /// the squad report.
    pub(super) fn after_tick(&mut self, w: &Senses, me: &Me, team_stats: &mut TeamStats, cx: &mut Cx, dt: f32) {
        let position = me.motion.position;
        // Engaged: an enemy in sight, one just lost, or hurt just now (the same test for
        // both behaviours, for the statistics).
        let engaged = self.target.is_some() || self.last_seen.is_some_and(|(_, age)| age < 4.0) || self.hurt_ago < 3.0;
        // Every half second: is it in cover from what it fights? A ray from the threat's
        // eye to the middle of its body.
        if engaged && (self.seq.wrapping_add(self.seed)) % 30 == 0 {
            let threat = self
                .target
                .and_then(|t| w.soldiers.get(t).ok())
                .map(|s| s.1.eye_position())
                .or(self.last_seen.map(|(at, _)| at + Vec3::Y * 0.4))
                .or(self.threat.map(|(_, at)| at + Vec3::Y * 1.5));
            if let Some(threat) = threat {
                let body = position + Vec3::Y * chest_height(me.motion.stance) * 0.75;
                team_stats.engaged_seconds += 0.5;
                if !tactics::line_of_sight(&w.spatial, threat, body) {
                    team_stats.covered_seconds += 0.5;
                }
            }
        }
        if !self.tactical {
            return;
        }
        self.cover_cooldown -= dt;
        self.suppress_cooldown -= dt;
        self.spot_cooldown -= dt;
        let skill = self.personality.skill(&w.settings, me.team);
        self.memory.tick(dt, skill.memory(), |e| w.soldiers.get(e).is_ok_and(|s| !s.8));
        if let Some(cover) = self.cover {
            let away = flat(cover.spot - position).length() > 4.0
                && cover.peek.is_none_or(|p| flat(p - position).length() > 4.0);
            // Attackers don't camp: with little fire coming in, a while in cover is enough, on
            // to the flag.
            let attacking = self.order.is_some_and(|(kind, area)| {
                kind == OrderKind::Attack
                    && w.map.areas.get(area).is_some_and(|a| a.position.distance(position) > a.radius)
            });
            let camped = attacking
                && self.suppression < 0.3
                && self.in_cover_time > 10.0 + 6.0 * (1.0 - self.personality.aggression);
            if away || camped {
                self.cover = None;
                if camped {
                    self.cover_cooldown = 6.0;
                    self.in_cover_time = 0.0;
                }
            } else {
                self.in_cover_time += dt;
            }
        }
        let fighting = engaged || self.suppression > 0.3;
        if fighting {
            if self.fighting <= 0.0 {
                self.fighting = 0.0;
                self.roll_cover(skill);
            }
            let before = self.fighting;
            self.fighting += dt;
            if (before / 8.0).floor() != (self.fighting / 8.0).floor() {
                self.roll_cover(skill);
            }
        } else {
            if self.fighting > 0.0 {
                self.fighting = 0.0;
            }
            self.fighting -= dt;
        }
        if let Some(member) = me.member
            && fighting
        {
            let report = cx.reports.0.entry((me.team, member.squad)).or_default();
            report.engaged += 1;
            report.suppressed += u32::from(self.suppression > 0.5);
            if let Some(contact) = self.memory.most_pressing(position, 5.0).filter(|c| c.position.distance(position) < 150.0) {
                report.contact_sum += contact.position;
                report.contacts += 1;
            }
        }
        if self.cover.is_some() && fighting {
            team_stats.cover_fights += dt;
        }
    }

    /// Whether to fight from cover this time (by difficulty and courage).
    fn roll_cover(&mut self, skill: Skill) {
        let chance = (0.15 + skill.tactics()) * (0.8 + 0.4 * (1.0 - self.personality.courage));
        self.wants_cover = fastrand::f32() < chance;
    }

    /// Bounding overwatch (see [`squad`]): fire team 1 bounds past the leader while his team
    /// holds and covers, then the leader's team moves while team 1 covers. Returns whether it
    /// took care of the movement (the leader and fire team 0 move as usual when it's their
    /// turn).
    #[allow(clippy::too_many_arguments)]
    pub(super) fn bound(
        &mut self,
        w: &Senses,
        me: &Me,
        tactic: &squad::SquadTactic,
        squad: &squad::SquadInfo,
        leader: Option<&squad::SoldierInfo>,
        is_leader: bool,
        intent: &mut Intent,
        dt: f32,
    ) -> bool {
        let position = me.motion.position;
        let slot = squad.slot(me.player);
        let team_no = squad::fire_team(if is_leader { None } else { slot });
        if tactic.phase != self.bound_phase {
            self.bound_phase = tactic.phase;
            self.overwatch = (tactic.moving != team_no).then_some(position);
        }
        if tactic.moving == team_no {
            self.overwatch = None;
            if is_leader || team_no == 0 {
                return false;
            }
            let mut place = squad::bound_slot(tactic, tactic.anchor, slot.unwrap_or(0));
            if let Some(nav) = w.nav()
                && let Some(region) = nav.locate(position, 2.0, None).map(|c| nav.cell(c).region)
                && let Some(cell) = nav.locate(place, 6.0, Some(region))
            {
                place = nav.position(cell);
            }
            let d = flat(place - position).length();
            if d > 1.5 {
                intent.goal = Some(Goal {
                    position: place,
                    tolerance: 2.0,
                    sprint: d > 8.0,
                });
            } else {
                self.hold_facing(me, tactic, intent, dt);
            }
            return true;
        }
        // Too far behind the leader to cover him: catch up instead.
        if !is_leader && leader.is_some_and(|l| l.position.distance(position) > 45.0) {
            self.overwatch = None;
            return false;
        }
        let hold = *self.overwatch.get_or_insert(position);
        if flat(hold - position).length() > 3.0 {
            intent.goal = Some(Goal {
                position: hold,
                tolerance: 1.0,
                sprint: false,
            });
        } else {
            self.hold_facing(me, tactic, intent, dt);
        }
        true
    }

    /// Holding while the other fire team moves: low, watching the way ahead (or where the
    /// enemy was).
    fn hold_facing(&mut self, me: &Me, tactic: &squad::SquadTactic, intent: &mut Intent, dt: f32) {
        intent.buttons |= Buttons::CROUCH;
        self.sweep += dt * 0.5;
        let facing = self
            .memory
            .threat_axis(me.motion.position, 10.0)
            .map_or(yaw_to(tactic.axis), yaw_to);
        intent.look = Look::Yaw(facing + self.sweep.sin() * 0.7);
    }

    /// The cover it holds or runs to, kept clear by others.
    pub(super) fn claimed_cover(&self) -> Option<Vec3> {
        match self.activity {
            Activity::TakeCover { cover, .. } => Some(cover.spot),
            _ => self.cover.map(|c| c.spot),
        }
    }

    /// Per-minute idle diagnostics: what it is doing (and why, when carrying out its order).
    pub(super) fn idle_reason(&self) -> &'static str {
        match self.activity {
            Activity::Objective if self.stranded > 0.0 => "stranded",
            Activity::Objective if self.overwatch.is_some() => "overwatch",
            Activity::Objective if self.regroup > 0.0 => "waiting for squad",
            Activity::Objective if self.goal.is_none() => "holding",
            Activity::Objective => "moving",
            Activity::Engage => "fighting",
            Activity::Cover { .. } | Activity::TakeCover { .. } | Activity::Watch { .. } => "cover",
            Activity::Suppress { .. } => "suppressing",
            Activity::Search { .. } => "searching",
            Activity::Revive { .. } | Activity::Supply { .. } => "medic/supply",
            Activity::Repair { .. } => "repairing",
            Activity::Mount { .. } => "mounting",
            Activity::Charge { .. } => "charge",
            _ => "other",
        }
    }
}
