//! Man down, and what medics, support soldiers and engineers do for their team, after BF2.
//!
//! - A soldier whose health runs out is critically wounded ([`Downed`]) unless the blow took
//!   him more than `WRECK_HIT_POINTS` (320) below zero (or he was in a vehicle): he lies where he
//!   fell for [`MAN_DOWN_SECONDS`] (`sv.manDownTime`), only blasts and enemy shock paddles
//!   hurt him further, and a medic's shock paddles bring him back with `restoreHP` hit
//!   points. He dies when the time runs out or he gives up, and only then does his team lose
//!   the ticket (BF2 `onPlayerDeath`; the kill itself is scored when he goes down, BF2
//!   `onPlayerKilled`).
//! - Kits with replenishing gadgets have an ability charge that refills at the kit's
//!   `abilityRestoreRate`. A thrown bag or a shock costs `abilityCost` of it; replenishing
//!   with a gadget in hand drains `abilityDrain` per second.
//! - In hand, the medic bag heals teammates (and the medic) within its radius, the ammo bag
//!   resupplies them, and the wrench, while its trigger is held, repairs vehicles and
//!   damaged destroyable objects. Rates go through the damage table (heal 73 against the
//!   soldier's 24, repair 84 against a vehicle's `armor.defaultMaterial`: 2 for soft
//!   vehicles, 1 for armor, 10 for small objects), in percent of the target's maximum.
//! - A thrown bag is picked up by the first soldier who walks over it and needs it, and
//!   gives `replenishingStrength` percent at once.
//! - Scoring as BF2's `scoringCommon.py`: +2 per revive, +1 per 100 hit points healed or
//!   repaired or 100 percent of a loadout resupplied (once per helped player every 30 s),
//!   +2 per kill, -4 per teamkill, -2 per suicide, +1 per kill assist.

use avian3d::prelude::*;
use bevy::{ecs::system::SystemParam, platform::collections::HashMap, prelude::*};
use bevy_replicon::prelude::*;
use game_data::{ReplenishDesc, ReplenishKind, WeaponDesc};
use game_shared::{
    conquest::RoundState,
    physics::GameLayer,
    projectile::{Projectile, ProjectileMotion},
    protocol::{ControlledBy, KillFeed, Player, Score, Team},
    revive::{Downed, GiveUp, MAN_DOWN_SECONDS, NoticeKind, ReplenishNotice},
    soldier::{Health, Soldier, SoldierMotion, Stance},
    statics::{Destructible, DestroyedStatics, Inactive},
    vehicle::{Seated, VehicleData, VehicleHealth},
    weapons::{Armory, Inventory, Loadout},
};

use crate::{
    AppliedInput, ClientPlayer, Controls, HostPlayer, PlayerClient, RespawnTimer, ServerSettings,
    combat::{CombatSystems, Died},
    destruction::{Materials, ObjectHealth},
    sender_player,
};

pub struct AbilitiesPlugin;

impl Plugin for AbilitiesPlugin {
    fn build(&self, app: &mut App) {
        app.init_resource::<Assists>()
            .init_resource::<Ledger>()
            .init_resource::<AmmoCredit>()
            .add_systems(
                PreUpdate,
                receive_give_up
                    .after(ServerSystems::Receive)
                    .run_if(in_state(ClientState::Disconnected)),
            )
            .add_systems(
                FixedUpdate,
                (
                    charge_gadgets,
                    revive_with_paddles,
                    pick_up_bags,
                    replenish_in_hand,
                    bleed_out,
                    send_notices,
                    forget_the_gone,
                )
                    .chain()
                    .in_set(AbilitySystems)
                    .after(CombatSystems)
                    .run_if(in_state(ClientState::Disconnected)),
            );
    }
}

/// Gadgets, revives and bleeding out, after [`CombatSystems`] (tickets are counted after).
#[derive(SystemSet, Debug, Clone, PartialEq, Eq, Hash)]
pub struct AbilitySystems;

/// Damage table column of a soldier's body (`armor.defaultMaterial`).
const SOLDIER_MATERIAL: u32 = 24;
/// Damage table row of the wrench (`Engineer_repair`); without a table only it repairs.
const REPAIR_MATERIAL: u32 = 84;
/// Hit points or percent of a loadout per replenish score (BF2 `HEAL_POINT_LIMIT`,
/// `REPAIR_POINT_LIMIT`, `GIVEAMMO_POINT_LIMIT`).
const POINTS_PER_SCORE: f32 = 100.0;
/// A player helped again this soon earns the helper no more (`REPLENISH_POINT_MIN_INTERVAL`).
const GRIND_BLOCK_SECONDS: f64 = 30.0;
/// Scores (`SCORE_KILL`, `SCORE_TEAMKILL`, `SCORE_SUICIDE`, `SCORE_REVIVE`,
/// `SCORE_KILLASSIST_DAMAGE`).
const SCORE_KILL: i32 = 2;
const SCORE_TEAMKILL: i32 = -4;
const SCORE_SUICIDE: i32 = -2;
const SCORE_REVIVE: i32 = 2;
const SCORE_ASSIST: i32 = 1;
/// Damage someone other than the killer must have dealt for an assist: half a soldier's
/// 100 hit points (BF2's threshold is in the engine; half is the commonly quoted rule).
const ASSIST_DAMAGE: f32 = 50.0;
/// Shock paddles reach this far from the eye: their projectile flies 80 m/s for 0.035 s.
pub const PADDLES_REACH: f32 = 2.8;
/// Seconds between replenish notices.
const NOTICE_INTERVAL: f32 = 1.0;
/// Seconds a revived soldier stays down before he can get up (about the length of BF2's
/// `3p_reviveOnBack`).
const REVIVE_RECOVERY: f32 = 2.5;

/// Server-side, next to [`Downed`]: how long a downed soldier has left, and how long he
/// has been down.
#[derive(Component)]
pub(crate) struct BleedOut {
    left: f32,
    pub(crate) down_for: f32,
}

/// Server-side: a kit's ability charge (0..1), and what its gadgets had left last tick.
#[derive(Component)]
struct KitAbility {
    charge: f32,
    restore: f32,
    /// Uses left per loadout weapon as last set here (magazine plus spare).
    left: Vec<u16>,
}

/// Damage each player dealt to each soldier, for kill assists.
#[derive(Resource, Default)]
pub(crate) struct Assists(HashMap<Entity, Vec<(Entity, f32)>>);

impl Assists {
    pub(crate) fn record(&mut self, victim: Entity, attacker: Option<Entity>, damage: f32) {
        let Some(attacker) = attacker else {
            return;
        };
        let dealt = self.0.entry(victim).or_default();
        match dealt.iter_mut().find(|(player, _)| *player == attacker) {
            Some((_, total)) => *total += damage,
            None => dealt.push((attacker, damage)),
        }
    }
}

/// Rounds each resupplied weapon has been given towards its next whole one.
#[derive(Resource, Default)]
struct AmmoCredit(HashMap<(Entity, usize), f32>);

/// Replenish points, the grind block, and notices waiting to be sent.
#[derive(Resource, Default)]
struct Ledger {
    /// Points towards the next score, per helper and kind.
    points: HashMap<(Entity, NoticeKind), f32>,
    /// When a helper last scored for helping a player.
    scored: HashMap<(Entity, Entity), f64>,
    /// Given since the last notice: (helper, helped player, kind) -> (amount, score).
    pending: HashMap<(Entity, Option<Entity>, NoticeKind), (f32, i32)>,
    since_notice: f32,
}

impl Ledger {
    /// Books `amount` given by `helper` to `helped` (a player, if any) and returns the score
    /// it earned. Helping yourself earns nothing.
    fn give(&mut self, now: f64, helper: Entity, helped: Option<Entity>, kind: NoticeKind, amount: f32) -> i32 {
        if amount <= 0.0 {
            return 0;
        }
        let mut score = 0;
        if helped != Some(helper) {
            let points = self.points.entry((helper, kind)).or_default();
            *points += amount;
            while *points >= POINTS_PER_SCORE {
                *points -= POINTS_PER_SCORE;
                let blocked = helped.is_some_and(|helped| {
                    self.scored
                        .get(&(helper, helped))
                        .is_some_and(|at| now - at < GRIND_BLOCK_SECONDS)
                });
                if !blocked {
                    score += 1;
                    if let Some(helped) = helped {
                        self.scored.insert((helper, helped), now);
                    }
                }
            }
        }
        let pending = self.pending.entry((helper, helped, kind)).or_default();
        pending.0 += amount;
        pending.1 += score;
        score
    }
}

/// Where to send messages meant for a player's human, if it has one.
fn player_client(player: Entity, clients: &Query<&PlayerClient>, host: Option<&HostPlayer>) -> Option<ClientId> {
    if host.is_some_and(|h| h.0 == player) {
        return Some(ClientId::Server);
    }
    clients.get(player).ok().map(|c| ClientId::Client(c.0))
}

/// Kills, going down and dying: the death flow `combat` and the systems here share.
#[derive(SystemParam)]
pub(crate) struct Deaths<'w, 's> {
    commands: Commands<'w, 's>,
    settings: Res<'w, ServerSettings>,
    players: Query<'w, 's, (&'static mut Score, &'static Player, &'static Team)>,
    kills: MessageWriter<'w, ToClients<KillFeed>>,
    died: MessageWriter<'w, Died>,
    assists: ResMut<'w, Assists>,
    notices: MessageWriter<'w, ToClients<ReplenishNotice>>,
    clients: Query<'w, 's, &'static PlayerClient>,
    host: Option<Res<'w, HostPlayer>>,
}

impl Deaths<'_, '_> {
    fn name(&self, player: Entity) -> String {
        self.players.get(player).map_or_else(|_| "?".into(), |(_, p, _)| p.name.clone())
    }

    fn add_score(&mut self, player: Entity, score: i32) {
        if let Ok((mut total, ..)) = self.players.get_mut(player) {
            total.score += score;
        }
    }

    /// Damage `attacker` dealt to `victim`, for kill assists.
    pub(crate) fn record_damage(&mut self, victim: Entity, attacker: Option<Entity>, damage: f32) {
        self.assists.record(victim, attacker, damage);
    }

    /// The killing blow on `soldier` of `victim` (BF2 `onPlayerKilled`): the kill feed, the
    /// killer's score and the assists. What happens to the soldier is [`Self::wound`] or
    /// [`Self::die`].
    pub(crate) fn kill(&mut self, soldier: Entity, victim: Entity, killer: Option<Entity>, weapon: &str, headshot: bool) {
        let victim_team = self.players.get(victim).map(|(_, _, t)| *t).unwrap_or_default();
        let killer = killer.filter(|k| self.players.contains(*k));
        match killer {
            Some(killer) if killer == victim => {
                info!("{} killed himself ({weapon})", self.name(victim));
                self.add_score(killer, SCORE_SUICIDE);
            }
            Some(killer) => {
                let killer_team = self.players.get(killer).map(|(_, _, t)| *t).unwrap_or_default();
                let teamkill = killer_team == victim_team;
                info!(
                    "{} {} {} ({weapon}{})",
                    self.name(killer),
                    if teamkill { "teamkilled" } else { "killed" },
                    self.name(victim),
                    if headshot { ", headshot" } else { "" }
                );
                if teamkill {
                    self.add_score(killer, SCORE_TEAMKILL);
                } else if let Ok((mut score, ..)) = self.players.get_mut(killer) {
                    score.kills += 1;
                    score.score += SCORE_KILL;
                }
            }
            // Falls, scripts, wrecks: `wound` or `die` says what became of him.
            None => {}
        }
        // Enemies other than the killer who did much of the damage.
        let dealt = self.assists.0.remove(&soldier).unwrap_or_default();
        for (assister, damage) in dealt {
            let team = self.players.get(assister).map(|(_, _, t)| *t).ok();
            if Some(assister) == killer || team.is_none_or(|t| t == victim_team) || damage < ASSIST_DAMAGE {
                continue;
            }
            info!("{} assisted in killing {} ({damage:.0} damage)", self.name(assister), self.name(victim));
            self.add_score(assister, SCORE_ASSIST);
            if let Some(client) = player_client(assister, &self.clients, self.host.as_deref()) {
                self.notices.write(ToClients {
                    targets: SendTargets::Single(client),
                    message: ReplenishNotice {
                        kind: NoticeKind::KillAssist,
                        received: false,
                        other: Some(victim),
                        amount: damage,
                        score: SCORE_ASSIST,
                    },
                });
            }
        }
        self.kills.write(ToClients {
            targets: SendTargets::All,
            message: KillFeed {
                killer,
                victim,
                weapon: weapon.to_string(),
                headshot,
            },
        });
    }

    /// `soldier` goes down, critically wounded, facing `yaw`.
    pub(crate) fn wound(&mut self, soldier: Entity, victim: Entity, yaw: f32) {
        info!("{} is critically wounded", self.name(victim));
        self.commands.entity(soldier).insert((
            Downed {
                left: MAN_DOWN_SECONDS,
                yaw,
            },
            BleedOut {
                left: MAN_DOWN_SECONDS,
                down_for: 0.0,
            },
        ));
    }

    /// `soldier` of `player` is dead for good (BF2 `onPlayerDeath`): the body goes, the
    /// team loses a ticket, and the player respawns after the respawn time, counted from
    /// when he went down (`down_for` seconds ago).
    pub(crate) fn die(&mut self, soldier: Entity, player: Entity, down_for: f32) {
        self.commands.entity(soldier).try_despawn();
        self.assists.0.remove(&soldier);
        let mut timer = Timer::from_seconds(self.settings.respawn_seconds, TimerMode::Once);
        timer.tick(std::time::Duration::from_secs_f32(down_for.max(0.0)));
        if let Ok(mut entity) = self.commands.get_entity(player) {
            entity.remove::<Controls>().insert(RespawnTimer(timer));
        }
        let team = match self.players.get_mut(player) {
            Ok((mut score, _, team)) => {
                score.deaths += 1;
                *team
            }
            Err(_) => Team::default(),
        };
        match down_for > 0.0 {
            true => info!("{} died after {down_for:.1} s down", self.name(player)),
            false => info!("{} died", self.name(player)),
        }
        self.died.write(Died { player, team });
    }

    /// A downed `soldier` is back up with `health` hit points, thanks to `medic` (a player).
    fn revive(&mut self, soldier: Entity, player: Entity, medic: Entity) {
        self.commands.entity(soldier).remove::<(Downed, BleedOut)>();
        let (medic_team, team) = (
            self.players.get(medic).map(|(_, _, t)| *t).ok(),
            self.players.get(player).map(|(_, _, t)| *t).ok(),
        );
        // BF2 `onPlayerRevived`: only reviving a teammate scores.
        let score = if medic_team.is_some() && medic_team == team && medic != player { SCORE_REVIVE } else { 0 };
        self.add_score(medic, score);
        info!("{} revived {} (+{score})", self.name(medic), self.name(player));
        for (to, other, received, score) in [(medic, player, false, score), (player, medic, true, 0)] {
            if let Some(client) = player_client(to, &self.clients, self.host.as_deref()) {
                self.notices.write(ToClients {
                    targets: SendTargets::Single(client),
                    message: ReplenishNotice {
                        kind: NoticeKind::Revive,
                        received,
                        other: Some(other),
                        amount: 0.0,
                        score,
                    },
                });
            }
        }
    }
}

/// Whether a hit with `weapon` hurts a critically wounded soldier: blasts and anything
/// that isn't a hand weapon (vehicle guns, run-overs) do, bullets don't, and shock paddles
/// only enemies (BF2 `damageMandownSoldiers`, set only on the paddles' projectile).
pub(crate) fn hurts_downed(armory: &Armory, weapon: &str, enemy: bool) -> bool {
    match armory.weapon(weapon) {
        None => true,
        Some(weapon) => {
            weapon.projectile.explodes() || (enemy && weapon.replenish.as_ref().is_some_and(|r| r.revive_health > 0.0))
        }
    }
}

/// A downed soldier's client asks to die now.
fn receive_give_up(
    mut requests: MessageReader<FromClient<GiveUp>>,
    clients: Query<&ClientPlayer>,
    host: Option<Res<HostPlayer>>,
    controls: Query<&Controls>,
    mut bleeding: Query<&mut BleedOut>,
) {
    for request in requests.read() {
        let Some(player) = sender_player(request.client_id, &clients, host.as_deref()) else {
            continue;
        };
        if let Some(mut bleed) = controls.get(player).ok().and_then(|c| bleeding.get_mut(c.0).ok()) {
            bleed.left = 0.0;
        }
    }
}

/// Downed soldiers bleed out, and die when the time is up.
fn bleed_out(
    time: Res<Time>,
    mut soldiers: Query<(Entity, &ControlledBy, &mut Downed, &mut BleedOut)>,
    mut deaths: Deaths,
) {
    let dt = time.delta_secs();
    for (soldier, controlled_by, mut downed, mut bleed) in &mut soldiers {
        bleed.left -= dt;
        bleed.down_for += dt;
        if bleed.left <= 0.0 {
            deaths.die(soldier, controlled_by.0, bleed.down_for);
            continue;
        }
        let shown = bleed.left.ceil();
        if downed.left != shown {
            downed.left = shown;
        }
    }
}

/// The replenishing gadget of `weapon`, if it is one.
fn replenisher(weapon: &WeaponDesc) -> Option<&ReplenishDesc> {
    weapon.replenish.as_ref()
}

/// Kit ability charge: refills over time; thrown bags and shocks use it up. Gadgets that
/// cost charge show what it allows as their ammo (in hand plus spare), so the weapon logic
/// throws and reloads them like grenades.
#[allow(clippy::type_complexity)]
fn charge_gadgets(
    mut commands: Commands,
    time: Res<Time>,
    armory: Res<Armory>,
    mut soldiers: Query<(Entity, &Loadout, &mut Inventory, Option<&mut KitAbility>), With<Soldier>>,
) {
    let dt = time.delta_secs();
    for (soldier, loadout, mut inventory, ability) in &mut soldiers {
        let Some(mut ability) = ability else {
            let kit_restore = armory.kits.get(&loadout.kit).map_or(0.0, |k| k.ability_restore);
            let has_gadget = loadout
                .weapons
                .iter()
                .filter_map(|w| armory.weapon(w))
                .any(|w| replenisher(w).is_some());
            if has_gadget {
                commands.entity(soldier).insert(KitAbility {
                    charge: 1.0,
                    // Kits without a rate of their own recharge like BF2's medic kit.
                    restore: if kit_restore > 0.0 { kit_restore } else { 0.05 },
                    left: inventory.ammo.iter().map(|[a, b]| a + b).collect(),
                });
            }
            continue;
        };
        ability.charge = (ability.charge + ability.restore * dt).min(1.0);
        for (index, name) in loadout.weapons.iter().enumerate() {
            let Some(cost) = armory
                .weapon(name)
                .filter(|w| w.magazine_size > 0)
                .and_then(|w| replenisher(w))
                .map(|r| r.cost)
                .filter(|c| *c > 0.0)
            else {
                continue;
            };
            let Some(&[in_hand, spare]) = inventory.ammo.get(index) else {
                continue;
            };
            let before = ability.left.get(index).copied().unwrap_or(in_hand + spare);
            let used = before.saturating_sub(in_hand + spare);
            if used > 0 {
                ability.charge = (ability.charge - used as f32 * cost).max(0.0);
            }
            let allowed = ((ability.charge + 1e-4) / cost).floor().min(9.0) as u16;
            let ammo = [in_hand.min(allowed), allowed - in_hand.min(allowed)];
            if inventory.ammo[index] != ammo {
                inventory.ammo[index] = ammo;
            }
            if let Some(left) = ability.left.get_mut(index) {
                *left = allowed;
            }
        }
    }
}

/// Shock paddles: a shock revives the downed teammate the medic reaches, the nearest to
/// where he aims. (Enemies are hit by the paddles' projectile, see `combat`.)
#[allow(clippy::type_complexity)]
fn revive_with_paddles(
    armory: Res<Armory>,
    teams: Query<&Team>,
    medics: Query<(Entity, &ControlledBy, &SoldierMotion, &Loadout, &Inventory), (Without<Downed>, Without<Seated>)>,
    mut downed: Query<(Entity, &ControlledBy, &mut SoldierMotion, &mut Health), With<Downed>>,
    mut shocked: Local<HashMap<Entity, u16>>,
    mut deaths: Deaths,
) {
    for (medic, controlled_by, motion, loadout, inventory) in &medics {
        let active = inventory.active as usize;
        let Some(paddles) = loadout
            .weapons
            .get(active)
            .and_then(|w| armory.weapon(w))
            .and_then(|w| replenisher(w))
            .filter(|r| r.revive_health > 0.0)
        else {
            shocked.remove(&medic);
            continue;
        };
        // A shock is a round gone from the paddles since last tick.
        let left = inventory.ammo.get(active).map_or(0, |[a, b]| a + b);
        let before = shocked.insert(medic, left).unwrap_or(left);
        if left >= before {
            continue;
        }
        let team = teams.get(controlled_by.0).ok().copied();
        let eye = motion.eye_position();
        let aim = motion.view_rotation() * Vec3::NEG_Z;
        let target = downed
            .iter()
            .filter(|(_, owner, ..)| teams.get(owner.0).ok().copied() == team)
            .filter_map(|(soldier, _, body, _)| {
                // The body lies along its facing, around its feet position.
                let along = Quat::from_rotation_y(body.yaw) * Vec3::NEG_Z;
                let center = body.position + Vec3::Y * 0.25;
                let t = (eye - center).dot(along).clamp(-0.8, 0.8);
                let closest = center + along * t;
                let to = closest - eye;
                let flat = Vec2::new(body.position.x - motion.position.x, body.position.z - motion.position.z);
                let in_reach = to.length() <= PADDLES_REACH + 0.3;
                let aimed = to.angle_between(aim).to_degrees() < 40.0 || flat.length() < 1.0;
                (in_reach && aimed).then_some((soldier, to.angle_between(aim)))
            })
            .min_by(|a, b| a.1.total_cmp(&b.1))
            .map(|(soldier, _)| soldier);
        let Some(target) = target else {
            debug!("shock paddles: nobody to revive");
            continue;
        };
        let Ok((soldier, owner, mut body, mut health)) = downed.get_mut(target) else {
            continue;
        };
        health.current = (health.current + paddles.revive_health).clamp(1.0, health.max);
        // He comes to lying down, and takes a moment to get up (BF2's revive animation).
        body.stance = Stance::Prone;
        body.stance_lock = body.stance_lock.max(REVIVE_RECOVERY);
        deaths.revive(soldier, owner.0, controlled_by.0);
    }
}

/// How much `material` (a gadget's damage table row) replenishes a target of armor
/// `target`. Cells the table leaves out give nothing.
fn factor(materials: Option<&Materials>, material: u32, target: u32, soldier: bool) -> f32 {
    match materials {
        Some(materials) if !materials.0.damage.is_empty() => {
            materials.0.damage.get(&(material, target)).copied().unwrap_or(0.0).max(0.0)
        }
        // Without the table: the wrench repairs, everything else is for soldiers.
        _ => ((material == REPAIR_MATERIAL) != soldier) as u8 as f32,
    }
}

/// Gives a soldier `percent` of the ammo he carries when full, weapon by weapon (not the
/// gadgets that recharge by themselves). Returns percent of a full loadout given.
fn resupply(
    soldier: Entity,
    loadout: &Loadout,
    inventory: &mut Inventory,
    armory: &Armory,
    credit: &mut AmmoCredit,
    percent: f32,
) -> f32 {
    let mut given = 0.0;
    let mut weapons = 0;
    for (index, name) in loadout.weapons.iter().enumerate() {
        let Some(weapon) = armory.weapon(name) else {
            continue;
        };
        let full = weapon.magazine_size * weapon.magazines;
        if full == 0 || replenisher(weapon).is_some() {
            continue;
        }
        weapons += 1;
        let Some(&[in_hand, spare]) = inventory.ammo.get(index) else {
            continue;
        };
        let missing = full.saturating_sub(in_hand as u32 + spare as u32);
        let owed = credit.0.entry((soldier, index)).or_default();
        if missing == 0 {
            *owed = 0.0;
            continue;
        }
        *owed += percent / 100.0 * full as f32;
        let take = (owed.floor() as u32).min(missing);
        if take == 0 {
            continue;
        }
        *owed -= take as f32;
        inventory.ammo[index][1] = spare + take as u16;
        given += take as f32 / full as f32;
    }
    if weapons == 0 { 0.0 } else { given / weapons as f32 * 100.0 }
}

/// Whether a soldier is short of ammo for any weapon that uses it.
fn needs_ammo(loadout: &Loadout, inventory: &Inventory, armory: &Armory) -> bool {
    loadout.weapons.iter().enumerate().any(|(index, name)| {
        armory.weapon(name).is_some_and(|w| {
            let full = w.magazine_size * w.magazines;
            full > 0
                && replenisher(w).is_none()
                && inventory.ammo.get(index).is_some_and(|[a, b]| (*a as u32 + *b as u32) < full)
        })
    })
}

/// Server-side: seconds a thrown bag has been lying around.
#[derive(Component)]
struct BagAge(f32);

/// Thrown medic and ammo bags: the first soldier over one who needs it picks it up.
#[allow(clippy::too_many_arguments, clippy::type_complexity)]
fn pick_up_bags(
    mut commands: Commands,
    time: Res<Time>,
    armory: Res<Armory>,
    materials: Option<Res<Materials>>,
    mut bags: Query<(Entity, &Projectile, &ProjectileMotion, Option<&mut BagAge>)>,
    mut soldiers: Query<
        (Entity, &ControlledBy, &SoldierMotion, &mut Health, &Loadout, &mut Inventory),
        (With<Soldier>, Without<Downed>, Without<Seated>),
    >,
    teams: Query<&Team>,
    mut credit: ResMut<AmmoCredit>,
    mut ledger: ResMut<Ledger>,
    mut deaths: Deaths,
) {
    let dt = time.delta_secs();
    let now = time.elapsed_secs_f64();
    for (bag, projectile, motion, age) in &mut bags {
        let Some(weapon) = armory.weapon(&projectile.weapon) else {
            continue;
        };
        let Some(desc) = replenisher(weapon).filter(|r| r.pickup_strength > 0.0) else {
            continue;
        };
        let age = match age {
            Some(mut age) => {
                age.0 += dt;
                age.0
            }
            None => {
                commands.entity(bag).insert(BagAge(0.0));
                0.0
            }
        };
        if age < weapon.projectile.arming_delay {
            continue;
        }
        for (soldier, controlled_by, body, mut health, loadout, mut inventory) in &mut soldiers {
            let offset = motion.position - body.position;
            let reach = desc.pickup_radius + 0.3;
            if Vec2::new(offset.x, offset.z).length() > reach || !(-0.5..=2.0).contains(&offset.y) {
                continue;
            }
            let (kind, amount) = match desc.kind {
                ReplenishKind::Health => {
                    let factor = factor(materials.as_deref(), desc.material, SOLDIER_MATERIAL, true);
                    if factor <= 0.0 || health.current >= health.max {
                        continue;
                    }
                    let before = health.current;
                    health.current = (health.current + desc.pickup_strength / 100.0 * health.max * factor).min(health.max);
                    (NoticeKind::Heal, health.current - before)
                }
                ReplenishKind::Ammo => {
                    if !needs_ammo(loadout, &inventory, &armory) {
                        continue;
                    }
                    let given = resupply(soldier, loadout, &mut inventory, &armory, &mut credit, desc.pickup_strength);
                    (NoticeKind::Resupply, given)
                }
            };
            commands.entity(bag).try_despawn();
            let thrower = projectile.player;
            let player = controlled_by.0;
            let teammates = teams.get(thrower).ok() == teams.get(player).ok();
            info!(
                "{} picked up {}'s {} ({amount:.0})",
                deaths.name(player),
                deaths.name(thrower),
                weapon.name
            );
            if teammates {
                let score = ledger.give(now, thrower, Some(player), kind, amount);
                deaths.add_score(thrower, score);
            }
            break;
        }
    }
}

/// What an engineer can repair: vehicles and damaged destroyable objects.
#[derive(SystemParam)]
struct Repairables<'w, 's> {
    spatial: SpatialQuery<'w, 's>,
    colliders: Query<'w, 's, &'static ColliderOf>,
    vehicles: Query<'w, 's, (&'static VehicleData, &'static mut VehicleHealth)>,
    crews: Query<'w, 's, (&'static Seated, &'static ControlledBy)>,
    parts: Query<'w, 's, &'static Destructible, Without<Inactive>>,
    destroyed: Query<'w, 's, &'static DestroyedStatics>,
    object_health: ResMut<'w, ObjectHealth>,
}

/// Gadgets in hand: the medic bag heals and the ammo bag resupplies teammates around, the
/// wrench repairs what is around while its trigger is held.
#[allow(clippy::too_many_arguments, clippy::type_complexity)]
fn replenish_in_hand(
    time: Res<Time>,
    armory: Res<Armory>,
    materials: Option<Res<Materials>>,
    rounds: Query<&RoundState>,
    mut helpers: Query<
        (Entity, &ControlledBy, &SoldierMotion, &Loadout, &AppliedInput, &mut KitAbility),
        (Without<Downed>, Without<Seated>),
    >,
    mut soldiers: Query<
        (Entity, &ControlledBy, &SoldierMotion, &mut Health, &Loadout, &mut Inventory),
        (With<Soldier>, Without<Downed>, Without<Seated>),
    >,
    teams: Query<&Team>,
    mut repairables: Repairables,
    mut credit: ResMut<AmmoCredit>,
    mut ledger: ResMut<Ledger>,
    mut deaths: Deaths,
) {
    if rounds.single().is_ok_and(|r| *r != RoundState::Playing) {
        return;
    }
    let dt = time.delta_secs();
    let now = time.elapsed_secs_f64();
    let materials = materials.as_deref();
    for (helper, controlled_by, motion, loadout, applied, mut ability) in &mut helpers {
        let Ok(active) = soldiers.get(helper).map(|(.., inventory)| inventory.active) else {
            continue;
        };
        let Some(desc) = loadout
            .weapons
            .get(active as usize)
            .and_then(|w| armory.weapon(w))
            .and_then(|w| replenisher(w))
            .filter(|r| r.radius > 0.0 && r.strength > 0.0)
            .cloned()
        else {
            continue;
        };
        if (desc.while_firing && !applied.0.pressed(game_shared::input::Buttons::FIRE))
            || (desc.drain > 0.0 && ability.charge <= 0.0)
        {
            continue;
        }
        let player = controlled_by.0;
        let team = teams.get(player).ok().copied();
        let center = motion.position + Vec3::Y * 0.9;
        let rate = desc.strength / 100.0 * dt;
        let mut worked = false;

        // Teammates (and the helper himself) around.
        for (soldier, owner, body, mut health, loadout, mut inventory) in &mut soldiers {
            if body.position.distance(motion.position) > desc.radius || teams.get(owner.0).ok().copied() != team {
                continue;
            }
            let (kind, amount) = match desc.kind {
                ReplenishKind::Health => {
                    let factor = factor(materials, desc.material, SOLDIER_MATERIAL, true);
                    if factor <= 0.0 || health.current >= health.max {
                        continue;
                    }
                    let before = health.current;
                    health.current = (health.current + rate * health.max * factor).min(health.max);
                    (NoticeKind::Heal, health.current - before)
                }
                ReplenishKind::Ammo => {
                    if !needs_ammo(loadout, &inventory, &armory) {
                        continue;
                    }
                    (NoticeKind::Resupply, resupply(soldier, loadout, &mut inventory, &armory, &mut credit, rate * 100.0))
                }
            };
            worked = true;
            let score = ledger.give(now, player, Some(owner.0), kind, amount);
            deaths.add_score(player, score);
        }

        // Vehicles and destroyable objects around (repairs).
        if desc.kind == ReplenishKind::Health {
            let filter = SpatialQueryFilter::from_mask([GameLayer::Vehicle, GameLayer::World]);
            let mut vehicles_seen = Vec::new();
            let mut objects_seen = Vec::new();
            let Repairables {
                spatial,
                colliders,
                vehicles,
                crews,
                parts,
                destroyed,
                object_health,
            } = &mut repairables;
            for collider in spatial.shape_intersections(&Collider::sphere(desc.radius), center, Quat::IDENTITY, &filter) {
                let body = colliders.get(collider).map_or(collider, |c| c.body);
                if vehicles.contains(body) {
                    if !vehicles_seen.contains(&body) {
                        vehicles_seen.push(body);
                    }
                } else if let Ok(part) = parts.get(collider)
                    && !part.wreck
                    && !objects_seen.iter().any(|(instance, _)| *instance == part.instance)
                {
                    objects_seen.push((part.instance, part.armor.clone()));
                }
            }
            for vehicle in vehicles_seen {
                let Ok((data, mut health)) = vehicles.get_mut(vehicle) else {
                    continue;
                };
                let desc_vehicle = &data.0.desc;
                let factor = factor(materials, desc.material, desc_vehicle.blast_material, false);
                if factor <= 0.0 || health.wrecked() || health.current >= health.max {
                    continue;
                }
                let before = health.current;
                health.current = (health.current + rate * health.max * factor).min(health.max);
                worked = true;
                // The crew decides whose vehicle it is: repairing the enemy's scores nothing.
                let crew: Vec<Entity> = crews
                    .iter()
                    .filter(|(seated, _)| seated.vehicle == vehicle)
                    .map(|(_, c)| c.0)
                    .collect();
                if crew.iter().all(|c| teams.get(*c).ok().copied() == team) {
                    let score = ledger.give(now, player, crew.first().copied(), NoticeKind::Repair, health.current - before);
                    deaths.add_score(player, score);
                }
                debug!("{} repairs {}: {:.0}/{:.0}", deaths.name(player), desc_vehicle.name, health.current, health.max);
            }
            let destroyed = destroyed.single().ok();
            for (instance, armor) in objects_seen {
                if destroyed.is_some_and(|d| d.0.contains(&instance)) {
                    continue;
                }
                let Some(left) = object_health.0.get_mut(&instance) else {
                    // Undamaged.
                    continue;
                };
                let factor = factor(materials, desc.material, armor.material, false);
                if factor <= 0.0 || *left >= armor.hit_points {
                    continue;
                }
                let before = *left;
                *left = (*left + rate * armor.hit_points * factor).min(armor.hit_points);
                let repaired = *left - before;
                if *left >= armor.hit_points {
                    object_health.0.remove(&instance);
                    info!("{} repaired object {instance}", deaths.name(player));
                }
                worked = true;
                let score = ledger.give(now, player, None, NoticeKind::Repair, repaired);
                deaths.add_score(player, score);
            }
        }
        if worked {
            ability.charge = (ability.charge - desc.drain * dt).max(0.0);
        }
    }
}

/// Sends what was healed, resupplied and repaired over the last second to both sides.
fn send_notices(
    time: Res<Time>,
    clients: Query<&PlayerClient>,
    host: Option<Res<HostPlayer>>,
    mut ledger: ResMut<Ledger>,
    mut notices: MessageWriter<ToClients<ReplenishNotice>>,
) {
    ledger.since_notice += time.delta_secs();
    if ledger.since_notice < NOTICE_INTERVAL || ledger.pending.is_empty() {
        return;
    }
    ledger.since_notice = 0.0;
    let mut send = |to: Entity, message: ReplenishNotice| {
        if let Some(client) = player_client(to, &clients, host.as_deref()) {
            notices.write(ToClients {
                targets: SendTargets::Single(client),
                message,
            });
        }
    };
    for ((helper, helped, kind), (amount, score)) in ledger.pending.drain() {
        if amount < 0.5 && score == 0 {
            continue;
        }
        let own = helped == Some(helper);
        send(helper, ReplenishNotice {
            kind,
            received: own,
            other: if own { None } else { helped },
            amount,
            score,
        });
        if let Some(helped) = helped.filter(|h| *h != helper) {
            send(helped, ReplenishNotice {
                kind,
                received: true,
                other: Some(helper),
                amount,
                score: 0,
            });
        }
    }
}

/// Drops what is kept about soldiers that are gone (round restarts, players leaving).
fn forget_the_gone(
    time: Res<Time>,
    soldiers: Query<(), With<Soldier>>,
    mut assists: ResMut<Assists>,
    mut credit: ResMut<AmmoCredit>,
    mut since: Local<f32>,
) {
    *since += time.delta_secs();
    if *since < 5.0 {
        return;
    }
    *since = 0.0;
    assists.0.retain(|soldier, _| soldiers.contains(*soldier));
    credit.0.retain(|(soldier, _), _| soldiers.contains(*soldier));
}

/// What a bot can use a gadget for.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Gadget {
    /// Shock paddles: revive downed teammates.
    Paddles,
    /// Medic bag: heal in hand, or throw for someone to pick up.
    MedicBag,
    /// Ammo bag: resupply in hand, or throw.
    AmmoBag,
    /// Wrench: repair vehicles and objects around while firing.
    Wrench,
}

/// The loadout index of a soldier's gadget of this kind, if his kit has one.
pub fn gadget(loadout: &Loadout, armory: &Armory, which: Gadget) -> Option<u8> {
    loadout.weapons.iter().position(|name| {
        armory.weapon(name).and_then(|w| replenisher(w)).is_some_and(|r| match which {
            Gadget::Paddles => r.revive_health > 0.0,
            Gadget::MedicBag => r.kind == ReplenishKind::Health && r.pickup_strength > 0.0,
            Gadget::AmmoBag => r.kind == ReplenishKind::Ammo && r.pickup_strength > 0.0,
            Gadget::Wrench => r.while_firing,
        })
    }).map(|i| i as u8)
}

/// Wounded soldiers, for bots: medics revive the downed and drop bags for the hurt.
#[derive(SystemParam)]
pub struct Wounded<'w, 's> {
    downed: Query<'w, 's, (Entity, &'static SoldierMotion, &'static ControlledBy, &'static Downed)>,
    hurt: Query<
        'w,
        's,
        (Entity, &'static SoldierMotion, &'static ControlledBy, &'static Health),
        (With<Soldier>, Without<Downed>, Without<Seated>),
    >,
    teams: Query<'w, 's, &'static Team>,
}

impl Wounded<'_, '_> {
    /// Downed soldiers of `team` within `radius` of `position`, nearest first: the soldier,
    /// where he lies and the seconds left to revive him.
    pub fn downed_near(&self, team: Team, position: Vec3, radius: f32) -> Vec<(Entity, Vec3, f32)> {
        let mut found: Vec<_> = self
            .downed
            .iter()
            .filter(|(_, motion, owner, _)| {
                self.teams.get(owner.0).ok() == Some(&team) && motion.position.distance(position) <= radius
            })
            .map(|(soldier, motion, _, downed)| (soldier, motion.position, downed.left))
            .collect();
        found.sort_by(|a, b| a.1.distance(position).total_cmp(&b.1.distance(position)));
        found
    }

    /// Standing soldiers of `team` within `radius` of `position` with less than `below`
    /// of their health (0..1), nearest first: the soldier, where he is and his health share.
    pub fn hurt_near(&self, team: Team, position: Vec3, radius: f32, below: f32) -> Vec<(Entity, Vec3, f32)> {
        let mut found: Vec<_> = self
            .hurt
            .iter()
            .filter(|(_, motion, owner, health)| {
                self.teams.get(owner.0).ok() == Some(&team)
                    && motion.position.distance(position) <= radius
                    && health.current < health.max * below
            })
            .map(|(soldier, motion, _, health)| (soldier, motion.position, health.current / health.max.max(1.0)))
            .collect();
        found.sort_by(|a, b| a.1.distance(position).total_cmp(&b.1.distance(position)));
        found
    }

    /// Whether `soldier` is critically wounded (bots shouldn't shoot bodies).
    pub fn is_downed(&self, soldier: Entity) -> bool {
        self.downed.contains(soldier)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn replenishing_scores_every_hundred_points_once_per_player_per_half_minute() {
        let mut ledger = Ledger::default();
        let mut world = World::new();
        let (medic, a, b) = (world.spawn_empty().id(), world.spawn_empty().id(), world.spawn_empty().id());
        assert_eq!(ledger.give(0.0, medic, Some(a), NoticeKind::Heal, 60.0), 0);
        assert_eq!(ledger.give(1.0, medic, Some(a), NoticeKind::Heal, 60.0), 1);
        // The same player again within 30 s: the points go, the score doesn't come.
        assert_eq!(ledger.give(5.0, medic, Some(a), NoticeKind::Heal, 100.0), 0);
        assert_eq!(ledger.give(6.0, medic, Some(b), NoticeKind::Heal, 100.0), 1);
        assert_eq!(ledger.give(40.0, medic, Some(a), NoticeKind::Heal, 100.0), 1);
        // Healing yourself earns nothing.
        assert_eq!(ledger.give(80.0, medic, Some(medic), NoticeKind::Heal, 500.0), 0);
        // Objects have no grind block.
        assert_eq!(ledger.give(81.0, medic, None, NoticeKind::Repair, 250.0), 2);
    }
}
