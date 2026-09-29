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
//! server -> master: "BF2R-BYE" game_port u16                            shutting down (ignored,
//!                                                                        see below)
//! client -> master: "BF2R-LIST"                                         list please
//! master -> client: "BF2R-SERVERS" then "ip game_port query_port\n" per server
//! ```
//!
//! A server is dropped after [`TIMEOUT`] without a heartbeat. The address is the one the
//! heartbeat came from.
//!
//! None of this is authenticated (a heartbeat needs no account or API key), so the master
//! treats it defensively (S7/S18):
//!
//! - Heartbeats are capped globally, per address and per /24 (v4) or /64 (v6) network, with
//!   O(1) counters (not a scan of the whole list per heartbeat).
//! - `BYE` is **ignored**: its source can't be verified, so a spoofed one could otherwise
//!   delist any server instantly. A real shutdown just falls off after [`TIMEOUT`] instead.
//! - `LIST` answers are throttled per source address: the query is 9 bytes and the answer can
//!   be many times that, so unlimited replies would make this a reflection amplifier for a
//!   spoofed source. A spoofed heartbeat that guesses another server's `ip:port` can still
//!   deface its listing (there is no session/identity for unranked UDP servers to check
//!   against); that's a smaller, cosmetic version of the same fundamentally unauthenticated
//!   protocol, matching vanilla BF2's own LAN discovery.

use std::{
    collections::HashMap,
    net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr, UdpSocket},
    sync::{Arc, Mutex},
    time::{Duration, Instant},
};

use game_auth::api::{QuickJoin, ServerEntry};

use crate::http::lock;

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
/// Servers per /24 (v4) or /64 (v6) network, against many spoofed source addresses in one
/// range.
const MAX_PER_SUBNET: usize = 64;
/// Servers listed in total, against unlimited spoofed heartbeats.
const MAX_SERVERS: usize = 4096;
/// How often one address gets a fresh `LIST` answer (a reflection/amplification brake).
const LIST_INTERVAL: Duration = Duration::from_secs(2);

/// The /24 (v4) or /64 (v6) network `ip` is in, as a lookup key.
fn subnet_key(ip: IpAddr) -> IpAddr {
    match ip {
        IpAddr::V4(v4) => {
            let [a, b, c, _] = v4.octets();
            IpAddr::V4(Ipv4Addr::new(a, b, c, 0))
        }
        IpAddr::V6(v6) => {
            let mut segments = v6.segments();
            segments[4..].fill(0);
            IpAddr::V6(Ipv6Addr::from(segments))
        }
    }
}

/// A listed server and when it was last heard from.
#[derive(Clone, Debug)]
pub struct Listed {
    pub entry: ServerEntry,
    pub seen: Instant,
}

#[derive(Default)]
pub struct ServerList {
    servers: HashMap<(IpAddr, u16), Listed>,
    /// Entries per address, kept incrementally so a heartbeat never has to scan the whole
    /// list (S7).
    by_ip: HashMap<IpAddr, usize>,
    /// Entries per /24 (v4) or /64 (v6) network.
    by_subnet: HashMap<IpAddr, usize>,
    /// When each address last got a `LIST` answer.
    list_answered: HashMap<IpAddr, Instant>,
}

pub type SharedList = Arc<Mutex<ServerList>>;

impl ServerList {
    /// Removes one entry and its `by_ip`/`by_subnet` counts.
    fn remove(&mut self, key: (IpAddr, u16)) -> bool {
        if self.servers.remove(&key).is_none() {
            return false;
        }
        let (ip, _) = key;
        if let Some(count) = self.by_ip.get_mut(&ip) {
            *count = count.saturating_sub(1);
            if *count == 0 {
                self.by_ip.remove(&ip);
            }
        }
        let subnet = subnet_key(ip);
        if let Some(count) = self.by_subnet.get_mut(&subnet) {
            *count = count.saturating_sub(1);
            if *count == 0 {
                self.by_subnet.remove(&subnet);
            }
        }
        true
    }

    fn expire(&mut self) {
        let stale: Vec<(IpAddr, u16)> = self.servers.iter().filter(|(_, l)| l.seen.elapsed() >= TIMEOUT).map(|(k, _)| *k).collect();
        for (ip, port) in stale {
            println!("{ip}:{port} timed out");
            self.remove((ip, port));
        }
        self.list_answered.retain(|_, t| t.elapsed() < LIST_INTERVAL * 4);
    }

    /// Adds or refreshes a server. `false` if the global, per-address or per-subnet cap is
    /// already reached.
    pub fn beat(&mut self, ip: IpAddr, entry: ServerEntry) -> bool {
        self.expire();
        let key = (ip, entry.port);
        if !self.servers.contains_key(&key) {
            let subnet = subnet_key(ip);
            let over_cap = self.servers.len() >= MAX_SERVERS
                || *self.by_ip.get(&ip).unwrap_or(&0) >= MAX_PER_ADDRESS
                || *self.by_subnet.get(&subnet).unwrap_or(&0) >= MAX_PER_SUBNET;
            if over_cap {
                return false;
            }
            *self.by_ip.entry(ip).or_insert(0) += 1;
            *self.by_subnet.entry(subnet).or_insert(0) += 1;
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

    /// Removes a server by address and port. Kept for tooling/tests; the UDP `BYE` message
    /// itself is ignored (see the module docs: its source can't be verified).
    pub fn bye(&mut self, ip: IpAddr, port: u16) {
        if self.remove((ip, port)) {
            println!("{ip}:{port} left");
        }
    }

    /// Whether `ip` may get a fresh `LIST` answer now (a reflection/amplification brake: the
    /// query is small, the answer can be much larger, and the source isn't authenticated).
    fn allow_list(&mut self, ip: IpAddr) -> bool {
        let now = Instant::now();
        match self.list_answered.get(&ip) {
            Some(last) if now.duration_since(*last) < LIST_INTERVAL => false,
            _ => {
                self.list_answered.insert(ip, now);
                true
            }
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
            lock(&list).expire();
            continue;
        };
        handle_udp(&socket, &list, &buffer[..len], from);
    }
}

fn handle_udp(socket: &UdpSocket, list: &SharedList, packet: &[u8], from: SocketAddr) {
    if let Some(rest) = packet.strip_prefix(HEARTBEAT) {
        if let Some(entry) = parse_heartbeat(from.ip(), rest) {
            lock(list).beat(from.ip(), entry);
        }
    } else if packet.strip_prefix(BYE).is_some() {
        // Ignored: the source of a UDP heartbeat isn't authenticated, so a spoofed BYE could
        // otherwise delist any server (S18). A real shutdown just falls off after `TIMEOUT`.
    } else if packet == LIST {
        let mut list = lock(list);
        if !list.allow_list(from.ip()) {
            return;
        }
        let mut answer = SERVERS.to_vec();
        for server in list.entries().iter().take(MAX_LISTED) {
            answer.extend(format!("{} {} {}\n", server.address, server.port, server.query_port).as_bytes());
        }
        drop(list);
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

    #[test]
    fn caps_per_address_subnet_and_total() {
        let mut list = ServerList::default();
        let ip: IpAddr = "203.0.113.7".parse().unwrap();
        for port in 0..MAX_PER_ADDRESS as u16 {
            assert!(list.beat(ip, entry("s", 0, 0, false, "", port)));
        }
        assert!(!list.beat(ip, entry("s", 0, 0, false, "", MAX_PER_ADDRESS as u16)), "per-address cap");
        // A different address in the same /24 still hits the per-subnet cap.
        let mut next_ip = 8u8;
        let mut added = MAX_PER_ADDRESS;
        while added < MAX_PER_SUBNET {
            let other: IpAddr = format!("203.0.113.{next_ip}").parse().unwrap();
            assert!(list.beat(other, entry("s", 0, 0, false, "", 0)));
            next_ip += 1;
            added += 1;
        }
        let over: IpAddr = format!("203.0.113.{next_ip}").parse().unwrap();
        assert!(!list.beat(over, entry("s", 0, 0, false, "", 0)), "per-subnet cap");
        // A different /24 is unaffected.
        assert!(list.beat("203.0.114.1".parse().unwrap(), entry("s", 0, 0, false, "", 0)));
    }

    #[test]
    fn bye_is_ignored_a_spoofed_source_cant_delist() {
        let list: SharedList = Arc::new(Mutex::new(ServerList::default()));
        let socket = UdpSocket::bind("127.0.0.1:0").unwrap();
        let from: SocketAddr = "203.0.113.5:40000".parse().unwrap();
        let mut hb = HEARTBEAT.to_vec();
        hb.extend([0x67, 0x40, 0x68, 0x40]);
        handle_udp(&socket, &list, &hb, from);
        assert_eq!(lock(&list).entries().len(), 1);
        let mut bye = BYE.to_vec();
        bye.extend([0x67, 0x40]);
        // From anywhere, including the server's own address: BYE no longer delists, since a
        // spoofed UDP source could otherwise send it for any server.
        handle_udp(&socket, &list, &bye, from);
        handle_udp(&socket, &list, &bye, "198.51.100.9:9".parse().unwrap());
        assert_eq!(lock(&list).entries().len(), 1);
    }

    #[test]
    fn list_replies_are_throttled_per_address() {
        let list: SharedList = Arc::new(Mutex::new(ServerList::default()));
        lock(&list).beat("10.0.0.1".parse().unwrap(), entry("s", 0, 0, false, "", 1));
        let socket = UdpSocket::bind("127.0.0.1:0").unwrap();
        let asker = UdpSocket::bind("127.0.0.1:0").unwrap();
        asker.set_read_timeout(Some(Duration::from_millis(200))).unwrap();
        let from = asker.local_addr().unwrap();
        handle_udp(&socket, &list, LIST, from);
        let mut buf = [0u8; 512];
        assert!(asker.recv_from(&mut buf).is_ok(), "the first query gets an answer");
        handle_udp(&socket, &list, LIST, from);
        assert!(asker.recv_from(&mut buf).is_err(), "a second query right away doesn't");
    }
}
