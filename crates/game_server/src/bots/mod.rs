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
//! keep up with its leader. Its kit adds options ([`equipment`]): launchers and rockets,
//! reviving, bags, repairs, flashbangs and tear gas. Movement follows paths on the
//! navigation grid ([`crate::nav`]).

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
use game_data::modes::ModeKind;
use game_shared::{
    conquest::{ControlPoint, Deployment, FlagState, team_index},
    modes::{Charge, ChargeState, ModeState},
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
    vehicle::{Seated, Vehicle, VehicleData, VehicleHealth, VehicleMotion, VehicleState, VehicleWeapons},
    weapons::{Armory, Inventory, Loadout, cooks},
};

use crate::{
    abilities::{Gadget, PADDLES_REACH, Wounded, gadget},
    AppliedInput, Controls, InputBuffer, ServerSettings, ServerSimSystems, balanced_team,
    ai::{
        self, AiData,
        awareness::{Memory, Source},
        cover::CoverSpot,
        skill::{Personality, Skill},
        squad::{self, SquadSnapshot, SquadTactics},
        gadgets::Flashes,
        stats::{AiStats, TeamStats},
        strategy::{self, OrderKind, StrategicMap, Strategy, TeamIntel, hash01},
        tactics,
        vehicles::{SeatWish, VehicleClaims, VehicleProfiles},
    },
    destruction::ObjectHealth,
    nav::{LadderStep, NavBlocked, NavGrid, NavObstacles, NavPath, Navigation, Waypoint, vehicle::VehicleNavigation},
};

mod combat;
mod equipment;
mod vehicle;

use equipment::{LaunchTarget, RepairTarget};
use vehicle::{Crew, Crews, Ride, VehicleCx};

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
        .init_resource::<SquadTactics>()
        .init_resource::<ai::squad::SquadReports>()
        .init_resource::<crate::nav::StuckCells>()
        .init_resource::<AiStats>()
        .init_resource::<ai::commander::AiCommander>()
        .init_resource::<Flashes>()
        .init_resource::<VehicleProfiles>()
        .init_resource::<VehicleClaims>()
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
                squad::coordinate,
                strategy::update_map,
                strategy::update_regions,
                strategy::plan,
                ai::commander::yield_to_humans,
                ai::commander::command,
                ai::gadgets::wear_gas_masks,
                ai::vehicles::update_profiles,
                think,
                learn_stuck_spots,
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
/// Rush: how far from a charge that needs arming (or defusing) bots go for it, meters, and
/// how much they want to once close (more than shooting at anyone but a close enemy).
const CHARGE_DISTANCE: f32 = 30.0;
const CHARGE_UTILITY: f32 = 8.2;
/// How far medics go to revive someone, meters, and how much they want to (BF2's revive
/// behaviour weight is 3, fire 7.5).
const REVIVE_DISTANCE: f32 = 35.0;
const REVIVE_UTILITY: f32 = 4.5;
/// Seconds a goal it kept getting stuck on the way to is avoided.
const BAD_GOAL_SECONDS: f32 = 20.0;
/// How close teammates must be for a held bag to reach them, meters.
const BAG_REACH: f32 = 4.0;
/// `BotBrain::spot` keys that aren't areas: a commander's point, and roaming.
const POINT_SPOT: usize = usize::MAX - 1;
const ROAM_SPOT: usize = usize::MAX;

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
    /// Stuck events of this tick walking a path: where, and the waypoint it was going for
    /// (see [`crate::nav::StuckCells`]); the same for drivers.
    stuck_reports: Vec<(Vec3, Vec3)>,
    vehicle_stuck_reports: Vec<(Vec3, Vec3)>,
    /// The last stuck event per square: where exactly, the waypoint it was going for, what
    /// it was doing.
    stuck_samples: bevy::platform::collections::HashMap<(i32, i32), (Vec3, Option<Vec3>, &'static str)>,
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
    /// Stuck events on or around aircraft carriers (within [`CARRIER_REACH`] of one).
    stuck_near_carriers: u32,
    /// Milliseconds spent in `think`.
    think_ms: f32,
    max_think_ms: f32,
    ticks: u32,
    /// Vehicles: seats taken and left, meters driven and seconds at the wheel by bot drivers,
    /// their path requests, stuck events (and where), aircraft taking off and crashing.
    vehicle_entries: u32,
    vehicle_exits: u32,
    driven: f32,
    driving_seconds: f32,
    vehicle_paths: u32,
    vehicle_partial_paths: u32,
    vehicle_failed_paths: u32,
    vehicle_stuck: u32,
    vehicle_stuck_spots: bevy::platform::collections::HashMap<(i32, i32), u32>,
    vehicle_stuck_samples: bevy::platform::collections::HashMap<(i32, i32), (Vec3, Vec3, String)>,
    flights: u32,
    crashes: u32,
    /// Milliseconds of `think` spent on seated bots, and per what they do (role and seat).
    seated_ms: f32,
    seated_by: bevy::platform::collections::HashMap<&'static str, (f32, u32)>,
    /// Bots on foot that moved under 2 m in 30 s, by what they were doing, and a few of the
    /// ones "moving" (a goal, but they didn't get anywhere): where, to where, how.
    idle_by: bevy::platform::collections::HashMap<&'static str, u32>,
    idle_samples: Vec<String>,
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
    /// C4 on an enemy vehicle: up to it, charges on it (`placed`), away, and off.
    Demolish { vehicle: Entity, time: f32, placed: u8 },
    /// To a vehicle's entry point and in, for a seat of this kind (see [`vehicle`]).
    Mount { vehicle: Entity, wish: SeatWish, time: f32 },
    /// Rush: to a charge and the use key held at it, to arm it (attackers) or defuse it
    /// (defenders), for at most `time` more seconds.
    Charge { charge: Entity, time: f32 },
    /// In a firefight: to cover to fight from (see [`combat`]), for at most `time` seconds.
    TakeCover { cover: CoverSpot, time: f32 },
    /// In cover with the enemy out of sight: watching (and peeking at) where he was.
    Watch { at: Vec3, time: f32 },
    /// Suppressive fire at where an enemy was, to keep his head down.
    Suppress { at: Vec3, time: f32 },
    /// Medics and support: to a squad mate who needs a bag.
    Supply { soldier: Entity, time: f32 },
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
    /// C4 and anti-tank mines (loadout indices), and seconds until the next mine.
    c4: Option<u8>,
    at_mine: Option<u8>,
    mine_cooldown: f32,
    /// C4 charges left at the last look (a charge was placed when it goes down), and whether
    /// the detonator is out.
    c4_left: Option<u16>,
    c4_detonator: bool,
    launch_cooldown: f32,
    bag_cooldown: f32,
    repair_cooldown: f32,
    /// What it repaired last (repairs taken up again after a fight count once).
    last_repair: Option<RepairTarget>,
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
    /// Recent times it got onto a ladder it didn't mean to take; decays over time.
    unwanted_ladders: f32,
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

    /// In a vehicle: what it does there (see [`vehicle`]).
    ride: Option<Ride>,
    /// The vehicle and seat it went for.
    mount_wish: Option<(Entity, SeatWish)>,
    /// Seconds before it looks for vehicles again.
    vehicle_cooldown: f32,
    /// The vehicle it last left, and for how many more seconds to leave it alone.
    left_vehicle: Option<(Entity, f32)>,
    /// Use was pressed last tick (getting in presses it on alternate ticks).
    use_toggle: bool,

    /// Plays with the tactics of [`combat`] (cover, suppression, memory, squad
    /// coordination); off for `--bot-legacy-team`, to compare with the behaviour before them.
    tactical: bool,
    /// Enemies it knows about but may not see (see [`ai::awareness`]).
    memory: Memory,
    /// Bullets passing close, 0..1.5; decays.
    suppression: f32,
    /// Seconds since the last near miss.
    shot_at_ago: f32,
    /// Cover it fights from, once there.
    cover: Option<CoverSpot>,
    /// Fighting from cover: up and shooting, or down; seconds left of that.
    exposed: bool,
    peek_timer: f32,
    /// Seconds in the current cover.
    in_cover_time: f32,
    /// Seconds before it looks for cover again.
    cover_cooldown: f32,
    /// Seconds before it lays down suppressive fire again.
    suppress_cooldown: f32,
    /// Rolled at the start of every firefight (and now and then in one): fights from cover
    /// this time.
    wants_cover: bool,
    /// Seconds in the current firefight (negative: seconds since the last one).
    fighting: f32,
    /// Seconds before it spots an enemy for the team again.
    spot_cooldown: f32,
    /// Bounding overwatch: where it holds while the other fire team moves, and the phase of
    /// the squad's bound it belongs to.
    overwatch: Option<Vec3>,
    bound_phase: u32,
    /// The enemy whose fire passed close last (checked once per burst).
    near_shooter: Option<Entity>,
    /// The squad's pinned-down episode it last decided whether to flank in.
    flanked_episode: u32,
    /// Rounds in a magazine of the main weapon.
    magazine_size: u32,
    /// Where it was at the last idle check, and seconds until the next (statistics).
    idle_anchor: Vec3,
    idle_timer: f32,
    /// Swimming: the shore it heads for to get out, and seconds before it looks again.
    swim_exit: Option<Vec3>,
    swim_exit_timer: f32,
    /// Seconds swimming without getting anywhere, and shores that didn't work out.
    swim_stall: f32,
    failed_exits: Vec<Vec3>,
    /// At the end of a complete path to the current goal: whether the goal itself can be
    /// walked to in a straight line from there (it may be the other side of a wall, in a
    /// closed room the path can't get into: it stays at the path's end then).
    path_done: Option<bool>,
    /// Statistics: seconds since an enemy came into sight while it hasn't fired yet, the main
    /// weapon's magazine last tick (rounds fired), and its going down already counted.
    sighted: Option<f32>,
    last_magazine: Option<(u8, u16)>,
    last_stance: Stance,
    death_noted: bool,
    /// In the moving fire team of its squad's bound this tick (statistics).
    bounding: bool,
    /// Its fight state as the tick began, before reacting to what happened (statistics of
    /// hits it scored or took then).
    prev_state: usize,
    /// Distance to what it last aimed at (statistics).
    aim_distance: f32,
    /// The walkable region it is in (see [`walk_region`]), and where that was found out.
    region: Option<u16>,
    region_at: Vec3,
    /// A goal it kept getting stuck on the way to, avoided for the seconds left.
    bad_goal: Option<(Vec3, f32)>,
    /// The enemy it last lost sight of and its aim error then: peeking at him again, the aim
    /// is where it was.
    lost_aim: Option<(Entity, Vec2)>,
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
            c4: None,
            at_mine: None,
            mine_cooldown: 20.0,
            c4_left: None,
            c4_detonator: false,
            launch_cooldown: 5.0,
            bag_cooldown: 0.0,
            repair_cooldown: 0.0,
            last_repair: None,
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
            unwanted_ladders: 0.0,
            path: None,
            waypoint: 0,
            path_goal: None,
            path_task: None,
            repath: false,
            repath_cooldown: 0.0,
            waypoint_best: f32::MAX,
            waypoint_timer: 0.0,
            ride: None,
            mount_wish: None,
            vehicle_cooldown: 5.0 * fastrand::f32(),
            left_vehicle: None,
            use_toggle: false,
            tactical: true,
            memory: Memory::default(),
            suppression: 0.0,
            shot_at_ago: 100.0,
            cover: None,
            exposed: false,
            peek_timer: 0.0,
            in_cover_time: 0.0,
            cover_cooldown: 0.0,
            suppress_cooldown: 0.0,
            wants_cover: false,
            fighting: -100.0,
            spot_cooldown: 5.0 + 10.0 * fastrand::f32(),
            overwatch: None,
            bound_phase: 0,
            near_shooter: None,
            flanked_episode: 0,
            magazine_size: 0,
            idle_anchor: Vec3::ZERO,
            idle_timer: 30.0,
            swim_exit: None,
            swim_exit_timer: 0.0,
            swim_stall: 0.0,
            failed_exits: Vec::new(),
            path_done: None,
            sighted: None,
            last_magazine: None,
            last_stance: Stance::Standing,
            death_noted: false,
            bounding: false,
            prev_state: 12,
            aim_distance: 0.0,
            region: None,
            region_at: Vec3::ZERO,
            bad_goal: None,
            lost_aim: None,
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
    /// Cells parked vehicles stand on, which paths go around.
    obstacles: Option<Res<'w, NavObstacles>>,
    spatial: SpatialQuery<'w, 's>,
    smoke: Smoke<'w, 's>,
    control_points: Query<'w, 's, (&'static ControlPoint, &'static FlagState)>,
    /// Rush's charges, and the mode being played.
    charges: Query<'w, 's, (Entity, &'static Charge, &'static ChargeState)>,
    modes: Query<'w, 's, &'static ModeState>,
    /// Flags closed for spawning (staged modes: enemies at them).
    blocked: Query<'w, 's, &'static game_shared::modes::SpawnBlocked>,
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
    /// Vehicles with what riding them needs.
    rides: Query<
        'w,
        's,
        (
            Entity,
            &'static Vehicle,
            &'static VehicleData,
            &'static VehicleMotion,
            &'static VehicleState,
            Option<&'static VehicleHealth>,
            Option<&'static VehicleWeapons>,
        ),
    >,
    profiles: Res<'w, VehicleProfiles>,
    vehicle_nav: Option<Res<'w, VehicleNavigation>>,
    players: Query<'w, 's, &'static Player>,
    /// Rockets, missiles and grenades in flight (crews fire countermeasures at them).
    projectiles: Query<'w, 's, (&'static game_shared::projectile::Projectile, &'static game_shared::projectile::ProjectileMotion)>,
    /// Enemies marked for a team (commo rose spots, the UAV, the commander).
    spotted: Query<'w, 's, &'static game_shared::radio::Spotted>,
    /// Fire teams, bounds and flanks of the squads (see [`squad::coordinate`]).
    tactics: Res<'w, SquadTactics>,
    /// Where bots learned not to walk or drive.
    stuck: Res<'w, crate::nav::StuckCells>,
}

impl Senses<'_, '_> {
    fn nav(&self) -> Option<&NavGrid> {
        self.nav.as_deref().map(|n| &*n.0)
    }

    fn blocked(&self) -> Option<&NavBlocked> {
        self.obstacles.as_deref().map(|o| &*o.0)
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

    /// Whether `team` can't spawn at an area's flag right now (enemies at it).
    fn spawn_blocked(&self, area: usize, team: Team) -> bool {
        self.map
            .areas
            .get(area)
            .and_then(|a| a.control_point)
            .and_then(|index| self.map.control_points.get(index as usize).copied().flatten())
            .and_then(|entity| self.blocked.get(entity).ok())
            .is_some_and(|b| b.0 == team)
    }

    /// The state of an area's charge (Rush).
    fn charge_state(&self, area: usize) -> Option<ChargeState> {
        let entity = self.map.charge_of(area)?;
        self.charges.get(entity).ok().map(|(_, _, state)| *state)
    }

    /// Whether `team` has work at a charge in this state: arming it as the attackers,
    /// defusing it as the defenders.
    fn charge_work(&self, team: Team, state: &ChargeState) -> bool {
        let Some(mode) = self.modes.iter().next().filter(|m| m.kind == ModeKind::Rush) else {
            return false;
        };
        match state {
            ChargeState::Active { .. } => team == mode.attacker,
            ChargeState::Armed { .. } => team == mode.defender(),
            _ => false,
        }
    }

    /// The nearest charge of the current stage within `radius` of `position` that `team` has
    /// work at, where it is and whether it is armed (to be defused before it goes off).
    fn charge_to_work(&self, team: Team, position: Vec3, radius: f32) -> Option<(Entity, Vec3, bool)> {
        let stage = self.modes.iter().next()?.stage;
        self.charges
            .iter()
            .filter(|(_, charge, state)| charge.stage == stage && self.charge_work(team, state))
            .map(|(entity, charge, state)| (entity, charge.position, state.armed()))
            .filter(|(_, at, _)| at.distance(position) < radius)
            .min_by(|a, b| a.1.distance(position).total_cmp(&b.1.distance(position)))
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
        self.magazine_size = w.weapon(me.loadout, self.primary).map_or(0, |w| w.magazine_size);
        self.memory.clear();
        self.suppression = 0.0;
        self.shot_at_ago = 100.0;
        self.cover = None;
        self.fighting = -100.0;
        self.overwatch = None;
        self.idle_anchor = me.motion.position;
        self.idle_timer = 30.0;
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
        self.ride = None;
        self.mount_wish = None;
    }

    #[allow(clippy::too_many_arguments)]
    fn tick(
        &mut self,
        w: &Senses,
        me: &Me,
        intel: &mut TeamIntel,
        stats: &mut BotStats,
        team_stats: &mut TeamStats,
        covers: &mut u32,
        vcx: &mut VehicleCx,
        cx: &mut combat::Cx,
    ) -> InputFrame {
        let dt = w.time.delta_secs();
        self.vehicle_cooldown -= dt;
        if let Some((_, left)) = &mut self.left_vehicle {
            *left -= dt;
            if *left <= 0.0 {
                self.left_vehicle = None;
            }
        }
        if self.ride.is_some() {
            // Just got out.
            self.end_ride(w, me, vcx.claims, stats);
        }
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
        if self.region.is_none() || flat(me.motion.position - self.region_at).length() > 0.5 {
            self.region_at = me.motion.position;
            self.region = w.nav().and_then(|nav| walk_region(nav, &w.spatial, me.motion.position));
        }
        let skill = self.personality.skill(&w.settings, me.team);
        self.sense(w, me, skill, intel, team_stats, cx, dt);
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
            self.decide(w, me, skill, team_stats, covers, vcx, cx);
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
            Activity::Revive { soldier, time } => self.revive(w, me, soldier, time, &mut intent, team_stats, dt),
            Activity::Launch { target, time, weapon, shots, fired } => {
                self.launch(w, me, skill, target, time, weapon, shots, fired, &mut intent, dt)
            }
            Activity::Repair { target, time } => self.repair(w, me, target, time, &mut intent, dt),
            Activity::Demolish { vehicle, time, placed } => self.demolish(w, me, vehicle, time, placed, &mut intent, dt),
            Activity::Mount { vehicle, wish, time } => self.mount(w, me, vcx, vehicle, wish, time, &mut intent, dt),
            Activity::Charge { charge, time } => self.work_charge(w, me, charge, time, &mut intent, dt),
            Activity::TakeCover { cover, time } => self.take_cover(w, me, skill, cover, time, &mut intent, dt),
            Activity::Watch { at, time } => self.watch(w, me, at, time, &mut intent, dt),
            Activity::Suppress { at, time } => self.suppress(w, me, skill, at, time, &mut intent, dt),
            Activity::Supply { soldier, time } => self.supply(w, me, soldier, time, &mut intent, dt),
            Activity::Objective => {
                self.objective(w, me, &mut intent, dt);
                intent.weapon = intent.weapon.or(self.bag);
            }
        }

        team_stats.alive += dt;
        if self.activity == Activity::Engage {
            team_stats.fighting += dt;
        }
        self.after_tick(w, me, team_stats, cx, dt);
        self.idle_timer -= dt;
        if self.idle_timer <= 0.0 {
            self.idle_timer = 30.0;
            if flat(me.motion.position - self.idle_anchor).length() < 2.0 {
                let reason = self.idle_reason();
                *stats.idle_by.entry(reason).or_default() += 1;
                if reason == "moving" && stats.idle_samples.len() < 4 {
                    stats.idle_samples.push(format!(
                        "{} at {:.0} for {} ({}, {} waypoints left, {:.0} stuck strikes)",
                        w.name(me.player),
                        me.motion.position,
                        self.goal.map_or("-".into(), |g| format!("{g:.0}")),
                        match &self.path {
                            Some(p) if p.complete => "path",
                            Some(_) => "partial path",
                            None if self.direct => "walking straight",
                            None => "no path",
                        },
                        self.path.as_ref().map_or(0, |p| p.waypoints.len().saturating_sub(self.waypoint)),
                        self.stuck_strikes,
                    ));
                }
            }
            self.idle_anchor = me.motion.position;
        }
        if matches!(self.activity, Activity::Charge { .. }) {
            team_stats.charge_seconds += dt;
        }
        if (self.seq.wrapping_add(self.seed)) % 30 == 7
            && let Some((kind, area)) = self.order
            && let Some(area) = w.map.areas.get(area)
        {
            let d = area.position.distance(me.motion.position);
            let bucket = match d {
                d if d < area.radius => 0,
                d if d < area.radius + 30.0 => 1,
                d if d < 100.0 => 2,
                d if d < 200.0 => 3,
                _ => 4,
            };
            team_stats.combat.obj_dist[bucket + if kind == OrderKind::Defend { 5 } else { 0 }] += 0.5;
        }
        if let Some((_, area)) = self.order
            && let Some(area) = w.map.areas.get(area)
            && area.position.distance(me.motion.position) < area.radius + 30.0
        {
            team_stats.at_objective += dt;
        }
        let frame = self.act(w, me, intent, stats, dt);
        self.fire_stats(me, &frame, team_stats, dt);
        frame
    }

    /// Notices being hurt, looks around for enemies, listens for gunfire.
    #[allow(clippy::too_many_arguments)]
    fn sense(&mut self, w: &Senses, me: &Me, skill: Skill, intel: &mut TeamIntel, team_stats: &mut TeamStats, cx: &mut combat::Cx, dt: f32) {
        self.alert -= dt;
        self.hurt_ago += dt;
        self.grenade_cooldown -= dt;
        self.flank_cooldown -= dt;
        self.launch_cooldown -= dt;
        self.mine_cooldown -= dt;
        self.bag_cooldown -= dt;
        self.repair_cooldown -= dt;
        if self.tactical {
            self.feel_fire(w, me, skill, team_stats, cx, dt);
        }
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
                if self.tactical
                    && let Some((enemy, at)) = self.threat
                {
                    self.memory.learn(enemy, at + Vec3::Y * 1.2, Source::Shot);
                }
            }
            if self.tactical {
                self.suppression = (self.suppression + 0.5).min(1.5);
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
        let hearing = if self.tactical { skill.hearing_range() } else { 70.0 };
        // Enemies it knew about a moment ago but lost sight of (re-acquired, it is already
        // aimed their way).
        let known: Vec<Entity> = self
            .memory
            .contacts
            .iter()
            .filter(|c| (0.3..4.0).contains(&c.age) && c.source >= Source::Shot)
            .map(|c| c.enemy)
            .collect();
        let mut candidates: Vec<(Entity, f32, Vec3, bool)> = Vec::new();
        let mut heard: Option<(f32, Vec3)> = None;
        for (entity, motion, controlled_by, _, _, applied, _, seated, downed) in &w.soldiers {
            // Crews behind armour are fought through their vehicles (rocket launchers,
            // `scan_armor`), exposed ones (open seats: gunners, jeep riders) like anyone; the
            // critically wounded are left alone.
            let exposed = seated.is_none_or(|s| {
                w.vehicle(s.vehicle)
                    .and_then(|v| v.data.0.desc.seats.get(s.seat as usize).map(|d| d.open))
                    .unwrap_or(false)
            });
            if entity == me.soldier || !exposed || downed || !w.is_enemy(controlled_by.0, me.team) {
                continue;
            }
            let to = motion.position - position;
            let distance = to.length();
            if distance > range {
                continue;
            }
            let firing = applied.is_some_and(|a| a.0.pressed(Buttons::FIRE));
            if firing && distance < hearing && heard.is_none_or(|(d, _)| distance < d) {
                heard = Some((distance, motion.position));
            }
            if self.tactical {
                self.notice(w, me, entity, motion, firing, distance, hearing);
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
            if self.tactical {
                self.memory.learn(entity, chest, Source::Seen);
            }
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
        match (self.target, seen) {
            (None, Some(_)) => {
                team_stats.combat.sightings += 1;
                self.sighted = Some(0.0);
            }
            (_, None) => self.sighted = None,
            _ => {}
        }
        if seen != self.target {
            if let Some(lost) = self.target {
                self.lost_aim = Some((lost, self.aim_error));
            }
            self.target = seen;
            self.engaged = 0.0;
            // Someone in sight at last: decide now whether to shoot him (peeking from cover).
            if seen.is_some() && self.tactical && crate::ai::tune::knob("peek_fast", 0.0) > 0.5 {
                self.decide_timer = 0.0;
            }
            if let Some((_, _, chest, outside)) = best {
                let impairment = self.impairment();
                self.reaction = (skill.reaction_time() + if outside { 0.3 } else { 0.0 }) * impairment;
                let angle = fastrand::f32() * TAU;
                self.aim_error =
                    Vec2::new(angle.cos(), angle.sin()) * skill.aim_error() * (0.6 + 0.8 * fastrand::f32()) * impairment;
                self.style = self.engage_style(w, me, chest.distance(eye));
                if self.tactical {
                    // Someone it knew was there (peeking at him again, or he came round the
                    // corner it watched): already aimed that way.
                    if self.target.is_some_and(|t| known.contains(&t)) {
                        self.reaction *= crate::ai::tune::knob("known_react", 0.4);
                        self.aim_error *= 0.6;
                        // The one it just lost: still aimed about where it was.
                        if crate::ai::tune::knob("keep_aim", 0.0) > 0.5
                            && let Some((lost, aim)) = self.lost_aim
                            && Some(lost) == self.target
                        {
                            if (aim * 1.3).length() < self.aim_error.length() {
                                self.aim_error = aim * 1.3;
                            }
                        }
                    }
                    // Under fire it takes a little longer to settle on someone.
                    let supp = crate::ai::tune::knob("supp_aim", 1.0);
                    self.reaction += 0.12 * self.suppression.min(1.0) * supp;
                    self.aim_error *= 1.0 + 0.3 * self.suppression.min(1.0) * supp;
                    self.call_spot(w, me, skill, cx);
                }
            }
        }
        if self.tactical {
            self.team_knowledge(me, intel, range);
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
        if self.tactical && crate::ai::tune::knob("style", 1.0) > 0.5 {
            // Low at range: prone far off (steadier), crouched at middle distances.
            return match pose {
                FiringPose::Prone if distance > 30.0 => EngageStyle::Prone,
                _ if distance > 45.0 && fastrand::f32() < 0.5 => EngageStyle::Prone,
                _ if distance > 15.0 && fastrand::f32() < 0.8 => EngageStyle::Crouch,
                _ => EngageStyle::Strafe,
            };
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
    #[allow(clippy::too_many_arguments)]
    fn decide(
        &mut self,
        w: &Senses,
        me: &Me,
        skill: Skill,
        team_stats: &mut TeamStats,
        covers: &mut u32,
        vcx: &mut VehicleCx,
        cx: &mut combat::Cx,
    ) {
        if me.motion.climbing {
            // Hands on the rungs: nothing to do but climb on.
            if matches!(
                self.activity,
                Activity::Engage
                    | Activity::Throw { .. }
                    | Activity::Search { .. }
                    | Activity::Launch { .. }
                    | Activity::Repair { .. }
                    | Activity::Watch { .. }
                    | Activity::Suppress { .. }
            ) {
                self.activity = Activity::Objective;
            }
            return;
        }
        if matches!(self.activity, Activity::Throw { .. } | Activity::Launch { .. })
            || matches!(self.activity, Activity::Demolish { placed, .. } if placed > 0)
        {
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
            Activity::Revive { time, soldier } if time > 0.0 => {
                // Tactical medics see a revive through once close, unless it gets too hot.
                let close = w.soldiers.get(soldier).is_ok_and(|s| flat(s.1.position - position).length() < 20.0);
                let committed = self.tactical
                    && crate::ai::tune::knob("revive_commit", 0.0) > 0.5
                    && close
                    && self.suppression < 0.8
                    && hp > 0.4
                    && distance.is_none_or(|d| d > 12.0);
                best = (if committed { crate::ai::tune::knob("revive_keep", 8.6) } else { REVIVE_UTILITY }, self.activity);
            }
            Activity::Repair { time, .. } if time > 0.0 => best = (4.0, self.activity),
            Activity::Mount { time, .. } if time < 90.0 => best = (5.0, self.activity),
            Activity::Demolish { time, .. } if time < 40.0 => best = (7.0, self.activity),
            Activity::Charge { time, .. } if time > 0.0 => best = (CHARGE_UTILITY, self.activity),
            // Getting to cover in a firefight beats standing in the open shooting.
            Activity::TakeCover { time, .. } if time > 0.0 => best = (7.8 + 2.0 * self.suppression, self.activity),
            Activity::Watch { time, .. } if time > 0.0 => best = (5.0, self.activity),
            Activity::Suppress { time, .. } if time > 0.0 => best = (5.5, self.activity),
            Activity::Supply { time, .. } if time > 0.0 => best = (3.8, self.activity),
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

        // Rush: a charge close by to arm, or an armed one to defuse (before it goes off), comes
        // before shooting at anyone not close.
        if !matches!(self.activity, Activity::Charge { .. })
            && let Some((charge, at, armed)) = w.charge_to_work(me.team, position, CHARGE_DISTANCE)
        {
            let utility = if armed || at.distance(position) < 8.0 { CHARGE_UTILITY } else { 6.0 };
            consider(&mut best, utility, Activity::Charge { charge, time: 40.0 });
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

        // Medics revive teammates down nearby, unless in a fight; squad mates first, from
        // further away and more readily.
        if self.paddles.is_some()
            && !matches!(self.activity, Activity::Revive { .. })
        {
            let squad_mate = |soldier: Entity| self.tactical && self.is_squad_mate(w, me, soldier);
            let reach = if self.tactical { REVIVE_DISTANCE * 1.7 } else { REVIVE_DISTANCE };
            let downed = w.wounded.downed_near(me.team, position, reach);
            let pick = downed
                .iter()
                .filter(|(_, _, left)| *left > 3.0)
                .map(|&(soldier, at, _)| {
                    let mate = squad_mate(soldier);
                    let far = flat(at - position).length() > REVIVE_DISTANCE;
                    (soldier, mate, far)
                })
                .filter(|(_, mate, far)| *mate || !*far)
                .max_by_key(|(_, mate, _)| *mate);
            if let Some((soldier, mate, _)) = pick {
                // Not into the fire: the squad mate first only while nobody shoots at us.
                let fight = distance.is_some_and(|d| d < 40.0) || self.suppression > 0.3;
                let mut utility = if mate && !fight { REVIVE_UTILITY + 2.0 } else { REVIVE_UTILITY };
                // Tactical medics go for a teammate down close by even in a firefight, unless
                // the enemy is right there: a revive saves a life (and a ticket).
                let near = downed.iter().find(|d| d.0 == soldier).is_some_and(|d| flat(d.1 - position).length() < 25.0);
                if self.tactical
                    && crate::ai::tune::knob("revive_commit", 0.0) > 0.5
                    && (near || mate)
                    && self.suppression < 0.6
                    && distance.is_none_or(|d| d > 15.0)
                {
                    utility = utility.max(7.8);
                }
                consider(&mut best, utility, Activity::Revive { soldier, time: 25.0 });
            }
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
            let remembered = self
                .tactical
                .then(|| self.memory.most_pressing(position, 6.0))
                .flatten()
                .filter(|c| c.source == Source::Seen && c.age > 1.0)
                .map(|c| (c.position, c.age));
            let at = match (target, self.last_seen.or(remembered)) {
                (Some(t), _) => Some((t.position, true)),
                (None, Some((at, age))) if age < if self.tactical { 6.0 } else { 4.0 } => Some((at - Vec3::Y * 1.0, false)),
                _ => None,
            };
            if let Some((at, visible)) = at {
                let d = flat(at - position).length();
                let friends_clear = me.team_index().is_none_or(|t| {
                    w.snapshot.soldiers[t]
                        .iter()
                        .all(|s| s.player == me.player || s.position.distance(at) > GRENADE_SAFETY)
                });
                let depth = if self.tactical { 0.6 + 0.8 * skill.tactics() } else { 1.0 };
                // Winding up a grenade in the open, in a firefight, is time not shooting back:
                // tactical bots throw at an enemy in sight from cover.
                let exposed = self.tactical
                    && crate::ai::tune::knob("vis_throw", 1.0) < 0.5
                    && self.cover.is_none_or(|c| flat(c.spot - position).length() > 2.0);
                let utility = match visible {
                    true if self.engaged > 2.5 && !exposed && fastrand::f32() < 0.1 + 0.25 * aggression => 8.0,
                    true => 0.0,
                    false => 5.0 * (0.5 + aggression) * depth,
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
        if !matches!(self.activity, Activity::Mount { .. }) {
            self.vehicle_options(w, me, vcx, target, &mut best);
        }
        if self.tactical {
            self.tactical_options(w, me, skill, target, &mut best, cx);
        }
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
        if let Activity::Revive { soldier, .. } = self.activity
            && std::mem::discriminant(&next) != std::mem::discriminant(&self.activity)
        {
            // Off to something else before the revive was done.
            let still_down = w.soldiers.get(soldier).is_ok_and(|s| s.8);
            team_stats.combat.revive[if still_down { 4 } else { 1 }] += 1;
        }
        if std::mem::discriminant(&next) != std::mem::discriminant(&self.activity) {
            match next {
                Activity::Cover { .. } => team_stats.covers += 1,
                Activity::Flank { .. } => {
                    team_stats.flanks += 1;
                    self.flank_cooldown = 20.0;
                }
                Activity::Throw { weapon, .. } if Some(weapon) == self.at_mine => team_stats.mines += 1,
                Activity::Throw { weapon, .. } if Some(weapon) == self.grenade || Some(weapon) == self.flashbang => {
                    team_stats.grenades += 1
                }
                Activity::Throw { .. } => team_stats.bags += 1,
                Activity::Launch { target: LaunchTarget::Vehicle(_), .. } => team_stats.rockets += 1,
                Activity::Demolish { .. } => team_stats.demolitions += 1,
                Activity::Launch { .. } => team_stats.launches += 1,
                Activity::Repair { target, .. } if self.last_repair != Some(target) => {
                    self.last_repair = Some(target);
                    team_stats.repairs += 1;
                }
                Activity::Revive { .. } => {
                    team_stats.revives += 1;
                    team_stats.combat.revive[0] += 1;
                }
                Activity::TakeCover { .. } => team_stats.takecovers += 1,
                Activity::Suppress { .. } => team_stats.suppressions += 1,
                Activity::Supply { .. } => team_stats.supplies_run += 1,
                Activity::Mount { vehicle, wish, .. } => {
                    let template = w.vehicle(vehicle).map_or("?", |v| v.template);
                    debug!("{} goes for {template} ({wish:?})", w.name(me.player));
                }
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
        let (distance, on_target) = self.aim_at(me, skill, target, intent, dt);

        // From cover: up to shoot, down to reload or when the fire gets too close.
        let from_cover = if self.tactical { self.fight_from_cover(me, intent, dt) } else { None };
        // Attackers work their way forward: a few seconds moving, a few shooting (with
        // tactics, from cover to cover when there is any; see `combat`).
        let advance = match self.order {
            _ if from_cover.is_some() => None,
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
            // Tactical bots keep shooting as they go.
            if self.tactical && crate::ai::tune::knob("adv_fire", 0.0) > 0.5 {
                self.pull_trigger(w, me, distance, on_target, intent, dt);
            }
            return;
        }

        let may_fire = from_cover.unwrap_or(true);
        self.pull_trigger(w, me, distance, on_target && may_fire, intent, dt);

        if from_cover.is_some() {
            return;
        }
        let style = match distance {
            d if d < 8.0 => EngageStyle::Strafe,
            // Pinned down in the open: get low.
            d if self.tactical && self.suppression > 0.6 && d > 15.0 && crate::ai::tune::knob("style", 1.0) > 0.5 => {
                if d > 30.0 { EngageStyle::Prone } else { EngageStyle::Crouch }
            }
            _ => self.style,
        };
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

    /// Aims at `target`'s chest with an error that shrinks while tracking; counts the reaction
    /// time down. Returns the distance and whether the aim is on target.
    fn aim_at(&mut self, me: &Me, skill: Skill, target: &SoldierMotion, intent: &mut Intent, dt: f32) -> (f32, bool) {
        let eye = me.motion.eye_position();
        // Aim at the chest, with an error that shrinks while tracking.
        let aim_at = target.position + Vec3::Y * chest_height(target.stance);
        let to = aim_at - eye;
        let distance = to.length();
        self.aim_distance = distance;
        let desired_yaw = yaw_to(to) + self.aim_error.x;
        let desired_pitch = to.y.atan2(Vec2::new(to.x, to.z).length()) + self.aim_error.y;
        // The error settles towards a wander whose size depends on skill and distance
        // (an Ornstein-Uhlenbeck process).
        let settle = skill.aim_settle();
        self.aim_error *= 1.0 - (settle * dt).min(1.0);
        // Bullets cracking past shake the aim (a little: it mostly sends it to cover).
        let shaken = 1.0 + 0.4 * self.suppression.min(1.0) * (1.0 - 0.5 * skill.0) * crate::ai::tune::knob("supp_aim", 1.0);
        let wander = skill.aim_spread(distance) * (2.0 * settle * dt).sqrt() * self.impairment() * shaken;
        self.aim_error += Vec2::new(gaussian(), gaussian()) * wander;
        self.yaw = turn_towards(self.yaw, desired_yaw, skill.turn_rate() * dt);
        self.pitch += (desired_pitch - self.pitch).clamp(-4.0 * dt, 4.0 * dt);
        intent.look = Look::Aimed;
        self.reaction -= dt;
        let tolerance = (1.2 / distance.max(1.0)).clamp(0.02, 0.15);
        let on_target = angle_delta(self.yaw, desired_yaw).abs() + (self.pitch - desired_pitch).abs() < tolerance;
        (distance, on_target)
    }

    /// Works the trigger at a target `distance` away: bursts for automatic fire (long up
    /// close, short far away), a fresh pull per shot otherwise; only once the reaction time
    /// is up and `aimed`.
    fn pull_trigger(&mut self, w: &Senses, me: &Me, distance: f32, aimed: bool, intent: &mut Intent, dt: f32) {
        let weapon = w.weapon(me.loadout, self.primary);
        let mode = weapon
            .and_then(|weapon| weapon.fire_modes.get(me.inventory.map_or(0, |i| i.fire_mode) as usize).copied())
            .unwrap_or(FireMode::Auto);
        if self.reaction <= 0.0 && aimed {
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
            if Some(weapon) == self.at_mine {
                self.mine_cooldown = 30.0 + 30.0 * fastrand::f32();
            } else if Some(weapon) == self.grenade || Some(weapon) == self.flashbang {
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
    #[allow(clippy::too_many_arguments)]
    fn revive(&mut self, w: &Senses, me: &Me, soldier: Entity, time: f32, intent: &mut Intent, team_stats: &mut TeamStats, dt: f32) {
        let body = w.soldiers.get(soldier).ok().filter(|s| s.8).map(|s| s.1.position);
        let (Some(body), Some(paddles), true) = (body, self.paddles, time > 0.0) else {
            // How it ended: up again, dead, or out of time.
            let outcome = match w.soldiers.get(soldier) {
                Ok(s) if !s.8 => 1,
                Ok(_) => 3,
                Err(_) => 2,
            };
            team_stats.combat.revive[outcome] += 1;
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

    /// The flag (or charge) someone at `position` is at: to take or to hold.
    fn area_near(&self, w: &Senses, position: Vec3, team: Team) -> Option<(OrderKind, usize)> {
        w.map
            .areas
            .iter()
            .enumerate()
            .filter(|(_, a)| (a.control_point.is_some() && !a.uncapturable) || a.charge.is_some())
            .find(|(_, a)| a.position.distance(position) < a.radius + 25.0)
            .map(|(i, a)| {
                let attack = match a.charge {
                    Some(_) => w.charge_state(i).is_some_and(|s| matches!(s, ChargeState::Active { .. }) && w.charge_work(team, &s)),
                    None => !w.holds(i, team),
                };
                (if attack { OrderKind::Attack } else { OrderKind::Defend }, i)
            })
    }

    /// Rush: walks up to a charge and holds the use key at it, crouched and looking at it,
    /// until it is armed (or defused), someone else did it, or `time` runs out.
    fn work_charge(&mut self, w: &Senses, me: &Me, charge: Entity, time: f32, intent: &mut Intent, dt: f32) {
        let target = w
            .charges
            .get(charge)
            .ok()
            .filter(|(_, _, state)| w.charge_work(me.team, state))
            .map(|(_, charge, _)| charge.position);
        let (Some(at), true) = (target, time > 0.0) else {
            self.activity = Activity::Objective;
            return;
        };
        let distance = flat(at - me.motion.position).length();
        if distance > Charge::REACH - 0.7 || (at.y - me.motion.position.y).abs() > 1.5 {
            intent.goal = Some(Goal {
                position: at,
                tolerance: 0.8,
                sprint: distance > 12.0,
            });
        } else {
            intent.look = Look::At(at + Vec3::Y * 0.6);
            intent.buttons |= Buttons::USE | Buttons::CROUCH;
        }
        self.activity = Activity::Charge { charge, time: time - dt };
    }

    /// Squad and objective movement: follow the leader, wait for the squad, head for the
    /// objective and take a spot of its own there.
    fn objective(&mut self, w: &Senses, me: &Me, intent: &mut Intent, dt: f32) {
        let position = me.motion.position;
        let squad = me.member.and_then(|m| w.snapshot.squads.get(&(me.team, m.squad)));
        let is_leader = me.member.is_some_and(|m| m.leader);
        // Not a leader flying off in an aircraft (or far away): the objective instead.
        let leader = squad
            .filter(|_| !is_leader)
            .and_then(|s| s.leader_soldier)
            .filter(|l| l.position.y - position.y < 25.0 && l.position.distance(position) < 400.0);
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
        // A human commander may send a squad to a point away from the flags.
        let point = me
            .member
            .filter(|_| !human_led)
            .and_then(|m| w.strategy.orders.get(&(me.team, m.squad)))
            .and_then(|o| o.point);

        self.bounding = false;
        // Near the fight the squad bounds: one fire team moves while the other covers it.
        let tactic = me
            .member
            .filter(|_| self.tactical && !human_led)
            .and_then(|m| w.tactics.squads.get(&(me.team, m.squad)))
            .filter(|t| t.bounding && crate::ai::tune::knob("bound", 1.0) > 0.5)
            .copied();
        match (tactic, squad) {
            (Some(tactic), Some(squad)) if is_leader || leader.is_some() => {
                if self.bound(w, me, &tactic, squad, leader.as_ref(), is_leader, intent, dt) {
                    return;
                }
            }
            _ => self.overwatch = None,
        }

        // Members keep up with their leader until they are close to the objective.
        let follow = leader.filter(|l| match (area, point) {
            (_, Some(point)) => point.distance(position) > 40.0 && point.distance(l.position) > 20.0,
            (None, None) => true,
            (Some((_, area)), None) if human_led => l.position.distance(area.position) > area.radius + 25.0,
            (Some((_, area)), None) => {
                area.position.distance(position) > 45.0 && area.position.distance(l.position) > 25.0
            }
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

        if let Some(point) = point {
            self.hold_point(w, me, point, intent, dt);
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
            && tactic.is_none()
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
        let region = match self.region {
            Some(region) => region,
            None => nav.cell(nav.locate(me.motion.position, 2.0, None)?).region,
        };
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
        // Squad leaders hold back a little (the squad spawns on them).
        let leader = self.tactical && me.member.is_some_and(|m| m.leader);
        // Somewhere it can walk to: not a closed room of a house next to the flag.
        let region = self
            .region
            .or_else(|| w.nav().and_then(|nav| nav.locate(me.motion.position, 2.0, None).map(|c| nav.cell(c).region)));
        for attempt in 0..6 {
            let jitter = fastrand::f32();
            let (angle, radius) = match (kind, cp) {
                (OrderKind::Attack, Some(cp)) => (base + attempt as f32 * 1.3, cp.radius * (0.2 + 0.55 * jitter)),
                (OrderKind::Defend, Some(cp)) if leader => {
                    // Behind the flag, away from the enemy.
                    let spread = (jitter - 0.5) * 1.6;
                    (-(facing + spread) + std::f32::consts::FRAC_PI_2, cp.radius * 0.3 + 2.0 * fastrand::f32())
                }
                (OrderKind::Defend, Some(cp)) => {
                    // In front of the flag, towards the enemy.
                    let spread = (jitter - 0.5) * 2.4;
                    (-(facing + spread) - std::f32::consts::FRAC_PI_2, cp.radius * 0.5 + 4.0 + 8.0 * fastrand::f32())
                }
                // Guarding a charge (Rush): a ring around it, mostly towards the enemy, not all
                // crowded at the charge.
                (OrderKind::Defend, None) if area.charge.is_some() => {
                    let spread = (jitter - 0.5) * 3.0;
                    (-(facing + spread) - std::f32::consts::FRAC_PI_2, 6.0 + 12.0 * fastrand::f32())
                }
                _ => (base + attempt as f32 * 1.3, 8.0 * jitter),
            };
            let center = if cp.is_some() { area.position } else { area.order_position };
            let candidate = center + Vec3::new(angle.cos(), 0.0, angle.sin()) * radius;
            let Some(nav) = w.nav() else {
                return candidate;
            };
            if let Some(cell) = nav.locate(candidate, 2.0, region) {
                let spot = nav.position(cell);
                let inside = match (kind, cp) {
                    (OrderKind::Attack, Some(cp)) => cp.contains(spot),
                    (_, Some(cp)) => spot.distance(cp.position) < cp.radius + 16.0,
                    (OrderKind::Defend, None) if area.charge.is_some() => spot.distance(area.position) < 22.0,
                    _ => true,
                };
                if inside {
                    // Defenders take cover from the way the enemy comes, near their spot.
                    if self.tactical
                        && kind == OrderKind::Defend
                        && !leader
                    {
                        let ahead = Quat::from_rotation_y(facing) * Vec3::NEG_Z;
                        let query = crate::ai::cover::CoverQuery {
                            from: spot,
                            threat: spot + ahead * 40.0 + Vec3::Y * 1.6,
                            radius: 6.0,
                            toward: None,
                            taken: &[],
                            fire: true,
                            region,
                        };
                        let mut rays = 16;
                        if let Some(cover) = crate::ai::cover::find(nav, &w.spatial, &query, &mut rays)
                            && cover.peek.is_some()
                        {
                            return cover.spot;
                        }
                    }
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
        // The way the enemy came from lately.
        if self.tactical
            && let Some(axis) = self.memory.threat_axis(area.position, 20.0)
        {
            return yaw_to(axis);
        }
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

    /// A commander's point away from the flags: a spot of its own near it, held facing the
    /// enemy.
    fn hold_point(&mut self, w: &Senses, me: &Me, point: Vec3, intent: &mut Intent, dt: f32) {
        let position = me.motion.position;
        self.spot_timer -= dt;
        let stale = self.spot.is_none_or(|(key, spot)| key != POINT_SPOT || spot.distance(point) > 12.0);
        if stale || self.spot_timer <= 0.0 {
            let angle = hash01(self.seed, 5) * TAU + fastrand::f32();
            let candidate = point + Vec3::new(angle.cos(), 0.0, angle.sin()) * (2.0 + 6.0 * fastrand::f32());
            let spot = w
                .nav()
                .and_then(|nav| nav.locate(candidate, 3.0, None).map(|c| nav.position(c)))
                .unwrap_or(point);
            self.spot = Some((POINT_SPOT, spot));
            self.spot_timer = 15.0 + 15.0 * fastrand::f32();
        }
        let (_, spot) = self.spot.unwrap();
        let distance = flat(spot - position).length();
        if distance > 1.2 {
            intent.goal = Some(Goal {
                position: spot,
                tolerance: 1.0,
                sprint: distance > 40.0,
            });
            self.spot_timer = self.spot_timer.max(2.0);
        } else {
            self.sweep += dt * 0.4;
            let facing = self.last_seen.map_or(self.yaw, |(at, _)| yaw_to(at - position));
            intent.look = Look::Yaw(facing + self.sweep.sin() * 1.0);
            intent.buttons |= Buttons::CROUCH;
        }
    }

    /// Levels without control points: wander about.
    fn roam(&mut self, w: &Senses, me: &Me, intent: &mut Intent) {
        let position = me.motion.position;
        let reached = self.spot.is_none_or(|(_, spot)| flat(spot - position).length() < 2.0);
        if reached || self.spot_timer <= 0.0 {
            self.spot = Some((ROAM_SPOT, random_spot(&w.level, position)));
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
    fn act(&mut self, w: &Senses, me: &Me, mut intent: Intent, stats: &mut BotStats, dt: f32) -> InputFrame {
        let position = me.motion.position;
        let mut direction = Vec3::ZERO;
        let mut jump = false;
        let mut ladder = None;
        // In the water and getting nowhere (a quay, a hull or a bank too deep to stand up at:
        // a swimmer only finds its feet within wading depth of the surface): swims straight
        // for the nearest shallow shore, and another one if that doesn't work either.
        self.swim_exit_timer -= dt;
        let mut swim_to = None;
        if !me.motion.swimming {
            self.swim_exit = None;
            self.swim_stall = 0.0;
            self.failed_exits.clear();
        } else if !matches!(self.activity, Activity::Mount { .. }) {
            let moved = flat(position - self.last_position).length();
            self.swim_stall = if moved < 0.4 * dt { self.swim_stall + dt } else { (self.swim_stall - dt).max(0.0) };
            let lost = self.stranded > 0.0 || self.swim_stall > 2.0 || self.path.as_ref().is_some_and(|p| !p.complete);
            if (self.swim_exit_timer <= 0.0 && lost) || (self.swim_exit.is_some() && self.swim_stall > 2.5) {
                if let Some(old) = self.swim_exit.take() {
                    if self.failed_exits.len() >= 6 {
                        self.failed_exits.remove(0);
                    }
                    self.failed_exits.push(old);
                }
                self.swim_exit_timer = 20.0;
                self.swim_stall = 0.0;
                let water = w.level.desc.water.as_ref().map(|w| w.height);
                self.swim_exit = w.nav().and_then(|nav| nearest_shore(nav, w.blocked(), water, position, &self.failed_exits));
                if self.swim_exit.is_some() {
                    self.stranded = 0.0;
                }
            }
            if let Some(exit) = self.swim_exit {
                intent.goal = Some(Goal {
                    position: exit,
                    tolerance: 2.0,
                    sprint: true,
                });
                swim_to = Some(exit);
            }
        }
        if let Some((at, left)) = &mut self.bad_goal {
            *left -= dt;
            if *left <= 0.0 {
                self.bad_goal = None;
            } else if intent.goal.is_some_and(|g| flat(g.position - *at).length() < 2.5) {
                // It can't get there: wait here instead.
                intent.goal = None;
            }
        }
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
                    && w.nav().is_none_or(|nav| nav.walkable_line_avoiding(position, goal.position, w.blocked()));
            }
            let (target, j, step) = match (w.nav.as_deref(), self.direct) {
                (Some(nav), false) => match self.follow_path(nav, w.obstacles.as_deref(), goal.position, goal.tolerance, me.motion, dt, stats) {
                    Steer::Toward { target, jump, ladder } => (target, jump, ladder),
                    // At the path's end: on to the goal if it's in walking reach, otherwise here.
                    Steer::Arrived if self.path_done == Some(false) => (position, false, None),
                    Steer::Arrived => (goal.position, false, None),
                    Steer::Stranded => {
                        stats.stranded += 1;
                        self.stranded = 10.0;
                        debug!("{} can't walk from {position:.0} to {:.0}", w.name(me.player), goal.position);
                        (position, false, None)
                    }
                },
                _ => (goal.position, false, None),
            };
            ladder = step;
            // Swimming for the shore: straight at it (paths run along the bottom).
            let target = swim_to.unwrap_or(target);
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
        self.unwanted_ladders = (self.unwanted_ladders - dt / 8.0).max(0.0);
        if me.motion.climbing && !self.climbing {
            stats.climbs += 1;
            // Onto a ladder it didn't mean to take (walking at a wall it hangs on): it jumps off
            // (see `act`), and walking into it again and again is being stuck there.
            if ladder.is_none() && intent.goal.is_some() {
                self.unwanted_ladders += 1.0;
                if self.unwanted_ladders >= 2.0 {
                    self.unwanted_ladders = 0.0;
                    self.stuck_event(position, stats);
                    stats.stuck_near_carriers += u32::from(near_carrier(&w.level, position));
                }
            }
        }
        self.climbing = me.motion.climbing;

        // Detect being stuck on geometry (or against a bank, swimming): wiggle free, then find
        // a new path from there.
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
                self.stuck_event(position, stats);
                stats.stuck_near_carriers += u32::from(near_carrier(&w.level, position));
            }
        } else {
            self.stuck_time = 0.0;
        }

        // Moving with an enemy known close by: watches his way, not the path ("checking
        // corners"), unless running flat out or on a ladder.
        let mut look = intent.look;
        if self.tactical
            && matches!(look, Look::Along)
            && !me.motion.climbing
            && !frame_sprint_wanted(&intent, dodging)
            && let Some(contact) = self.memory.most_pressing(position, 8.0).filter(|c| flat(c.position - position).length() < 70.0)
        {
            look = Look::At(contact.position);
        }
        match look {
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
            // Never sideways off a ledge (a carrier's deck, a rooftop): the other side, or
            // just ahead.
            if self.unstuck_dir != 0.0 {
                let side = Vec3::new(-direction.z, 0.0, direction.x);
                let ok = |dir: f32| tactics::can_step(w.nav(), position, side * dir, 1.0);
                if !ok(self.unstuck_dir) {
                    self.unstuck_dir = if ok(-self.unstuck_dir) { -self.unstuck_dir } else { 0.0 };
                }
            }
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
            let urgent = dodging || matches!(self.activity, Activity::Cover { .. } | Activity::TakeCover { .. });
            let reserve = if urgent { 0.0 } else { SPRINT_RESERVE };
            if me.motion.stamina <= reserve {
                self.sprinting = false;
            } else if me.motion.stamina > 0.8 || urgent {
                self.sprinting = true;
            }
            let to_cover = matches!(self.activity, Activity::TakeCover { .. } | Activity::Cover { .. } | Activity::Flank { .. });
            let sprint = (intent.goal.is_some_and(|g| g.sprint) || dodging)
                && movement.y > 0.7
                && (self.target.is_none() || (to_cover && self.tactical))
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

    /// Stuck where it is: wiggle free, then find a new path from there; after a few times
    /// here, go somewhere else.
    fn stuck_event(&mut self, position: Vec3, stats: &mut BotStats) {
        stats.stuck_events += 1;
        let square = ((position.x / 10.0).floor() as i32, (position.z / 10.0).floor() as i32);
        *stats.stuck_spots.entry(square).or_default() += 1;
        let on_path = self.path.as_ref().and_then(|p| p.waypoints.get(self.waypoint)).map(|w| w.position);
        let waypoint = on_path.or(self.goal);
        stats.stuck_samples.insert(square, (position, waypoint, self.idle_reason()));
        // Mounting stops at a vehicle's door, which may be what it is stuck on: the door
        // isn't a cell to avoid.
        if let Some(toward) = on_path.filter(|_| !matches!(self.activity, Activity::Mount { .. })) {
            stats.stuck_reports.push((position, toward));
        }
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
            // Keeps failing here: go somewhere else, and not back to where it was going for a
            // while (a spot the grid thinks it can reach but it can't).
            self.stuck_strikes = 0.0;
            self.spot = None;
            self.via = None;
            if crate::ai::tune::knob("bad_goal", 1.0) > 0.5 {
                self.bad_goal = self.goal.map(|g| (g, BAD_GOAL_SECONDS));
            }
        }
    }

    /// Requests paths as needed and walks them waypoint by waypoint. Without a path yet,
    /// heads straight for the goal. The path is kept while the goal moves less than
    /// `tolerance` meters.
    #[allow(clippy::too_many_arguments)]
    fn follow_path(
        &mut self,
        nav: &Navigation,
        obstacles: Option<&NavObstacles>,
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
            self.path_done = None;
            let grid = nav.0.clone();
            let blocked = obstacles.map(|o| o.0.clone());
            let region = self.region;
            self.path_task = Some(AsyncComputeTaskPool::get().spawn(async move {
                let started = Instant::now();
                let path = grid.find_path_from(position, region, goal, blocked.as_deref());
                PathResult {
                    path,
                    seconds: started.elapsed().as_secs_f32(),
                }
            }));
        }

        let Some(path) = &self.path else {
            if self.path_done.is_some() && self.path_task.is_none() {
                return Steer::Arrived;
            }
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
                self.path_done =
                    Some(flat(goal - position).length() < 1.0 || nav.0.walkable_line_avoiding(position, goal, obstacles.map(|o| &*o.0)));
                Steer::Arrived
            } else {
                Steer::Stranded
            };
        };
        if motion.climbing {
            // Height changes and no progress along the ground are what climbing is.
            self.waypoint_timer = 0.0;
            // On the ladder a little before the point it is climbed from (walking into it at
            // an angle gets on it within reach of the rungs): that is the ladder it means to
            // take, not one to jump off again.
            if waypoint.ladder.is_none()
                && let Some(next) = path.waypoints.get(self.waypoint + 1)
                && next.ladder.is_some()
                && flat(waypoint.position - position).length() < 1.5
            {
                self.waypoint += 1;
                self.waypoint_best = f32::MAX;
                return Steer::Toward {
                    target: next.position,
                    jump: false,
                    ladder: next.ladder,
                };
            }
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
            let enemy_vehicles = w
                .vehicles
                .iter()
                .filter(|(vehicle, ..)| equipment::crew_team(w, *vehicle).is_some_and(|t| t == team.opponent()))
                .count();
            let kit = squad::choose_kit(
                &w.armory,
                t,
                &w.snapshot.kits[t],
                player,
                deployment.kit,
                &self.personality,
                enemy_vehicles,
            );
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
        let held: Vec<(usize, u8, Vec3)> = w
            .map
            .areas
            .iter()
            .enumerate()
            .filter(|(i, a)| a.has_spawns && w.holds(*i, team) && !w.spawn_blocked(*i, team))
            .filter_map(|(i, a)| Some((i, a.control_point?, a.position)))
            .collect();
        // Not at a base walking can't get anywhere from (a carrier) while there's another.
        let goal_region = area.and_then(|a| w.map.walk_regions.get(a).copied().flatten());
        let connected = |i: usize| w.map.walk_regions.get(i).copied().flatten() == goal_region;
        let spawns: Vec<(u8, Vec3)> = if goal_region.is_some() && held.iter().any(|(i, ..)| connected(*i)) {
            held.iter().filter(|(i, ..)| connected(*i)).map(|(_, cp, p)| (*cp, *p)).collect()
        } else {
            held.iter().map(|(_, cp, p)| (*cp, *p)).collect()
        };
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
    players: Query<&Player>,
) {
    let existing = bots.iter().count() as u32;
    if existing >= settings.bots {
        return;
    }
    let mut teams: Vec<Team> = teams.iter().copied().collect();
    // The first free name: "Alpha (bot)" ... "Zulu (bot)", then "Alpha 2 (bot)", ...
    let mut taken: Vec<String> = players.iter().map(|p| p.name.clone()).collect();
    for _ in existing..settings.bots {
        let team = balanced_team(teams.iter());
        teams.push(team);
        let name = (0..)
            .map(|n| {
                let base = BOT_NAMES[n % BOT_NAMES.len()];
                match n / BOT_NAMES.len() {
                    0 => format!("{base} (bot)"),
                    round => format!("{base} {} (bot)", round + 1),
                }
            })
            .find(|name| !taken.contains(name))
            .expect("names never run out");
        taken.push(name.clone());
        commands.spawn((
            Player { name, is_bot: true },
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
    mut claims: ResMut<VehicleClaims>,
    (mut reports, mut spot_marks, mut radio, mut taken): (
        ResMut<ai::squad::SquadReports>,
        MessageWriter<crate::radio::Spot>,
        MessageWriter<ToClients<game_shared::radio::RadioMessage>>,
        Local<Vec<Vec3>>,
    ),
) {
    let started = Instant::now();
    let mut covers = 3;
    claims.tick(w.time.delta_secs());
    // Triggers held on foot this tick: bullets passing close suppress.
    let shots: Vec<combat::Shot> = w
        .soldiers
        .iter()
        .filter(|s| s.5.is_some_and(|a| a.0.pressed(Buttons::FIRE)) && s.7.is_none() && !s.8)
        .filter_map(|(soldier, motion, controlled_by, inventory, _, _, loadout, ..)| {
            let inventory = inventory?;
            let weapon = w.weapon(loadout, inventory.active)?;
            let loaded = inventory.ammo.get(inventory.active as usize).is_some_and(|a| a[0] > 0);
            (weapon.fire.kind == FireKind::Gun && weapon.projectile.explosion_radius <= 0.0 && loaded).then(|| combat::Shot {
                soldier,
                team: w.teams.get(controlled_by.0).copied().unwrap_or_default(),
                eye: motion.eye_position(),
                dir: motion.view_rotation() * Vec3::NEG_Z,
                range: (w.data.weapon(weapon).max_range * 1.5).clamp(60.0, 400.0),
                feet: motion.position,
            })
        })
        .collect();
    let mut rays = combat::COVER_RAYS;
    let mut near_rays = combat::NEAR_RAYS;
    let mut spots: Vec<combat::SpotCall> = Vec::new();
    let mut next_taken: Vec<Vec3> = Vec::new();
    // Who sits where.
    let mut crews = Crews::default();
    for (soldier, _, controlled_by, .., seated, _) in &w.soldiers {
        if let Some(seated) = seated {
            let _ = soldier;
            crews.entry(seated.vehicle).or_default().push(Crew {
                seat: seated.seat,
                player: controlled_by.0,
                team: w.teams.get(controlled_by.0).copied().unwrap_or_default(),
            });
        }
    }
    let mut seated_ms = 0.0;
    for (player, mut brain, mut buffer, team, controls, member, mut deployment) in &mut bots {
        let soldier = controls.and_then(|c| w.soldiers.get(c.0).ok());
        if soldier.is_none_or(|s| s.8) && !brain.death_noted && brain.soldier.is_some() {
            brain.death_noted = true;
            if let Some(team_stats) = ai_stats.team(*team) {
                let state = brain.fight_state();
                team_stats.combat.deaths_by[state] += 1;
                team_stats.combat.deaths_aware[state] += u32::from(brain.target.is_some());
            }
        }
        let Some((own, motion, _, inventory, health, _, loadout, seated, downed)) = soldier else {
            brain.while_dead(&w, player, *team, member.copied(), &mut deployment);
            claims.release(player);
            continue;
        };
        // Down, the server ignores its input.
        if downed {
            brain.cover = None;
            brain.target = None;
            brain.fighting = -1.0;
            brain.seq = brain.seq.wrapping_add(1);
            buffer.push(InputFrame {
                seq: brain.seq,
                yaw: brain.yaw,
                ..default()
            });
            continue;
        }
        brain.death_noted = false;
        brain.prev_state = brain.fight_state();
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
        let mut vcx = VehicleCx {
            claims: &mut claims,
            crews: &crews,
        };
        brain.tactical = !squad::legacy(&w.settings, *team);
        let mut cx = combat::Cx {
            rays: &mut rays,
            near_rays: &mut near_rays,
            shots: &shots,
            taken: &taken,
            spots: &mut spots,
            reports: &mut reports,
        };
        let frame = match seated {
            Some(seated) => {
                let started = Instant::now();
                let frame = brain.tick_seated(&w, &me, seated, &mut vcx, &mut intel, &mut stats, team_stats);
                let ms = started.elapsed().as_secs_f32() * 1000.0;
                seated_ms += ms;
                let entry = stats.seated_by.entry(brain.ride_kind(&w)).or_default();
                entry.0 += ms;
                entry.1 += 1;
                frame
            }
            None => brain.tick(&w, &me, &mut intel, &mut stats, team_stats, &mut covers, &mut vcx, &mut cx),
        };
        buffer.push(frame);
        if let Some(spot) = brain.claimed_cover() {
            next_taken.push(spot);
        }
    }
    *taken = next_taken;
    for call in spots {
        spot_marks.write(crate::radio::Spot {
            target: call.target,
            team: call.team,
            seconds: game_shared::radio::SPOT_SECONDS,
        });
        radio.write(ToClients {
            targets: SendTargets::All,
            message: game_shared::radio::RadioMessage {
                player: call.player,
                command: if call.sniper {
                    game_shared::radio::RadioCommand::SpottedSniper
                } else {
                    game_shared::radio::RadioCommand::SpottedInfantry
                },
                position: call.position,
                target: Some(call.target),
                squad: None,
            },
        });
        if let Some(team_stats) = ai_stats.team(call.team) {
            team_stats.spots += 1;
        }
    }
    let ms = started.elapsed().as_secs_f32() * 1000.0;
    stats.think_ms += ms;
    stats.seated_ms += seated_ms;
    stats.max_think_ms = stats.max_think_ms.max(ms);
    stats.ticks += 1;
}

/// Stuck events of this tick teach the bots where not to walk or drive (see
/// [`crate::nav::StuckCells`]).
fn learn_stuck_spots(
    mut stats: ResMut<BotStats>,
    nav: Option<Res<Navigation>>,
    level: Res<LoadedLevel>,
    mut stuck_cells: ResMut<crate::nav::StuckCells>,
) {
    if level.is_changed() {
        stuck_cells.forget();
    }
    let reports = std::mem::take(&mut stats.stuck_reports);
    if let Some(nav) = nav {
        for (from, toward) in reports {
            stuck_cells.report(&nav.0, from, toward);
        }
    }
    for (from, toward) in std::mem::take(&mut stats.vehicle_stuck_reports) {
        stuck_cells.report_vehicle(from, toward);
    }
}

fn log_stats(time: Res<Time>, mut stats: ResMut<BotStats>, bots: Query<(), With<BotBrain>>, stuck_cells: Res<crate::nav::StuckCells>) {
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
             stuck most at {}; {} stuck events near carriers; {} cells and {} driving spots learned to avoid; idle bots on foot (30 s checks): {}",
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
            stats.stuck_near_carriers,
            stuck_cells.learned(),
            stuck_cells.vehicle_learned(),
            {
                let mut by: Vec<_> = stats.idle_by.iter().collect();
                by.sort_by(|a, b| b.1.cmp(a.1));
                if by.is_empty() {
                    "none".to_string()
                } else {
                    let list = by.iter().map(|(reason, n)| format!("{n} {reason}")).collect::<Vec<_>>().join(", ");
                    match stats.idle_samples.is_empty() {
                        true => list,
                        false => format!("{list} (moving: {})", stats.idle_samples.join("; ")),
                    }
                }
            },
        );
        let mut spots: Vec<_> = stats.stuck_spots.iter().collect();
        spots.sort_by(|a, b| b.1.cmp(a.1));
        for (square, n) in spots.iter().take(3).filter(|(_, n)| **n >= 3) {
            if let Some((at, waypoint, doing)) = stats.stuck_samples.get(*square) {
                info!(
                    "bots: stuck {n} times near {} {}: last at {at:.1} going for {} ({doing})",
                    square.0 * 10 + 5,
                    square.1 * 10 + 5,
                    waypoint.map_or("-".into(), |w| format!("{w:.1}"))
                );
            }
        }
        let mut spots: Vec<_> = stats.vehicle_stuck_spots.iter().collect();
        spots.sort_by(|a, b| b.1.cmp(a.1));
        for (square, n) in spots.iter().take(2).filter(|(_, n)| **n >= 3) {
            if let Some((at, target, what)) = stats.vehicle_stuck_samples.get(*square) {
                info!(
                    "bots in vehicles: stuck {n} times near {} {}: last at {at:.1} steering for {target:.1} ({what})",
                    square.0 * 10 + 5,
                    square.1 * 10 + 5,
                );
            }
        }
        info!(
            "bots in vehicles: {} seats taken, {} left; {:.2} km driven in {:.0} s at the wheel ({:.1} m/s); \
             {} vehicle stuck events ({:.1} per vehicle-minute, most at {}); {} vehicle paths ({} partial, {} failed); \
             {} takeoffs, {} crashes; seated bots {:.3} ms per tick ({})",
            stats.vehicle_entries,
            stats.vehicle_exits,
            stats.driven / 1000.0,
            stats.driving_seconds,
            stats.driven / stats.driving_seconds.max(1.0),
            stats.vehicle_stuck,
            stats.vehicle_stuck as f32 / (stats.driving_seconds / 60.0).max(1.0),
            hotspots(&stats.vehicle_stuck_spots),
            stats.vehicle_paths,
            stats.vehicle_partial_paths,
            stats.vehicle_failed_paths,
            stats.flights,
            stats.crashes,
            stats.seated_ms / stats.ticks.max(1) as f32,
            {
                let mut by: Vec<_> = stats.seated_by.iter().collect();
                by.sort_by(|a, b| b.1.0.total_cmp(&a.1.0));
                by.iter()
                    .take(4)
                    .map(|(kind, (ms, n))| format!("{kind} {:.1} us", ms * 1000.0 / (*n).max(1) as f32))
                    .collect::<Vec<_>>()
                    .join(", ")
            },
        );
    }
    *stats = BotStats::default();
}

/// The three 10 m squares with the most stuck events, as `x z (count)`.
/// How far from an aircraft carrier's statics stuck events count as near it, meters.
const CARRIER_REACH: f32 = 150.0;

/// Whether a position is on or around an aircraft carrier.
fn near_carrier(level: &LoadedLevel, position: Vec3) -> bool {
    level.desc.statics.iter().any(|s| {
        s.template.contains("carrier")
            && Vec2::new(s.placement.position[0], s.placement.position[2]).distance(position.xz()) < CARRIER_REACH
    })
}

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
    let water = level.desc.water.as_ref().map(|w| w.height);
    // Don't send bots wandering out to sea for no reason: resample a few times if the spot
    // is over deep water, and give up and stay put rather than pick one anyway.
    for _ in 0..8 {
        let spot = center + Vec3::new(fastrand::f32() * 2.0 - 1.0, 0.0, fastrand::f32() * 2.0 - 1.0) * half;
        let ground = heightmap.height_at(spot.x, spot.z);
        if water.is_none_or(|w| w - ground < ROAM_WADE_DEPTH) {
            return Vec3::new(spot.x, ground, spot.z);
        }
    }
    from
}

/// Roaming avoids water deeper than this (m): wading is fine, swimming out for no reason isn't.
const ROAM_WADE_DEPTH: f32 = 0.4;
/// A swimmer finds its feet where the bottom is less deep than the wading depth (0.4 m,
/// `SoldierTuning::wade_depth`): shores it swims for are shallower still.
const SHORE_DEPTH: f32 = 0.25;

/// The nearest place a swimmer at `from` can walk out of the water: a cell of the grid
/// within wading depth of the surface (or above it), connected to the bottom it swims over
/// (any, out at sea), not one bots learned to avoid. Looks up to 160 m out.
fn nearest_shore(nav: &NavGrid, blocked: Option<&NavBlocked>, water: Option<f32>, from: Vec3, failed: &[Vec3]) -> Option<Vec3> {
    let water = water?;
    let region = nav.locate(from, 4.0, None).map(|c| nav.cell(c).region);
    for radius in [15.0, 40.0, 80.0, 160.0] {
        let best = nav
            .cells_near(from.xz(), radius)
            .filter(|c| {
                let cell = nav.cell(*c);
                region.is_none_or(|r| r == cell.region)
                    && water - cell.y < SHORE_DEPTH
                    && cell.y - water < 1.0
                    && cell.dist > 1
                    && blocked.is_none_or(|b| !b.contains(c.index))
            })
            .map(|c| nav.position(c))
            .filter(|p| failed.iter().all(|f| f.distance(*p) > 8.0))
            .min_by(|a, b| a.xz().distance_squared(from.xz()).total_cmp(&b.xz().distance_squared(from.xz())));
        if best.is_some() {
            return best;
        }
    }
    None
}

/// Whether a soldier's main weapons are down to their last magazine.
/// The walkable region a soldier with his feet at `from` is in: that of the nearest cell it
/// can see from knee height. Beside a thin wall the nearest cell may be on its other side (a
/// closed room of a house on the coarse grid of a big map): paths, spots and cover from there
/// would lead through the wall.
fn walk_region(nav: &NavGrid, spatial: &avian3d::prelude::SpatialQuery, from: Vec3) -> Option<u16> {
    let mut cells: Vec<(f32, crate::nav::CellRef)> = nav
        .cells_near(from.xz(), 2.0)
        .filter(|&c| nav.cell(c).region != 0 && (-3.0..=1.0).contains(&(nav.cell(c).y - from.y)))
        .map(|c| {
            let p = nav.position(c);
            (p.xz().distance_squared(from.xz()) + 4.0 * (p.y - from.y).powi(2), c)
        })
        .collect();
    cells.sort_by(|a, b| a.0.total_cmp(&b.0));
    cells.truncate(6);
    let first = nav.cell(cells.first()?.1).region;
    if cells.iter().all(|(_, c)| nav.cell(*c).region == first) {
        return Some(first);
    }
    let knee = from + Vec3::Y * 0.5;
    cells
        .iter()
        .find(|(d2, c)| *d2 < 0.3 * 0.3 || tactics::line_of_sight(spatial, knee, nav.position(*c) + Vec3::Y * 0.5))
        .map_or(Some(first), |(_, c)| Some(nav.cell(*c).region))
}

fn low_on_ammo(inventory: &Inventory, loadout: &Loadout, armory: &Armory) -> bool {
    loadout.weapons.iter().zip(&inventory.ammo).any(|(name, [_, spare])| {
        armory
            .weapon(name)
            .is_some_and(|w| w.slot == 3 && w.magazine_size > 0 && (*spare as u32) < w.magazine_size)
    })
}

/// Whether the intent wants a sprint (dodging a vehicle, or a goal it runs to).
fn frame_sprint_wanted(intent: &Intent, dodging: bool) -> bool {
    dodging || intent.goal.is_some_and(|g| g.sprint)
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
