//! Loadouts on the server (see `game_shared::arsenal`): the rules from the server settings
//! go on the match for clients to see, players' picks are checked against their class's
//! pool when they arrive and again when they spawn, and the quick melee and grenade keys
//! are logged.

use bevy::prelude::*;
use bevy_replicon::prelude::*;
use game_data::WeaponDesc;
use game_shared::{
    arsenal::{Arsenal, ClassPick, LoadoutPicks, LoadoutRequest, LoadoutRules, MAX_NAME, MAX_PICKS, PickSlot, team_factions},
    conquest::team_index,
    join::AccountBadge,
    protocol::{MatchInfo, Player, Team},
    weapons::{Armory, Loadout, WeaponState},
};

use crate::{
    ClientPlayer, HostPlayer, ServerSettings,
    limits::{Rate, RateLimiter},
    sender_player,
};

pub struct LoadoutsPlugin;

impl Plugin for LoadoutsPlugin {
    fn build(&self, app: &mut App) {
        app.add_systems(
            PreUpdate,
            receive_loadout_requests
                .after(ServerSystems::Receive)
                .run_if(in_state(ClientState::Disconnected)),
        )
        .add_systems(Update, publish_rules.run_if(in_state(ClientState::Disconnected)));
    }
}

/// Loadout requests from one player: a handful when joining or changing picks.
const LOADOUT_RATE: Rate = Rate { burst: 8.0, per_second: 1.0 };

/// The server's rules on the match entity (replicated), kept in step with the settings.
fn publish_rules(
    mut commands: Commands,
    settings: Res<ServerSettings>,
    matches: Query<(Entity, Option<&LoadoutRules>), With<MatchInfo>>,
) {
    for (entity, rules) in &matches {
        if rules != Some(&settings.loadouts) {
            commands.entity(entity).insert(settings.loadouts.clone());
        }
    }
}

/// A player's account rank, if verified.
fn rank(badge: Option<&AccountBadge>) -> Option<u32> {
    badge.map(|b| b.rank)
}

/// Checks each pick of a request against the pools and the rules; the accepted ones replace
/// the player's picks.
#[allow(clippy::too_many_arguments)]
fn receive_loadout_requests(
    mut commands: Commands,
    time: Res<Time<Real>>,
    mut requests: MessageReader<FromClient<LoadoutRequest>>,
    clients: Query<&ClientPlayer>,
    host: Option<Res<HostPlayer>>,
    settings: Res<ServerSettings>,
    armory: Res<Armory>,
    arsenal: Res<Arsenal>,
    players: Query<(&Player, &Team, Option<&AccountBadge>, Option<&LoadoutPicks>)>,
    mut limiter: Local<RateLimiter<Entity>>,
) {
    let now = time.elapsed_secs_f64();
    for request in requests.read() {
        let Some(player) = sender_player(request.client_id, &clients, host.as_deref()) else {
            continue;
        };
        let Ok((info, team, badge, current)) = players.get(player) else {
            continue;
        };
        if !limiter.allow(player, LOADOUT_RATE, now) {
            continue;
        }
        let factions = team_index(*team).map(|t| team_factions(&armory, t)).unwrap_or_default();
        let mut accepted = LoadoutPicks::default();
        for (class, pick) in request.message.picks.iter().take(MAX_PICKS) {
            let class = class.to_ascii_lowercase();
            if class.len() > MAX_NAME {
                continue;
            }
            let mut kept = ClassPick::default();
            for (slot, weapon, out) in [
                (PickSlot::Primary, &pick.primary, &mut kept.primary),
                (PickSlot::Sidearm, &pick.sidearm, &mut kept.sidearm),
            ] {
                let Some(weapon) = weapon.as_deref().filter(|w| w.len() <= MAX_NAME) else {
                    continue;
                };
                match arsenal.check(&settings.loadouts, &class, slot, weapon, &factions, rank(badge)) {
                    Ok(_) => *out = Some(weapon.to_string()),
                    Err(refusal) => info!("loadout refused: {} {class} {weapon}: {refusal}", info.name),
                }
            }
            if !kept.is_empty() {
                info!(
                    "loadout accepted: {} {class}: {} / {}",
                    info.name,
                    kept.primary.as_deref().unwrap_or("kit primary"),
                    kept.sidearm.as_deref().unwrap_or("kit sidearm")
                );
                accepted.0.insert(class, kept);
            }
        }
        if current != Some(&accepted) {
            commands.entity(player).insert(accepted);
        }
    }
}

/// The loadout of a soldier of kit slot `kit` for a player with `picks`, when the rules
/// still allow them (the team may have changed since); `None`: the kit as it is.
pub fn picked_loadout(
    settings: &ServerSettings,
    armory: &Armory,
    arsenal: &Arsenal,
    team: Team,
    kit: u8,
    picks: Option<&LoadoutPicks>,
    badge: Option<&AccountBadge>,
) -> Option<Loadout> {
    let team = team_index(team)?;
    let kit = armory.kit_for(team, kit as usize)?;
    let class = kit.kind.to_ascii_lowercase();
    let pick = picks?.0.get(&class)?;
    let factions = team_factions(armory, team);
    let allowed = |slot, weapon: &Option<String>| {
        weapon
            .as_deref()
            .filter(|w| arsenal.check(&settings.loadouts, &class, slot, w, &factions, rank(badge)).is_ok())
            .map(str::to_string)
    };
    let pick = ClassPick {
        primary: allowed(PickSlot::Primary, &pick.primary),
        sidearm: allowed(PickSlot::Sidearm, &pick.sidearm),
    };
    if pick.is_empty() {
        return None;
    }
    Some(Loadout {
        kit: kit.name.clone(),
        weapons: arsenal.kit_weapons(kit, &pick, armory),
    })
}

/// Logs the melee and grenade keys at work (the input's `QUICK` with a switch): the
/// knife or grenade coming out, and the weapon it interrupted coming back.
pub fn log_quick_switch(players: &Query<&Player>, player: Entity, state: &WeaponState, weapon: &WeaponDesc, quick: bool) {
    if !quick {
        return;
    }
    let name = players.get(player).map_or("?", |p| p.name.as_str());
    if weapon.is_melee() {
        info!("{name} quick melee with {}", weapon.name);
    } else if weapon.is_hand_grenade() {
        info!("{name} quick throw with {}", weapon.name);
    } else if state.quick {
        info!("{name} back to {} after a quick action", weapon.name);
    }
}
