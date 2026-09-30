//! The AI commander. While no human holds a team's commander post, one of its bots takes it
//! (a human who applies takes it over at once, see `commander::handle_commands`). It passes
//! the team's plan ([`super::strategy`]) on as commander orders, which the squads' humans see, and calls in
//! the team's assets (`game_shared::commander`): artillery on enemies gathered where the
//! team has seen them, the UAV over the fight for the flag that matters most, a satellite
//! scan while the team is fighting, and supply drops for hurt squads. Everything goes
//! through [`CommanderCommand`]s, under the same rules as a human commander.

use bevy::{platform::collections::HashMap, prelude::*};
use game_shared::{
    commander::{
        Asset, Commander, CommanderAssets, CommanderRequest, OrderKind as CommanderOrderKind, SquadOrder as CommanderOrder,
        TeamAssets,
    },
    conquest::team_index,
    protocol::{Player, Team},
    squad::SquadMember,
};

use super::{
    squad::SquadSnapshot,
    stats::AiStats,
    strategy::{OrderKind, Posture, StrategicMap, Strategy, TeamIntel},
};
use crate::{bots::BotBrain, commander::CommanderCommand};

/// Seconds between the commander's looks at the battle.
const INTERVAL: f32 = 2.0;
/// Seconds between orders to the same squad, unless the order changes.
const REPEAT_ORDER: f32 = 30.0;
/// Enemies seen within this many seconds count for targeting.
const FRESH: f32 = 8.0;
/// Artillery needs at least this many enemies under it.
const ARTILLERY_MIN_ENEMIES: usize = 4;
/// Keep this far (beyond the barrage) from friends, meters.
const ARTILLERY_SAFETY: f32 = 12.0;
/// A squad this hurt on average gets a supply drop.
const SUPPLY_HEALTH: f32 = 0.6;

/// What the AI commanders passed on and when.
#[derive(Resource, Default)]
pub struct AiCommander {
    /// By squad: the order sent and when.
    sent: HashMap<(Team, u8), (OrderKind, usize, Option<Vec3>, f32)>,
    /// Who held each team's post at the last look: a new commander (a bot again after a
    /// player, or another bot) passes the whole plan on afresh.
    holders: [Option<Entity>; 2],
    timer: f32,
}

/// The AI commanders' turn: fill vacant posts, pass orders on, call in assets.
#[allow(clippy::too_many_arguments, clippy::type_complexity)]
pub fn command(
    time: Res<Time>,
    mut state: ResMut<AiCommander>,
    map: Res<StrategicMap>,
    strategy: Res<Strategy>,
    intel: Res<TeamIntel>,
    snapshot: Res<SquadSnapshot>,
    assets: Res<CommanderAssets>,
    players: Query<(Entity, &Team, &Player, Has<Commander>, Option<&SquadMember>, Has<BotBrain>)>,
    team_assets: Query<&TeamAssets>,
    orders: Query<&CommanderOrder>,
    mut stats: ResMut<AiStats>,
    mut commands: MessageWriter<CommanderCommand>,
) {
    let now = time.elapsed_secs();
    state.timer -= time.delta_secs();
    if state.timer > 0.0 || map.areas.is_empty() {
        return;
    }
    state.timer = INTERVAL;

    for team in [Team::One, Team::Two] {
        let t = team_index(team).unwrap();
        let commander = players.iter().find(|p| *p.1 == team && p.3);
        let holder = commander.map(|p| p.0);
        if state.holders[t] != holder {
            state.holders[t] = holder;
            state.sent.retain(|(order_team, _), _| *order_team != team);
        }
        let Some((bot, ..)) = commander else {
            // A vacant post (at the start, or a player left it): a bot takes it, preferably
            // one that leads no squad. A player who applies takes it back at once
            // (`commander::handle_commands`); while one commands, this stays out of it.
            let candidate = players
                .iter()
                .filter(|p| *p.1 == team && p.5)
                .min_by_key(|p| (p.4.is_some_and(|m| m.leader), p.0));
            if let Some((bot, _, player, ..)) = candidate {
                info!("ai commander: {} applies for {team:?}'s commander post", player.name);
                commands.write(CommanderCommand {
                    player: bot,
                    request: CommanderRequest::Apply,
                });
            }
            continue;
        };
        if !commander.is_some_and(|p| p.5) {
            continue;
        }
        let mut write = |request: CommanderRequest| {
            commands.write(CommanderCommand { player: bot, request });
        };

        // The plan, as orders the squads see.
        for (&(order_team, squad), order) in &strategy.orders {
            if order_team != team || order.area >= map.areas.len() {
                continue;
            }
            let shown = orders.iter().any(|o| o.team == team && o.squad == squad);
            let same = state
                .sent
                .get(&(team, squad))
                .is_some_and(|&(kind, area, point, at)| {
                    kind == order.kind && area == order.area && point == order.point && (shown || now - at < REPEAT_ORDER)
                });
            if same {
                continue;
            }
            let target = order.point.unwrap_or(map.areas[order.area].order_position);
            let kind = match order.kind {
                OrderKind::Attack => CommanderOrderKind::Attack,
                OrderKind::Defend => CommanderOrderKind::Defend,
            };
            write(CommanderRequest::Order { squad, kind, target });
            state.sent.insert((team, squad), (order.kind, order.area, order.point, now));
            stats.teams[t].orders += 1;
        }
        state.sent.retain(|(order_team, squad), _| *order_team != team || strategy.orders.contains_key(&(team, *squad)));

        let Some(status) = team_assets.iter().find(|a| a.team == team) else {
            continue;
        };
        let enemies = intel.recent(team, FRESH);
        let friends: Vec<Vec3> = snapshot.soldiers[t].iter().map(|s| s.position).collect();

        if status.get(Asset::Artillery).ready() {
            let desc = &assets.desc.artillery;
            let reach = desc.spread + desc.radius * 0.5;
            let clear = desc.spread + desc.radius + ARTILLERY_SAFETY;
            let target = enemies
                .iter()
                .map(|&p| (p, enemies.iter().filter(|q| q.xz().distance(p.xz()) < reach).count()))
                .filter(|&(p, count)| {
                    count >= ARTILLERY_MIN_ENEMIES && friends.iter().all(|f| f.xz().distance(p.xz()) > clear)
                })
                .max_by_key(|&(_, count)| count);
            if let Some((target, count)) = target {
                info!("ai commander: {team:?} artillery on {count} enemies at {target:.0}");
                write(CommanderRequest::Use { asset: Asset::Artillery, target });
                stats.teams[t].artillery += 1;
            }
        }

        if status.get(Asset::Uav).ready() && !enemies.is_empty() {
            // Over the flag the team fights for most, else over the most enemies seen.
            let objective = strategy.objectives[t]
                .iter()
                .find(|o| strategy.posture[t] == Posture::Guard || o.kind == OrderKind::Attack)
                .map(|o| map.areas[o.area].position);
            let busiest = enemies
                .iter()
                .max_by_key(|&&p| enemies.iter().filter(|q| q.distance(p) < assets.desc.uav.radius).count())
                .copied();
            if let Some(target) = objective.or(busiest) {
                write(CommanderRequest::Use { asset: Asset::Uav, target });
                stats.teams[t].uavs += 1;
            }
        }

        if status.get(Asset::Scan).ready() && enemies.len() >= 3 {
            write(CommanderRequest::Use { asset: Asset::Scan, target: Vec3::ZERO });
            stats.teams[t].scans += 1;
        }

        if status.get(Asset::Supply).ready() {
            let hurt = snapshot
                .squads
                .iter()
                .filter(|((squad_team, _), info)| *squad_team == team && info.alive.len() >= 2)
                .map(|(_, info)| {
                    let health = info.alive.iter().map(|s| s.health).sum::<f32>() / info.alive.len() as f32;
                    (info, health)
                })
                .filter(|(_, health)| *health < SUPPLY_HEALTH)
                .min_by(|a, b| a.1.total_cmp(&b.1));
            if let Some(target) = hurt.and_then(|(info, _)| info.centroid()) {
                // A little aside, so the crate doesn't land on anyone's head.
                let target = target + Vec3::new(3.0, 0.0, 3.0);
                write(CommanderRequest::Use { asset: Asset::Supply, target });
                stats.teams[t].supplies += 1;
            }
        }
    }
}
