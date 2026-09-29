//! Finding servers on the LAN: a client sends a padded UDP query (broadcast, or to a known
//! address) to the discovery ports; every server answers with a [`ServerInfo`].
//!
//! ```text
//! query: "BF2R?" token (u64 LE), zeros up to QUERY_SIZE (1024) bytes
//! reply: "BF2R!" token (u64 LE) ServerInfo as RON
//! ```
//!
//! A server listens on the first free port of [`DISCOVERY_PORTS`], so several servers can
//! run on one machine; clients query all of them. The token comes back unchanged, which is
//! how the client measures the ping.

use std::{
    net::IpAddr,
    ops::RangeInclusive,
};

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
    /// What the server shares with joining clients, if anything (see [`crate::content`]).
    #[serde(default)]
    pub content: Option<crate::content::ContentAdvert>,
    /// Needs an account from a master server (see `game_server::accounts`).
    #[serde(default)]
    pub ranked: bool,
}

/// Queries are padded with zeros to this many bytes, and servers ignore shorter ones, so a
/// reply (a few hundred bytes) is never larger than the query that asked for it: a spoofed
/// query can't make a server send more to its victim than the spoofer sent.
pub const QUERY_SIZE: usize = 1024;

pub fn encode_query(token: u64) -> Vec<u8> {
    let mut query = [QUERY.as_slice(), &token.to_le_bytes()].concat();
    query.resize(QUERY_SIZE, 0);
    query
}

/// The token of a query packet (padded to [`QUERY_SIZE`]).
pub fn parse_query(packet: &[u8]) -> Option<u64> {
    if packet.len() < QUERY_SIZE {
        return None;
    }
    let rest = packet.strip_prefix(QUERY)?;
    let (token, _padding) = rest.split_at_checked(8)?;
    Some(u64::from_le_bytes(token.try_into().ok()?))
}

pub fn encode_reply(token: u64, info: &ServerInfo) -> Vec<u8> {
    let body = ron::to_string(info).unwrap_or_default();
    [REPLY.as_slice(), &token.to_le_bytes(), body.as_bytes()].concat()
}

/// The optional master server's port (`crates/master_server`).
pub const MASTER_PORT: u16 = 16580;

/// Server -> master, every [`HEARTBEAT_SECONDS`]: "I'm here".
pub fn encode_heartbeat(game_port: u16, query_port: u16) -> Vec<u8> {
    [b"BF2R-HB".as_slice(), &game_port.to_le_bytes(), &query_port.to_le_bytes()].concat()
}

/// Server -> master, every [`HEARTBEAT_SECONDS`], with how full the server is (for the
/// master's quick join): the plain heartbeat followed by `players u16, max u16, bots u16` and
/// name, level and mode on a line each (UTF-8). Older masters read only the plain part.
pub fn encode_heartbeat_with(game_port: u16, query_port: u16, players: u16, max_players: u16, bots: u16, text: &str) -> Vec<u8> {
    let mut text = text.to_string();
    while text.len() > 160 {
        text.pop();
    }
    [
        encode_heartbeat(game_port, query_port).as_slice(),
        &players.to_le_bytes(),
        &max_players.to_le_bytes(),
        &bots.to_le_bytes(),
        text.as_bytes(),
    ]
    .concat()
}

/// Server -> master when it stops.
pub fn encode_bye(game_port: u16) -> Vec<u8> {
    [b"BF2R-BYE".as_slice(), &game_port.to_le_bytes()].concat()
}

pub const HEARTBEAT_SECONDS: f32 = 30.0;

/// Client -> master: the server list, please.
pub const LIST_QUERY: &[u8] = b"BF2R-LIST";

/// The master's answer: `(ip, game port, query port)` per server.
pub fn parse_server_list(packet: &[u8]) -> Option<Vec<(IpAddr, u16, u16)>> {
    let text = std::str::from_utf8(packet.strip_prefix(b"BF2R-SERVERS")?).ok()?;
    Some(
        text.lines()
            .filter_map(|line| {
                let mut parts = line.split_whitespace();
                Some((parts.next()?.parse().ok()?, parts.next()?.parse().ok()?, parts.next()?.parse().ok()?))
            })
            .collect(),
    )
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
        assert!(encode_reply(7, &info).len() < QUERY_SIZE, "replies are smaller than queries");
        assert_eq!(parse_reply(&encode_reply(7, &info)), Some((7, info)));
        assert_eq!(parse_query(b"BF2R!12345678"), None);
        // Unpadded (amplifying) queries get no answer.
        assert_eq!(parse_query(b"BF2R?12345678"), None);
        assert_eq!(encode_query(1).len(), QUERY_SIZE);
        let list = parse_server_list(b"BF2R-SERVERS127.0.0.1 16567 16568
10.0.0.2 16600 16569
").unwrap();
        assert_eq!(list.len(), 2);
        assert_eq!(list[1].1, 16600);
    }
}
