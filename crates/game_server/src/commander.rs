//! The server side of the commander (see `game_shared::commander`): the post, squad orders,
//! and the assets: artillery barrages, UAV sweeps, satellite scans and supply drops, their
//! recharge, their level objects being destroyed, repaired by engineers or coming back.
//!
//! Requests from clients and from an AI commander go through [`CommanderCommand`]: a bot
//! strategy can apply for the post and give orders by writing those with its bot player.

use std::collections::{HashMap, HashSet};

use avian3d::prelude::*;
use bevy::prelude::*;
use bevy_replicon::prelude::*;
use game_shared::{
    commander::{
        ARTILLERY_DELAY, ASSET_RESPAWN_SECONDS, Asset, AssetEffect, AssetStatus, Commander, CommanderAssets,
        CommanderRequest, MUTINY_SHARE, OrderKind, SCAN_SECONDS, SquadOrder, TeamAssets, UAV_SECONDS,
    },
    effects::PlayEffect,
    input::Buttons,
    level::{LevelEntity, LoadedLevel},
    physics::GameLayer,
    protocol::{ControlledBy, Player, Team},
    radio::{RadioCommand, RadioMessage},
    revive::Downed,
    soldier::{Health, Soldier, SoldierMotion},
    squad::SquadMember,
    statics::DestroyedStatics,
    vehicle::{Seated, VehicleMotion},
    weapons::{Armory, Inventory, Loadout},
};

use crate::{
    AppliedInput, ClientPlayer, Controls, HostPlayer,
    combat::{Attacker, Explosion},
    destruction::{Materials, ObjectHealth},
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
                    handle_commands,
                    track_assets,
                    repair_assets,
                    run_strikes,
                    run_uavs,
                    run_crates,
                    tidy_orders,
                    leave_squads,
                )
                    .chain()
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

/// Artillery shells still to land: seconds until each, and where.
#[derive(Component)]
struct Strike {
    shells: Vec<(f32, Vec3)>,
    attacker: Attacker,
}

#[derive(Component)]
struct Uav {
    left: f32,
    next_sweep: f32,
}

#[derive(Component)]
struct SupplyCrate {
    falling: bool,
    stock: f32,
    left: f32,
    /// It hands out once a second.
    next_round: f32,
}

fn team_index(team: Team) -> Option<usize> {
    match team {
        Team::One => Some(0),
        Team::Two => Some(1),
        Team::Spectator => None,
    }
}

fn receive_requests(
    mut requests: MessageReader<FromClient<CommanderRequest>>,
    clients: Query<&ClientPlayer>,
    host: Option<Res<HostPlayer>>,
    mut commands: MessageWriter<CommanderCommand>,
) {
    for request in requests.read() {
        if let Some(player) = sender_player(request.client_id, &clients, host.as_deref()) {
            commands.write(CommanderCommand {
                player,
                request: request.message,
            });
        }
    }
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
    team_assets: Query<&TeamAssets>,
    destroyed: Query<&DestroyedStatics>,
    mut state: ResMut<CommanderState>,
    mut radio: MessageWriter<ToClients<RadioMessage>>,
    mut spots: MessageWriter<Spot>,
) {
    for command in requests.read() {
        let player = command.player;
        let Ok((_, info, &team, _, is_commander, controls)) = players.get(player) else {
            continue;
        };
        let Some(team_state) = state.team(team) else {
            continue;
        };
        let position = controls.and_then(|c| motions.get(c.0).ok()).map_or(Vec3::ZERO, |m| m.position);
        let commander = players.iter().find(|p| *p.2 == team && p.4).map(|p| p.0);
        match command.request {
            CommanderRequest::Apply if commander.is_none() => {
                // Like BF2, the commander leads no squad.
                commands.entity(player).insert(Commander).remove::<SquadMember>();
                team_state.mutiny.clear();
                info!("{} is now {team:?}'s commander", info.name);
                say(&mut radio, player, position, RadioCommand::NewCommander, None);
            }
            CommanderRequest::Resign if is_commander => {
                commands.entity(player).remove::<Commander>();
                info!("{} resigned as commander", info.name);
                say(&mut radio, player, position, RadioCommand::CommanderResigned, None);
            }
            CommanderRequest::Mutiny if commander.is_some_and(|c| c != player) => {
                team_state.mutiny.insert(player);
                let voters = players.iter().filter(|p| *p.2 == team && !p.4).count().max(1);
                let votes = team_state.mutiny.len();
                info!("mutiny against {team:?}'s commander: {votes}/{voters}");
                if votes as f32 / voters as f32 > MUTINY_SHARE
                    && let Some(commander) = commander
                {
                    commands.entity(commander).remove::<Commander>();
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
                match orders.iter().find(|(_, o)| o.team == team && o.squad == squad) {
                    Some((entity, _)) => {
                        commands.entity(entity).insert(order);
                    }
                    None => {
                        commands.spawn((order, Replicated, LevelEntity));
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
            CommanderRequest::CancelOrder { squad } if is_commander => {
                for (entity, order) in &orders {
                    if order.team == team && order.squad == squad {
                        commands.entity(entity).despawn();
                    }
                }
            }
            CommanderRequest::Use { asset, target } if is_commander => {
                let ready = team_assets.iter().find(|t| t.team == team).is_some_and(|t| t.get(asset).ready());
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
                        let destroyed = destroyed.single().ok();
                        let guns = assets
                            .of(team, game_data::AssetKind::Artillery)
                            .filter(|g| !destroyed.is_some_and(|d| d.0.contains(&g.instance)))
                            .count()
                            .max(1);
                        let desc = &assets.desc.artillery;
                        let mut shells = Vec::new();
                        for gun in 0..guns {
                            for shell in 0..desc.shells {
                                let angle = fastrand::f32() * std::f32::consts::TAU;
                                let offset = Vec2::from_angle(angle) * desc.spread * fastrand::f32().sqrt();
                                let at = ground(&level, target + Vec3::new(offset.x, 0.0, offset.y));
                                let when = ARTILLERY_DELAY + shell as f32 * desc.interval + gun as f32 * 0.4;
                                shells.push((when, at));
                            }
                        }
                        commands.spawn((
                            AssetEffect {
                                team,
                                asset,
                                position: target,
                                radius: desc.spread + desc.radius,
                            },
                            Strike { shells, attacker },
                            Replicated,
                            LevelEntity,
                        ));
                        RadioCommand::ArtilleryStrike
                    }
                    Asset::Uav => {
                        commands.spawn((
                            AssetEffect {
                                team,
                                asset,
                                position: target,
                                radius: assets.desc.uav.radius,
                            },
                            Uav {
                                left: UAV_SECONDS,
                                next_sweep: 0.0,
                            },
                            Replicated,
                            LevelEntity,
                        ));
                        RadioCommand::UavOnline
                    }
                    Asset::Scan => {
                        let enemy = |c: &ControlledBy| players.get(c.0).is_ok_and(|p| *p.2 != team && *p.2 != Team::Spectator);
                        for (soldier, controlled_by, seated) in &enemies {
                            if enemy(controlled_by) {
                                let target = seated.map_or(soldier, |s| s.vehicle);
                                spots.write(Spot {
                                    target,
                                    team,
                                    seconds: SCAN_SECONDS,
                                });
                            }
                        }
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
/// teams see of it.
#[allow(clippy::too_many_arguments)]
fn track_assets(
    time: Res<Time>,
    assets: Res<CommanderAssets>,
    commanders: Query<(Entity, &Team), With<Commander>>,
    mut destroyed: Query<&mut DestroyedStatics>,
    mut health: ResMut<ObjectHealth>,
    mut state: ResMut<CommanderState>,
    mut team_assets: Query<&mut TeamAssets>,
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
    for asset in &assets.instances {
        let down = destroyed.0.contains(&asset.instance);
        match (down, state.destroyed_at.get(&asset.instance).copied()) {
            (true, None) => {
                state.destroyed_at.insert(asset.instance, now);
                info!("{:?} {:?} ({}) destroyed", asset.team, asset.kind, asset.template);
                if let Some((commander, _)) = commanders.iter().find(|(_, t)| **t == asset.team) {
                    let line = match asset.kind {
                        game_data::AssetKind::Artillery => RadioCommand::ArtilleryDestroyed,
                        game_data::AssetKind::Uav => RadioCommand::UavDestroyed,
                        game_data::AssetKind::Radar => RadioCommand::RadarDestroyed,
                    };
                    say(&mut radio, commander, Vec3::ZERO, line, None);
                }
            }
            (true, Some(at)) if now - at >= ASSET_RESPAWN_SECONDS => {
                destroyed.0.remove(&asset.instance);
                health.0.remove(&asset.instance);
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
        let Some(index) = team_index(team_assets.team) else { continue };
        let status = Asset::ALL.map(|asset| AssetStatus {
            intact: asset.object().is_none_or(|kind| {
                assets
                    .of(team_assets.team, kind)
                    .any(|a| !destroyed.0.contains(&a.instance))
            }),
            recharge: state.teams[index].recharge[asset.index()].ceil() as u16,
        });
        if team_assets.status != status {
            team_assets.status = status;
        }
    }
}

/// Engineers bring destroyed assets back with the wrench, like repairing a wreck.
#[allow(clippy::type_complexity)]
fn repair_assets(
    time: Res<Time>,
    armory: Res<Armory>,
    materials: Option<Res<Materials>>,
    assets: Res<CommanderAssets>,
    engineers: Query<(&SoldierMotion, &Loadout, &Inventory, &AppliedInput, &ControlledBy), Without<Downed>>,
    teams: Query<&Team>,
    mut destroyed: Query<&mut DestroyedStatics>,
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
                info!("{:?} {:?} ({}) repaired", asset.team, asset.kind, asset.template);
            }
        }
    }
}

/// Artillery shells landing: each a blast like the guns' own shells, and its effect.
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
        if strike.shells.is_empty() {
            commands.entity(entity).despawn();
        }
    }
}

/// UAVs keep marking the enemies below them.
#[allow(clippy::type_complexity)]
fn run_uavs(
    mut commands: Commands,
    time: Res<Time>,
    mut uavs: Query<(Entity, &AssetEffect, &mut Uav)>,
    soldiers: Query<(Entity, &SoldierMotion, &ControlledBy, Option<&Seated>), With<Soldier>>,
    vehicles: Query<&VehicleMotion>,
    teams: Query<&Team>,
    mut spots: MessageWriter<Spot>,
) {
    let dt = time.delta_secs();
    for (entity, effect, mut uav) in &mut uavs {
        uav.left -= dt;
        if uav.left <= 0.0 {
            commands.entity(entity).despawn();
            continue;
        }
        uav.next_sweep -= dt;
        if uav.next_sweep > 0.0 {
            continue;
        }
        uav.next_sweep = 1.0;
        for (soldier, motion, controlled_by, seated) in &soldiers {
            let enemy = teams.get(controlled_by.0).is_ok_and(|t| *t != effect.team && *t != Team::Spectator);
            let (target, position) = match seated.and_then(|s| vehicles.get(s.vehicle).ok().map(|v| (s.vehicle, v.position))) {
                Some(vehicle) => vehicle,
                None => (soldier, motion.position),
            };
            if enemy && position.xz().distance(effect.position.xz()) <= effect.radius {
                spots.write(Spot {
                    target,
                    team: effect.team,
                    seconds: UAV_MARK,
                });
            }
        }
    }
}

/// Supply crates float down, then heal and resupply the team around them until their stock
/// or their time runs out.
#[allow(clippy::too_many_arguments, clippy::type_complexity)]
fn run_crates(
    mut commands: Commands,
    time: Res<Time>,
    armory: Res<Armory>,
    assets: Res<CommanderAssets>,
    spatial: SpatialQuery,
    mut crates: Query<(Entity, &mut AssetEffect, &mut SupplyCrate)>,
    mut soldiers: Query<(&SoldierMotion, &ControlledBy, &mut Health, &Loadout, &mut Inventory), (With<Soldier>, Without<Downed>)>,
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
