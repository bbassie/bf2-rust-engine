//! Finding servers on the LAN: a client sends a small UDP query (broadcast, or to a known
//! address) to the discovery ports; every server answers with a [`ServerInfo`].
//!
//! ```text
//! query: "BF2R?" token (u64 LE)
//! reply: "BF2R!" token (u64 LE) ServerInfo as RON
//! ```
//!
//! A server listens on the first free port of [`DISCOVERY_PORTS`], so several servers can
//! run on one machine; clients query all of them. The token comes back unchanged, which is
//! how the client measures the ping.

use std::ops::RangeInclusive;

use serde::{Deserialize, Serialize};

/// UDP ports servers answer queries on (the game port is 16567).
pub const DISCOVERY_PORTS: RangeInclusive<u16> = 16568..=16575;

const QUERY: &[u8; 5] = b"BF2R?";
const REPLY: &[u8; 5] = b"BF2R!";

/// What a server tells a browsing client.
#[derive(Serialize, Deserialize, Clone, Debug, Default, PartialEq)]
pub struct ServerInfo {
    pub name: String,
    /// Level folder name.
    pub level: String,
    /// Level display name.
    pub level_name: String,
    pub mode: String,
    pub size: u32,
    /// Humans playing.
    pub players: u32,
    pub max_players: u32,
    pub bots: u32,
    /// Game port to connect to.
    pub port: u16,
    /// [`crate::PROTOCOL_ID`]: clients can only join servers with theirs.
    pub protocol: u64,
}

pub fn encode_query(token: u64) -> Vec<u8> {
    [QUERY.as_slice(), &token.to_le_bytes()].concat()
}

/// The token of a query packet.
pub fn parse_query(packet: &[u8]) -> Option<u64> {
    let token = packet.strip_prefix(QUERY)?;
    Some(u64::from_le_bytes(token.try_into().ok()?))
}

pub fn encode_reply(token: u64, info: &ServerInfo) -> Vec<u8> {
    let body = ron::to_string(info).unwrap_or_default();
    [REPLY.as_slice(), &token.to_le_bytes(), body.as_bytes()].concat()
}

pub fn parse_reply(packet: &[u8]) -> Option<(u64, ServerInfo)> {
    let rest = packet.strip_prefix(REPLY)?;
    let (token, body) = rest.split_at_checked(8)?;
    let token = u64::from_le_bytes(token.try_into().ok()?);
    let info = ron::from_str(std::str::from_utf8(body).ok()?).ok()?;
    Some((token, info))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn round_trip() {
        assert_eq!(parse_query(&encode_query(42)), Some(42));
        let info = ServerInfo {
            name: "Test".into(),
            level: "strike_at_karkand".into(),
            players: 3,
            port: 16567,
            ..Default::default()
        };
        assert_eq!(parse_reply(&encode_reply(7, &info)), Some((7, info)));
        assert_eq!(parse_query(b"BF2R!12345678"), None);
    }
}
