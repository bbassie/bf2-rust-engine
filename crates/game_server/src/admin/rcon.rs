//! The remote console, after BF2's ModManager rcon: a line-based TCP protocol on port 4711.
//!
//! ```text
//! server: ### Battlefield 2 ModManager Rcon v1.0.
//!         ### Digest seed: <seed>
//!         <empty line>
//! client: login <md5 hex of seed + password>
//! server: Authentication successful, rcon ready.
//! client: \x02players                            (\x02: end the answer with \x04)
//! server: <answer>\x04
//! ```
//!
//! Connections are served on their own threads; commands run on the main thread between
//! frames (see [`process_requests`]). `server rcon` is a small client ([`run_client`]).
//!
//! Only the digest logs in (never the plain password, which would cross the network), compared
//! in constant time. Failed logins lock the address out for a while (see
//! [`crate::limits::LoginBackoff`]), each address has a few sessions and connections a
//! minute at most, and idle sessions are closed, so nobody can hold every session.

use std::{
    collections::HashMap,
    io::{self, BufRead, Read, Write},
    net::{IpAddr, Ipv4Addr, SocketAddr, TcpListener, TcpStream},
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, Ordering},
        mpsc::{self, Receiver, Sender},
    },
    thread,
    time::{Duration, Instant},
};

use bevy::prelude::*;

use super::AdminSettings;
use crate::limits::{Failed, LoginBackoff, Rate, RateLimiter, constant_time_eq};

/// BF2's remote console port.
pub const DEFAULT_PORT: u16 = 4711;
const MAX_SESSIONS: usize = 8;
/// Sessions one address may hold at once.
const MAX_SESSIONS_PER_ADDRESS: usize = 2;
/// A connection that hasn't logged in by then is closed.
const LOGIN_TIMEOUT: Duration = Duration::from_secs(30);
/// A logged-in session that sends nothing for this long is closed.
const IDLE_TIMEOUT: Duration = Duration::from_secs(10 * 60);
const END_OF_ANSWER: u8 = 0x04;
const NON_INTERACTIVE: char = '\x02';

/// The listening remote console. Dropping it closes the listener and every connection.
#[derive(Resource)]
pub struct RconServer {
    requests: Mutex<Receiver<Request>>,
    stop: Arc<AtomicBool>,
}

/// A command from a connection, waiting for the main thread.
struct Request {
    line: String,
    peer: SocketAddr,
    reply: Sender<String>,
}

impl RconServer {
    pub fn start(settings: &AdminSettings) -> io::Result<Self> {
        let ip = if settings.rcon_public { Ipv4Addr::UNSPECIFIED } else { Ipv4Addr::LOCALHOST };
        let listener = TcpListener::bind((ip, settings.rcon_port))?;
        listener.set_nonblocking(true)?;
        let (requests, receiver) = mpsc::channel();
        let stop = Arc::new(AtomicBool::new(false));
        let password = settings.password.clone();
        let flag = stop.clone();
        thread::Builder::new()
            .name("rcon".into())
            .spawn(move || accept(listener, password, requests, flag))?;
        Ok(Self {
            requests: Mutex::new(receiver),
            stop,
        })
    }
}

impl Drop for RconServer {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
    }
}

/// What the connections share: sessions per address, connection rates and failed logins.
struct Guard {
    start: Instant,
    sessions: HashMap<IpAddr, usize>,
    connections: RateLimiter<IpAddr>,
    logins: LoginBackoff<IpAddr>,
}

impl Guard {
    fn now(&self) -> f64 {
        self.start.elapsed().as_secs_f64()
    }

    /// Whether a new connection from `ip` is let in (and counts it if so); why not otherwise.
    fn admit(&mut self, ip: IpAddr) -> Result<(), &'static str> {
        let now = self.now();
        if self.logins.locked(ip, now).is_some() {
            return Err("Too many failed logins. Try again later.");
        }
        if self.sessions.values().sum::<usize>() >= MAX_SESSIONS
            || self.sessions.get(&ip).copied().unwrap_or(0) >= MAX_SESSIONS_PER_ADDRESS
        {
            return Err("Too many sessions.");
        }
        if !self.connections.allow(ip, Rate::RCON_CONNECTIONS, now) {
            return Err("Too many connections. Try again later.");
        }
        *self.sessions.entry(ip).or_default() += 1;
        Ok(())
    }

    fn leave(&mut self, ip: IpAddr) {
        if let Some(count) = self.sessions.get_mut(&ip) {
            *count = count.saturating_sub(1);
            if *count == 0 {
                self.sessions.remove(&ip);
            }
        }
    }
}

fn accept(listener: TcpListener, password: String, requests: Sender<Request>, stop: Arc<AtomicBool>) {
    let guard = Arc::new(Mutex::new(Guard {
        start: Instant::now(),
        sessions: HashMap::new(),
        connections: RateLimiter::default(),
        logins: LoginBackoff::default(),
    }));
    while !stop.load(Ordering::Relaxed) {
        match listener.accept() {
            Ok((mut stream, peer)) => {
                let admitted = guard.lock().map_or(Err("Server error."), |mut g| g.admit(peer.ip()));
                if let Err(why) = admitted {
                    debug!("rcon: {peer} refused: {why}");
                    let _ = stream.set_nonblocking(false);
                    let _ = stream.set_write_timeout(Some(Duration::from_millis(200)));
                    let _ = writeln!(stream, "{why}");
                    continue;
                }
                let (password, requests, stop, guard) = (password.clone(), requests.clone(), stop.clone(), guard.clone());
                thread::spawn(move || {
                    info!("rcon: {peer} connected");
                    if let Err(err) = session(stream, peer, &password, &requests, &stop, &guard) {
                        debug!("rcon: {peer}: {err}");
                    }
                    info!("rcon: {peer} disconnected");
                    if let Ok(mut guard) = guard.lock() {
                        guard.leave(peer.ip());
                    }
                });
            }
            Err(err) if err.kind() == io::ErrorKind::WouldBlock => thread::sleep(Duration::from_millis(100)),
            Err(err) => {
                warn!("rcon: {err}");
                thread::sleep(Duration::from_millis(500));
            }
        }
    }
}

/// Whether a login answer is the digest of the seed and the password (only the digest: the
/// plain password is refused), compared in constant time.
fn login_accepted(answer: &str, digest: &str) -> bool {
    constant_time_eq(answer.trim().to_ascii_lowercase().as_bytes(), digest.as_bytes())
}

fn session(
    mut stream: TcpStream,
    peer: SocketAddr,
    password: &str,
    requests: &Sender<Request>,
    stop: &AtomicBool,
    guard: &Mutex<Guard>,
) -> io::Result<()> {
    // Accepted sockets may inherit the listener's non-blocking mode.
    stream.set_nonblocking(false)?;
    stream.set_read_timeout(Some(Duration::from_millis(500)))?;
    stream.set_write_timeout(Some(Duration::from_secs(10)))?;
    let _ = stream.set_nodelay(true);
    let seed: String = (0..16).map(|_| fastrand::alphanumeric()).collect();
    write!(stream, "### Battlefield 2 ModManager Rcon v1.0.\n### Digest seed: {seed}\n\n")?;
    let digest = md5_hex(format!("{seed}{password}").as_bytes());
    let mut reader = Reader::default();
    let mut logged_in = false;
    let mut last_line = Instant::now();
    loop {
        if stop.load(Ordering::Relaxed) {
            let _ = stream.write_all(b"Server shutting down.\n");
            return Ok(());
        }
        let Some(line) = reader.line(&mut stream)? else {
            let idle = last_line.elapsed();
            if !logged_in && idle > LOGIN_TIMEOUT {
                let _ = stream.write_all(b"Login timed out.\n");
                return Ok(());
            }
            if idle > IDLE_TIMEOUT {
                let _ = stream.write_all(b"Idle for too long; closing.\n");
                return Ok(());
            }
            continue;
        };
        last_line = Instant::now();
        if !logged_in {
            match line.trim().strip_prefix("login ") {
                Some(answer) if login_accepted(answer, &digest) => {
                    logged_in = true;
                    if let Ok(mut guard) = guard.lock() {
                        guard.logins.succeed(peer.ip());
                    }
                    info!("rcon: {peer} logged in");
                    stream.write_all(b"Authentication successful, rcon ready.\n")?;
                }
                Some(_) => {
                    let failed = guard.lock().ok().map(|mut g| {
                        let now = g.now();
                        g.logins.fail(peer.ip(), now)
                    });
                    thread::sleep(Duration::from_secs(1));
                    stream.write_all(b"Authentication failed.\n")?;
                    match failed {
                        Some(Failed::TriesLeft(_)) => info!("rcon: {peer} failed to log in"),
                        Some(Failed::LockedOut(seconds)) => {
                            warn!("rcon: {peer} failed to log in too often; locked out for {seconds:.0} s");
                            return Ok(());
                        }
                        None => return Ok(()),
                    }
                }
                None => stream.write_all(b"Authentication required: login <digest>\n")?,
            }
            continue;
        }
        let (line, terminate) = match line.strip_prefix(NON_INTERACTIVE) {
            Some(rest) => (rest.trim(), true),
            None => (line.trim(), false),
        };
        if matches!(line, "quit" | "exit" | "logout") {
            return Ok(());
        }
        let (reply, answer) = mpsc::channel();
        let request = Request {
            line: line.to_string(),
            peer,
            reply,
        };
        if requests.send(request).is_err() {
            return Ok(());
        }
        let mut text = answer
            .recv_timeout(Duration::from_secs(10))
            .unwrap_or_else(|_| "The server did not answer.".into());
        if !text.ends_with('\n') {
            text.push('\n');
        }
        stream.write_all(text.as_bytes())?;
        if terminate {
            stream.write_all(&[END_OF_ANSWER])?;
        }
    }
}

/// Runs the commands that came in over the remote console.
pub(super) fn process_requests(world: &mut World) {
    let requests: Vec<Request> = {
        let server = world.resource::<RconServer>();
        let Ok(receiver) = server.requests.lock() else {
            return;
        };
        receiver.try_iter().take(32).collect()
    };
    for request in requests {
        let reply = super::commands::execute(world, &request.line, &format!("rcon {}", request.peer));
        let _ = request.reply.send(reply);
    }
}

/// Splits a byte stream into lines or answers.
#[derive(Default)]
struct Reader {
    buffer: Vec<u8>,
}

impl Reader {
    /// Everything up to the next `delimiter`; `None` if nothing complete arrived within the
    /// read timeout. The end of the stream is an error.
    fn until(&mut self, stream: &mut TcpStream, delimiter: u8) -> io::Result<Option<String>> {
        loop {
            if let Some(end) = self.buffer.iter().position(|b| *b == delimiter) {
                let bytes: Vec<u8> = self.buffer.drain(..=end).collect();
                let text = String::from_utf8_lossy(&bytes[..end]);
                return Ok(Some(text.trim_end_matches('\r').to_string()));
            }
            if self.buffer.len() > 64 * 1024 {
                return Err(io::Error::other("line too long"));
            }
            let mut chunk = [0; 1024];
            match stream.read(&mut chunk) {
                Ok(0) => return Err(io::ErrorKind::UnexpectedEof.into()),
                Ok(n) => self.buffer.extend_from_slice(&chunk[..n]),
                Err(err) if matches!(err.kind(), io::ErrorKind::WouldBlock | io::ErrorKind::TimedOut) => {
                    return Ok(None);
                }
                Err(err) => return Err(err),
            }
        }
    }

    fn line(&mut self, stream: &mut TcpStream) -> io::Result<Option<String>> {
        self.until(stream, b'\n')
    }
}

/// `server rcon`: logs in to a remote console, runs each command and prints the answers.
/// Without commands, reads them from standard input.
pub fn run_client(host: &str, port: u16, password: &str, commands: &[String]) -> io::Result<()> {
    let mut stream = TcpStream::connect((host, port))?;
    stream.set_read_timeout(Some(Duration::from_secs(15)))?;
    let mut reader = Reader::default();
    let timeout = || io::Error::from(io::ErrorKind::TimedOut);
    let mut seed = None;
    loop {
        let line = reader.line(&mut stream)?.ok_or_else(timeout)?;
        if let Some(value) = line.strip_prefix("### Digest seed: ") {
            seed = Some(value.trim().to_string());
        }
        if line.is_empty() {
            break;
        }
    }
    let seed = seed.ok_or_else(|| io::Error::other("no digest seed in the greeting"))?;
    writeln!(stream, "login {}", md5_hex(format!("{seed}{password}").as_bytes()))?;
    let answer = reader.line(&mut stream)?.ok_or_else(timeout)?;
    if !answer.starts_with("Authentication successful") {
        return Err(io::Error::other(answer));
    }
    let mut run = |command: &str| -> io::Result<()> {
        write!(stream, "{NON_INTERACTIVE}{command}\n")?;
        let reply = reader.until(&mut stream, END_OF_ANSWER)?.ok_or_else(timeout)?;
        print!("{reply}");
        io::stdout().flush()
    };
    if commands.is_empty() {
        for line in io::stdin().lock().lines() {
            let line = line?;
            if matches!(line.trim(), "quit" | "exit") {
                break;
            }
            run(line.trim())?;
        }
    } else {
        for command in commands {
            println!("> {command}");
            run(command)?;
        }
    }
    Ok(())
}

/// MD5 as lowercase hex (RFC 1321), for BF2's rcon login digest.
pub fn md5_hex(input: &[u8]) -> String {
    const SHIFTS: [u32; 64] = [
        7, 12, 17, 22, 7, 12, 17, 22, 7, 12, 17, 22, 7, 12, 17, 22, 5, 9, 14, 20, 5, 9, 14, 20, 5, 9,
        14, 20, 5, 9, 14, 20, 4, 11, 16, 23, 4, 11, 16, 23, 4, 11, 16, 23, 4, 11, 16, 23, 6, 10, 15,
        21, 6, 10, 15, 21, 6, 10, 15, 21, 6, 10, 15, 21,
    ];
    let constants: [u32; 64] =
        std::array::from_fn(|i| ((i as f64 + 1.0).sin().abs() * 4_294_967_296.0) as u32);
    let mut message = input.to_vec();
    message.push(0x80);
    while message.len() % 64 != 56 {
        message.push(0);
    }
    message.extend_from_slice(&(input.len() as u64).wrapping_mul(8).to_le_bytes());
    let mut state: [u32; 4] = [0x6745_2301, 0xefcd_ab89, 0x98ba_dcfe, 0x1032_5476];
    for block in message.chunks_exact(64) {
        let words: [u32; 16] =
            std::array::from_fn(|i| u32::from_le_bytes(block[i * 4..i * 4 + 4].try_into().unwrap()));
        let [mut a, mut b, mut c, mut d] = state;
        for i in 0..64 {
            let (f, g) = match i / 16 {
                0 => ((b & c) | (!b & d), i),
                1 => ((d & b) | (!d & c), (5 * i + 1) % 16),
                2 => (b ^ c ^ d, (3 * i + 5) % 16),
                _ => (c ^ (b | !d), (7 * i) % 16),
            };
            let f = f.wrapping_add(a).wrapping_add(constants[i]).wrapping_add(words[g]);
            a = d;
            d = c;
            c = b;
            b = b.wrapping_add(f.rotate_left(SHIFTS[i]));
        }
        for (s, v) in state.iter_mut().zip([a, b, c, d]) {
            *s = s.wrapping_add(v);
        }
    }
    state
        .iter()
        .flat_map(|w| w.to_le_bytes())
        .map(|b| format!("{b:02x}"))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_the_digest_logs_in() {
        let digest = md5_hex(b"seedsecret");
        assert!(login_accepted(&digest, &digest));
        assert!(login_accepted(&format!(" {} ", digest.to_uppercase()), &digest));
        assert!(!login_accepted("secret", &digest), "the plain password is refused");
        assert!(!login_accepted("", &digest));
        assert!(!login_accepted(&digest[..31], &digest));
    }

    #[test]
    fn addresses_get_few_sessions_and_are_locked_out_after_failed_logins() {
        let mut guard = Guard {
            start: Instant::now(),
            sessions: HashMap::new(),
            connections: RateLimiter::default(),
            logins: LoginBackoff::default(),
        };
        let (a, b): (IpAddr, IpAddr) = ("10.0.0.1".parse().unwrap(), "10.0.0.2".parse().unwrap());
        assert!(guard.admit(a).is_ok());
        assert!(guard.admit(a).is_ok());
        assert!(guard.admit(a).is_err(), "two sessions per address");
        assert!(guard.admit(b).is_ok(), "others still get in");
        guard.leave(a);
        assert!(guard.admit(a).is_ok());
        let now = guard.now();
        for _ in 0..LoginBackoff::<IpAddr>::FREE_ATTEMPTS {
            guard.logins.fail(b, now);
        }
        guard.leave(b);
        assert!(guard.admit(b).is_err(), "locked out");
    }

    #[test]
    fn md5_vectors() {
        assert_eq!(md5_hex(b""), "d41d8cd98f00b204e9800998ecf8427e");
        assert_eq!(md5_hex(b"abc"), "900150983cd24fb0d6963f7d28e17f72");
        assert_eq!(
            md5_hex(b"The quick brown fox jumps over the lazy dog"),
            "9e107d9d372bb6826bd81d3542a419d6"
        );
        assert_eq!(md5_hex(&[b'a'; 64]), "014842d480b571495a4a0363793f7367");
    }
}
