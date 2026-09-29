//! The server list: game servers announce themselves with a heartbeat every 30 s, over UDP
//! (any server) or over the web API with an API key (ranked servers, see `api`). Browsers get
//! the list over UDP (the game's LAN browser protocol) or as JSON, and quick join picks from
//! it.
//!
//! UDP messages (integers little-endian):
//!
//! ```text
//! server -> master: "BF2R-HB" game_port u16 query_port u16             heartbeat
//!                   [players u16 max u16 bots u16 "name\nlevel\nmode"]  (newer servers)
//! server -> master: "BF2R-BYE" game_port u16                            shutting down
//! client -> master: "BF2R-LIST"                                         list please
//! master -> client: "BF2R-SERVERS" then "ip game_port query_port\n" per server
//! ```
//!
//! A server is dropped after [`TIMEOUT`] without a heartbeat. The address is the one the
//! heartbeat came from.

use std::{
    collections::HashMap,
    net::{IpAddr, SocketAddr, UdpSocket},
    sync::{Arc, Mutex},
    time::{Duration, Instant},
};

use game_auth::api::{QuickJoin, ServerEntry};

const HEARTBEAT: &[u8] = b"BF2R-HB";
const BYE: &[u8] = b"BF2R-BYE";
const LIST: &[u8] = b"BF2R-LIST";
const SERVERS: &[u8] = b"BF2R-SERVERS";
/// Servers not heard from for this long are forgotten (they beat every 30 s).
pub const TIMEOUT: Duration = Duration::from_secs(95);
/// Servers listed per UDP answer, to stay within one datagram.
const MAX_LISTED: usize = 60;
/// Servers per address, against someone filling the list.
const MAX_PER_ADDRESS: usize = 16;

/// A listed server and when it was last heard from.
#[derive(Clone, Debug)]
pub struct Listed {
    pub entry: ServerEntry,
    pub seen: Instant,
}

#[derive(Default)]
pub struct ServerList {
    servers: HashMap<(IpAddr, u16), Listed>,
}

pub type SharedList = Arc<Mutex<ServerList>>;

impl ServerList {
    fn expire(&mut self) {
        self.servers.retain(|(ip, port), listed| {
            let alive = listed.seen.elapsed() < TIMEOUT;
            if !alive {
                println!("{ip}:{port} timed out");
            }
            alive
        });
    }

    /// Adds or refreshes a server. `false` if its address has too many already.
    pub fn beat(&mut self, ip: IpAddr, entry: ServerEntry) -> bool {
        self.expire();
        let key = (ip, entry.port);
        if !self.servers.contains_key(&key) {
            if self.servers.keys().filter(|(other, _)| *other == ip).count() >= MAX_PER_ADDRESS {
                return false;
            }
            println!("{ip}:{} registered{}", entry.port, if entry.ranked { " (ranked)" } else { "" });
        }
        // A plain UDP beat doesn't make a ranked server unranked.
        let keep = self.servers.get(&key).filter(|old| old.entry.ranked && !entry.ranked && old.seen.elapsed() < TIMEOUT).cloned();
        let entry = match keep {
            Some(old) => ServerEntry { query_port: entry.query_port, ..old.entry },
            None => entry,
        };
        self.servers.insert(key, Listed { entry, seen: Instant::now() });
        true
    }

    pub fn bye(&mut self, ip: IpAddr, port: u16) {
        if self.servers.remove(&(ip, port)).is_some() {
            println!("{ip}:{port} left");
        }
    }

    pub fn entries(&mut self) -> Vec<ServerEntry> {
        self.expire();
        let mut entries: Vec<ServerEntry> = self.servers.values().map(|l| l.entry.clone()).collect();
        entries.sort_by(|a, b| b.players.cmp(&a.players).then(a.name.cmp(&b.name)));
        entries
    }

    /// Servers to join, best first: with free slots, `ranked` if asked (`Some`), then in the
    /// region asked for, then with players (but not nearly full). The client pings the first
    /// few and takes the closest.
    pub fn quick_join(&mut self, ranked: Option<bool>, region: &str) -> QuickJoin {
        let mut servers: Vec<ServerEntry> = self
            .entries()
            .into_iter()
            .filter(|s| s.max_players == 0 || s.free_slots() > 0)
            .filter(|s| ranked.is_none_or(|r| s.ranked == r))
            .collect();
        let region = region.trim().to_ascii_lowercase();
        servers.sort_by_key(|s| {
            let other_region = !region.is_empty() && s.region.to_ascii_lowercase() != region;
            let empty = s.players == 0;
            let nearly_full = s.max_players > 0 && s.free_slots() < 2;
            (other_region, empty, nearly_full, std::cmp::Reverse(s.players))
        });
        servers.truncate(10);
        QuickJoin { players: servers.iter().map(|s| s.players).sum(), servers }
    }
}

/// Parses a UDP heartbeat into a list entry.
fn parse_heartbeat(from: IpAddr, rest: &[u8]) -> Option<ServerEntry> {
    let u16_at = |i: usize| rest.get(i..i + 2).map(|b| u16::from_le_bytes([b[0], b[1]]));
    let (port, query_port) = (u16_at(0)?, u16_at(2)?);
    let mut entry = ServerEntry {
        address: from.to_string(),
        port,
        query_port,
        ..Default::default()
    };
    if let (Some(players), Some(max), Some(bots)) = (u16_at(4), u16_at(6), u16_at(8)) {
        entry.players = players as u32;
        entry.max_players = max as u32;
        entry.bots = bots as u32;
        let text = String::from_utf8_lossy(rest.get(10..).unwrap_or_default());
        let mut lines = text.lines().map(|l| l.chars().filter(|c| !c.is_control()).take(64).collect::<String>());
        entry.name = lines.next().unwrap_or_default();
        entry.level = lines.next().unwrap_or_default();
        entry.mode = lines.next().unwrap_or_default();
    }
    Some(entry)
}

/// Answers heartbeats and list queries on `socket`, forever.
pub fn run_udp(socket: UdpSocket, list: SharedList) {
    socket.set_read_timeout(Some(Duration::from_secs(5))).unwrap();
    let mut buffer = [0u8; 512];
    loop {
        let Ok((len, from)) = socket.recv_from(&mut buffer) else {
            list.lock().unwrap().expire();
            continue;
        };
        handle_udp(&socket, &list, &buffer[..len], from);
    }
}

fn handle_udp(socket: &UdpSocket, list: &SharedList, packet: &[u8], from: SocketAddr) {
    if let Some(rest) = packet.strip_prefix(HEARTBEAT) {
        if let Some(entry) = parse_heartbeat(from.ip(), rest) {
            list.lock().unwrap().beat(from.ip(), entry);
        }
    } else if let Some(rest) = packet.strip_prefix(BYE) {
        if let Some(b) = rest.get(0..2) {
            list.lock().unwrap().bye(from.ip(), u16::from_le_bytes([b[0], b[1]]));
        }
    } else if packet == LIST {
        let mut answer = SERVERS.to_vec();
        for server in list.lock().unwrap().entries().iter().take(MAX_LISTED) {
            answer.extend(format!("{} {} {}\n", server.address, server.port, server.query_port).as_bytes());
        }
        let _ = socket.send_to(&answer, from);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn entry(name: &str, players: u32, max: u32, ranked: bool, region: &str, port: u16) -> ServerEntry {
        ServerEntry {
            address: "10.0.0.1".into(),
            port,
            name: name.into(),
            players,
            max_players: max,
            ranked,
            region: region.into(),
            ..Default::default()
        }
    }

    #[test]
    fn heartbeats_and_quick_join() {
        let mut list = ServerList::default();
        let ip: IpAddr = "10.0.0.1".parse().unwrap();
        list.beat(ip, entry("full", 16, 16, false, "eu", 1));
        list.beat(ip, entry("empty", 0, 16, false, "eu", 2));
        list.beat(ip, entry("busy", 10, 32, true, "us", 3));
        list.beat(ip, entry("nice", 6, 32, false, "eu", 4));
        let names = |q: QuickJoin| q.servers.iter().map(|s| s.name.clone()).collect::<Vec<_>>();
        assert_eq!(names(list.quick_join(None, "")), ["busy", "nice", "empty"]);
        assert_eq!(names(list.quick_join(None, "eu")), ["nice", "empty", "busy"]);
        assert_eq!(names(list.quick_join(Some(true), "")), ["busy"]);
        assert_eq!(list.quick_join(Some(false), "").players, 6);
        // A UDP beat of a ranked server keeps it ranked.
        list.beat(ip, entry("busy", 11, 32, false, "", 3));
        assert!(list.entries().iter().any(|s| s.name == "busy" && s.ranked));
        list.bye(ip, 3);
        assert_eq!(list.entries().len(), 3);

        // Newer heartbeats say how full the server is; older ones only the ports.
        let mut packet = vec![0x67, 0x40, 0x68, 0x40];
        packet.extend([3, 0, 32, 0, 5, 0]);
        packet.extend(b"My server\nStrike at Karkand\ngpm_cq");
        let parsed = parse_heartbeat(ip, &packet).unwrap();
        assert_eq!((parsed.port, parsed.players, parsed.max_players, parsed.bots), (16487, 3, 32, 5));
        assert_eq!((parsed.name.as_str(), parsed.mode.as_str()), ("My server", "gpm_cq"));
        let old = parse_heartbeat(ip, &packet[..4]).unwrap();
        assert_eq!((old.query_port, old.players), (16488, 0));
    }
}
