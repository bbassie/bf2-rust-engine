//! The server side of voice chat (see `game_shared::voice`): relays each 20 ms frame to the
//! players who may hear it, decided here from the talker's squad, leadership and team, never
//! from what the client asks for. Frames are dropped when they are empty or too big, come
//! faster than a talker can speak ([`Rate::VOICE`], the shared limiter), come from a bot, a
//! spectator, a player muted by an admin ([`VoiceMuted`], the `mute` admin command) or a
//! player who may not use the channel. Each talk burst is logged once, with its listeners.

use bevy::{platform::collections::HashMap, prelude::*};
use bevy_replicon::prelude::*;
use game_shared::{
    commander::Commander,
    protocol::{Player, PlayerNetId, Team},
    squad::SquadMember,
    voice::{MAX_VOICE_BYTES, VoiceChannel, VoiceMuted, VoicePacket, VoiceRelay, VoiceRole, effective_channel, hears, refusal},
};

use crate::{
    ClientPlayer, HostPlayer, PlayerClient,
    limits::{Rate, RateLimiter},
    sender_player,
};

pub struct VoicePlugin;

impl Plugin for VoicePlugin {
    fn build(&self, app: &mut App) {
        app.add_systems(
            PreUpdate,
            relay_voice
                .after(ServerSystems::Receive)
                .run_if(in_state(ClientState::Disconnected)),
        );
    }
}

/// A pause this long between a talker's frames ends a burst (for the logs).
const BURST_GAP: f64 = 0.5;

/// A talker's current burst, for the logs.
struct Burst {
    last: f64,
    channel: VoiceChannel,
    frames: u32,
    listeners: usize,
    /// Why frames of this burst were dropped, if they were.
    refused: Option<&'static str>,
}

#[derive(Default)]
struct VoiceState {
    limiter: RateLimiter<Entity>,
    bursts: HashMap<Entity, Burst>,
}

type PlayerRow = (
    Entity,
    &'static Player,
    &'static PlayerNetId,
    &'static Team,
    Option<&'static SquadMember>,
    Has<Commander>,
    Has<VoiceMuted>,
    Option<&'static PlayerClient>,
);

fn role(player: &Player, team: &Team, squad: Option<&SquadMember>, commander: bool) -> VoiceRole {
    VoiceRole { team: *team, squad: squad.copied(), commander, bot: player.is_bot }
}

#[allow(clippy::too_many_arguments)]
fn relay_voice(
    time: Res<Time<Real>>,
    mut packets: MessageReader<FromClient<VoicePacket>>,
    clients: Query<&ClientPlayer>,
    host: Option<Res<HostPlayer>>,
    players: Query<PlayerRow>,
    mut relays: MessageWriter<ToClients<VoiceRelay>>,
    mut state: Local<VoiceState>,
) {
    let now = time.elapsed_secs_f64();
    let host_player = host.as_ref().map(|h| h.0);
    for packet in packets.read() {
        let Some(talker) = sender_player(packet.client_id, &clients, host.as_deref()) else {
            continue;
        };
        let Ok(row) = players.get(talker) else {
            continue;
        };
        let (_, player, net_id, ..) = row;
        let message = &packet.message;
        let talker_role = role(row.1, row.3, row.4, row.5);
        let channel = effective_channel(message.channel, &talker_role);
        let refused = if message.data.is_empty() || message.data.len() > MAX_VOICE_BYTES {
            Some("bad frame size")
        } else if row.6 {
            Some("muted by an admin")
        } else if !state.limiter.allow(talker, Rate::VOICE, now) {
            Some("too many frames")
        } else {
            refusal(channel, &talker_role)
        };

        // A new burst?
        let new_burst = state
            .bursts
            .get(&talker)
            .is_none_or(|b| now - b.last > BURST_GAP || b.channel != channel);
        if new_burst {
            if let Some(old) = state.bursts.remove(&talker) {
                log_end(&player.name, &old);
            }
            state.bursts.insert(talker, Burst { last: now, channel, frames: 0, listeners: 0, refused: None });
        }
        let burst = state.bursts.get_mut(&talker).expect("inserted above");
        burst.last = now;

        if let Some(reason) = refused {
            if burst.refused.is_none() {
                info!("voice: dropping {}'s {} voice: {reason}", player.name, channel.label());
                burst.refused = Some(reason);
            }
            continue;
        }

        let relay = VoiceRelay { talker: *net_id, channel, seq: message.seq, data: message.data.clone() };
        let mut names = Vec::new();
        for listener in &players {
            if listener.0 == talker || !hears(channel, &talker_role, &role(listener.1, listener.3, listener.4, listener.5)) {
                continue;
            }
            let target = match listener.7 {
                Some(client) => ClientId::Client(client.0),
                None if Some(listener.0) == host_player => ClientId::Server,
                None => continue,
            };
            if new_burst {
                names.push(listener.1.name.as_str());
            }
            burst.listeners += 1;
            relays.write(ToClients { targets: SendTargets::Single(target), message: relay.clone() });
        }
        burst.frames += 1;
        if new_burst {
            info!(
                "voice: {} talks on {} to {} player(s): {}",
                player.name,
                channel.label(),
                names.len(),
                names.join(", ")
            );
        }
    }
    // Bursts that ended.
    let ended: Vec<Entity> = state.bursts.iter().filter(|(_, b)| now - b.last > BURST_GAP).map(|(e, _)| *e).collect();
    for talker in ended {
        if let Some(burst) = state.bursts.remove(&talker) {
            let name = players.get(talker).map_or_else(|_| "someone".to_string(), |row| row.1.name.clone());
            log_end(&name, &burst);
        }
    }
}

fn log_end(name: &str, burst: &Burst) {
    if burst.frames > 0 {
        info!(
            "voice: {name} stopped on {}: {} frames relayed {} times",
            burst.channel.label(),
            burst.frames,
            burst.listeners
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn app() -> App {
        let mut app = App::new();
        app.add_message::<FromClient<VoicePacket>>()
            .add_message::<ToClients<VoiceRelay>>()
            .insert_resource(Time::<Real>::default())
            .add_systems(Update, relay_voice);
        app
    }

    /// A human player on another machine: its client entity and player entity.
    fn player(app: &mut App, name: &str, team: Team, squad: Option<(u8, bool)>, commander: bool) -> (Entity, Entity) {
        let world = app.world_mut();
        let client = world.spawn_empty().id();
        let player = world
            .spawn((
                Player { name: name.into(), is_bot: false },
                PlayerNetId(client.to_bits()),
                team,
                PlayerClient(client),
            ))
            .id();
        if let Some((squad, leader)) = squad {
            world.entity_mut(player).insert(SquadMember { squad, leader });
        }
        if commander {
            world.entity_mut(player).insert(Commander);
        }
        world.entity_mut(client).insert(ClientPlayer(player));
        (client, player)
    }

    fn talk(app: &mut App, client: Entity, channel: VoiceChannel, frames: u16, bytes: usize) {
        for seq in 0..frames {
            app.world_mut().write_message(FromClient {
                client_id: ClientId::Client(client),
                message: VoicePacket { channel, seq, data: vec![7; bytes] },
            });
        }
    }

    /// Who got relays this update: the client entities, sorted, with a count each.
    fn heard(app: &mut App) -> Vec<(Entity, usize)> {
        let messages = app.world().resource::<Messages<ToClients<VoiceRelay>>>();
        let mut counts: HashMap<Entity, usize> = HashMap::default();
        for message in messages.iter_current_update_messages() {
            match message.targets {
                SendTargets::Single(ClientId::Client(client)) => *counts.entry(client).or_default() += 1,
                ref other => panic!("unexpected targets {other:?}"),
            }
        }
        let mut out: Vec<_> = counts.into_iter().collect();
        out.sort();
        out
    }

    #[test]
    fn squad_voice_goes_to_squad_mates_only() {
        let mut app = app();
        let (alice, _) = player(&mut app, "Alice", Team::One, Some((1, true)), false);
        let (bob, _) = player(&mut app, "Bob", Team::One, Some((1, false)), false);
        let (_carol, _) = player(&mut app, "Carol", Team::One, Some((2, true)), false);
        let (_enemy, _) = player(&mut app, "Enemy", Team::Two, Some((1, false)), false);
        talk(&mut app, alice, VoiceChannel::Squad, 3, 60);
        app.update();
        assert_eq!(heard(&mut app), vec![(bob, 3)]);
    }

    #[test]
    fn command_voice_goes_to_the_commander_and_leaders_only() {
        let mut app = app();
        let (leader, _) = player(&mut app, "Leader", Team::One, Some((1, true)), false);
        let (member, _) = player(&mut app, "Member", Team::One, Some((1, false)), false);
        let (other_leader, _) = player(&mut app, "Other", Team::One, Some((2, true)), false);
        let (commander, _) = player(&mut app, "Commander", Team::One, None, true);
        let (_enemy_commander, _) = player(&mut app, "Enemy", Team::Two, None, true);
        talk(&mut app, leader, VoiceChannel::Command, 1, 60);
        app.update();
        let mut expected = vec![(other_leader, 1), (commander, 1)];
        expected.sort();
        assert_eq!(heard(&mut app), expected);
        // A member asking for the command channel gets nobody.
        talk(&mut app, member, VoiceChannel::Command, 1, 60);
        app.update();
        assert_eq!(heard(&mut app), vec![]);
        // The commander's squad key reaches his squad leaders.
        talk(&mut app, commander, VoiceChannel::Squad, 1, 60);
        app.update();
        let mut expected = vec![(leader, 1), (other_leader, 1)];
        expected.sort();
        assert_eq!(heard(&mut app), expected);
    }

    #[test]
    fn oversized_muted_and_flooding_talkers_are_dropped() {
        let mut app = app();
        let (alice, alice_player) = player(&mut app, "Alice", Team::One, Some((1, true)), false);
        let (bob, _) = player(&mut app, "Bob", Team::One, Some((1, false)), false);
        talk(&mut app, alice, VoiceChannel::Squad, 1, MAX_VOICE_BYTES + 1);
        talk(&mut app, alice, VoiceChannel::Squad, 1, 0);
        app.update();
        assert_eq!(heard(&mut app), vec![]);
        // A flood: only the burst allowance gets through.
        talk(&mut app, alice, VoiceChannel::Squad, 200, 60);
        app.update();
        let got = heard(&mut app);
        assert_eq!(got.len(), 1);
        assert_eq!(got[0].0, bob);
        assert!(got[0].1 <= Rate::VOICE.burst as usize + 2, "{} frames got through", got[0].1);
        // Muted by an admin: nothing.
        app.world_mut().entity_mut(alice_player).insert(VoiceMuted);
        talk(&mut app, alice, VoiceChannel::Squad, 1, 60);
        app.update();
        assert_eq!(heard(&mut app), vec![]);
    }

    #[test]
    fn bots_never_talk() {
        let mut app = app();
        let (bot_client, bot) = player(&mut app, "Bot", Team::One, Some((1, true)), false);
        let (_bob, _) = player(&mut app, "Bob", Team::One, Some((1, false)), false);
        app.world_mut().get_mut::<Player>(bot).unwrap().is_bot = true;
        talk(&mut app, bot_client, VoiceChannel::Squad, 1, 60);
        app.update();
        assert_eq!(heard(&mut app), vec![]);
    }
}
