//! Voice chat like BF2's: push to talk on two channels, relayed by the server.
//!
//! - **Squad**: everyone in your squad hears you.
//! - **Command**: the commander and the team's squad leaders hear each other (BF2's squad
//!   leaders to the commander and commander to all squad leaders, as one channel). Only
//!   they can talk on it. A commander outside any squad who presses the squad key talks
//!   here too ([`effective_channel`]).
//!
//! Clients send Opus frames of 20 ms ([`VoicePacket`], a dedicated unreliable channel); the
//! server decides who hears them ([`hears`], never the client), drops what is too big, too
//! fast or from a muted player, and relays each frame to those players ([`VoiceRelay`]).
//! Bots never talk. The codec and the audio live in the client (`game_voice`,
//! `game_client::voice`); the server only moves bytes.
//!
//! Bandwidth: a frame is about 60 bytes at 24 kbps plus a few bytes of header, 50 a second:
//! about 30 kbit/s for each listener of a talker. A squad of six with one talking costs the
//! server five of those, so even eight squads talking at once stay under 1.5 Mbit/s.

use bevy::prelude::*;
use serde::{Deserialize, Serialize};

use crate::{
    protocol::{PlayerNetId, Team},
    squad::SquadMember,
};

/// Biggest voice frame the server relays, in bytes (an Opus frame of 20 ms at 24 kbps is
/// about 60 bytes; this leaves room for VBR peaks and higher bitrates).
pub const MAX_VOICE_BYTES: usize = 256;

#[derive(Serialize, Deserialize, Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum VoiceChannel {
    Squad,
    /// The commander and the squad leaders.
    Command,
}

impl VoiceChannel {
    pub fn label(self) -> &'static str {
        match self {
            VoiceChannel::Squad => "squad",
            VoiceChannel::Command => "command",
        }
    }
}

/// Client -> server: one 20 ms Opus frame, on the unreliable voice channel.
#[derive(Message, Serialize, Deserialize, Clone, Debug)]
pub struct VoicePacket {
    /// The channel the talker's key asked for; the server checks it.
    pub channel: VoiceChannel,
    /// Counts frames, wrapping, so listeners can order them and notice losses.
    pub seq: u16,
    #[serde(with = "serde_bytes_compat")]
    pub data: Vec<u8>,
}

/// Server -> a listener: a frame someone said to it.
#[derive(Message, Serialize, Deserialize, Clone, Debug)]
pub struct VoiceRelay {
    /// Who talks: its [`PlayerNetId`] (an id rather than an entity, so the message needn't
    /// wait for replication and never gets dropped for an unmapped entity).
    pub talker: PlayerNetId,
    pub channel: VoiceChannel,
    pub seq: u16,
    #[serde(with = "serde_bytes_compat")]
    pub data: Vec<u8>,
}

/// On a player: an admin muted its voice for this session (`mute` admin command). The
/// server drops its frames; the client shows it and doesn't send. Replicated.
#[derive(Component, Serialize, Deserialize, Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct VoiceMuted;

/// Who a player is, as far as voice goes.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct VoiceRole {
    pub team: Team,
    pub squad: Option<SquadMember>,
    pub commander: bool,
    pub bot: bool,
}

impl VoiceRole {
    fn leads(&self) -> bool {
        self.commander || self.squad.is_some_and(|s| s.leader)
    }
}

/// The channel a key press talks on: the squad key of a commander who isn't in a squad talks
/// to his squad leaders.
pub fn effective_channel(requested: VoiceChannel, role: &VoiceRole) -> VoiceChannel {
    match requested {
        VoiceChannel::Squad if role.commander && role.squad.is_none() => VoiceChannel::Command,
        other => other,
    }
}

/// Why a player can't talk on a channel, if it can't.
pub fn refusal(channel: VoiceChannel, role: &VoiceRole) -> Option<&'static str> {
    if role.bot {
        return Some("bots don't talk");
    }
    if role.team == Team::Spectator {
        return Some("spectators have no channel");
    }
    match channel {
        VoiceChannel::Squad if role.squad.is_none() => Some("not in a squad"),
        VoiceChannel::Command if !role.leads() => Some("only the commander and squad leaders talk on command"),
        _ => None,
    }
}

/// Whether `listener` hears `talker` on `channel` (the talker himself doesn't; see
/// [`refusal`] for whether the talker may talk at all).
pub fn hears(channel: VoiceChannel, talker: &VoiceRole, listener: &VoiceRole) -> bool {
    if listener.bot || listener.team != talker.team || listener.team == Team::Spectator {
        return false;
    }
    match channel {
        VoiceChannel::Squad => {
            talker.squad.is_some() && listener.squad.map(|s| s.squad) == talker.squad.map(|s| s.squad)
        }
        VoiceChannel::Command => listener.leads(),
    }
}

/// `Vec<u8>` as one byte string (postcard writes a length and the bytes either way; this
/// keeps other serde formats from writing a list of numbers).
mod serde_bytes_compat {
    use serde::{Deserializer, Serializer, de::Error};

    use super::MAX_VOICE_BYTES;

    pub fn serialize<S: Serializer>(bytes: &[u8], serializer: S) -> Result<S::Ok, S::Error> {
        serializer.serialize_bytes(bytes)
    }

    pub fn deserialize<'de, D: Deserializer<'de>>(deserializer: D) -> Result<Vec<u8>, D::Error> {
        struct Visitor;
        impl<'de> serde::de::Visitor<'de> for Visitor {
            type Value = Vec<u8>;
            fn expecting(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
                write!(f, "at most {MAX_VOICE_BYTES} bytes")
            }
            fn visit_bytes<E: Error>(self, v: &[u8]) -> Result<Vec<u8>, E> {
                // Refused while decoding, so an oversized frame never gets allocated twice.
                if v.len() > MAX_VOICE_BYTES * 4 {
                    return Err(E::invalid_length(v.len(), &self));
                }
                Ok(v.to_vec())
            }
            fn visit_byte_buf<E: Error>(self, v: Vec<u8>) -> Result<Vec<u8>, E> {
                self.visit_bytes(&v)
            }
            fn visit_seq<A: serde::de::SeqAccess<'de>>(self, mut seq: A) -> Result<Vec<u8>, A::Error> {
                let mut out = Vec::new();
                while let Some(byte) = seq.next_element::<u8>()? {
                    if out.len() >= MAX_VOICE_BYTES * 4 {
                        return Err(A::Error::invalid_length(out.len() + 1, &self));
                    }
                    out.push(byte);
                }
                Ok(out)
            }
        }
        deserializer.deserialize_bytes(Visitor)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn role(team: Team, squad: Option<(u8, bool)>, commander: bool) -> VoiceRole {
        VoiceRole {
            team,
            squad: squad.map(|(squad, leader)| SquadMember { squad, leader }),
            commander,
            bot: false,
        }
    }

    #[test]
    fn squad_voice_reaches_the_squad_only() {
        let talker = role(Team::One, Some((1, false)), false);
        assert_eq!(refusal(VoiceChannel::Squad, &talker), None);
        let mate = role(Team::One, Some((1, true)), false);
        let other_squad = role(Team::One, Some((2, true)), false);
        let no_squad = role(Team::One, None, false);
        let enemy_same_number = role(Team::Two, Some((1, false)), false);
        let commander = role(Team::One, None, true);
        assert!(hears(VoiceChannel::Squad, &talker, &mate));
        assert!(!hears(VoiceChannel::Squad, &talker, &other_squad));
        assert!(!hears(VoiceChannel::Squad, &talker, &no_squad));
        assert!(!hears(VoiceChannel::Squad, &talker, &enemy_same_number), "enemies never hear");
        assert!(!hears(VoiceChannel::Squad, &talker, &commander));
    }

    #[test]
    fn squad_voice_needs_a_squad() {
        let loner = role(Team::One, None, false);
        assert!(refusal(VoiceChannel::Squad, &loner).is_some());
        // Nobody hears a talker without a squad, even others without one.
        assert!(!hears(VoiceChannel::Squad, &loner, &role(Team::One, None, false)));
    }

    #[test]
    fn command_voice_is_for_the_commander_and_squad_leaders() {
        let leader = role(Team::One, Some((1, true)), false);
        let member = role(Team::One, Some((1, false)), false);
        let other_leader = role(Team::One, Some((3, true)), false);
        let commander = role(Team::One, None, true);
        let enemy_commander = role(Team::Two, None, true);
        let enemy_leader = role(Team::Two, Some((1, true)), false);
        assert_eq!(refusal(VoiceChannel::Command, &leader), None);
        assert_eq!(refusal(VoiceChannel::Command, &commander), None);
        assert!(refusal(VoiceChannel::Command, &member).is_some(), "members can't talk to the commander");
        for talker in [&leader, &commander] {
            assert!(!hears(VoiceChannel::Command, talker, &member), "members don't hear the command channel");
            assert!(!hears(VoiceChannel::Command, talker, &enemy_commander));
            assert!(!hears(VoiceChannel::Command, talker, &enemy_leader));
        }
        assert!(hears(VoiceChannel::Command, &leader, &commander));
        assert!(hears(VoiceChannel::Command, &commander, &leader));
        assert!(hears(VoiceChannel::Command, &commander, &other_leader));
        assert!(hears(VoiceChannel::Command, &leader, &other_leader));
    }

    #[test]
    fn a_commander_without_a_squad_talks_to_his_leaders_on_the_squad_key() {
        let commander = role(Team::One, None, true);
        assert_eq!(effective_channel(VoiceChannel::Squad, &commander), VoiceChannel::Command);
        let leader = role(Team::One, Some((1, true)), false);
        assert_eq!(effective_channel(VoiceChannel::Squad, &leader), VoiceChannel::Squad);
        assert_eq!(effective_channel(VoiceChannel::Command, &leader), VoiceChannel::Command);
    }

    #[test]
    fn bots_and_spectators_neither_talk_nor_hear() {
        let mut bot = role(Team::One, Some((1, false)), false);
        bot.bot = true;
        let talker = role(Team::One, Some((1, true)), false);
        assert!(refusal(VoiceChannel::Squad, &bot).is_some());
        assert!(!hears(VoiceChannel::Squad, &talker, &bot));
        let spectator = role(Team::Spectator, None, false);
        assert!(refusal(VoiceChannel::Squad, &spectator).is_some());
        assert!(!hears(VoiceChannel::Command, &role(Team::Spectator, None, true), &spectator));
    }

    #[test]
    fn packets_round_trip_through_the_wire_format() {
        let packet = VoicePacket { channel: VoiceChannel::Command, seq: 65535, data: vec![1, 2, 3, 250] };
        let bytes = postcard::to_allocvec(&packet).unwrap();
        // Channel, seq (varint), length and the bytes: no per-byte overhead.
        assert!(bytes.len() <= 1 + 3 + 1 + 4, "{} bytes", bytes.len());
        let back: VoicePacket = postcard::from_bytes(&bytes).unwrap();
        assert_eq!((back.channel, back.seq, back.data), (packet.channel, packet.seq, packet.data));
        // Oversized frames are refused while decoding.
        let huge = VoicePacket { channel: VoiceChannel::Squad, seq: 0, data: vec![0; MAX_VOICE_BYTES * 5] };
        let bytes = postcard::to_allocvec(&huge).unwrap();
        assert!(postcard::from_bytes::<VoicePacket>(&bytes).is_err());
    }
}
