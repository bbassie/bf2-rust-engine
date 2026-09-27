//! Optional master server: game servers announce themselves with a heartbeat every 30 s,
//! and the menu's server browser fetches the list, then asks each server for its details
//! directly (LAN discovery protocol, `game_shared::discovery`). The game works without it.
//!
//! ```text
//! master                         # 127.0.0.1:16580 (this machine only)
//! master --bind 0.0.0.0:16580    # for everyone (open the port)
//! ```
//!
//! UDP messages (integers little-endian):
//!
//! ```text
//! server -> master: "BF2R-HB"  game_port u16  query_port u16     heartbeat
//! server -> master: "BF2R-BYE" game_port u16                     shutting down
//! client -> master: "BF2R-LIST"                                  list please
//! master -> client: "BF2R-SERVERS" then "ip game_port query_port\n" per server
//! ```
//!
//! A server is dropped after [`TIMEOUT`] without a heartbeat. The address is the one the
//! heartbeat came from.

use std::{
    collections::HashMap,
    net::{IpAddr, SocketAddr, UdpSocket},
    time::{Duration, Instant},
};

const HEARTBEAT: &[u8] = b"BF2R-HB";
const BYE: &[u8] = b"BF2R-BYE";
const LIST: &[u8] = b"BF2R-LIST";
const SERVERS: &[u8] = b"BF2R-SERVERS";
/// Servers not heard from for this long are forgotten (they beat every 30 s).
const TIMEOUT: Duration = Duration::from_secs(95);
/// Servers listed per answer, to stay within one datagram.
const MAX_LISTED: usize = 60;
/// Servers per address, against someone filling the list.
const MAX_PER_ADDRESS: usize = 16;

fn main() {
    let mut bind: SocketAddr = "127.0.0.1:16580".parse().unwrap();
    let mut args = std::env::args().skip(1);
    while let Some(arg) = args.next() {
        match (arg.as_str(), args.next()) {
            ("--bind", Some(value)) => match value.parse() {
                Ok(address) => bind = address,
                Err(err) => return eprintln!("--bind {value}: {err}"),
            },
            _ => {
                return eprintln!("usage: master [--bind <ip:port>]   (default 127.0.0.1:16580)");
            }
        }
    }
    let socket = match UdpSocket::bind(bind) {
        Ok(socket) => socket,
        Err(err) => return eprintln!("can't listen on {bind}: {err}"),
    };
    socket.set_read_timeout(Some(Duration::from_secs(5))).unwrap();
    println!("master server on UDP {bind}");
    let mut servers: HashMap<(IpAddr, u16), (u16, Instant)> = HashMap::new();
    let mut buffer = [0u8; 256];
    loop {
        let received = socket.recv_from(&mut buffer);
        servers.retain(|(ip, port), (_, seen)| {
            let alive = seen.elapsed() < TIMEOUT;
            if !alive {
                println!("{ip}:{port} timed out");
            }
            alive
        });
        let Ok((len, from)) = received else {
            continue;
        };
        let packet = &buffer[..len];
        let u16_at = |bytes: &[u8], i: usize| bytes.get(i..i + 2).map(|b| u16::from_le_bytes([b[0], b[1]]));
        if let Some(rest) = packet.strip_prefix(HEARTBEAT) {
            let (Some(game_port), Some(query_port)) = (u16_at(rest, 0), u16_at(rest, 2)) else {
                continue;
            };
            let key = (from.ip(), game_port);
            let known = servers.contains_key(&key);
            if !known && servers.keys().filter(|(ip, _)| *ip == from.ip()).count() >= MAX_PER_ADDRESS {
                continue;
            }
            servers.insert(key, (query_port, Instant::now()));
            if !known {
                println!("{}:{game_port} registered (queries on {query_port})", from.ip());
            }
        } else if let Some(rest) = packet.strip_prefix(BYE) {
            if let Some(game_port) = u16_at(rest, 0)
                && servers.remove(&(from.ip(), game_port)).is_some()
            {
                println!("{}:{game_port} left", from.ip());
            }
        } else if packet == LIST {
            let mut answer = SERVERS.to_vec();
            for ((ip, game_port), (query_port, _)) in servers.iter().take(MAX_LISTED) {
                answer.extend(format!("{ip} {game_port} {query_port}\n").as_bytes());
            }
            let _ = socket.send_to(&answer, from);
        }
    }
}
