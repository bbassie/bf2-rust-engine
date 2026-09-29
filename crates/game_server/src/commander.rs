//! The server side of the commander (see `game_shared::commander`): the post, squad orders,
//! and the assets: artillery barrages, UAV sweeps, satellite scans and supply drops, their
//! recharge, their level objects being destroyed, repaired by engineers or coming back.
//!
//! Like BF2, the artillery pieces are real objects (vehicles tagged [`AssetVehicle`]): a
//! strike gives each living piece of the team a fire mission, the piece turns onto the
//! target at its BF2 speeds and fires its burst, and its shells fly as projectiles in an arc
//! to where they land (at the strike's timing: the first after [`ARTILLERY_DELAY`], then one
//! per gun every `interval`, spread over the target area). The more pieces live, the more
//! shells; with none left, there is no artillery until one is repaired or comes back. The UAV
//! is a vehicle too, flown round its circle over the target, and shot down it falls.
//! (Levels imported before the pieces were vehicles keep the abstract strike: shells land
//! on schedule without flying.)
//!
//! Requests from clients and from an AI commander go through [`CommanderCommand`]: a bot
//! strategy can apply for the post and give orders by writing those with its bot player.

use std::collections::{HashMap, HashSet, VecDeque};

use avian3d::prelude::*;
use bevy::prelude::*;
use bevy_replicon::prelude::*;
use game_data::AssetKind;
use game_shared::{
    commander::{
        ARTILLERY_DELAY, ARTILLERY_GUN_STAGGER, ASSET_RESPAWN_SECONDS, Asset, AssetEffect, AssetStatus, AssetVehicle,
        Commander, CommanderAssets, CommanderRequest, MUTINY_SHARE, OrderKind, SCAN_SECONDS, ScanContact, ScanReport,
        SquadOrder, TeamAssets, UAV_SECONDS,
    },
    conquest::team_index,
    effects::PlayEffect,
    flight::{BodyState, Controls as FlightControls, FlightState, GRAVITY},
    input::Buttons,
    level::{LevelEntity, LoadedLevel},
    physics::GameLayer,
    protocol::{ControlledBy, Player, Team},
    radio::{RadioCommand, RadioMessage, SPOT_SECONDS},
    revive::Downed,
    soldier::{Health, Soldier, SoldierMotion},
    squad::SquadMember,
    statics::DestroyedStatics,
    vehicle::{
        Seated, Vehicle, VehicleData, VehicleHealth, VehicleModel, VehicleMotion, VehicleShot, VehicleState, VehicleSystems,
        step_joints,
    },
    weapons::{Armory, Inventory, Loadout},
};

use crate::{
    AppliedInput, ClientPlayer, Controls, HostPlayer, PlayerClient,
    combat::{self, Attacker, Explosion},
    destruction::{Materials, ObjectHealth},
    limits::{Rate, RateLimiter},
    modes::RoundReset,
    player_client,
    radio::Spot,
    sender_player,
};

pub struct CommanderPlugin;

impl Plugin for CommanderPlugin {
    fn build(&self, app: &mut App) {
        app.init_resource::<CommanderState>()
            .add_message::<CommanderCommand>()
            .add_systems(
                PreUpdate,
                receive_requests
                    .after(ServerSystems::Receive)
                    .run_if(in_state(ClientState::Disconnected)),
            )
            .add_systems(
                Update,
                (
                    setup_teams.run_if(resource_exists_and_changed::<LoadedLevel>),
                    reset_on_round,
                    prune_mutiny,
                    tag_asset_vehicles,
                    handle_commands,
                    track_assets,
                    repair_assets,
                    run_strikes,
                    run_uavs,
                    run_scans,
                    run_crates,
                    tidy_orders,
                    leave_squads,
                )
                    .chain()
                    .run_if(resource_exists::<LoadedLevel>)
                    .run_if(in_state(ClientState::Disconnected)),
            )
            .add_systems(
                FixedUpdate,
                // After the vehicles turned their joints for their gunners: a fire mission
                // aims the piece this tick, whoever sits in it.
                (fire_artillery.after(VehicleSystems::Simulate), fly_uav_vehicles)
                    .run_if(resource_exists::<LoadedLevel>)
                    .run_if(in_state(ClientState::Disconnected)),
            );
    }
}

/// A commander request from a client or an AI commander (then `player` is the bot's
/// player); the same rules apply.
#[derive(Message, Clone, Copy, Debug)]
pub struct CommanderCommand {
    pub player: Entity,
    pub request: CommanderRequest,
}

/// Seconds a UAV-seen enemy stays marked after leaving its view.
const UAV_MARK: f32 = 2.5;
/// Supply crates come down this fast on their parachute (m/s) [inferred].
const PARACHUTE_SPEED: f32 = 5.0;
/// Engineers repair a destroyed asset within this many meters of it.
const REPAIR_REACH: f32 = 6.0;
/// Assets' armor material (`Destroyable_obj_large`), which the wrench's repair rate goes
/// through the damage table against.
const ASSET_MATERIAL: u32 = 98;
/// Shells of a strike fly this long at least and at most (seconds): what is left of the
/// strike's delay once the gun has turned onto the target [our choice: BF2's shells follow a
/// predestined path of their own timing].
const MIN_FLIGHT: f32 = 1.5;
const MAX_FLIGHT: f32 = 4.5;
/// A gun fires once its barrel points this close to the shell's launch direction (radians).
const AIM_TOLERANCE: f32 = 0.035;
/// A strike's markers stay this long after its last shell is due.
const STRIKE_LINGER: f32 = 1.5;
/// The commander spots the enemy within this many meters of where he clicks.
const SPOT_REACH: f32 = 15.0;

#[derive(Default)]
struct TeamState {
    /// Seconds until each asset recharges, by [`Asset::index`].
    recharge: [f32; 4],
    mutiny: HashSet<Entity>,
}

#[derive(Resource, Default)]
struct CommanderState {
    teams: [TeamState; 2],
    /// When each destroyed asset object went down.
    destroyed_at: HashMap<u32, f32>,
    /// How far engineers got repairing destroyed asset objects (0..1).
    repaired: HashMap<u32, f32>,
}

impl CommanderState {
    fn team(&mut self, team: Team) -> Option<&mut TeamState> {
        match team {
            Team::One => Some(&mut self.teams[0]),
            Team::Two => Some(&mut self.teams[1]),
            Team::Spectator => None,
        }
    }
}

/// A strike under way (on its [`AssetEffect`]): artillery shells still to land on schedule
/// (seconds until each, and where; only without artillery vehicles) and the seconds until
/// the strike is over.
#[derive(Component)]
struct Strike {
    shells: Vec<(f32, Vec3)>,
    attacker: Attacker,
    left: f32,
}

/// Server-side, on an artillery piece firing at a strike: when (seconds from the call) and
/// where each shell it still has to fire lands, in order, and how long each flies.
#[derive(Component)]
struct FireMission {
    player: Entity,
    clock: f32,
    shells: VecDeque<(f32, Vec3)>,
    flight: f32,
}

#[derive(Component)]
struct Uav {
    left: f32,
    next_sweep: f32,
    /// The UAV flying the circle, if it is a vehicle.
    vehicle: Option<Entity>,
}

/// Server-side, on a UAV vehicle: the circle it flies.
#[derive(Component)]
struct UavFlight {
    center: Vec3,
    angle: f32,
    radius: f32,
    speed: f32,
}

/// Server-side: a satellite scan under way: the commander who called it and the seconds
/// left; it reports every second.
#[derive(Component)]
struct Scan {
    team: Team,
    commander: Entity,
    left: f32,
    next_report: f32,
}

#[derive(Component)]
struct SupplyCrate {
    falling: bool,
    stock: f32,
    left: f32,
    /// It hands out once a second.
    next_round: f32,
}

/// Requests from clients, rate-limited per player (each one plays a radio line to the team).
fn receive_requests(
    time: Res<Time<Real>>,
    mut requests: MessageReader<FromClient<CommanderRequest>>,
    clients: Query<&ClientPlayer>,
    host: Option<Res<HostPlayer>>,
    mut commands: MessageWriter<CommanderCommand>,
    mut limits: Local<RateLimiter<Entity>>,
) {
    let now = time.elapsed_secs_f64();
    for request in requests.read() {
        let Some(player) = sender_player(request.client_id, &clients, host.as_deref()) else {
            continue;
        };
        if matches!(request.client_id, ClientId::Client(_)) && !limits.allow(player, Rate::COMMANDER, now) {
            debug!("commander request from {player}: too many, dropped");
            continue;
        }
        commands.write(CommanderCommand {
            player,
            request: request.message,
        });
    }
}

/// Targets beyond the map are clamped to this box on levels without terrain (meters).
const MAP_LIMIT: f32 = 8192.0;

/// Where a commander's target lies: `None` unless it is a finite point, else clamped onto the
/// level's terrain (or a generous box without one). NaN targets would end up in orders,
/// UAV circles, shells and crates.
fn map_target(level: &LoadedLevel, target: Vec3) -> Option<Vec3> {
    let target = game_shared::validate::finite_point(target)?;
    Some(match &level.heightmap {
        Some(heightmap) => {
            let (min, size) = (heightmap.origin, heightmap.world_size());
            Vec3::new(
                target.x.clamp(min.x, min.x + size),
                target.y.clamp(-MAP_LIMIT, MAP_LIMIT),
                target.z.clamp(min.z, min.z + size),
            )
        }
        None => target.clamp(Vec3::splat(-MAP_LIMIT), Vec3::splat(MAP_LIMIT)),
    })
}

/// A request with its target checked by [`map_target`]; `None` if it has no valid target.
fn checked_request(level: &LoadedLevel, request: CommanderRequest) -> Option<CommanderRequest> {
    Some(match request {
        CommanderRequest::Order { squad, kind, target } => CommanderRequest::Order {
            squad,
            kind,
            target: map_target(level, target)?,
        },
        CommanderRequest::Use { asset, target } => CommanderRequest::Use {
            asset,
            target: map_target(level, target)?,
        },
        CommanderRequest::Spot { target } => CommanderRequest::Spot {
            target: map_target(level, target)?,
        },
        other => other,
    })
}

/// Whether a team's asset stands right now: its object (or at least one artillery piece that
/// isn't a wreck), read from the live state rather than the replicated [`TeamAssets`], which
/// is only rewritten after the requests are handled.
#[allow(clippy::type_complexity)]
fn asset_intact(
    asset: Asset,
    team: Team,
    assets: &CommanderAssets,
    destroyed: Option<&DestroyedStatics>,
    guns: &Query<(Entity, &AssetVehicle, &VehicleData, &VehicleState, &VehicleHealth, &VehicleMotion)>,
) -> bool {
    asset.object().is_none_or(|kind| {
        assets.of(team, kind).any(|a| match a.vehicle {
            true => guns.iter().any(|(_, gun, .., health, _)| gun.instance == a.instance && !health.wrecked()),
            false => !destroyed.is_some_and(|d| d.0.contains(&a.instance)),
        })
    })
}

/// A new level: no commanders, fresh assets.
fn setup_teams(
    mut commands: Commands,
    commanders: Query<Entity, With<Commander>>,
    mut state: ResMut<CommanderState>,
) {
    *state = CommanderState::default();
    for player in &commanders {
        commands.entity(player).remove::<Commander>();
    }
    for team in [Team::One, Team::Two] {
        commands.spawn((
            TeamAssets {
                team,
                status: [AssetStatus::default(); 4],
            },
            Replicated,
            LevelEntity,
        ));
    }
}

/// A round starting over on the same level (see [`RoundReset`]): the assets in play don't
/// belong to the new round, so they end along with their recharge and mutiny votes. A full
/// map change instead despawns them as [`LevelEntity`]s and `setup_teams` rebuilds
/// `CommanderState` from scratch; this covers the case that leaves the level standing.
#[allow(clippy::type_complexity)]
fn reset_on_round(
    mut commands: Commands,
    mut resets: MessageReader<RoundReset>,
    mut state: ResMut<CommanderState>,
    strikes: Query<Entity, With<Strike>>,
    uavs: Query<(Entity, &Uav)>,
    scans: Query<Entity, With<Scan>>,
    supply: Query<Entity, With<SupplyCrate>>,
    guns: Query<Entity, With<FireMission>>,
) {
    if resets.read().last().is_none() {
        return;
    }
    for team in &mut state.teams {
        team.recharge = [0.0; 4];
        team.mutiny.clear();
    }
    for entity in &strikes {
        commands.entity(entity).despawn();
    }
    for (entity, uav) in &uavs {
        if let Some(vehicle) = uav.vehicle {
            commands.entity(vehicle).despawn();
        }
        commands.entity(entity).despawn();
    }
    for entity in &scans {
        commands.entity(entity).despawn();
    }
    for entity in &supply {
        commands.entity(entity).despawn();
    }
    for gun in &guns {
        commands.entity(gun).remove::<FireMission>();
    }
    info!("round reset: commander assets and recharge cleared");
}

/// Mutiny votes for a commander who resigned, or from a player who left the team or the
/// match, no longer count: otherwise stale votes from an earlier commander or a player who
/// since left can tip a mutiny nobody currently active asked for.
fn prune_mutiny(players: Query<(Entity, &Team)>, mut state: ResMut<CommanderState>) {
    let [one, two] = &mut state.teams;
    for (team, team_state) in [(Team::One, one), (Team::Two, two)] {
        if team_state.mutiny.is_empty() {
            continue;
        }
        team_state
            .mutiny
            .retain(|&voter| players.get(voter).is_ok_and(|(_, &t)| t == team));
    }
}

/// Vehicles the layout's spawners put where an asset of the layout stands are that asset.
/// (Every untagged vehicle of an asset template is looked at, so the order in which the level's
/// assets and vehicles appear doesn't matter.)
fn tag_asset_vehicles(
    mut commands: Commands,
    assets: Res<CommanderAssets>,
    vehicles: Query<(Entity, &Vehicle, &VehicleMotion), Without<AssetVehicle>>,
) {
    for (entity, vehicle, motion) in &vehicles {
        if !assets.desc.assets.contains_key(&vehicle.template) {
            continue;
        }
        let asset = assets.instances.iter().find(|a| {
            a.vehicle
                && a.template == vehicle.template
                && Vec3::from_array(a.placement.position).xz().distance(motion.position.xz()) < 1.0
        });
        if let Some(asset) = asset {
            debug!("{:?} {:?} {} is asset {}", asset.team, asset.kind, asset.template, asset.instance);
            commands.entity(entity).insert(AssetVehicle {
                kind: asset.kind,
                team: asset.team,
                instance: asset.instance,
            });
        }
    }
}

/// The team's commander line to its players (or one squad).
fn say(radio: &mut MessageWriter<ToClients<RadioMessage>>, player: Entity, position: Vec3, command: RadioCommand, squad: Option<u8>) {
    radio.write(ToClients {
        targets: SendTargets::All,
        message: RadioMessage {
            player,
            command,
            position,
            target: None,
            squad,
        },
    });
}

/// Where a point on the map is on the ground.
fn ground(level: &LoadedLevel, point: Vec3) -> Vec3 {
    match &level.heightmap {
        Some(heightmap) => Vec3::new(point.x, heightmap.height_at(point.x, point.z), point.z),
        None => point,
    }
}

/// A random point within `spread` meters of `target`, on the ground.
fn spread_point(level: &LoadedLevel, target: Vec3, spread: f32) -> Vec3 {
    let angle = fastrand::f32() * std::f32::consts::TAU;
    let offset = Vec2::from_angle(angle) * spread * fastrand::f32().sqrt();
    ground(level, target + Vec3::new(offset.x, 0.0, offset.y))
}

/// The world transform of an artillery piece's muzzle with its joints at `joints`.
fn muzzle(model: &VehicleModel, body: &BodyState, joints: &[[f32; 3]]) -> Transform {
    let transforms = model.part_transforms(joints);
    Transform::from_translation(body.position).with_rotation(body.rotation) * model.muzzle(&transforms, 0)
}

/// The velocity that takes a shell from `from` to `to` in `flight` seconds under `gravity`.
fn launch_velocity(from: Vec3, to: Vec3, flight: f32, gravity: f32) -> Vec3 {
    (to - from) / flight + Vec3::Y * (0.5 * gravity * flight)
}

/// Gravity on the gun's shells (m/s²).
fn shell_gravity(model: &VehicleModel) -> f32 {
    GRAVITY * model.guns.first().map_or(1.0, |g| g.projectile.gravity).max(0.1)
}

/// How long a piece takes to turn from `joints` onto a shell to `target` flying `flight`
/// seconds, at its joints' speeds (seconds, at most 6).
fn aim_time(model: &VehicleModel, body: &BodyState, joints: &[[f32; 3]], target: Vec3, flight: f32) -> f32 {
    const DT: f32 = 0.05;
    let gravity = shell_gravity(model);
    let mut joints = joints.to_vec();
    for step in 0..120 {
        let muzzle = muzzle(model, body, &joints);
        let direction = launch_velocity(muzzle.translation, target, flight, gravity).normalize_or(Vec3::Y);
        if (muzzle.rotation * Vec3::NEG_Z).angle_between(direction) < AIM_TOLERANCE {
            return step as f32 * DT;
        }
        let aim = [Some(muzzle.translation + direction * 1000.0)];
        joints = step_joints(model, body, &aim, &joints, &FlightControls::default(), &FlightState::default(), DT).0;
    }
    6.0
}

#[allow(clippy::too_many_arguments, clippy::type_complexity)]
fn handle_commands(
    mut commands: Commands,
    mut requests: MessageReader<CommanderCommand>,
    level: Res<LoadedLevel>,
    assets: Res<CommanderAssets>,
    players: Query<(Entity, &Player, &Team, Option<&SquadMember>, Has<Commander>, Option<&Controls>)>,
    motions: Query<&SoldierMotion>,
    enemies: Query<(Entity, &ControlledBy, Option<&Seated>), (With<Soldier>, Without<Downed>)>,
    orders: Query<(Entity, &SquadOrder)>,
    destroyed: Query<&DestroyedStatics>,
    guns: Query<(Entity, &AssetVehicle, &VehicleData, &VehicleState, &VehicleHealth, &VehicleMotion)>,
    vehicle_positions: Query<&VehicleMotion>,
    mut state: ResMut<CommanderState>,
    mut radio: MessageWriter<ToClients<RadioMessage>>,
    mut spots: MessageWriter<Spot>,
) {
    // What this pass changed that the queries only see once its commands are applied: who
    // holds each team's post, and the order entity of each squad (`None`: cancelled).
    let mut posts: HashMap<Team, Option<Entity>> = HashMap::new();
    let mut squad_orders: HashMap<(Team, u8), Option<Entity>> = HashMap::new();
    for command in requests.read() {
        let player = command.player;
        let Ok((_, info, &team, _, _, controls)) = players.get(player) else {
            continue;
        };
        let Some(team_state) = state.team(team) else {
            continue;
        };
        let Some(request) = checked_request(&level, command.request) else {
            debug!("{} sent a commander request without a valid target: {:?}", info.name, command.request);
            continue;
        };
        let position = controls.and_then(|c| motions.get(c.0).ok()).map_or(Vec3::ZERO, |m| m.position);
        let commander = *posts
            .entry(team)
            .or_insert_with(|| players.iter().find(|p| *p.2 == team && p.4).map(|p| p.0));
        let is_commander = commander == Some(player);
        match request {
            CommanderRequest::Apply if commander.is_none() => {
                // Like BF2, the commander leads no squad.
                commands.entity(player).insert(Commander).remove::<SquadMember>();
                posts.insert(team, Some(player));
                team_state.mutiny.clear();
                info!("{} is now {team:?}'s commander", info.name);
                say(&mut radio, player, position, RadioCommand::NewCommander, None);
            }
            CommanderRequest::Resign if is_commander => {
                commands.entity(player).remove::<Commander>();
                posts.insert(team, None);
                info!("{} resigned as commander", info.name);
                say(&mut radio, player, position, RadioCommand::CommanderResigned, None);
            }
            CommanderRequest::Mutiny if commander.is_some_and(|c| c != player) => {
                team_state.mutiny.insert(player);
                // The team's humans vote (bots never do).
                let voters = players.iter().filter(|p| *p.2 == team && !p.4 && !p.1.is_bot).count().max(1);
                let votes = team_state.mutiny.len();
                info!("mutiny against {team:?}'s commander: {votes}/{voters}");
                if votes as f32 / voters as f32 > MUTINY_SHARE
                    && let Some(commander) = commander
                {
                    commands.entity(commander).remove::<Commander>();
                    posts.insert(team, None);
                    team_state.mutiny.clear();
                    say(&mut radio, commander, position, RadioCommand::CommanderResigned, None);
                }
            }
            CommanderRequest::Order { squad, kind, target } if is_commander => {
                if !players.iter().any(|p| *p.2 == team && p.3.is_some_and(|s| s.squad == squad)) {
                    continue;
                }
                let order = SquadOrder {
                    team,
                    squad,
                    kind,
                    position: ground(&level, target),
                };
                // One order entity per squad, also for several orders in one pass.
                let existing = *squad_orders.entry((team, squad)).or_insert_with(|| {
                    orders.iter().find(|(_, o)| o.team == team && o.squad == squad).map(|(entity, _)| entity)
                });
                match existing {
                    Some(entity) => {
                        commands.entity(entity).insert(order);
                    }
                    None => {
                        let entity = commands.spawn((order, Replicated, LevelEntity)).id();
                        squad_orders.insert((team, squad), Some(entity));
                    }
                }
                let line = match kind {
                    OrderKind::Attack => RadioCommand::OrderAttack,
                    OrderKind::Defend => RadioCommand::OrderDefend,
                    OrderKind::Move => RadioCommand::OrderMove,
                };
                info!("{team:?} commander: squad {squad} {kind:?} at {:.0}", order.position);
                say(&mut radio, player, position, line, Some(squad));
            }
            CommanderRequest::Spot { target } if is_commander => {
                // The enemy (or the vehicle he sits in) nearest to the point.
                let enemy = |c: &ControlledBy| players.get(c.0).is_ok_and(|p| *p.2 != team && *p.2 != Team::Spectator);
                let nearest = enemies
                    .iter()
                    .filter(|(_, controlled_by, _)| enemy(controlled_by))
                    .filter_map(|(soldier, _, seated)| {
                        let body = seated.map_or(soldier, |s| s.vehicle);
                        let at = match seated {
                            Some(s) => vehicle_positions.get(s.vehicle).ok()?.position,
                            None => motions.get(soldier).ok()?.position,
                        };
                        Some((body, at.xz().distance(target.xz())))
                    })
                    .min_by(|a, b| a.1.total_cmp(&b.1));
                debug!("{team:?} commander spots at {target:.0}: nearest enemy {:.0} m away", nearest.map_or(f32::NAN, |n| n.1));
                if let Some((body, _)) = nearest.filter(|(_, distance)| *distance <= SPOT_REACH) {
                    spots.write(Spot {
                        target: body,
                        team,
                        seconds: SPOT_SECONDS,
                    });
                    say(&mut radio, player, target, RadioCommand::EnemySpotted, None);
                }
            }
            CommanderRequest::CancelOrder { squad } if is_commander => {
                let mut cancelled: Vec<Entity> = orders
                    .iter()
                    .filter(|(_, order)| order.team == team && order.squad == squad)
                    .map(|(entity, _)| entity)
                    .collect();
                cancelled.extend(squad_orders.get(&(team, squad)).copied().flatten());
                cancelled.sort();
                cancelled.dedup();
                for entity in cancelled {
                    commands.entity(entity).try_despawn();
                }
                squad_orders.insert((team, squad), None);
            }
            CommanderRequest::Use { asset, target } if is_commander => {
                // The live recharge (set below for this pass's earlier requests), not the
                // replicated `TeamAssets`: several requests in one packet get one strike.
                let ready = team_state.recharge[asset.index()] <= 0.0
                    && asset_intact(asset, team, &assets, destroyed.single().ok(), &guns);
                if !ready {
                    debug!("{team:?} commander: {asset:?} not ready");
                    continue;
                }
                team_state.recharge[asset.index()] = asset.recharge();
                let target = ground(&level, target);
                let attacker = Attacker {
                    player: Some(player),
                    soldier: None,
                    weapon: "artillery".into(),
                };
                let line = match asset {
                    Asset::Artillery => {
                        let desc = &assets.desc.artillery;
                        let mut strike = Strike {
                            shells: Vec::new(),
                            attacker,
                            left: 0.0,
                        };
                        let pieces: Vec<_> = guns
                            .iter()
                            .filter(|(_, a, _, _, health, _)| {
                                a.team == team && a.kind == AssetKind::Artillery && !health.wrecked()
                            })
                            .collect();
                        let last_shell = desc.shells.saturating_sub(1) as f32 * desc.interval;
                        if assets.of(team, AssetKind::Artillery).any(|a| a.vehicle) {
                            // The living pieces fire their bursts.
                            for (index, (gun, _, data, joints, _, motion)) in pieces.iter().enumerate() {
                                let model = &data.0;
                                let first = ARTILLERY_DELAY + index as f32 * ARTILLERY_GUN_STAGGER;
                                let shells: VecDeque<(f32, Vec3)> = (0..desc.shells)
                                    .map(|shell| (first + shell as f32 * desc.interval, spread_point(&level, target, desc.spread)))
                                    .collect();
                                let body = BodyState {
                                    position: motion.position,
                                    rotation: motion.rotation,
                                    ..default()
                                };
                                let turn = aim_time(model, &body, &joints.joints, shells[0].1, 3.0);
                                let flight = (first - turn - 0.2).clamp(MIN_FLIGHT, MAX_FLIGHT);
                                debug!("{team:?} artillery {gun}: turns in {turn:.1} s, shells fly {flight:.1} s");
                                commands.entity(*gun).insert(FireMission {
                                    player,
                                    clock: 0.0,
                                    shells,
                                    flight,
                                });
                            }
                            strike.left = ARTILLERY_DELAY
                                + last_shell
                                + pieces.len().saturating_sub(1) as f32 * ARTILLERY_GUN_STAGGER
                                + STRIKE_LINGER;
                        } else {
                            // Older imports: the pieces are objects; the shells land on schedule.
                            let destroyed = destroyed.single().ok();
                            let count = assets
                                .of(team, AssetKind::Artillery)
                                .filter(|g| !destroyed.is_some_and(|d| d.0.contains(&g.instance)))
                                .count()
                                .max(1);
                            for gun in 0..count {
                                for shell in 0..desc.shells {
                                    let when = ARTILLERY_DELAY
                                        + shell as f32 * desc.interval
                                        + gun as f32 * ARTILLERY_GUN_STAGGER;
                                    strike.shells.push((when, spread_point(&level, target, desc.spread)));
                                }
                            }
                        }
                        info!("{team:?} artillery strike: {} pieces", pieces.len());
                        commands.spawn((
                            AssetEffect {
                                team,
                                asset,
                                position: target,
                                radius: desc.spread + desc.radius,
                            },
                            strike,
                            Replicated,
                            LevelEntity,
                        ));
                        RadioCommand::ArtilleryStrike
                    }
                    Asset::Uav => {
                        let uav = &assets.desc.uav;
                        let effect = commands
                            .spawn((
                                AssetEffect {
                                    team,
                                    asset,
                                    position: target,
                                    radius: uav.radius,
                                },
                                Replicated,
                                LevelEntity,
                            ))
                            .id();
                        // The UAV itself, flying its circle.
                        let vehicle = uav.vehicle.as_ref().map(|template| {
                            let flight = UavFlight {
                                center: target + Vec3::Y * uav.height,
                                angle: fastrand::f32() * std::f32::consts::TAU,
                                radius: uav.radius.max(10.0),
                                speed: uav.speed.max(1.0),
                            };
                            let (position, rotation) = flight.pose();
                            commands
                                .spawn((
                                    Vehicle {
                                        template: template.clone(),
                                    },
                                    flight,
                                    Transform::from_translation(position).with_rotation(rotation),
                                    VehicleMotion {
                                        position,
                                        rotation,
                                        ..default()
                                    },
                                    Replicated,
                                    LevelEntity,
                                ))
                                .id()
                        });
                        commands.entity(effect).insert(Uav {
                            left: UAV_SECONDS,
                            next_sweep: 0.0,
                            vehicle,
                        });
                        RadioCommand::UavOnline
                    }
                    Asset::Scan => {
                        commands.spawn((
                            Scan {
                                team,
                                commander: player,
                                left: SCAN_SECONDS,
                                next_report: 0.0,
                            },
                            LevelEntity,
                        ));
                        RadioCommand::ScanInitiated
                    }
                    Asset::Supply => {
                        let supply = &assets.desc.supply;
                        commands.spawn((
                            AssetEffect {
                                team,
                                asset,
                                position: target + Vec3::Y * supply.drop_height,
                                radius: supply.radius,
                            },
                            SupplyCrate {
                                falling: true,
                                stock: supply.storage,
                                left: supply.lifetime,
                                next_round: 0.0,
                            },
                            Replicated,
                            LevelEntity,
                        ));
                        RadioCommand::SupplyDrop
                    }
                };
                info!("{team:?} commander: {asset:?} at {target:.0}");
                say(&mut radio, player, position, line, None);
            }
            _ => {}
        }
    }
}

/// Recharging, asset objects going down (and coming back after a while), and what the
/// teams see of it. An artillery piece (a vehicle) counts as destroyed while it is a wreck.
#[allow(clippy::too_many_arguments, clippy::type_complexity)]
fn track_assets(
    time: Res<Time>,
    assets: Res<CommanderAssets>,
    commanders: Query<(Entity, &Team), With<Commander>>,
    mut destroyed: Query<&mut DestroyedStatics>,
    mut health: ResMut<ObjectHealth>,
    mut state: ResMut<CommanderState>,
    mut team_assets: Query<&mut TeamAssets>,
    mut vehicles: Query<(&AssetVehicle, &mut VehicleHealth)>,
    mut radio: MessageWriter<ToClients<RadioMessage>>,
) {
    let dt = time.delta_secs();
    let now = time.elapsed_secs();
    for team in &mut state.teams {
        for recharge in &mut team.recharge {
            *recharge = (*recharge - dt).max(0.0);
        }
    }
    let Ok(mut destroyed) = destroyed.single_mut() else {
        return;
    };
    // Which asset vehicles are wrecks now.
    let wrecks: HashMap<u32, bool> = vehicles.iter().map(|(a, h)| (a.instance, h.wrecked())).collect();
    for asset in &assets.instances {
        if asset.vehicle {
            let wreck = wrecks.get(&asset.instance).copied().unwrap_or(false);
            // (Only touched when it changes: the set is replicated.)
            if wreck && !destroyed.0.contains(&asset.instance) {
                destroyed.0.insert(asset.instance);
            } else if !wreck && destroyed.0.contains(&asset.instance) {
                destroyed.0.remove(&asset.instance);
            }
        }
        let down = destroyed.0.contains(&asset.instance);
        match (down, state.destroyed_at.get(&asset.instance).copied()) {
            (true, None) => {
                state.destroyed_at.insert(asset.instance, now);
                info!("{:?} {:?} ({}) destroyed", asset.team, asset.kind, asset.template);
                if let Some((commander, _)) = commanders.iter().find(|(_, t)| **t == asset.team) {
                    let line = match asset.kind {
                        AssetKind::Artillery => RadioCommand::ArtilleryDestroyed,
                        AssetKind::Uav => RadioCommand::UavDestroyed,
                        AssetKind::Radar => RadioCommand::RadarDestroyed,
                    };
                    say(&mut radio, commander, Vec3::ZERO, line, None);
                }
            }
            (true, Some(at)) if now - at >= ASSET_RESPAWN_SECONDS => {
                destroyed.0.remove(&asset.instance);
                health.0.remove(&asset.instance);
                if asset.vehicle {
                    revive(&mut vehicles, asset.instance);
                }
                info!("{:?} {:?} ({}) is back", asset.team, asset.kind, asset.template);
            }
            (false, Some(_)) => {
                state.destroyed_at.remove(&asset.instance);
                state.repaired.remove(&asset.instance);
            }
            _ => {}
        }
    }
    for mut team_assets in &mut team_assets {
        let team = team_assets.team;
        let Some(index) = team_index(team) else { continue };
        let status = Asset::ALL.map(|asset| AssetStatus {
            intact: asset.object().is_none_or(|kind| {
                assets.of(team, kind).any(|a| match a.vehicle {
                    // A piece counts once it stands, and while it isn't a wreck.
                    true => wrecks.get(&a.instance) == Some(&false),
                    false => !destroyed.0.contains(&a.instance),
                })
            }),
            recharge: state.teams[index].recharge[asset.index()].ceil() as u16,
        });
        if team_assets.status != status {
            team_assets.status = status;
        }
    }
}

/// Brings an asset vehicle's wreck back to full hit points (the vehicle code then takes it
/// for a vehicle again).
fn revive(vehicles: &mut Query<(&AssetVehicle, &mut VehicleHealth)>, instance: u32) {
    for (asset, mut health) in vehicles.iter_mut() {
        if asset.instance == instance {
            health.current = health.max;
        }
    }
}

/// Engineers bring destroyed assets back with the wrench, like repairing a wreck.
#[allow(clippy::type_complexity, clippy::too_many_arguments)]
fn repair_assets(
    time: Res<Time>,
    armory: Res<Armory>,
    materials: Option<Res<Materials>>,
    assets: Res<CommanderAssets>,
    engineers: Query<(&SoldierMotion, &Loadout, &Inventory, &AppliedInput, &ControlledBy), Without<Downed>>,
    teams: Query<&Team>,
    mut destroyed: Query<&mut DestroyedStatics>,
    mut vehicles: Query<(&AssetVehicle, &mut VehicleHealth)>,
    mut health: ResMut<ObjectHealth>,
    mut state: ResMut<CommanderState>,
) {
    let Ok(mut destroyed) = destroyed.single_mut() else {
        return;
    };
    let dt = time.delta_secs();
    for (motion, loadout, inventory, input, controlled_by) in &engineers {
        if !input.0.pressed(Buttons::FIRE) {
            continue;
        }
        let Some(wrench) = loadout
            .weapons
            .get(inventory.active as usize)
            .and_then(|w| armory.weapon(w))
            .and_then(|w| w.replenish.as_ref())
            .filter(|r| r.while_firing && r.strength > 0.0)
        else {
            continue;
        };
        let team = teams.get(controlled_by.0).ok().copied();
        for asset in &assets.instances {
            if Some(asset.team) != team
                || !destroyed.0.contains(&asset.instance)
                || Vec3::from_array(asset.placement.position).distance(motion.position) > REPAIR_REACH
            {
                continue;
            }
            let factor = materials.as_ref().map_or(1.0, |m| m.0.damage_mod(wrench.material, ASSET_MATERIAL));
            let progress = state.repaired.entry(asset.instance).or_default();
            *progress += wrench.strength / 100.0 * factor.max(0.1) * dt;
            if *progress >= 1.0 {
                destroyed.0.remove(&asset.instance);
                health.0.remove(&asset.instance);
                if asset.vehicle {
                    revive(&mut vehicles, asset.instance);
                }
                info!("{:?} {:?} ({}) repaired", asset.team, asset.kind, asset.template);
            }
        }
    }
}

/// Artillery pieces on a fire mission turn onto the next shell's launch direction (at their
/// joints' speeds) and fire it when its time comes and the barrel points there: a projectile
/// on the arc that lands where and when the strike planned.
#[allow(clippy::type_complexity)]
fn fire_artillery(
    mut commands: Commands,
    time: Res<Time>,
    mut guns: Query<(Entity, &VehicleData, &mut VehicleState, &Position, &Rotation, &VehicleHealth, &mut FireMission)>,
    mut shots: MessageWriter<ToClients<VehicleShot>>,
) {
    let dt = time.delta_secs();
    for (gun, data, mut state, position, rotation, health, mut mission) in &mut guns {
        let model = &data.0;
        let (Some(weapon), Some(&(land, point)), false) = (model.guns.first(), mission.shells.front(), health.wrecked())
        else {
            // Done, or knocked out: the rest of its shells are never fired.
            commands.entity(gun).remove::<FireMission>();
            continue;
        };
        mission.clock += dt;
        let body = BodyState {
            position: position.0,
            rotation: rotation.0,
            ..default()
        };
        let muzzle = muzzle(model, &body, &state.joints);
        let fire_at = land - mission.flight;
        let due = mission.clock >= fire_at;
        let flight = if due { (land - mission.clock).max(MIN_FLIGHT) } else { mission.flight };
        let velocity = launch_velocity(muzzle.translation, point, flight, shell_gravity(model));
        let direction = velocity.normalize_or(Vec3::Y);
        let aim = [Some(muzzle.translation + direction * 1000.0)];
        let joints = step_joints(model, &body, &aim, &state.joints, &FlightControls::default(), &FlightState::default(), dt).0;
        if state.joints != joints {
            state.joints = joints;
        }
        if !due || (muzzle.rotation * Vec3::NEG_Z).angle_between(direction) > AIM_TOLERANCE {
            continue;
        }
        // A projectile of the gun's own shell, launched at the arc's velocity.
        combat::spawn_projectile(
            &mut commands,
            weapon.clone(),
            gun,
            mission.player,
            Some(gun),
            muzzle.translation,
            direction,
            velocity - direction * weapon.projectile.velocity,
            None,
        );
        shots.write(ToClients {
            targets: SendTargets::All,
            message: VehicleShot {
                vehicle: gun,
                gun: 0,
                origin: muzzle.translation,
                direction,
            },
        });
        debug!("artillery {gun} fired, lands at {point:.0} in {flight:.1} s");
        mission.shells.pop_front();
    }
}

/// Strikes end once their shells are down; older imports' shells land on schedule, each a
/// blast like the guns' own shells, with its effect.
fn run_strikes(
    mut commands: Commands,
    time: Res<Time>,
    assets: Res<CommanderAssets>,
    mut strikes: Query<(Entity, &mut Strike)>,
    mut explosions: MessageWriter<Explosion>,
    mut effects: MessageWriter<ToClients<PlayEffect>>,
) {
    let dt = time.delta_secs();
    let desc = &assets.desc.artillery;
    for (entity, mut strike) in &mut strikes {
        strike.left -= dt;
        let attacker = strike.attacker.clone();
        strike.shells.retain_mut(|(left, at)| {
            *left -= dt;
            if *left > 0.0 {
                return true;
            }
            explosions.write(Explosion {
                position: *at + Vec3::Y * 0.3,
                damage: desc.damage,
                radius: desc.radius,
                material: desc.material,
                attacker: attacker.clone(),
                cone: None,
            });
            if let Some(effect) = &desc.effect {
                effects.write(ToClients {
                    targets: SendTargets::All,
                    message: PlayEffect {
                        material: desc.material,
                        ..PlayEffect::new(effect.clone(), *at)
                    },
                });
            }
            false
        });
        if strike.shells.is_empty() && strike.left <= 0.0 {
            commands.entity(entity).despawn();
        }
    }
}

impl UavFlight {
    /// Where it is on its circle and which way it faces: along the circle with its middle to
    /// the right, banked into the turn (right wing down).
    fn pose(&self) -> (Vec3, Quat) {
        let (sin, cos) = self.angle.sin_cos();
        let position = self.center + Vec3::new(cos, 0.0, sin) * self.radius;
        let tangent = Vec3::new(-sin, 0.0, cos);
        let bank = (self.speed * self.speed / (self.radius * GRAVITY)).atan();
        let rotation = Transform::IDENTITY.looking_to(tangent, Vec3::Y).rotation * Quat::from_rotation_z(-bank);
        (position, rotation)
    }
}

/// UAVs fly their circle (kinematic bodies moved by their velocity); shot down, they fall.
#[allow(clippy::type_complexity)]
fn fly_uav_vehicles(
    mut commands: Commands,
    time: Res<Time>,
    mut uavs: Query<(Entity, &mut UavFlight, &Position, &mut Rotation, &mut LinearVelocity, &mut AngularVelocity, &VehicleHealth)>,
) {
    let dt = time.delta_secs();
    if dt <= 0.0 {
        return;
    }
    for (entity, mut flight, position, mut rotation, mut velocity, mut spin, health) in &mut uavs {
        if health.wrecked() {
            info!("UAV shot down at {:.0}", position.0);
            spin.0 = Vec3::new(fastrand::f32() - 0.5, fastrand::f32() - 0.5, 1.5);
            commands
                .entity(entity)
                .remove::<UavFlight>()
                .insert((RigidBody::Dynamic, GravityScale(1.0), LinearDamping(0.1), AngularDamping(0.5)));
            continue;
        }
        flight.angle = (flight.angle + flight.speed / flight.radius * dt) % std::f32::consts::TAU;
        let (next, facing) = flight.pose();
        velocity.0 = (next - position.0) / dt;
        spin.0 = Vec3::ZERO;
        rotation.0 = facing;
    }
}

/// UAVs keep marking the enemies below them, until their time is up (they leave) or they
/// are shot down.
#[allow(clippy::type_complexity)]
fn run_uavs(
    mut commands: Commands,
    time: Res<Time>,
    mut uavs: Query<(Entity, &AssetEffect, &mut Uav)>,
    soldiers: Query<(Entity, &SoldierMotion, &ControlledBy, Option<&Seated>), With<Soldier>>,
    vehicles: Query<(&VehicleMotion, Option<&VehicleHealth>)>,
    teams: Query<&Team>,
    mut spots: MessageWriter<Spot>,
) {
    let dt = time.delta_secs();
    for (entity, effect, mut uav) in &mut uavs {
        uav.left -= dt;
        let shot_down = uav.vehicle.is_some_and(|v| vehicles.get(v).map_or(true, |(_, h)| h.is_some_and(|h| h.wrecked())));
        if shot_down {
            info!("{:?} UAV lost", effect.team);
            commands.entity(entity).despawn();
            continue;
        }
        if uav.left <= 0.0 {
            if let Some(vehicle) = uav.vehicle {
                commands.entity(vehicle).despawn();
            }
            commands.entity(entity).despawn();
            continue;
        }
        uav.next_sweep -= dt;
        if uav.next_sweep > 0.0 {
            continue;
        }
        uav.next_sweep = 1.0;
        let mut seen = 0;
        for (soldier, motion, controlled_by, seated) in &soldiers {
            let enemy = teams.get(controlled_by.0).is_ok_and(|t| *t != effect.team && *t != Team::Spectator);
            let (target, position) = match seated.and_then(|s| vehicles.get(s.vehicle).ok().map(|(v, _)| (s.vehicle, v.position))) {
                Some(vehicle) => vehicle,
                None => (soldier, motion.position),
            };
            if enemy && position.xz().distance(effect.position.xz()) <= effect.radius {
                seen += 1;
                spots.write(Spot {
                    target,
                    team: effect.team,
                    seconds: UAV_MARK,
                });
            }
        }
        debug!("{:?} UAV sees {seen} enemies", effect.team);
    }
}

/// Satellite scans show the commander every enemy on his map, once a second while they last
/// (BF2: "your map will show the position of every enemy"); he spots them for his team. An
/// AI commander has no map to look at: what its scan shows is spotted for its team, as a
/// commander would.
#[allow(clippy::too_many_arguments, clippy::type_complexity)]
fn run_scans(
    mut commands: Commands,
    time: Res<Time>,
    mut scans: Query<(Entity, &mut Scan)>,
    soldiers: Query<(Entity, &SoldierMotion, &ControlledBy, Option<&Seated>), (With<Soldier>, Without<Downed>)>,
    vehicles: Query<&VehicleMotion>,
    players: Query<(&Player, &Team)>,
    clients: Query<&PlayerClient>,
    host: Option<Res<HostPlayer>>,
    mut reports: MessageWriter<ToClients<ScanReport>>,
    mut spots: MessageWriter<Spot>,
) {
    let dt = time.delta_secs();
    for (entity, mut scan) in &mut scans {
        scan.left -= dt;
        scan.next_report -= dt;
        if scan.left <= 0.0 {
            commands.entity(entity).despawn();
            continue;
        }
        if scan.next_report > 0.0 {
            continue;
        }
        scan.next_report = 1.0;
        let bot = players.get(scan.commander).is_ok_and(|(p, _)| p.is_bot);
        let mut contacts = Vec::new();
        for (soldier, motion, controlled_by, seated) in &soldiers {
            if !players.get(controlled_by.0).is_ok_and(|(_, t)| *t != scan.team && *t != Team::Spectator) {
                continue;
            }
            let (body, position) = match seated.and_then(|s| vehicles.get(s.vehicle).ok().map(|v| (s.vehicle, v.position))) {
                Some(vehicle) => vehicle,
                None => (soldier, motion.position),
            };
            if bot {
                spots.write(Spot {
                    target: body,
                    team: scan.team,
                    seconds: 1.5,
                });
            }
            contacts.push(ScanContact {
                position,
                vehicle: seated.is_some(),
            });
        }
        debug!("{:?} scan: {} enemies", scan.team, contacts.len());
        if let Some(client) = player_client(scan.commander, &clients, host.as_deref()) {
            reports.write(ToClients {
                targets: SendTargets::Single(client),
                message: ScanReport { contacts },
            });
        }
    }
}

/// Supply crates float down, then heal, resupply and repair the team around them (soldiers
/// and vehicles, BF2's `workOnVehicles`) until their stock or their time runs out.
#[allow(clippy::too_many_arguments, clippy::type_complexity)]
fn run_crates(
    mut commands: Commands,
    time: Res<Time>,
    armory: Res<Armory>,
    assets: Res<CommanderAssets>,
    spatial: SpatialQuery,
    mut crates: Query<(Entity, &mut AssetEffect, &mut SupplyCrate)>,
    mut soldiers: Query<(&SoldierMotion, &ControlledBy, &mut Health, &Loadout, &mut Inventory), (With<Soldier>, Without<Downed>)>,
    mut vehicles: Query<(Entity, &VehicleMotion, &mut VehicleHealth, Option<&AssetVehicle>)>,
    crews: Query<(&Seated, &ControlledBy)>,
    teams: Query<&Team>,
) {
    let dt = time.delta_secs();
    let desc = &assets.desc.supply;
    for (entity, mut effect, mut supply) in &mut crates {
        supply.left -= dt;
        if supply.left <= 0.0 || supply.stock <= 0.0 {
            commands.entity(entity).despawn();
            continue;
        }
        if supply.falling {
            let step = PARACHUTE_SPEED * dt;
            let filter = SpatialQueryFilter::from_mask(GameLayer::World);
            match spatial.cast_ray(effect.position, Dir3::NEG_Y, step, true, &filter) {
                Some(hit) => {
                    effect.position.y -= hit.distance;
                    supply.falling = false;
                    info!("{:?} supply crate landed at {:.0}", effect.team, effect.position);
                }
                None => effect.position.y -= step,
            }
            continue;
        }
        supply.next_round -= dt;
        if supply.next_round > 0.0 {
            continue;
        }
        supply.next_round = 1.0;
        for (motion, controlled_by, mut health, loadout, mut inventory) in &mut soldiers {
            if teams.get(controlled_by.0).ok().copied() != Some(effect.team)
                || motion.position.distance(effect.position) > desc.radius
            {
                continue;
            }
            if health.current < health.max {
                let before = health.current;
                health.current = (health.current + desc.heal / 100.0 * health.max).min(health.max);
                supply.stock -= (health.current - before) / health.max * 100.0;
            }
            let full = Inventory::full(loadout, &armory);
            let mut given = 0.0;
            for (ammo, full) in inventory.ammo.iter_mut().zip(&full.ammo) {
                let total = (full[0] + full[1]) as f32;
                if total <= 0.0 || ammo[1] >= full[1] {
                    continue;
                }
                let add = (desc.ammo / 100.0 * total).ceil() as u16;
                let spare = (ammo[1] + add).min(full[1]);
                given += (spare - ammo[1]) as f32 / total * 100.0;
                ammo[1] = spare;
            }
            if given > 0.0 {
                supply.stock -= given;
            }
        }
        // The team's vehicles around (crewed by it, empty, or its assets) are repaired like
        // soldiers are healed.
        for (vehicle, motion, mut health, asset) in &mut vehicles {
            if health.wrecked() || health.current >= health.max || motion.position.distance(effect.position) > desc.radius + 2.0 {
                continue;
            }
            let crew_team = crews
                .iter()
                .find(|(seated, _)| seated.vehicle == vehicle)
                .and_then(|(_, crew)| teams.get(crew.0).ok().copied());
            let team = asset.map(|a| a.team).or(crew_team);
            if team.is_some_and(|t| t != effect.team) {
                continue;
            }
            let before = health.current;
            health.current = (health.current + desc.heal / 100.0 * health.max).min(health.max);
            supply.stock -= (health.current - before) / health.max * 100.0;
        }
    }
}

/// Commanders are in no squad (a squad joined or a bot squad filled up meanwhile), and a
/// commander switching teams gives up the post.
fn leave_squads(
    mut commands: Commands,
    in_squad: Query<Entity, (With<Commander>, With<SquadMember>)>,
    switched: Query<Entity, (With<Commander>, Changed<Team>)>,
) {
    for player in &in_squad {
        commands.entity(player).remove::<SquadMember>();
    }
    for player in &switched {
        commands.entity(player).remove::<Commander>();
    }
}

/// Orders of squads that no longer exist go.
fn tidy_orders(
    mut commands: Commands,
    orders: Query<(Entity, &SquadOrder)>,
    members: Query<(&Team, &SquadMember)>,
) {
    for (entity, order) in &orders {
        if !members.iter().any(|(team, member)| *team == order.team && member.squad == order.squad) {
            commands.entity(entity).despawn();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn shells_land_where_and_when_planned() {
        // The launch velocity's arc through the projectile's own integration (velocity
        // averaged over each tick) reaches the target at the planned time.
        let (from, to, flight, gravity) = (Vec3::new(0.0, 2.0, 0.0), Vec3::new(300.0, -10.0, -400.0), 3.2, GRAVITY * 5.0);
        let mut velocity = launch_velocity(from, to, flight, gravity);
        let mut position = from;
        let dt = 1.0 / 60.0;
        let steps = (flight / dt).round() as usize;
        for _ in 0..steps {
            let start = velocity;
            velocity += Vec3::NEG_Y * gravity * dt;
            position += (start + velocity) * 0.5 * dt;
        }
        assert!(position.distance(to) < 0.5, "{position:?}");
    }

    #[test]
    fn uav_flies_round_its_center() {
        let flight = UavFlight {
            center: Vec3::new(10.0, 200.0, -20.0),
            angle: 1.0,
            radius: 60.0,
            speed: 30.0,
        };
        let (position, rotation) = flight.pose();
        assert!((position.distance(flight.center) - 60.0).abs() < 1e-3);
        // Facing along the circle, banked towards its middle.
        let forward = rotation * Vec3::NEG_Z;
        assert!(forward.dot(position - flight.center).abs() < 1e-3);
        let right = rotation * Vec3::X;
        assert!(right.dot(flight.center - position) > 0.0, "the middle is to the right");
        assert!(right.y < 0.0, "banked right");
    }
}

/// The request handler's rules for requests that arrive together (one packet, or a client and
/// an AI commander in the same frame) and for bad targets.
#[cfg(test)]
mod request_tests {
    use super::*;

    fn app() -> App {
        let mut app = App::new();
        app.add_message::<CommanderCommand>()
            .add_message::<ToClients<RadioMessage>>()
            .add_message::<Spot>()
            .insert_resource(game_shared::level::test_range())
            .init_resource::<CommanderAssets>()
            .init_resource::<CommanderState>()
            .add_systems(Update, handle_commands);
        app
    }

    fn player(app: &mut App, name: &str, team: Team) -> Entity {
        app.world_mut()
            .spawn((
                Player {
                    name: name.into(),
                    is_bot: false,
                },
                team,
            ))
            .id()
    }

    fn send(app: &mut App, player: Entity, request: CommanderRequest) {
        app.world_mut().write_message(CommanderCommand { player, request });
    }

    fn count<C: Component>(app: &mut App) -> usize {
        let world = app.world_mut();
        world.query_filtered::<(), With<C>>().iter(world).count()
    }

    fn supply(target: Vec3) -> CommanderRequest {
        CommanderRequest::Use {
            asset: Asset::Supply,
            target,
        }
    }

    #[test]
    fn a_double_use_in_one_frame_gives_one_strike() {
        let mut app = app();
        let commander = player(&mut app, "Commander", Team::One);
        app.world_mut().entity_mut(commander).insert(Commander);
        for _ in 0..3 {
            send(&mut app, commander, supply(Vec3::new(5.0, 0.0, 5.0)));
        }
        app.update();
        assert_eq!(count::<SupplyCrate>(&mut app), 1);
        // Recharging now: the next frame's request is refused too.
        send(&mut app, commander, supply(Vec3::new(5.0, 0.0, 5.0)));
        app.update();
        assert_eq!(count::<SupplyCrate>(&mut app), 1);
    }

    #[test]
    fn two_applications_in_one_frame_give_one_commander() {
        let mut app = app();
        let (a, b) = (player(&mut app, "A", Team::One), player(&mut app, "B", Team::One));
        let other_team = player(&mut app, "C", Team::Two);
        send(&mut app, a, CommanderRequest::Apply);
        send(&mut app, b, CommanderRequest::Apply);
        send(&mut app, other_team, CommanderRequest::Apply);
        app.update();
        assert!(app.world().entity(a).contains::<Commander>());
        assert!(!app.world().entity(b).contains::<Commander>());
        assert!(app.world().entity(other_team).contains::<Commander>(), "one per team");
    }

    #[test]
    fn two_orders_for_a_squad_in_one_frame_give_one_order() {
        let mut app = app();
        let commander = player(&mut app, "Commander", Team::One);
        app.world_mut().entity_mut(commander).insert(Commander);
        let member = player(&mut app, "Member", Team::One);
        app.world_mut().entity_mut(member).insert(SquadMember { squad: 1, leader: true });
        for x in [10.0, 20.0] {
            send(
                &mut app,
                commander,
                CommanderRequest::Order {
                    squad: 1,
                    kind: OrderKind::Attack,
                    target: Vec3::new(x, 0.0, 0.0),
                },
            );
        }
        app.update();
        let world = app.world_mut();
        let orders: Vec<SquadOrder> = world.query::<&SquadOrder>().iter(world).copied().collect();
        assert_eq!(orders.len(), 1);
        assert_eq!(orders[0].position.x, 20.0, "the last order counts");
        // Cancelled and given again in one frame: still one.
        send(&mut app, commander, CommanderRequest::CancelOrder { squad: 1 });
        send(
            &mut app,
            commander,
            CommanderRequest::Order {
                squad: 1,
                kind: OrderKind::Defend,
                target: Vec3::ZERO,
            },
        );
        app.update();
        assert_eq!(count::<SquadOrder>(&mut app), 1);
    }

    #[test]
    fn targets_must_be_finite_and_on_the_map() {
        let mut app = app();
        let commander = player(&mut app, "Commander", Team::One);
        app.world_mut().entity_mut(commander).insert(Commander);
        send(&mut app, commander, supply(Vec3::new(f32::NAN, 0.0, 0.0)));
        send(&mut app, commander, supply(Vec3::new(0.0, 0.0, f32::INFINITY)));
        app.update();
        assert_eq!(count::<SupplyCrate>(&mut app), 0, "refused");
        // And refusing it didn't use the asset up.
        send(&mut app, commander, supply(Vec3::new(1e9, 0.0, -1e9)));
        app.update();
        let world = app.world_mut();
        let effects: Vec<AssetEffect> = world.query::<&AssetEffect>().iter(world).copied().collect();
        assert_eq!(effects.len(), 1);
        let level = app.world().resource::<LoadedLevel>();
        let heightmap = level.heightmap.as_ref().unwrap();
        let (min, size) = (heightmap.origin, heightmap.world_size());
        let at = effects[0].position;
        assert!(at.is_finite());
        assert!((min.x..=min.x + size).contains(&at.x) && (min.z..=min.z + size).contains(&at.z), "{at}");
    }
}
