//! AI soldiers.
//!
//! A bot is an ordinary [`Player`] whose [`InputBuffer`] is filled by a [`BotBrain`] instead
//! of the network, so bots move with exactly the same rules as humans.
//!
//! BF2's AI is layered and so is this: each team's commander gives squads orders
//! ([`crate::ai::strategy`]), squads move together ([`crate::ai::squad`]), and every bot
//! picks what to do from moment to moment by weighing its options, like BF2's behaviour
//! weights (`AIBehaviours.ai`: fire 7.5, special 3, take cover 2, move 1): shoot what it
//! sees, take cover when hurt, flank or throw a grenade at enemies behind cover, turn on
//! whoever shoots it, and otherwise carry out its squad's order: capture or hold a flag, or
//! keep up with its leader. Movement follows paths on the navigation grid ([`crate::nav`]).

use std::{
    f32::consts::{PI, TAU},
    time::Instant,
};

use avian3d::prelude::*;
use bevy::{
    ecs::system::SystemParam,
    prelude::*,
    tasks::{AsyncComputeTaskPool, Task, futures::check_ready},
};
use bevy_replicon::prelude::*;
use game_data::{FireKind, FireMode, FiringPose, FlashbangDesc, WeaponDesc};
use game_shared::{
    conquest::{ControlPoint, Deployment, FlagState, team_index},
    input::{Buttons, InputFrame},
    level::LoadedLevel,
    projectile::{GRAVITY, Smoke},
    protocol::{ControlledBy, Player, Team},
    revive::Downed,
    soldier::{Health, Soldier, SoldierMotion, Stance},
    squad::SquadMember,
    commander::CommanderAssets,
    gear::{SoldierGear, TearGas},
    projectile::SmokeCloud,
    statics::DestroyedStatics,
    vehicle::{Seated, Vehicle, VehicleHealth, VehicleMotion},
    weapons::{Armory, Inventory, Loadout, cooks},
};

use crate::{
    abilities::{Gadget, PADDLES_REACH, Wounded, gadget},
    AppliedInput, Controls, InputBuffer, ServerSettings, ServerSimSystems, balanced_team,
    ai::{
        self, AiData,
        skill::{Personality, Skill},
        squad::{self, SquadSnapshot},
        gadgets::Flashes,
        stats::{AiStats, TeamStats},
        strategy::{self, OrderKind, StrategicMap, Strategy, TeamIntel, hash01},
        tactics,
    },
    destruction::ObjectHealth,
    nav::{LadderStep, NavGrid, NavPath, Navigation, Waypoint},
};

mod equipment;

use equipment::{LaunchTarget, RepairTarget};

pub struct BotPlugin;

impl Plugin for BotPlugin {
    fn build(&self, app: &mut App) {
        app.configure_sets(
            FixedUpdate,
            (ServerSimSystems::Think, ServerSimSystems::ApplyInputs).chain(),
        )
        .init_resource::<BotStats>()
        .init_resource::<AiData>()
        .init_resource::<StrategicMap>()
        .init_resource::<Strategy>()
        .init_resource::<TeamIntel>()
        .init_resource::<SquadSnapshot>()
        .init_resource::<AiStats>()
        .init_resource::<ai::commander::AiCommander>()
        .init_resource::<Flashes>()
        .add_systems(
            Update,
            (
                spawn_bots.run_if(resource_exists::<LoadedLevel>),
                ai::load_ai_data.run_if(resource_exists_and_changed::<LoadedLevel>),
            )
                .run_if(in_state(ClientState::Disconnected)),
        )
        .add_systems(
            FixedUpdate,
            // Detonation effects are sent to clients (and gone) by the next tick.
            ai::gadgets::watch_detonations
                .after(crate::combat::CombatSystems)
                .run_if(in_state(ClientState::Disconnected)),
        )
        .add_systems(
            FixedUpdate,
            (
                squad::snapshot,
                strategy::update_map,
                strategy::plan,
                ai::commander::yield_to_humans,
                ai::commander::command,
                ai::gadgets::wear_gas_masks,
                think,
                ai::gadgets::forget_flashes,
                log_stats,
                ai::stats::track_events,
                ai::stats::log_stats,
            )
                .chain()
                .in_set(ServerSimSystems::Think)
                .run_if(resource_exists::<LoadedLevel>)
                .run_if(in_state(ClientState::Disconnected)),
        );
    }
}

const BOT_NAMES: &[&str] = &[
    "Alpha", "Bravo", "Charlie", "Delta", "Echo", "Foxtrot", "Golf", "Hotel", "India", "Juliet",
    "Kilo", "Lima", "Mike", "November", "Oscar", "Papa", "Quebec", "Romeo", "Sierra", "Tango",
    "Uniform", "Victor", "Whiskey", "Xray", "Yankee", "Zulu",
];

/// Enemies a bot checks line of sight to per look around, nearest first.
const MAX_SIGHT_CHECKS: usize = 6;
/// Seconds between looks around.
const SCAN_INTERVAL: f32 = 0.25;
/// Seconds between decisions.
const DECIDE_INTERVAL: f32 = 0.25;
/// Beyond this distance from its objective a bot heads for the area's order position.
const APPROACH_DISTANCE: f32 = 60.0;
/// How far a grenade may land from friends, meters.
const GRENADE_SAFETY: f32 = 9.0;
/// Bots keep this far from teammates while moving, meters.
const CROWD_DISTANCE: f32 = 1.2;
/// Stamina bots keep for fights and escapes when sprinting.
const SPRINT_RESERVE: f32 = 0.35;
/// Vehicles faster than this run soldiers over, m/s (see `roadkill`).
const DANGEROUS_SPEED: f32 = 3.5;
/// How far medics go to revive someone, meters, and how much they want to (BF2's revive
/// behaviour weight is 3, fire 7.5).
const REVIVE_DISTANCE: f32 = 35.0;
const REVIVE_UTILITY: f32 = 4.5;
/// How close teammates must be for a held bag to reach them, meters.
const BAG_REACH: f32 = 4.0;

/// Per-minute movement statistics, logged to see how well bots get around.
#[derive(Resource, Default)]
pub struct BotStats {
    elapsed: f32,
    stuck_events: u32,
    /// Stuck events while carrying out orders, advancing under fire, and taking cover or
    /// flanking.
    stuck_by_activity: [u32; 3],
    /// Stuck events by 10 m square, to find the places bots get stuck at.
    stuck_spots: bevy::platform::collections::HashMap<(i32, i32), u32>,
    stuck_seconds: f32,
    /// Bot-seconds spent alive and trying to move (the time stuck events can happen in).
    moving_seconds: f32,
    /// Meters walked in that time.
    moved: f32,
    /// New paths asked for because a bot made no progress towards its waypoint.
    no_progress: u32,
    paths: u32,
    partial_paths: u32,
    failed_paths: u32,
    path_seconds: f32,
    max_path_seconds: f32,
    /// Ladders bots got on.
    climbs: u32,
    /// Times a bot found it couldn't walk to its goal.
    stranded: u32,
    /// Milliseconds spent in `think`.
    think_ms: f32,
    max_think_ms: f32,
    ticks: u32,
}

/// A finished path request.
struct PathResult {
    path: Option<NavPath>,
    seconds: f32,
}

/// What a bot is doing, chosen by weighing its options (see [`BotBrain::decide`]).
#[derive(Clone, Copy, Debug, PartialEq)]
enum Activity {
    /// Carrying out its order, or keeping up with its leader.
    Objective,
    /// Shooting at its target.
    Engage,
    /// Hurt: going to a spot the threat can't see, staying `time` seconds once there.
    Cover { spot: Vec3, time: f32 },
    /// Going round an enemy behind cover, to attack him from the side.
    Flank { spot: Vec3, time: f32 },
    /// Looking (and maybe moving) towards where shots came from.
    Search { at: Vec3, time: f32 },
    /// Throwing a grenade at `at`, `time` seconds into it.
    Throw { at: Vec3, time: f32, weapon: u8 },
    /// Medics: to a critically wounded teammate and shocking him back to life, for at most
    /// `time` more seconds.
    Revive { soldier: Entity, time: f32 },
    /// Firing a grenade or rocket launcher: `time` seconds into it, `shots` rounds left
    /// before, fired `fired` seconds into it (negative: not yet).
    Launch { target: LaunchTarget, time: f32, weapon: u8, shots: u16, fired: f32 },
    /// Engineers: to something of the team's that is damaged, and the wrench at it.
    Repair { target: RepairTarget, time: f32 },
}

/// How a bot stands while shooting.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum EngageStyle {
    Strafe,
    Crouch,
    Prone,
}

#[derive(Component)]
pub struct BotBrain {
    seq: u32,
    personality: Personality,
    seed: u32,
    /// The soldier this brain drives; a new one means a new life.
    soldier: Option<Entity>,
    /// Loadout indices of the main weapon and of a frag grenade.
    primary: u8,
    grenade: Option<u8>,
    /// Medics: the loadout index of the shock paddles.
    paddles: Option<u8>,
    /// Medics' and support soldiers' bags.
    medic_bag: Option<u8>,
    ammo_bag: Option<u8>,
    /// The bag to hold out now (see `decide`).
    bag: Option<u8>,
    /// Grenade launcher, rocket launcher, tear gas launcher, flashbang, wrench (loadout
    /// indices).
    launcher: Option<u8>,
    rocket: Option<u8>,
    gas_launcher: Option<u8>,
    flashbang: Option<u8>,
    wrench: Option<u8>,
    /// Rocket bots: the enemy vehicle in sight, and where.
    armor: Option<(Entity, Vec3)>,
    launch_cooldown: f32,
    bag_cooldown: f32,
    repair_cooldown: f32,
    /// The last flashbang that blinded it: how, how strongly, how long ago.
    flash: Option<(FlashbangDesc, f32, f32)>,
    /// Blinded or dazed by it right now.
    blind: bool,
    dazed: bool,
    /// How deep in tear gas it is without a mask, 0..1.
    gassed: f32,

    /// Enemy soldier being engaged.
    target: Option<Entity>,
    /// Where an enemy was last seen, and how many seconds ago.
    last_seen: Option<(Vec3, f32)>,
    scan_timer: f32,
    /// Seconds before shooting at a newly seen target.
    reaction: f32,
    /// Seconds of looking all around (after being hurt).
    alert: f32,
    health: f32,
    /// Seconds since last hurt.
    hurt_ago: f32,
    /// Who probably hurt it last, and where he was.
    threat: Option<(Entity, Vec3)>,
    /// A nearby enemy heard firing.
    heard: Option<Vec3>,

    yaw: f32,
    pitch: f32,
    /// Slowly wandering aim offset (radians), so bots aren't perfect shots.
    aim_error: Vec2,
    /// Automatic fire: positive while holding the trigger, negative between bursts. Other
    /// fire modes: seconds until the next pull.
    burst: f32,
    strafe: f32,
    strafe_ok: bool,
    style: EngageStyle,
    /// Seconds engaging the current target.
    engaged: f32,

    activity: Activity,
    decide_timer: f32,
    flank_cooldown: f32,
    grenade_cooldown: f32,
    /// The order being carried out, to notice new ones.
    order: Option<(OrderKind, usize)>,
    /// A spot of its own in or around the objective, and the area it belongs to.
    spot: Option<(usize, Vec3)>,
    spot_timer: f32,
    /// A waypoint to pass through on the way to the objective.
    via: Option<Vec3>,
    /// Squad leaders: seconds spent waiting for the squad.
    regroup: f32,
    /// Squad leaders: the squad gathered before the assault.
    staged: bool,
    /// [`StrategicMap::generation`] its area indices belong to.
    map_generation: u32,
    /// Phase of looking around while holding a position.
    sweep: f32,
    /// Kit and spawn were chosen for this death.
    deployed: bool,
    /// It chose to spawn on its squad leader.
    spawn_on_leader: bool,
    /// Jump was pressed last tick.
    jump_held: bool,
    /// On a ladder last tick.
    climbing: bool,
    /// Sprinting while stamina lasts (see `act`).
    sprinting: bool,
    /// Seconds left waiting after finding its goal out of walking reach.
    stranded: f32,

    goal: Option<Vec3>,
    /// Walking straight to the goal without a path; and the goal that was checked for.
    direct: bool,
    direct_goal: Option<Vec3>,
    last_position: Vec3,
    stuck_time: f32,
    /// While positive, strafe in `unstuck_dir` and jump to get free.
    unstuck_timer: f32,
    unstuck_dir: f32,
    /// Recent stuck events; decays over time. Too many and the bot gives up on its goal.
    stuck_strikes: f32,
    path: Option<NavPath>,
    /// Index of the waypoint being walked to.
    waypoint: usize,
    /// The goal `path` (or the pending request) leads to.
    path_goal: Option<Vec3>,
    path_task: Option<Task<PathResult>>,
    /// Ask for a new path to the same goal (after getting stuck or pushed off the path).
    repath: bool,
    repath_cooldown: f32,
    /// Closest the bot got to the current waypoint, and for how long it hasn't got closer.
    waypoint_best: f32,
    waypoint_timer: f32,
}

impl Default for BotBrain {
    fn default() -> Self {
        Self {
            seq: 0,
            personality: Personality::roll(),
            seed: fastrand::u32(..),
            soldier: None,
            primary: 0,
            grenade: None,
            paddles: None,
            medic_bag: None,
            ammo_bag: None,
            bag: None,
            launcher: None,
            rocket: None,
            gas_launcher: None,
            flashbang: None,
            wrench: None,
            armor: None,
            launch_cooldown: 5.0,
            bag_cooldown: 0.0,
            repair_cooldown: 0.0,
            flash: None,
            blind: false,
            dazed: false,
            gassed: 0.0,
            target: None,
            last_seen: None,
            scan_timer: fastrand::f32() * SCAN_INTERVAL,
            reaction: 0.0,
            alert: 0.0,
            health: 0.0,
            hurt_ago: 100.0,
            threat: None,
            heard: None,
            yaw: fastrand::f32() * TAU,
            pitch: 0.0,
            aim_error: Vec2::ZERO,
            burst: 0.0,
            strafe: 1.0,
            strafe_ok: true,
            style: EngageStyle::Strafe,
            engaged: 0.0,
            activity: Activity::Objective,
            decide_timer: fastrand::f32() * DECIDE_INTERVAL,
            flank_cooldown: 0.0,
            grenade_cooldown: 10.0,
            order: None,
            spot: None,
            spot_timer: 0.0,
            via: None,
            regroup: 0.0,
            staged: false,
            map_generation: 0,
            sweep: fastrand::f32() * TAU,
            deployed: false,
            spawn_on_leader: false,
            jump_held: false,
            climbing: false,
            sprinting: true,
            stranded: 0.0,
            goal: None,
            direct: false,
            direct_goal: None,
            last_position: Vec3::ZERO,
            stuck_time: 0.0,
            unstuck_timer: 0.0,
            unstuck_dir: 1.0,
            stuck_strikes: 0.0,
            path: None,
            waypoint: 0,
            path_goal: None,
            path_task: None,
            repath: false,
            repath_cooldown: 0.0,
            waypoint_best: f32::MAX,
            waypoint_timer: 0.0,
        }
    }
}

/// Where path following wants to go this tick.
enum Steer {
    /// `ladder`: the waypoint is reached by climbing.
    Toward { target: Vec3, jump: bool, ladder: Option<LadderStep> },
    /// At the end of the path.
    Arrived,
    /// At the end of a path that doesn't reach the goal: it can't be walked to from here.
    Stranded,
}

/// Where a bot wants to go this tick.
#[derive(Clone, Copy)]
struct Goal {
    position: Vec3,
    /// How far the goal may move before the path to it is redone.
    tolerance: f32,
    sprint: bool,
}

/// Where a bot looks this tick.
#[derive(Clone, Copy, Default)]
enum Look {
    /// Where it's going.
    #[default]
    Along,
    /// At a point.
    At(Vec3),
    /// In a direction (yaw).
    Yaw(f32),
    /// Already aimed by the activity.
    Aimed,
}

/// What a bot wants to do this tick, turned into an [`InputFrame`] by [`BotBrain::act`].
#[derive(Default)]
struct Intent {
    goal: Option<Goal>,
    /// Movement without a goal (strafing), world space.
    step: Vec3,
    look: Look,
    buttons: Buttons,
    weapon: Option<u8>,
}

/// Everything bots look at.
#[derive(SystemParam)]
struct Senses<'w, 's> {
    time: Res<'w, Time>,
    level: Res<'w, LoadedLevel>,
    settings: Res<'w, ServerSettings>,
    armory: Res<'w, Armory>,
    data: Res<'w, AiData>,
    map: Res<'w, StrategicMap>,
    strategy: Res<'w, Strategy>,
    snapshot: Res<'w, SquadSnapshot>,
    nav: Option<Res<'w, Navigation>>,
    spatial: SpatialQuery<'w, 's>,
    smoke: Smoke<'w, 's>,
    control_points: Query<'w, 's, (&'static ControlPoint, &'static FlagState)>,
    soldiers: Query<
        'w,
        's,
        (
            Entity,
            &'static SoldierMotion,
            &'static ControlledBy,
            Option<&'static Inventory>,
            Option<&'static Health>,
            Option<&'static AppliedInput>,
            Option<&'static Loadout>,
            Option<&'static Seated>,
            Has<Downed>,
        ),
        With<Soldier>,
    >,
    teams: Query<'w, 's, &'static Team>,
    wounded: Wounded<'w, 's>,
    vehicles: Query<'w, 's, (Entity, &'static VehicleMotion, Option<&'static VehicleHealth>), With<Vehicle>>,
    flashes: Res<'w, Flashes>,
    gas: Query<'w, 's, (&'static SmokeCloud, &'static TearGas)>,
    gear: Query<'w, 's, &'static SoldierGear>,
    assets: Res<'w, CommanderAssets>,
    destroyed: Query<'w, 's, &'static DestroyedStatics>,
    object_health: Res<'w, ObjectHealth>,
}

impl Senses<'_, '_> {
    fn nav(&self) -> Option<&NavGrid> {
        self.nav.as_deref().map(|n| &*n.0)
    }

    fn weapon(&self, loadout: Option<&Loadout>, index: u8) -> Option<&WeaponDesc> {
        let name = loadout?.weapons.get(index as usize)?;
        self.armory.weapon(name).map(|w| &**w)
    }

    /// The flag state of an area's control point.
    fn flag(&self, area: usize) -> Option<(&ControlPoint, &FlagState)> {
        let index = self.map.areas.get(area)?.control_point?;
        let entity = self.map.control_points.get(index as usize).copied().flatten()?;
        self.control_points.get(entity).ok()
    }

    /// Whether `team` holds an area.
    fn holds(&self, area: usize, team: Team) -> bool {
        self.flag(area).is_some_and(|(_, state)| state.owner == team)
    }

    fn is_enemy(&self, player: Entity, team: Team) -> bool {
        self.teams.get(player).is_ok_and(|t| *t != team && *t != Team::Spectator)
    }
}

/// The bot's own soldier this tick.
struct Me<'a> {
    player: Entity,
    team: Team,
    member: Option<SquadMember>,
    soldier: Entity,
    motion: &'a SoldierMotion,
    inventory: Option<&'a Inventory>,
    loadout: Option<&'a Loadout>,
    /// Health points and the fraction of the maximum.
    health: f32,
    health_fraction: f32,
}

impl BotBrain {
    /// Where the bot is heading.
    pub fn goal(&self) -> Option<Vec3> {
        self.goal
    }

    /// The part of the path still ahead, for debug views.
    pub fn remaining_path(&self) -> &[Waypoint] {
        self.path
            .as_ref()
            .map_or(&[], |p| &p.waypoints[self.waypoint.min(p.waypoints.len())..])
    }

    /// Just hurt: a bad moment to spawn on.
    pub fn busy(&self) -> bool {
        self.hurt_ago < 3.0
    }

    /// A new soldier: remember its weapons, forget the last life.
    fn new_life(&mut self, w: &Senses, me: &Me) {
        self.soldier = Some(me.soldier);
        self.stranded = 0.0;
        self.primary = me.inventory.map_or(0, |i| i.active);
        self.paddles = me.loadout.and_then(|l| gadget(l, &w.armory, Gadget::Paddles));
        self.medic_bag = me.loadout.and_then(|l| gadget(l, &w.armory, Gadget::MedicBag));
        self.ammo_bag = me.loadout.and_then(|l| gadget(l, &w.armory, Gadget::AmmoBag));
        self.bag = None;
        self.armor = None;
        self.flash = None;
        self.blind = false;
        self.dazed = false;
        self.gassed = 0.0;
        self.pick_equipment(w, me);
        self.grenade = me.loadout.and_then(|l| {
            (0..l.weapons.len() as u8).find(|&i| {
                // Frag grenades: thrown, on a fuse, no trigger (unlike mines).
                w.weapon(Some(l), i).is_some_and(|d| {
                    d.fire.kind == FireKind::Thrown
                        && d.projectile.explodes()
                        && d.projectile.trigger.is_none()
                        && cooks(&d.projectile)
                })
            })
        });
        self.health = me.health;
        self.hurt_ago = 100.0;
        self.threat = None;
        self.heard = None;
        self.last_seen = None;
        self.target = None;
        self.activity = Activity::Objective;
        self.order = None;
        self.spot = None;
        self.via = None;
        self.regroup = 0.0;
        self.deployed = false;
        self.yaw = me.motion.yaw;
        self.last_position = me.motion.position;
        self.stuck_strikes = 0.0;
    }

    fn forget_life(&mut self) {
        self.soldier = None;
        self.target = None;
        self.goal = None;
        self.path = None;
        self.path_task = None;
        self.path_goal = None;
        self.activity = Activity::Objective;
    }

    fn tick(
        &mut self,
        w: &Senses,
        me: &Me,
        intel: &mut TeamIntel,
        stats: &mut BotStats,
        team_stats: &mut TeamStats,
        covers: &mut u32,
    ) -> InputFrame {
        let dt = w.time.delta_secs();
        if self.soldier != Some(me.soldier) {
            self.new_life(w, me);
            team_stats.spawns += 1;
            if self.spawn_on_leader {
                team_stats.leader_spawns += 1;
            }
        }
        if self.map_generation != w.map.generation {
            // A new round or level: areas it remembers are gone.
            self.map_generation = w.map.generation;
            self.order = None;
            self.spot = None;
            self.via = None;
            self.staged = false;
        }
        self.seq = self.seq.wrapping_add(1);
        let skill = self.personality.skill(w.settings.bot_skill);
        self.sense(w, me, skill, intel, team_stats, dt);
        if self.blind {
            // Blinded by a flashbang: crouch where it stands until it can see again.
            self.activity = Activity::Objective;
            let intent = Intent {
                buttons: Buttons::CROUCH,
                look: Look::Yaw(self.yaw),
                weapon: Some(self.primary),
                ..default()
            };
            return self.act(w, me, intent, stats, dt);
        }
        self.decide_timer -= dt;
        if self.decide_timer <= 0.0 {
            self.decide_timer = DECIDE_INTERVAL;
            self.decide(w, me, team_stats, covers);
        }

        let mut intent = Intent::default();
        match self.activity {
            Activity::Engage => self.engage(w, me, skill, &mut intent, dt),
            Activity::Cover { spot, time } => {
                let arrived = flat(spot - me.motion.position).length() < 1.0;
                intent.goal = (!arrived).then_some(Goal {
                    position: spot,
                    tolerance: 0.5,
                    sprint: true,
                });
                if arrived {
                    // Crouched out of sight; stand up to look again near the end.
                    if time > 1.0 {
                        intent.buttons |= Buttons::CROUCH;
                    }
                    intent.look = self.threat_point().map_or(Look::Along, Look::At);
                }
                self.activity = Activity::Cover { spot, time: time - dt };
            }
            Activity::Flank { spot, time } => {
                intent.goal = Some(Goal {
                    position: spot,
                    tolerance: 1.0,
                    sprint: true,
                });
                let arrived = flat(spot - me.motion.position).length() < 1.5;
                self.activity = match (arrived, self.last_seen) {
                    (true, Some((at, _))) => Activity::Search { at, time: 3.0 },
                    (true, None) => Activity::Objective,
                    _ => Activity::Flank { spot, time: time - dt },
                };
            }
            Activity::Search { at, time } => {
                intent.look = Look::At(at);
                let far = flat(at - me.motion.position).length() > 12.0;
                if far && self.personality.aggression > 0.5 && self.hurt_ago > 1.0 {
                    intent.goal = Some(Goal {
                        position: at,
                        tolerance: 3.0,
                        sprint: false,
                    });
                } else if self.hurt_ago < 2.0 && self.personality.courage < 0.5 {
                    intent.buttons |= Buttons::CROUCH;
                }
                self.activity = Activity::Search { at, time: time - dt };
            }
            Activity::Throw { at, time, weapon } => self.throw(w, me, at, time, weapon, &mut intent, dt),
            Activity::Revive { soldier, time } => self.revive(w, me, soldier, time, &mut intent, dt),
            Activity::Launch { target, time, weapon, shots, fired } => {
                self.launch(w, me, skill, target, time, weapon, shots, fired, &mut intent, dt)
            }
            Activity::Repair { target, time } => self.repair(w, me, target, time, &mut intent, dt),
            Activity::Objective => {
                self.objective(w, me, &mut intent, dt);
                intent.weapon = intent.weapon.or(self.bag);
            }
        }

        team_stats.alive += dt;
        if self.activity == Activity::Engage {
            team_stats.fighting += dt;
        }
        if let Some((_, area)) = self.order
            && let Some(area) = w.map.areas.get(area)
            && area.position.distance(me.motion.position) < area.radius + 30.0
        {
            team_stats.at_objective += dt;
        }
        self.act(w, me, intent, stats, dt)
    }

    /// Notices being hurt, looks around for enemies, listens for gunfire.
    fn sense(&mut self, w: &Senses, me: &Me, skill: Skill, intel: &mut TeamIntel, team_stats: &mut TeamStats, dt: f32) {
        self.alert -= dt;
        self.hurt_ago += dt;
        self.grenade_cooldown -= dt;
        self.flank_cooldown -= dt;
        self.launch_cooldown -= dt;
        self.bag_cooldown -= dt;
        self.repair_cooldown -= dt;
        if self.feel_gadgets(w, me, team_stats, dt) {
            // Blind: sees nothing.
            self.target = None;
            return;
        }
        if let Some((_, age)) = &mut self.last_seen {
            *age += dt;
        }
        let position = me.motion.position;
        let eye = me.motion.eye_position();

        if me.health < self.health - 0.5 {
            self.hurt_ago = 0.0;
            self.alert = 3.0;
            self.scan_timer = 0.0;
            if self.target.is_none() {
                self.threat = self.find_attacker(w, me);
                team_stats.reactions += 1;
            }
        }
        self.health = me.health;

        if self.target.is_some_and(|t| !w.soldiers.contains(t)) {
            self.target = None;
        }
        self.scan_timer -= dt;
        if self.scan_timer > 0.0 {
            return;
        }
        self.scan_timer = SCAN_INTERVAL;
        self.scan_armor(w, me);

        // Tear gas in the eyes: sees less far.
        let range = skill.sight_range() * (1.0 - 0.5 * self.gassed);
        let view = skill.view_angle();
        let mut candidates: Vec<(Entity, f32, Vec3, bool)> = Vec::new();
        let mut heard: Option<(f32, Vec3)> = None;
        for (entity, motion, controlled_by, _, _, applied, _, seated, downed) in &w.soldiers {
            // Crews are fought through their vehicles (rocket launchers, `scan_armor`); the
            // critically wounded are left alone.
            if entity == me.soldier || seated.is_some() || downed || !w.is_enemy(controlled_by.0, me.team) {
                continue;
            }
            let to = motion.position - position;
            let distance = to.length();
            if distance > range {
                continue;
            }
            let firing = applied.is_some_and(|a| a.0.pressed(Buttons::FIRE));
            if firing && distance < 70.0 && heard.is_none_or(|(d, _)| distance < d) {
                heard = Some((distance, motion.position));
            }
            let outside = angle_delta(self.yaw, yaw_to(to)).abs() > view;
            let noticed = !outside
                || self.alert > 0.0
                || distance < 6.0
                || (firing && distance < 40.0)
                || Some(entity) == self.target;
            if noticed {
                let chest = motion.position + Vec3::Y * chest_height(motion.stance);
                candidates.push((entity, distance, chest, outside));
            }
        }
        candidates.sort_by(|a, b| a.1.total_cmp(&b.1));
        let mut best: Option<(f32, Entity, Vec3, bool)> = None;
        for &(entity, distance, chest, outside) in candidates.iter().take(MAX_SIGHT_CHECKS) {
            if w.smoke.blocks(eye, chest) || !tactics::line_of_sight(&w.spatial, eye, chest) {
                continue;
            }
            intel.report(me.team, entity, chest);
            let mut score = distance;
            if Some(entity) == self.target {
                score -= 15.0;
            }
            if self.threat.is_some_and(|(t, _)| t == entity) {
                score -= 25.0;
            }
            if best.is_none_or(|(s, ..)| score < s) {
                best = Some((score, entity, chest, outside));
            }
        }

        let seen = best.map(|(_, entity, ..)| entity);
        if seen != self.target {
            self.target = seen;
            self.engaged = 0.0;
            if let Some((_, _, chest, outside)) = best {
                let impairment = self.impairment();
                self.reaction = (skill.reaction_time() + if outside { 0.3 } else { 0.0 }) * impairment;
                let angle = fastrand::f32() * TAU;
                self.aim_error =
                    Vec2::new(angle.cos(), angle.sin()) * skill.aim_error() * (0.6 + 0.8 * fastrand::f32()) * impairment;
                self.style = self.engage_style(w, me, chest.distance(eye));
            }
        }
        match best {
            Some((_, _, chest, _)) => {
                self.last_seen = Some((chest, 0.0));
                self.heard = None;
            }
            None => self.heard = heard.map(|(_, at)| at),
        }
    }

    /// The enemy most likely shooting at us: one aiming within a couple of meters of us.
    fn find_attacker(&self, w: &Senses, me: &Me) -> Option<(Entity, Vec3)> {
        let center = me.motion.position + Vec3::Y;
        w.soldiers
            .iter()
            .filter(|(entity, _, controlled_by, ..)| *entity != me.soldier && w.is_enemy(controlled_by.0, me.team))
            .filter_map(|(entity, motion, ..)| {
                let from = motion.eye_position();
                let aim = motion.view_rotation() * Vec3::NEG_Z;
                let to = center - from;
                let along = to.dot(aim);
                if !(1.0..300.0).contains(&along) {
                    return None;
                }
                let miss = (to - aim * along).length();
                (miss < 2.0 + along * 0.03).then_some((miss, entity, motion.position))
            })
            .min_by(|a, b| a.0.total_cmp(&b.0))
            .map(|(_, entity, position)| (entity, position))
    }

    fn engage_style(&self, w: &Senses, me: &Me, distance: f32) -> EngageStyle {
        let pose = w
            .weapon(me.loadout, self.primary)
            .map_or(FiringPose::Crouching, |weapon| w.data.weapon(weapon).pose);
        if distance < 10.0 {
            return EngageStyle::Strafe;
        }
        match pose {
            FiringPose::Prone if distance > 40.0 => EngageStyle::Prone,
            FiringPose::Crouching | FiringPose::Prone if fastrand::f32() < 0.6 => EngageStyle::Crouch,
            _ => EngageStyle::Strafe,
        }
    }

    /// Where the danger is: the target, whoever hurt us, or where an enemy was seen.
    fn threat_point(&self) -> Option<Vec3> {
        self.threat
            .map(|(_, at)| at + Vec3::Y * 1.5)
            .or(self.last_seen.map(|(at, _)| at))
            .or(self.heard)
    }

    /// Weighs the options (utility) and switches activity when another is worth more.
    /// `covers`: cover searches left this tick (they cast rays).
    fn decide(&mut self, w: &Senses, me: &Me, team_stats: &mut TeamStats, covers: &mut u32) {
        if me.motion.climbing {
            // Hands on the rungs: nothing to do but climb on.
            if matches!(
                self.activity,
                Activity::Engage
                    | Activity::Throw { .. }
                    | Activity::Search { .. }
                    | Activity::Launch { .. }
                    | Activity::Repair { .. }
            ) {
                self.activity = Activity::Objective;
            }
            return;
        }
        if matches!(self.activity, Activity::Throw { .. } | Activity::Launch { .. }) {
            return;
        }
        let position = me.motion.position;
        let hp = me.health_fraction;
        let (aggression, courage) = (self.personality.aggression, self.personality.courage);
        let target = self
            .target
            .and_then(|t| w.soldiers.get(t).ok())
            .map(|(_, motion, ..)| *motion);
        let distance = target.map(|t| t.position.distance(position));
        let engage_range = w
            .weapon(me.loadout, self.primary)
            .map_or(80.0, |weapon| (w.data.weapon(weapon).max_range * 1.1).max(35.0));

        let mut best = (1.0, Activity::Objective);
        // Carrying on with what it does is worth something.
        match self.activity {
            Activity::Cover { time, .. } if time > 0.0 => best = (6.0, self.activity),
            Activity::Flank { time, .. } if time > 0.0 => best = (3.0, self.activity),
            Activity::Search { time, .. } if time > 0.0 => best = (2.5, self.activity),
            Activity::Revive { time, .. } if time > 0.0 => best = (REVIVE_UTILITY, self.activity),
            _ => {}
        }
        let consider = |best: &mut (f32, Activity), utility: f32, activity: Activity| {
            if utility > best.0 {
                *best = (utility, activity);
            }
        };

        // Beyond its weapon's range it carries on, unless shot at.
        if let Some(d) = distance.filter(|d| *d < engage_range || self.hurt_ago < 3.0) {
            let close = if d < 12.0 { 1.3 } else { 1.0 };
            consider(&mut best, 7.5 * close * (0.6 + 0.4 * hp), Activity::Engage);
        }

        // Cover when hurt, more readily the less courage.
        let hurt = hp < 0.6 && self.hurt_ago < 2.5;
        let cover = if hurt || (target.is_some() && hp < 0.3) {
            2.0 * 4.0 * (1.0 - hp) * (1.4 - courage)
        } else {
            0.0
        };
        if cover > best.0
            && !matches!(self.activity, Activity::Cover { .. })
            && *covers > 0
            && let (Some(nav), Some(threat)) = (w.nav(), target.map(|t| t.eye_position()).or(self.threat_point()))
        {
            *covers -= 1;
            if let Some(spot) = tactics::find_cover(nav, &w.spatial, position, threat) {
                // Getting there counts against the time too.
                let time = 2.5 + 3.0 * (1.0 - courage) + spot.distance(position) / 4.0;
                consider(&mut best, cover, Activity::Cover { spot, time });
            }
        }

        // Medics revive teammates down nearby, unless in a fight.
        if self.paddles.is_some()
            && REVIVE_UTILITY > best.0
            && !matches!(self.activity, Activity::Revive { .. })
            && let Some((soldier, _, _)) = w
                .wounded
                .downed_near(me.team, position, REVIVE_DISTANCE)
                .into_iter()
                .find(|(_, _, left)| *left > 3.0)
        {
            consider(&mut best, REVIVE_UTILITY, Activity::Revive { soldier, time: 20.0 });
        }
        // Out of a fight, medics and support soldiers hold their bags out while someone
        // close by is hurt or low on ammo (held, a bag heals or resupplies everyone within a
        // few meters).
        // TODO: badly hurt bots go to medics.
        self.bag = None;
        if target.is_none() && self.hurt_ago > 3.0 {
            let team = me.team;
            let hurt = || !w.wounded.hurt_near(team, position, BAG_REACH, 0.7).is_empty();
            let empty = || {
                w.soldiers.iter().any(|(_, motion, controlled_by, inventory, _, _, loadout, ..)| {
                    motion.position.distance(position) < BAG_REACH
                        && w.teams.get(controlled_by.0).is_ok_and(|t| *t == team)
                        && inventory.zip(loadout).is_some_and(|(i, l)| low_on_ammo(i, l, &w.armory))
                })
            };
            self.bag = match (self.medic_bag, self.ammo_bag) {
                (Some(bag), _) if hurt() => Some(bag),
                (_, Some(bag)) if empty() => Some(bag),
                _ => None,
            };
        }

        // A grenade at enemies behind cover, or at one it can't get.
        if let Some(grenade) = self.grenade
            && self.grenade_cooldown <= 0.0
            && me
                .inventory
                .and_then(|i| i.ammo.get(grenade as usize))
                .is_some_and(|a| a[0] + a[1] > 0)
        {
            let at = match (target, self.last_seen) {
                (Some(t), _) => Some((t.position, true)),
                (None, Some((at, age))) if age < 4.0 => Some((at - Vec3::Y * 1.0, false)),
                _ => None,
            };
            if let Some((at, visible)) = at {
                let d = flat(at - position).length();
                let friends_clear = me.team_index().is_none_or(|t| {
                    w.snapshot.soldiers[t]
                        .iter()
                        .all(|s| s.player == me.player || s.position.distance(at) > GRENADE_SAFETY)
                });
                let utility = match visible {
                    true if self.engaged > 2.5 && fastrand::f32() < 0.1 + 0.25 * aggression => 8.0,
                    true => 0.0,
                    false => 5.0 * (0.5 + aggression),
                };
                // Nothing right in front to bounce it back: the start of the arc is clear.
                let eye = me.motion.eye_position();
                let early = eye + flat(at - eye).normalize_or_zero() * 5.0 + Vec3::Y * 1.5;
                if (12.0..=40.0).contains(&d)
                    && friends_clear
                    && utility > best.0
                    && tactics::line_of_sight(&w.spatial, eye, early)
                {
                    consider(&mut best, utility, Activity::Throw { at, time: 0.0, weapon: grenade });
                }
            }
        }

        self.equipment_options(w, me, target, &mut best);
        if self.gassed > 0.25
            && let Some(spot) = self.out_of_gas(w, me)
        {
            consider(&mut best, 6.5, Activity::Cover { spot, time: 1.0 + spot.distance(position) / 4.0 });
        }

        if target.is_none() {
            // Around an enemy who went behind cover.
            if let Some((at, age)) = self.last_seen
                && (1.0..6.0).contains(&age)
                && self.flank_cooldown <= 0.0
                && aggression > 0.35
                && (15.0..80.0).contains(&flat(at - position).length())
                && 2.0 + 2.0 * aggression > best.0
                && let Some(nav) = w.nav()
                && let Some(spot) = tactics::flank_spot(nav, position, at, if fastrand::bool() { 1.0 } else { -1.0 })
            {
                consider(&mut best, 2.0 + 2.0 * aggression, Activity::Flank { spot, time: 20.0 });
            }
            // Shot by someone unseen: turn to find him.
            if self.hurt_ago < 1.0 {
                let at = self
                    .threat
                    .map(|(_, at)| at + Vec3::Y * 1.5)
                    .unwrap_or_else(|| me.motion.eye_position() + me.motion.view_rotation() * Vec3::Z * 10.0);
                consider(&mut best, 5.0, Activity::Search { at, time: 1.5 + 2.0 * aggression });
            }
            if let Some(at) = self.heard {
                consider(&mut best, 2.0 + aggression, Activity::Search { at: at + Vec3::Y * 1.5, time: 1.5 + 2.0 * aggression });
            }
        }

        let (_, next) = best;
        if std::mem::discriminant(&next) != std::mem::discriminant(&self.activity) {
            match next {
                Activity::Cover { .. } => team_stats.covers += 1,
                Activity::Flank { .. } => {
                    team_stats.flanks += 1;
                    self.flank_cooldown = 20.0;
                }
                Activity::Throw { weapon, .. } if Some(weapon) == self.grenade || Some(weapon) == self.flashbang => {
                    team_stats.grenades += 1
                }
                Activity::Throw { .. } => team_stats.bags += 1,
                Activity::Launch { target: LaunchTarget::Vehicle(_), .. } => team_stats.rockets += 1,
                Activity::Launch { .. } => team_stats.launches += 1,
                Activity::Repair { .. } => team_stats.repairs += 1,
                Activity::Revive { .. } => team_stats.revives += 1,
                _ => {}
            }
            self.activity = next;
        }
        // Strafing stays on the grid: flip at walls and ledges, stand still if both are.
        if self.activity == Activity::Engage && self.style == EngageStyle::Strafe {
            let right = Quat::from_rotation_y(self.yaw) * Vec3::X;
            if !tactics::can_step(w.nav(), position, right * self.strafe, 1.5) {
                self.strafe = -self.strafe;
                self.strafe_ok = tactics::can_step(w.nav(), position, right * self.strafe, 1.5);
            } else {
                self.strafe_ok = true;
            }
        }
    }

    /// Aims at the target and shoots in bursts, strafing or crouched.
    fn engage(&mut self, w: &Senses, me: &Me, skill: Skill, intent: &mut Intent, dt: f32) {
        let Some((_, target, ..)) = self.target.and_then(|t| w.soldiers.get(t).ok()) else {
            self.activity = Activity::Objective;
            return;
        };
        self.engaged += dt;
        let eye = me.motion.eye_position();
        // Aim at the chest, with an error that shrinks while tracking.
        let aim_at = target.position + Vec3::Y * chest_height(target.stance);
        let to = aim_at - eye;
        let distance = to.length();
        let desired_yaw = yaw_to(to) + self.aim_error.x;
        let desired_pitch = to.y.atan2(Vec2::new(to.x, to.z).length()) + self.aim_error.y;
        // The error settles towards a wander whose size depends on skill and distance
        // (an Ornstein-Uhlenbeck process).
        let settle = skill.aim_settle();
        self.aim_error *= 1.0 - (settle * dt).min(1.0);
        let wander = skill.aim_spread(distance) * (2.0 * settle * dt).sqrt() * self.impairment();
        self.aim_error += Vec2::new(gaussian(), gaussian()) * wander;
        self.yaw = turn_towards(self.yaw, desired_yaw, skill.turn_rate() * dt);
        self.pitch += (desired_pitch - self.pitch).clamp(-4.0 * dt, 4.0 * dt);
        intent.look = Look::Aimed;
        self.reaction -= dt;

        // Attackers work their way forward: a few seconds moving, a few shooting.
        let advance = match self.order {
            Some((OrderKind::Attack, index)) if me.health_fraction > 0.5 && distance > 25.0 && index < w.map.areas.len() => {
                let area = &w.map.areas[index];
                let phase = (self.engaged + hash01(self.seed, 3) * 5.0) % 5.0;
                let moving = phase < 1.5 + 1.5 * self.personality.aggression;
                (area.position.distance(me.motion.position) > area.radius && moving).then(|| {
                    self.spot
                        .filter(|(a, _)| *a == index)
                        .map_or(area.order_position, |(_, spot)| spot)
                })
            }
            _ => None,
        };
        if let Some(position) = advance {
            intent.goal = Some(Goal {
                position,
                tolerance: 3.0,
                sprint: false,
            });
            return;
        }

        let tolerance = (1.2 / distance.max(1.0)).clamp(0.02, 0.15);
        let on_target = angle_delta(self.yaw, desired_yaw).abs() + (self.pitch - desired_pitch).abs() < tolerance;
        let weapon = w.weapon(me.loadout, self.primary);
        let mode = weapon
            .and_then(|weapon| weapon.fire_modes.get(me.inventory.map_or(0, |i| i.fire_mode) as usize).copied())
            .unwrap_or(FireMode::Auto);
        if self.reaction <= 0.0 && on_target {
            self.burst -= dt;
            match mode {
                FireMode::Auto => {
                    // Long bursts up close, short ones far away.
                    let (burst, pause) = match distance {
                        d if d < 15.0 => (0.4 + 0.4 * fastrand::f32(), 0.2),
                        d if d < 40.0 => (0.2 + 0.2 * fastrand::f32(), 0.35),
                        _ => (0.08 + 0.08 * fastrand::f32(), 0.5),
                    };
                    if self.burst < -pause - fastrand::f32() * 0.3 {
                        self.burst = burst;
                    }
                    if self.burst > 0.0 {
                        intent.buttons |= Buttons::FIRE;
                    }
                }
                // A fresh pull for every shot.
                FireMode::Single | FireMode::Burst => {
                    if self.burst <= 0.0 {
                        intent.buttons |= Buttons::FIRE;
                        let aimed = (distance / 100.0).clamp(0.1, 0.8);
                        self.burst = aimed + fastrand::f32() * 0.2;
                    }
                }
            }
        }

        let style = if distance < 8.0 { EngageStyle::Strafe } else { self.style };
        match style {
            EngageStyle::Strafe => {
                if fastrand::f32() < dt * 0.7 {
                    self.strafe = -self.strafe;
                    self.decide_timer = 0.0;
                }
                if self.strafe_ok {
                    intent.step = Quat::from_rotation_y(self.yaw) * Vec3::X * self.strafe;
                }
            }
            EngageStyle::Crouch => intent.buttons |= Buttons::CROUCH,
            EngageStyle::Prone => intent.buttons |= Buttons::PRONE,
        }
    }

    /// Switches to the grenade, winds up and throws it along an arc onto `at`, then goes
    /// back to the main weapon.
    #[allow(clippy::too_many_arguments)]
    fn throw(&mut self, w: &Senses, me: &Me, at: Vec3, time: f32, weapon: u8, intent: &mut Intent, dt: f32) {
        let Some(desc) = w.weapon(me.loadout, weapon) else {
            self.activity = Activity::Objective;
            return;
        };
        intent.weapon = Some(weapon);
        let eye = me.motion.eye_position();
        // A little short: grenades bounce and roll on.
        let aim = eye + (at - eye) * 0.9;
        let pitch = tactics::throw_pitch(eye, aim, desc.projectile.velocity, GRAVITY * desc.projectile.gravity)
            .unwrap_or(0.7);
        let yaw = yaw_to(aim - eye);
        self.yaw = turn_towards(self.yaw, yaw, 6.0 * dt);
        self.pitch += (pitch - self.pitch).clamp(-4.0 * dt, 4.0 * dt);
        intent.look = Look::Aimed;
        let aimed = angle_delta(self.yaw, yaw).abs() < 0.05 && (self.pitch - pitch).abs() < 0.05;
        let ready = desc.deploy_time + 0.15;
        let wound = ready + desc.fire.pull_back + 0.1;
        let mut time = time + dt;
        if time >= ready && time < wound {
            if aimed || time > ready + dt {
                intent.buttons |= Buttons::FIRE;
            } else {
                // Wind up once the aim is right.
                time = ready;
            }
        }
        if time > wound + desc.fire.launch_delay + 0.4 {
            self.activity = Activity::Objective;
            if Some(weapon) == self.grenade || Some(weapon) == self.flashbang {
                self.grenade_cooldown = 20.0 + 20.0 * fastrand::f32();
            } else {
                self.bag_cooldown = 8.0;
            }
            intent.weapon = Some(self.primary);
            if Some(weapon) == self.flashbang {
                // Turn away before it goes off.
                let behind = me.motion.eye_position() - flat(at - me.motion.position).normalize_or_zero() * 10.0;
                self.activity = Activity::Search { at: behind, time: 2.5 };
            }
            return;
        }
        self.activity = Activity::Throw { at, time, weapon };
    }

    /// Which way to step out of the path of a vehicle about to run it over, if one is.
    fn dodge_vehicles(&self, w: &Senses, me: &Me) -> Option<Vec3> {
        let position = me.motion.position;
        w.vehicles.iter().find_map(|(_, vehicle, _)| {
            let velocity = flat(vehicle.velocity);
            let speed = velocity.length();
            let offset = flat(position - vehicle.position);
            if speed < DANGEROUS_SPEED || offset.length() > 40.0 || (vehicle.position.y - position.y).abs() > 3.0 {
                return None;
            }
            // When it passes closest, and how close.
            let t = offset.dot(velocity) / (speed * speed);
            let miss = offset - velocity * t;
            ((0.0..2.5).contains(&t) && miss.length() < 4.0).then(|| {
                let side = Vec3::new(-velocity.z, 0.0, velocity.x) / speed;
                if miss.dot(side) >= 0.0 { side } else { -side }
            })
        })
    }

    /// Walks to a downed teammate with the paddles out and shocks him until he is back up
    /// (or gone).
    fn revive(&mut self, w: &Senses, me: &Me, soldier: Entity, time: f32, intent: &mut Intent, dt: f32) {
        let body = w.soldiers.get(soldier).ok().filter(|s| s.8).map(|s| s.1.position);
        let (Some(body), Some(paddles), true) = (body, self.paddles, time > 0.0) else {
            self.activity = Activity::Objective;
            return;
        };
        let distance = flat(body - me.motion.position).length();
        if distance > 1.2 {
            intent.goal = Some(Goal {
                position: body,
                tolerance: 1.0,
                sprint: distance > 10.0,
            });
        }
        if distance < 8.0 {
            intent.weapon = Some(paddles);
        }
        if distance < PADDLES_REACH - 0.8 {
            intent.look = Look::At(body + Vec3::Y * 0.2);
            // A fresh press for every shock.
            self.burst -= dt;
            if self.burst <= 0.0 {
                intent.buttons |= Buttons::FIRE;
                self.burst = 0.6;
            }
        }
        self.activity = Activity::Revive { soldier, time: time - dt };
    }

    /// The order this bot works on: its squad's, the flag its human leader is at, or the
    /// best for itself.
    fn current_order(&self, w: &Senses, me: &Me) -> Option<(OrderKind, usize)> {
        let key = me.member.map(|m| (me.team, m.squad));
        let squad = key.and_then(|k| w.snapshot.squads.get(&k));
        let human_leader = squad
            .filter(|s| !s.leader_is_bot && !me.member.is_some_and(|m| m.leader))
            .and_then(|s| s.leader_soldier);
        if let Some(leader) = human_leader {
            return self.area_near(w, leader.position, me.team);
        }
        key.and_then(|k| w.strategy.orders.get(&k))
            .map(|order| (order.kind, order.area))
            .or_else(|| {
                w.strategy
                    .objective_for(me.team, me.motion.position, &w.map, self.seed)
                    .map(|o| (o.kind, o.area))
            })
            .filter(|(_, area)| *area < w.map.areas.len())
    }

    /// The flag someone at `position` is at: to take or to hold.
    fn area_near(&self, w: &Senses, position: Vec3, team: Team) -> Option<(OrderKind, usize)> {
        w.map
            .areas
            .iter()
            .enumerate()
            .filter(|(_, a)| a.control_point.is_some() && !a.uncapturable)
            .find(|(_, a)| a.position.distance(position) < a.radius + 25.0)
            .map(|(i, _)| (if w.holds(i, team) { OrderKind::Defend } else { OrderKind::Attack }, i))
    }

    /// Squad and objective movement: follow the leader, wait for the squad, head for the
    /// objective and take a spot of its own there.
    fn objective(&mut self, w: &Senses, me: &Me, intent: &mut Intent, dt: f32) {
        let position = me.motion.position;
        let squad = me.member.and_then(|m| w.snapshot.squads.get(&(me.team, m.squad)));
        let is_leader = me.member.is_some_and(|m| m.leader);
        let leader = squad.filter(|_| !is_leader).and_then(|s| s.leader_soldier);
        let human_led = squad.is_some_and(|s| !s.leader_is_bot);

        let order = self.current_order(w, me);
        if order != self.order {
            self.order = order;
            self.spot = None;
            self.regroup = 0.0;
            self.staged = false;
            self.via = order
                .filter(|(_, area)| w.map.areas[*area].position.distance(position) > 120.0)
                .and_then(|(_, area)| w.map.route_waypoint(position, area).or_else(|| self.flank_via(w, me, area)));
        }
        let area = order.map(|(_, a)| (a, &w.map.areas[a]));

        // Members keep up with their leader until they are close to the objective.
        let follow = leader.filter(|l| match area {
            None => true,
            Some((_, area)) if human_led => l.position.distance(area.position) > area.radius + 25.0,
            Some((_, area)) => area.position.distance(position) > 45.0 && area.position.distance(l.position) > 25.0,
        });
        if let (Some(leader), Some(squad)) = (follow, squad) {
            let slot = squad.slot(me.player).unwrap_or(0);
            let spacing = 3.0 + 3.0 * (1.0 - self.personality.teamwork);
            let place = squad::formation_slot(&leader, slot, spacing);
            let distance = flat(place - position).length();
            if distance > 2.0 {
                intent.goal = Some(Goal {
                    position: place,
                    // The place moves with the leader: keep the path while it's roughly right.
                    tolerance: (distance * 0.4).clamp(3.0, 20.0),
                    sprint: distance > 15.0,
                });
            } else {
                intent.look = Look::Yaw(leader.yaw + (self.sweep.sin() * 0.8));
                self.sweep += dt * 0.5;
            }
            return;
        }

        let Some((area_index, area)) = area else {
            self.roam(w, me, intent);
            return;
        };
        let distance = area.position.distance(position);

        // Leaders wait for a squad that fell behind, a while, and gather it before an
        // assault so it arrives together rather than one by one.
        if is_leader
            && let Some(squad) = squad
            && let Some(spread) = squad.spread()
        {
            let others = squad.alive.len().saturating_sub(1);
            let close = squad
                .alive
                .iter()
                .filter(|m| m.player != me.player && m.position.distance(position) < 25.0)
                .count();
            let assault = order.is_some_and(|(k, _)| k == OrderKind::Attack)
                && distance < 2.0 * APPROACH_DISTANCE
                && self.target.is_none();
            let gathering = assault && !self.staged && close * 10 < others * 6 && self.regroup < 20.0;
            if assault && !gathering {
                self.staged = true;
            }
            if spread < 15.0 && !gathering {
                self.regroup = 0.0;
            } else if distance > APPROACH_DISTANCE && (gathering || (spread > 30.0 && self.regroup < 10.0)) {
                self.regroup += dt;
                let others: Vec<Vec3> =
                    squad.alive.iter().filter(|m| m.player != me.player).map(|m| m.position).collect();
                if !others.is_empty() {
                    let center = others.iter().sum::<Vec3>() / others.len() as f32;
                    intent.look = Look::At(center + Vec3::Y * 1.5);
                }
                intent.buttons |= Buttons::CROUCH;
                return;
            }
        }

        if distance > APPROACH_DISTANCE {
            if let Some(via) = self.via
                && flat(via - position).length() < 10.0
            {
                self.via = None;
            }
            intent.goal = Some(Goal {
                position: self.via.unwrap_or(area.order_position),
                tolerance: 3.0,
                sprint: distance > 40.0,
            });
            return;
        }

        // At the objective: a spot of its own, inside the radius to take the flag, around
        // it facing the enemy to hold it.
        self.spot_timer -= dt;
        if self.spot.is_none_or(|(a, _)| a != area_index) || self.spot_timer <= 0.0 {
            let kind = order.map_or(OrderKind::Attack, |(k, _)| k);
            let spot = self.pick_spot(w, me, area_index, kind);
            self.spot = Some((area_index, spot));
            self.spot_timer = 10.0 + 15.0 * fastrand::f32();
        }
        let (_, spot) = self.spot.unwrap();
        if flat(spot - position).length() > 1.2 {
            intent.goal = Some(Goal {
                position: spot,
                tolerance: 0.8,
                sprint: flat(spot - position).length() > 25.0,
            });
            // Don't wander off before the flag is ours.
            self.spot_timer = self.spot_timer.max(2.0);
        } else {
            let facing = self.facing(w, me.team, area_index);
            self.sweep += dt * 0.4;
            intent.look = Look::Yaw(facing + self.sweep.sin() * 1.0);
            let defending = order.is_some_and(|(k, _)| k == OrderKind::Defend);
            if defending && hash01(self.seed, 7) < 0.6 {
                intent.buttons |= Buttons::CROUCH;
            }
        }
    }

    /// For squads without a route laid out by the level: a waypoint off to one side of the
    /// straight line to the objective, so squads attacking the same flag come at it from
    /// several directions.
    fn flank_via(&self, w: &Senses, me: &Me, area: usize) -> Option<Vec3> {
        let squad = me.member?.squad;
        let angle = [0.0f32, 45.0, -45.0][squad as usize % 3];
        if angle == 0.0 {
            return None;
        }
        let target = w.map.areas[area].position;
        let from = (me.motion.position - target).with_y(0.0).normalize_or_zero();
        let via = target + Quat::from_rotation_y(angle.to_radians()) * from * 80.0;
        let nav = w.nav()?;
        let region = nav.cell(nav.locate(me.motion.position, 2.0, None)?).region;
        nav.locate(via, 12.0, Some(region)).map(|cell| nav.position(cell))
    }

    /// A spot in or around an area for this bot: inside the capture radius to attack, in a
    /// ring around the flag facing the enemy to defend.
    fn pick_spot(&self, w: &Senses, me: &Me, area_index: usize, kind: OrderKind) -> Vec3 {
        let area = &w.map.areas[area_index];
        let slot = me.member.and_then(|m| {
            w.snapshot.squads.get(&(me.team, m.squad)).and_then(|s| s.slot(me.player))
        });
        let base = hash01(self.seed, 1) * TAU + slot.unwrap_or(0) as f32 * 2.399;
        let cp = w.flag(area_index).map(|(cp, _)| cp);
        let facing = self.facing(w, me.team, area_index);
        for attempt in 0..6 {
            let jitter = fastrand::f32();
            let (angle, radius) = match (kind, cp) {
                (OrderKind::Attack, Some(cp)) => (base + attempt as f32 * 1.3, cp.radius * (0.2 + 0.55 * jitter)),
                (OrderKind::Defend, Some(cp)) => {
                    // In front of the flag, towards the enemy.
                    let spread = (jitter - 0.5) * 2.4;
                    (-(facing + spread) - std::f32::consts::FRAC_PI_2, cp.radius * 0.5 + 4.0 + 8.0 * fastrand::f32())
                }
                _ => (base + attempt as f32 * 1.3, 8.0 * jitter),
            };
            let center = if cp.is_some() { area.position } else { area.order_position };
            let candidate = center + Vec3::new(angle.cos(), 0.0, angle.sin()) * radius;
            let Some(nav) = w.nav() else {
                return candidate;
            };
            if let Some(cell) = nav.locate(candidate, 2.0, None) {
                let spot = nav.position(cell);
                let inside = match (kind, cp) {
                    (OrderKind::Attack, Some(cp)) => cp.contains(spot),
                    (_, Some(cp)) => spot.distance(cp.position) < cp.radius + 16.0,
                    _ => true,
                };
                if inside {
                    return spot;
                }
            }
        }
        area.order_position
    }

    /// Which way to face holding an area: towards the nearest neighbouring area the team
    /// doesn't hold, or where the enemy was seen.
    fn facing(&self, w: &Senses, team: Team, area_index: usize) -> f32 {
        let area = &w.map.areas[area_index];
        if let Some((at, age)) = self.last_seen
            && age < 20.0
        {
            return yaw_to(at - area.position);
        }
        area.neighbours
            .iter()
            .filter(|&&n| !w.holds(n, team))
            .map(|&n| w.map.areas[n].position)
            .min_by(|a, b| a.distance(area.position).total_cmp(&b.distance(area.position)))
            .map_or(self.yaw, |p| yaw_to(p - area.position))
    }

    /// Levels without control points: wander about.
    fn roam(&mut self, w: &Senses, me: &Me, intent: &mut Intent) {
        let position = me.motion.position;
        let reached = self.spot.is_none_or(|(_, spot)| flat(spot - position).length() < 2.0);
        if reached || self.spot_timer <= 0.0 {
            self.spot = Some((usize::MAX, random_spot(&w.level, position)));
            self.spot_timer = 40.0 + 40.0 * fastrand::f32();
        }
        if let Some((_, spot)) = self.spot {
            intent.goal = Some(Goal {
                position: spot,
                tolerance: 1.0,
                sprint: flat(spot - position).length() > 30.0,
            });
        }
    }

    /// Turns the intent into input: path following, getting unstuck, turning the view.
    fn act(&mut self, w: &Senses, me: &Me, intent: Intent, stats: &mut BotStats, dt: f32) -> InputFrame {
        let position = me.motion.position;
        let mut direction = Vec3::ZERO;
        let mut jump = false;
        let mut ladder = None;
        self.goal = intent.goal.map(|g| g.position);
        // Where walking can't get it (a carrier, an island), it waits for a while rather than
        // pushing against the edge, then tries again.
        // TODO: vehicles (boats, aircraft) off such spawns.
        self.stranded -= dt;
        if self.stranded <= 0.0 && self.stranded > -dt {
            self.repath = true;
        }
        if let Some(goal) = intent.goal.filter(|_| self.stranded <= 0.0) {
            // Short, clear moves need no path.
            if self.direct_goal.is_none_or(|g| g.distance_squared(goal.position) > 1.0) {
                self.direct_goal = Some(goal.position);
                self.direct = flat(goal.position - position).length() < 15.0
                    && w.nav().is_none_or(|nav| nav.walkable_line(position, goal.position));
            }
            let (target, j, step) = match (w.nav.as_deref(), self.direct) {
                (Some(nav), false) => match self.follow_path(nav, goal.position, goal.tolerance, me.motion, dt, stats) {
                    Steer::Toward { target, jump, ladder } => (target, jump, ladder),
                    Steer::Arrived => (goal.position, false, None),
                    Steer::Stranded => {
                        stats.stranded += 1;
                        self.stranded = 10.0;
                        (position, false, None)
                    }
                },
                _ => (goal.position, false, None),
            };
            ladder = step;
            let to = flat(target - position);
            if to.length() > 0.3 {
                direction = to.normalize();
            }
            jump = j;
        } else {
            direction = intent.step;
        }
        // Out of the way of vehicles coming at speed, friend or foe.
        let dodging = !me.motion.climbing
            && match self.dodge_vehicles(w, me) {
                Some(away) => {
                    direction = away;
                    true
                }
                None => false,
            };

        // Keep a little apart from teammates: crowds jam doorways, stairs and ladders. Being
        // held up by one isn't being stuck on the level.
        let mut queued = false;
        if direction.length_squared() > 0.01
            && ladder.is_none()
            && let Some(t) = me.team_index()
        {
            let mut push = Vec3::ZERO;
            for other in w.snapshot.soldiers[t].iter().filter(|s| s.player != me.player) {
                let away = flat(position - other.position);
                let d = away.length();
                if d > 0.01 && d < CROWD_DISTANCE && (other.position.y - position.y).abs() < 1.5 {
                    push += away / d * (CROWD_DISTANCE - d);
                    queued |= d < 1.0 && direction.dot(-away / d) > 0.5;
                }
            }
            direction = (direction + push.clamp_length_max(0.6)).normalize_or(direction);
        }
        let wants_move = direction.length_squared() > 0.01;
        if me.motion.climbing && !self.climbing {
            stats.climbs += 1;
        }
        self.climbing = me.motion.climbing;

        // Detect being stuck on geometry: wiggle free, then find a new path from there.
        let moved = flat(position - self.last_position).length();
        // More than a tick's worth: respawned.
        let moved = if moved > 1.0 { 0.0 } else { moved };
        self.last_position = position;
        self.stuck_strikes = (self.stuck_strikes - dt / 10.0).max(0.0);
        if intent.goal.is_some() && wants_move && !me.motion.climbing {
            stats.moving_seconds += dt;
            stats.moved += moved;
            if moved < 0.5 * dt && self.unstuck_timer <= 0.0 && me.motion.grounded && !queued {
                self.stuck_time += dt;
                stats.stuck_seconds += dt;
            } else {
                self.stuck_time = 0.0;
            }
            if self.stuck_time > 0.75 {
                stats.stuck_events += 1;
                let square = ((position.x / 10.0).floor() as i32, (position.z / 10.0).floor() as i32);
                *stats.stuck_spots.entry(square).or_default() += 1;
                stats.stuck_by_activity[match self.activity {
                    Activity::Engage => 1,
                    Activity::Cover { .. } | Activity::Flank { .. } => 2,
                    _ => 0,
                }] += 1;
                self.stuck_time = 0.0;
                self.unstuck_timer = 0.6 + fastrand::f32() * 0.8;
                // First try jumping ahead (a ledge the grid thinks is lower), then sideways.
                self.unstuck_dir = match self.stuck_strikes < 0.5 {
                    true => 0.0,
                    false if fastrand::bool() => 1.0,
                    false => -1.0,
                };
                self.repath = true;
                self.direct = false;
                self.stuck_strikes += 1.0;
                if self.stuck_strikes > 3.0 {
                    // Keeps failing here: go somewhere else.
                    self.stuck_strikes = 0.0;
                    self.spot = None;
                    self.via = None;
                }
            }
        } else {
            self.stuck_time = 0.0;
        }

        match intent.look {
            Look::Along => {
                if wants_move {
                    self.yaw = turn_towards(self.yaw, yaw_to(direction), 5.0 * dt);
                }
                self.pitch += (0.0 - self.pitch).clamp(-2.0 * dt, 2.0 * dt);
            }
            Look::At(point) => {
                let to = point - me.motion.eye_position();
                self.yaw = turn_towards(self.yaw, yaw_to(to), 4.0 * dt);
                let pitch = to.y.atan2(Vec2::new(to.x, to.z).length()).clamp(-0.6, 0.6);
                self.pitch += (pitch - self.pitch).clamp(-2.0 * dt, 2.0 * dt);
            }
            Look::Yaw(yaw) => {
                self.yaw = turn_towards(self.yaw, yaw, 2.0 * dt);
                self.pitch += (0.0 - self.pitch).clamp(-2.0 * dt, 2.0 * dt);
            }
            Look::Aimed => {}
        }

        let mut frame = InputFrame {
            seq: self.seq,
            yaw: self.yaw,
            pitch: self.pitch,
            weapon: intent.weapon.unwrap_or(self.primary),
            buttons: intent.buttons,
            ..default()
        };
        let local = |world: Vec3| {
            let local = Quat::from_rotation_y(-self.yaw) * world;
            Vec2::new(local.x, -local.z)
        };
        let mut movement = local(direction);
        if self.unstuck_timer > 0.0 && intent.goal.is_some() {
            self.unstuck_timer -= dt;
            let side = Vec3::new(-direction.z, 0.0, direction.x) * self.unstuck_dir;
            let forward = if self.unstuck_dir == 0.0 { 1.0 } else { 0.3 };
            movement = local(direction * forward + side);
            frame.buttons |= Buttons::JUMP;
            frame.buttons.remove(Buttons::CROUCH | Buttons::PRONE);
        } else {
            if jump {
                frame.buttons |= Buttons::JUMP;
            }
            // Sprint in bursts: start rested, stop with some stamina left for a fight or an
            // escape (all of it may go running for cover or out of a vehicle's way).
            let urgent = dodging || matches!(self.activity, Activity::Cover { .. });
            let reserve = if urgent { 0.0 } else { SPRINT_RESERVE };
            if me.motion.stamina <= reserve {
                self.sprinting = false;
            } else if me.motion.stamina > 0.8 || urgent {
                self.sprinting = true;
            }
            let sprint = (intent.goal.is_some_and(|g| g.sprint) || dodging)
                && movement.y > 0.7
                && self.target.is_none()
                && self.sprinting;
            if sprint && !frame.buttons.intersects(Buttons::CROUCH | Buttons::PRONE | Buttons::FIRE) {
                frame.buttons |= Buttons::SPRINT;
            }
        }
        // Reload between fights.
        if self.target.is_none()
            && intent.weapon.is_none()
            && let Some(inventory) = me.inventory
            && let Some(weapon) = w.weapon(me.loadout, self.primary)
            && weapon.magazine_size > 0
            && inventory.ammo.get(self.primary as usize).is_some_and(|a| (a[0] as u32) * 2 < weapon.magazine_size && a[1] > 0)
        {
            frame.buttons |= Buttons::RELOAD;
        }
        if me.motion.climbing {
            match ladder.filter(|_| intent.goal.is_some()) {
                // Forward climbs; looking down, forward climbs down.
                Some(step) => {
                    movement = Vec2::Y;
                    self.yaw = turn_towards(self.yaw, yaw_to(-step.front), 5.0 * dt);
                    self.pitch = if step.up { 0.1 } else { -0.9 };
                    frame.yaw = self.yaw;
                    frame.pitch = self.pitch;
                    frame.buttons.remove(Buttons::JUMP | Buttons::CROUCH | Buttons::PRONE | Buttons::SPRINT);
                }
                // On a ladder it didn't mean to take: jump off.
                None => frame.buttons |= Buttons::JUMP,
            }
        }
        // Jumping takes a fresh press: holding the button would jump once.
        let jump = frame.buttons.contains(Buttons::JUMP) && !self.jump_held;
        frame.buttons.set(Buttons::JUMP, jump);
        self.jump_held = jump;
        frame.set_movement(movement);
        frame
    }

    /// Requests paths as needed and walks them waypoint by waypoint. Without a path yet,
    /// heads straight for the goal. The path is kept while the goal moves less than
    /// `tolerance` meters.
    fn follow_path(
        &mut self,
        nav: &Navigation,
        goal: Vec3,
        tolerance: f32,
        motion: &SoldierMotion,
        dt: f32,
        stats: &mut BotStats,
    ) -> Steer {
        let position = motion.position;
        if let Some(task) = &mut self.path_task
            && let Some(result) = check_ready(task)
        {
            self.path_task = None;
            stats.paths += 1;
            stats.path_seconds += result.seconds;
            stats.max_path_seconds = stats.max_path_seconds.max(result.seconds);
            match &result.path {
                Some(path) if !path.complete => stats.partial_paths += 1,
                Some(_) => {}
                None => {
                    stats.failed_paths += 1;
                    self.repath_cooldown = 1.0;
                }
            }
            self.path = result.path;
            self.waypoint = 0;
            self.waypoint_best = f32::MAX;
        }

        self.repath_cooldown -= dt;
        let tolerance = tolerance.max(0.5);
        if self.path_goal.is_none_or(|g| g.distance_squared(goal) > tolerance * tolerance) {
            // New goal: the old path and any request for it are useless now.
            self.path = None;
            self.path_task = None;
            self.repath = true;
        }
        if self.repath && self.path_task.is_none() && self.repath_cooldown <= 0.0 {
            self.repath = false;
            self.repath_cooldown = 0.5;
            self.path_goal = Some(goal);
            let grid = nav.0.clone();
            self.path_task = Some(AsyncComputeTaskPool::get().spawn(async move {
                let started = Instant::now();
                let path = grid.find_path(position, goal);
                PathResult {
                    path,
                    seconds: started.elapsed().as_secs_f32(),
                }
            }));
        }

        let Some(path) = &self.path else {
            return Steer::Toward {
                target: goal,
                jump: false,
                ladder: None,
            };
        };
        while let Some(waypoint) = path.waypoints.get(self.waypoint) {
            let last = self.waypoint + 1 == path.waypoints.len();
            // Ladders are narrow: get to where they are climbed from exactly.
            let before_ladder = path.waypoints.get(self.waypoint + 1).is_some_and(|w| w.ladder.is_some());
            let reach = match (before_ladder, last) {
                (true, _) => 0.35,
                (_, true) => 0.6,
                _ => 0.8,
            };
            let close = flat(waypoint.position - position).length() < reach;
            if close && (waypoint.position.y - position.y).abs() < 1.5 {
                self.waypoint += 1;
                self.waypoint_best = f32::MAX;
            } else {
                break;
            }
        }
        let Some(waypoint) = path.waypoints.get(self.waypoint) else {
            let complete = path.complete;
            self.path = None;
            return if complete || flat(goal - position).length() < 3.0 {
                Steer::Arrived
            } else {
                Steer::Stranded
            };
        };
        if motion.climbing {
            // Height changes and no progress along the ground are what climbing is.
            self.waypoint_timer = 0.0;
            return Steer::Toward {
                target: waypoint.position,
                jump: false,
                ladder: waypoint.ladder,
            };
        }
        // Pushed off the path (by fighting, say) or fell off a ledge: find a new one.
        let previous = path.waypoints[self.waypoint.saturating_sub(1)].position;
        let (off_path, t) = segment_offset(flat(position), flat(previous), flat(waypoint.position));
        let path_height = previous.y + (waypoint.position.y - previous.y) * t;
        // Long straight stretches may cross humps.
        let height_tolerance = 1.5 + 0.05 * flat(waypoint.position - previous).length();
        if off_path > 3.0 || (position.y - path_height).abs() > height_tolerance {
            self.repath = true;
        }
        // Not getting any closer to the waypoint (sliding along a wall, say): same.
        let distance = flat(waypoint.position - position).length();
        if distance < self.waypoint_best - 0.3 {
            self.waypoint_best = distance;
            self.waypoint_timer = 0.0;
        } else {
            self.waypoint_timer += dt;
            if self.waypoint_timer > 2.0 {
                stats.no_progress += 1;
                self.repath = true;
                self.waypoint_best = distance;
                self.waypoint_timer = 0.0;
            }
        }
        // Onto a ladder: straight along its middle, into it going up, over its top going down.
        let target = match waypoint.ladder {
            Some(step) => previous + if step.up { -step.front } else { step.front },
            None => waypoint.position,
        };
        Steer::Toward {
            target,
            jump: waypoint.jump && motion.grounded && flat(waypoint.position - position).length() < 1.2,
            ladder: waypoint.ladder,
        }
    }

    /// While dead: pick the kit the team needs and where to spawn.
    fn while_dead(&mut self, w: &Senses, player: Entity, team: Team, member: Option<SquadMember>, deployment: &mut Mut<Deployment>) {
        self.forget_life();
        self.decide_timer -= w.time.delta_secs();
        if self.decide_timer > 0.0 {
            return;
        }
        self.decide_timer = 0.5;
        let Some(t) = team_index(team) else {
            return;
        };
        if !self.deployed {
            self.deployed = true;
            let kit = squad::choose_kit(&w.armory, t, &w.snapshot.kits[t], player, deployment.kit, &self.personality);
            if kit != deployment.kit {
                deployment.kit = kit;
            }
        }
        let squad = member.and_then(|m| w.snapshot.squads.get(&(team, m.squad)));
        let leader = squad
            .filter(|_| !member.is_some_and(|m| m.leader))
            .and_then(|s| s.leader_soldier);
        let from = squad.and_then(|s| s.centroid()).unwrap_or(self.last_position);
        let area = member
            .and_then(|m| w.strategy.orders.get(&(team, m.squad)))
            .map(|o| o.area)
            .or_else(|| w.strategy.objective_for(team, from, &w.map, self.seed).map(|o| o.area));
        let objective = match leader {
            // Human leaders decide where the squad goes.
            Some(l) if squad.is_some_and(|s| !s.leader_is_bot) => Some(l.position),
            _ => area.and_then(|a| w.map.areas.get(a)).map(|a| a.position),
        };
        let spawns: Vec<(u8, Vec3)> = w
            .map
            .areas
            .iter()
            .enumerate()
            .filter(|(i, a)| a.has_spawns && w.holds(*i, team))
            .filter_map(|(_, a)| Some((a.control_point?, a.position)))
            .collect();
        let (control_point, on_leader) = squad::choose_spawn(objective, leader.as_ref(), &spawns);
        self.spawn_on_leader = on_leader;
        if deployment.control_point != control_point {
            deployment.control_point = control_point;
        }
        if deployment.on_squad_leader != on_leader {
            deployment.on_squad_leader = on_leader;
        }
    }
}

impl Me<'_> {
    fn team_index(&self) -> Option<usize> {
        team_index(self.team)
    }
}

fn spawn_bots(
    mut commands: Commands,
    settings: Res<ServerSettings>,
    bots: Query<(), With<BotBrain>>,
    teams: Query<&Team, With<Player>>,
) {
    let existing = bots.iter().count() as u32;
    if existing >= settings.bots {
        return;
    }
    let mut teams: Vec<Team> = teams.iter().copied().collect();
    for i in existing..settings.bots {
        let team = balanced_team(teams.iter());
        teams.push(team);
        let name = BOT_NAMES[i as usize % BOT_NAMES.len()];
        commands.spawn((
            Player {
                name: format!("{name} (bot)"),
                is_bot: true,
            },
            team,
            InputBuffer::default(),
            BotBrain::default(),
            Deployment {
                kit: fastrand::u8(0..7),
                ..default()
            },
            Replicated,
        ));
    }
    info!("added {} bots", settings.bots - existing);
}

#[allow(clippy::type_complexity)]
fn think(
    w: Senses,
    mut bots: Query<(
        Entity,
        &mut BotBrain,
        &mut InputBuffer,
        &Team,
        Option<&Controls>,
        Option<&SquadMember>,
        &mut Deployment,
    )>,
    mut intel: ResMut<TeamIntel>,
    mut stats: ResMut<BotStats>,
    mut ai_stats: ResMut<AiStats>,
) {
    let started = Instant::now();
    let mut covers = 3;
    for (player, mut brain, mut buffer, team, controls, member, mut deployment) in &mut bots {
        let soldier = controls.and_then(|c| w.soldiers.get(c.0).ok());
        let Some((own, motion, _, inventory, health, _, loadout, seated, downed)) = soldier else {
            brain.while_dead(&w, player, *team, member.copied(), &mut deployment);
            continue;
        };
        // Down, the server ignores its input; in a vehicle, bots do nothing yet.
        if seated.is_some() || downed {
            // TODO: bots driving and gunning vehicles. They never get in by themselves.
            brain.seq = brain.seq.wrapping_add(1);
            buffer.push(InputFrame {
                seq: brain.seq,
                yaw: brain.yaw,
                ..default()
            });
            continue;
        }
        let health = health.copied().unwrap_or_default();
        let me = Me {
            player,
            team: *team,
            member: member.copied(),
            soldier: own,
            motion,
            inventory,
            loadout,
            health: health.current,
            health_fraction: (health.current / health.max.max(1.0)).clamp(0.0, 1.0),
        };
        let mut spare = TeamStats::default();
        let team_stats = ai_stats.team(*team).unwrap_or(&mut spare);
        let frame = brain.tick(&w, &me, &mut intel, &mut stats, team_stats, &mut covers);
        buffer.push(frame);
    }
    let ms = started.elapsed().as_secs_f32() * 1000.0;
    stats.think_ms += ms;
    stats.max_think_ms = stats.max_think_ms.max(ms);
    stats.ticks += 1;
}

fn log_stats(time: Res<Time>, mut stats: ResMut<BotStats>, bots: Query<(), With<BotBrain>>) {
    stats.elapsed += time.delta_secs();
    if stats.elapsed < 60.0 {
        return;
    }
    if !bots.is_empty() {
        info!(
            "bots: {} stuck events in the last minute ({} on orders, {} advancing, {} to cover or \
             flanking; {:.0} s stuck of {:.0} bot-seconds moving, \
             {:.1} m/s, {} bots); {} paths ({} partial, {} failed, {} for lack of progress), \
             {:.1} ms avg, {:.1} ms max; {} ladders climbed, {} goals out of reach; thinking {:.2} ms per tick, {:.1} ms max; \
             stuck most at {}",
            stats.stuck_events,
            stats.stuck_by_activity[0],
            stats.stuck_by_activity[1],
            stats.stuck_by_activity[2],
            stats.stuck_seconds,
            stats.moving_seconds,
            stats.moved / stats.moving_seconds.max(1.0),
            bots.iter().count(),
            stats.paths,
            stats.partial_paths,
            stats.failed_paths,
            stats.no_progress,
            stats.path_seconds * 1000.0 / stats.paths.max(1) as f32,
            stats.max_path_seconds * 1000.0,
            stats.climbs,
            stats.stranded,
            stats.think_ms / stats.ticks.max(1) as f32,
            stats.max_think_ms,
            hotspots(&stats.stuck_spots),
        );
    }
    *stats = BotStats::default();
}

/// The three 10 m squares with the most stuck events, as `x z (count)`.
fn hotspots(spots: &bevy::platform::collections::HashMap<(i32, i32), u32>) -> String {
    let mut spots: Vec<_> = spots.iter().collect();
    spots.sort_by(|a, b| b.1.cmp(a.1));
    let list: Vec<String> = spots
        .iter()
        .take(3)
        .map(|((x, z), n)| format!("{} {} ({n})", x * 10 + 5, z * 10 + 5))
        .collect();
    if list.is_empty() { "-".into() } else { list.join(", ") }
}

/// A random spot on the level, for levels without control points.
fn random_spot(level: &LoadedLevel, from: Vec3) -> Vec3 {
    let Some(heightmap) = &level.heightmap else {
        return from;
    };
    let half = heightmap.world_size() * 0.4;
    let center = heightmap.center();
    center + Vec3::new(fastrand::f32() * 2.0 - 1.0, 0.0, fastrand::f32() * 2.0 - 1.0) * half
}

/// Whether a soldier's main weapons are down to their last magazine.
fn low_on_ammo(inventory: &Inventory, loadout: &Loadout, armory: &Armory) -> bool {
    loadout.weapons.iter().zip(&inventory.ammo).any(|(name, [_, spare])| {
        armory
            .weapon(name)
            .is_some_and(|w| w.slot == 3 && w.magazine_size > 0 && (*spare as u32) < w.magazine_size)
    })
}

/// Roughly normally distributed, mean 0, standard deviation 1.
fn gaussian() -> f32 {
    (fastrand::f32() + fastrand::f32() + fastrand::f32() - 1.5) * 2.0
}

/// Where to aim at a soldier: his chest.
fn chest_height(stance: Stance) -> f32 {
    match stance {
        Stance::Prone => 0.3,
        Stance::Crouching => 0.8,
        Stance::Standing => 1.2,
    }
}

/// Distance from `p` to the segment `a`-`b`, and how far along the segment (0..1) the
/// closest point is.
fn segment_offset(p: Vec3, a: Vec3, b: Vec3) -> (f32, f32) {
    let ab = b - a;
    let t = ((p - a).dot(ab) / ab.length_squared().max(1e-6)).clamp(0.0, 1.0);
    (p.distance(a + ab * t), t)
}

fn flat(v: Vec3) -> Vec3 {
    Vec3::new(v.x, 0.0, v.z)
}

/// The view yaw that looks along `v` (0 = -Z, positive turns left).
fn yaw_to(v: Vec3) -> f32 {
    (-v.x).atan2(-v.z)
}

fn angle_delta(from: f32, to: f32) -> f32 {
    let mut delta = (to - from) % TAU;
    if delta > PI {
        delta -= TAU;
    } else if delta < -PI {
        delta += TAU;
    }
    delta
}

fn turn_towards(current: f32, target: f32, max_step: f32) -> f32 {
    current + angle_delta(current, target).clamp(-max_step, max_step)
}
