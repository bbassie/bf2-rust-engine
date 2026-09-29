//! Squads (see `game_shared::squad`): creating, joining and leaving them, keeping exactly one
//! leader per squad, and putting bots into squads like BF2 does.

use bevy::{platform::collections::HashMap, prelude::*};
use bevy_replicon::prelude::*;
use game_shared::{
    protocol::{Player, Team},
    squad::{MAX_MEMBERS, MAX_SQUADS, SquadMember, SquadRequest},
};

use crate::{ClientPlayer, HostPlayer, bots::BotBrain, sender_player};

pub struct SquadPlugin;

impl Plugin for SquadPlugin {
    fn build(&self, app: &mut App) {
        app.add_systems(
            PreUpdate,
            receive_squad_requests
                .after(ServerSystems::Receive)
                .run_if(in_state(ClientState::Disconnected)),
        )
        .add_systems(
            Update,
            (keep_leaders, fill_bot_squads)
                .chain()
                .run_if(in_state(ClientState::Disconnected)),
        );
    }
}

/// Members per (team, squad).
fn squad_sizes<'a>(
    players: impl Iterator<Item = (&'a Team, Option<&'a SquadMember>)>,
) -> HashMap<(Team, u8), usize> {
    let mut sizes = HashMap::default();
    for (team, member) in players {
        if let Some(member) = member {
            *sizes.entry((*team, member.squad)).or_default() += 1;
        }
    }
    sizes
}

fn free_squad(sizes: &HashMap<(Team, u8), usize>, team: Team) -> Option<u8> {
    (1..=MAX_SQUADS).find(|squad| !sizes.contains_key(&(team, *squad)))
}

fn receive_squad_requests(
    mut commands: Commands,
    mut requests: MessageReader<FromClient<SquadRequest>>,
    clients: Query<&ClientPlayer>,
    host: Option<Res<HostPlayer>>,
    players: Query<(Entity, &Team, Option<&SquadMember>), With<Player>>,
) {
    let mut sizes = squad_sizes(players.iter().map(|(_, t, m)| (t, m)));
    for request in requests.read() {
        let Some(player) = sender_player(request.client_id, &clients, host.as_deref()) else {
            continue;
        };
        let Ok((_, &team, current)) = players.get(player) else {
            continue;
        };
        if team == Team::Spectator {
            continue;
        }
        // Leaving first: the old squad gets a new leader in `keep_leaders`.
        if let Some(current) = current
            && let Some(size) = sizes.get_mut(&(team, current.squad))
        {
            *size -= 1;
        }
        match request.message {
            SquadRequest::Create => {
                if let Some(squad) = free_squad(&sizes, team) {
                    sizes.insert((team, squad), 1);
                    commands.entity(player).insert(SquadMember { squad, leader: true });
                }
            }
            SquadRequest::Join(squad) => {
                let size = sizes.get(&(team, squad)).copied().unwrap_or(0);
                if (1..MAX_MEMBERS).contains(&size) {
                    sizes.insert((team, squad), size + 1);
                    commands.entity(player).insert(SquadMember { squad, leader: false });
                }
            }
            SquadRequest::Leave => {
                commands.entity(player).remove::<SquadMember>();
            }
        }
    }
}

/// Every squad has exactly one leader: the first (by [`keep_leaders`]'s order) member takes
/// over when the leader leaves, several end up flagged leader at once, or a human joins a
/// squad a bot is leading (like BF2, a human always outranks a bot for the post).
fn keep_leaders(mut players: Query<(Entity, &Team, &mut SquadMember, Has<BotBrain>)>) {
    let mut squads: HashMap<(Team, u8), Vec<(Entity, bool, bool)>> = HashMap::default();
    for (entity, team, member, bot) in &players {
        squads.entry((*team, member.squad)).or_default().push((entity, member.leader, bot));
    }
    for (_, mut members) in squads {
        let leaders = members.iter().filter(|(_, leader, _)| *leader).count();
        let bot_leads_a_human =
            members.iter().any(|(_, leader, bot)| *leader && *bot) && members.iter().any(|(_, _, bot)| !bot);
        if leaders == 1 && !bot_leads_a_human {
            continue;
        }
        // Humans lead before bots; ties (picking a leader with none, or breaking one with
        // several) go to whoever already leads, then the longest-standing member.
        members.sort_by_key(|(entity, leader, bot)| (*bot, !*leader, *entity));
        for (index, (entity, _, _)) in members.iter().enumerate() {
            if let Ok((_, _, mut member, _)) = players.get_mut(*entity) {
                let leader = index == 0;
                if member.leader != leader {
                    member.leader = leader;
                }
            }
        }
    }
}

/// Bots join a squad of their team with room, or start one.
fn fill_bot_squads(
    mut commands: Commands,
    time: Res<Time>,
    mut timer: Local<f32>,
    players: Query<(Entity, &Team, Option<&SquadMember>, Has<BotBrain>), With<Player>>,
    commanders: Query<(), With<game_shared::commander::Commander>>,
) {
    *timer -= time.delta_secs();
    if *timer > 0.0 {
        return;
    }
    *timer = 2.0;
    let mut sizes = squad_sizes(players.iter().map(|(_, t, m, _)| (t, m)));
    for (entity, &team, member, bot) in &players {
        // Commanders lead the team, not a squad.
        if !bot || member.is_some() || team == Team::Spectator || commanders.contains(entity) {
            continue;
        }
        // Fill the fullest squad with room first, leaving one place for a human.
        let join = sizes
            .iter()
            .filter(|((t, _), size)| *t == team && **size < MAX_MEMBERS - 1)
            .max_by_key(|(_, size)| **size)
            .map(|((_, squad), _)| *squad);
        let (squad, leader) = match join {
            Some(squad) => (squad, false),
            None => match free_squad(&sizes, team) {
                Some(squad) => (squad, true),
                None => continue,
            },
        };
        *sizes.entry((team, squad)).or_default() += 1;
        commands.entity(entity).insert(SquadMember { squad, leader });
    }
}
