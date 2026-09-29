//! The HTTP side: requests become [`Req`], handlers answer with [`Resp`] (so tests call them
//! without sockets), [`serve`] runs them on worker threads with `tiny_http`.
//!
//! Plain HTTP only: in production the master runs behind a reverse proxy that terminates
//! TLS (see docs/MODDING.md), with `trust_proxy` so rate limits see the players' addresses.
//! `tiny_http` doesn't expose the accepted sockets, so it can't give us OS-level read/write
//! timeouts or a hard connection cap; [`ADMISSION`] and [`BODY_READ_TIMEOUT`] are the
//! practical substitute (see `game_auth::admission`).

use std::{
    collections::HashMap,
    io::Read,
    net::{IpAddr, Ipv4Addr},
    sync::{Arc, Mutex, PoisonError},
    time::{Duration, Instant},
};

use game_auth::{Identity, admission::Limiter, api::MAX_BODY_BYTES};

use crate::{auth::RateLimits, config::Config, db::Db, list::SharedList};

/// Everything the handlers share.
pub struct Master {
    pub config: Config,
    /// Signs session tokens and tickets.
    pub key: Identity,
    pub db: Mutex<Db>,
    pub list: SharedList,
    pub limits: Mutex<RateLimits>,
    /// Bounds how many requests are worked on at once, in total and per address (S8/S22: no
    /// real connection cap is reachable through `tiny_http`, see the module docs).
    pub admission: Limiter,
}

impl Master {
    pub fn public_key(&self) -> [u8; 32] {
        self.key.public_key()
    }
}

/// Locks a mutex, recovering from poisoning instead of panicking: one request that panics
/// while holding a lock must not take every other request down with it (they'd all panic on
/// the same poisoned lock forever after). The data can't be left in a worse state than a
/// normal early return would leave it in, since every write here goes through `rusqlite`
/// (itself `?`-based, not panic-based) or plain map/vec operations.
pub fn lock<T>(mutex: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(PoisonError::into_inner)
}

/// Total, and per source address, requests admitted at once (see the module docs).
pub(crate) const MAX_TOTAL_REQUESTS: u32 = 256;
pub(crate) const MAX_REQUESTS_PER_IP: u32 = 32;

/// How long we wait, in total, for a request's body to arrive. `tiny_http` gives no way to
/// set a real socket read timeout (see the module docs), so this bounds our own reads
/// instead: it stops a body that trickles in from occupying a worker thread forever, though a
/// peer that sends nothing at all after the headers can still block on the first read (the
/// admission cap above limits how many of those can pile up at once).
const BODY_READ_TIMEOUT: Duration = Duration::from_secs(20);

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Method {
    Get,
    Post,
    Other,
}

/// A request, read.
#[derive(Clone, Debug)]
pub struct Req {
    pub method: Method,
    /// Without the query.
    pub path: String,
    pub query: HashMap<String, String>,
    /// Lowercase names.
    pub headers: HashMap<String, String>,
    pub body: Vec<u8>,
    /// The client's address (from `X-Forwarded-For` behind a trusted proxy).
    pub ip: IpAddr,
}

impl Req {
    pub fn new(method: Method, url: &str) -> Self {
        let (path, query) = url.split_once('?').unwrap_or((url, ""));
        Self {
            method,
            path: path.to_string(),
            query: parse_form(query),
            headers: HashMap::new(),
            body: Vec::new(),
            ip: Ipv4Addr::LOCALHOST.into(),
        }
    }

    pub fn header(&self, name: &str) -> Option<&str> {
        self.headers.get(&name.to_ascii_lowercase()).map(String::as_str)
    }

    /// `Authorization: Bearer <token>`.
    pub fn bearer(&self) -> Option<&str> {
        self.header("authorization")?.strip_prefix("Bearer ").map(str::trim)
    }

    pub fn cookie(&self, name: &str) -> Option<String> {
        self.header("cookie")?.split(';').find_map(|pair| {
            let (key, value) = pair.trim().split_once('=')?;
            (key == name).then(|| value.to_string())
        })
    }

    /// The body as a form (`application/x-www-form-urlencoded`).
    pub fn form(&self) -> HashMap<String, String> {
        parse_form(&String::from_utf8_lossy(&self.body))
    }

    pub fn json<T: serde::de::DeserializeOwned>(&self) -> Result<T, Resp> {
        serde_json::from_slice(&self.body).map_err(|err| Resp::error(400, &format!("bad request: {err}")))
    }
}

/// A response.
#[derive(Clone, Debug)]
pub struct Resp {
    pub status: u16,
    pub content_type: &'static str,
    pub body: Vec<u8>,
    pub headers: Vec<(String, String)>,
}

impl Resp {
    pub fn json(status: u16, value: &impl serde::Serialize) -> Self {
        Self {
            status,
            content_type: "application/json",
            body: serde_json::to_vec(value).unwrap_or_default(),
            headers: vec![("Cache-Control".into(), "no-store".into())],
        }
    }

    pub fn ok(value: &impl serde::Serialize) -> Self {
        Self::json(200, value)
    }

    pub fn error(status: u16, message: &str) -> Self {
        Self::json(status, &game_auth::api::ApiError { error: message.to_string() })
    }

    pub fn html(status: u16, body: String) -> Self {
        Self {
            status,
            content_type: "text/html; charset=utf-8",
            body: body.into_bytes(),
            headers: vec![
                (
                    "Content-Security-Policy".into(),
                    "default-src 'none'; style-src 'unsafe-inline'; img-src 'self' data:; form-action 'self'; frame-ancestors 'none'; base-uri 'none'".into(),
                ),
                ("Referrer-Policy".into(), "same-origin".into()),
                // Pages can show secrets once (API keys, one-time passwords, recovery codes)
                // and depend on who is logged in: never cached.
                ("Cache-Control".into(), "no-store".into()),
            ],
        }
    }

    pub fn redirect(to: &str) -> Self {
        Self {
            status: 303,
            content_type: "text/plain; charset=utf-8",
            body: Vec::new(),
            headers: vec![("Location".into(), to.to_string())],
        }
    }

    pub fn with_header(mut self, name: &str, value: String) -> Self {
        self.headers.push((name.to_string(), value));
        self
    }

    #[cfg(test)]
    pub fn text(&self) -> String {
        String::from_utf8_lossy(&self.body).into_owned()
    }
}

/// `%XX` and `+` decoded.
pub fn url_decode(text: &str) -> String {
    let bytes = text.as_bytes();
    let digit = |b: u8| (b as char).to_digit(16).map(|d| d as u8);
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        let decoded = (bytes[i] == b'%' && i + 2 < bytes.len())
            .then(|| Some(digit(bytes[i + 1])? << 4 | digit(bytes[i + 2])?))
            .flatten();
        match (bytes[i], decoded) {
            (_, Some(b)) => {
                out.push(b);
                i += 2;
            }
            (b'+', None) => out.push(b' '),
            (b, None) => out.push(b),
        }
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

/// Percent-encodes a path part.
pub fn url_encode(text: &str) -> String {
    text.bytes()
        .map(|b| match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => (b as char).to_string(),
            _ => format!("%{b:02X}"),
        })
        .collect()
}

pub fn parse_form(text: &str) -> HashMap<String, String> {
    text.split('&')
        .filter(|pair| !pair.is_empty())
        .filter_map(|pair| {
            let (key, value) = pair.split_once('=').unwrap_or((pair, ""));
            Some((url_decode(key), url_decode(value)))
        })
        .collect()
}

/// Whether `peer` (the request's real, immediate TCP source) is one we take
/// `X-Forwarded-For`/`X-Real-IP` from. Honouring those headers from just anyone would let a
/// client that reaches this port directly spoof its address and dodge rate limits; only the
/// configured reverse proxy (loopback by default: that's how the documented deployment runs)
/// is trusted to have set them correctly.
fn is_trusted_proxy(config: &Config, peer: IpAddr) -> bool {
    if !config.trust_proxy {
        return false;
    }
    if config.trusted_proxies.is_empty() { peer.is_loopback() } else { config.trusted_proxies.contains(&peer) }
}

/// Reads a request's body with an overall wall-clock deadline (see [`BODY_READ_TIMEOUT`]) and
/// the usual byte cap.
fn read_body(request: &mut tiny_http::Request, max_len: usize) -> Result<Vec<u8>, Resp> {
    let mut reader = request.as_reader().take(max_len as u64 + 1);
    let deadline = Instant::now() + BODY_READ_TIMEOUT;
    let mut body = Vec::new();
    let mut buf = [0u8; 8192];
    loop {
        if Instant::now() > deadline {
            return Err(Resp::error(408, "the request body took too long"));
        }
        let n = reader.read(&mut buf).map_err(|_| Resp::error(400, "can't read the request"))?;
        if n == 0 {
            break;
        }
        body.extend_from_slice(&buf[..n]);
        if body.len() > max_len {
            return Err(Resp::error(413, "request too big"));
        }
    }
    Ok(body)
}

/// Reads a `tiny_http` request.
fn read_request(request: &mut tiny_http::Request, config: &Config) -> Result<Req, Resp> {
    let method = match request.method() {
        tiny_http::Method::Get | tiny_http::Method::Head => Method::Get,
        tiny_http::Method::Post => Method::Post,
        _ => Method::Other,
    };
    let mut req = Req::new(method, request.url());
    for header in request.headers() {
        req.headers.insert(header.field.as_str().as_str().to_ascii_lowercase(), header.value.as_str().to_string());
    }
    req.ip = request.remote_addr().map_or(Ipv4Addr::LOCALHOST.into(), |a| a.ip());
    if is_trusted_proxy(config, req.ip) {
        // The proxy appends the address it saw last.
        if let Some(ip) = req
            .header("x-forwarded-for")
            .and_then(|v| v.rsplit(',').next())
            .and_then(|v| v.trim().parse().ok())
            .or_else(|| req.header("x-real-ip").and_then(|v| v.trim().parse().ok()))
        {
            req.ip = ip;
        }
    }
    if request.body_length().is_some_and(|len| len > MAX_BODY_BYTES) {
        return Err(Resp::error(413, "request too big"));
    }
    req.body = read_body(request, MAX_BODY_BYTES)?;
    Ok(req)
}

fn respond(request: tiny_http::Request, resp: Resp) {
    let mut response = tiny_http::Response::from_data(resp.body).with_status_code(tiny_http::StatusCode(resp.status));
    let mut headers = resp.headers;
    headers.push(("Content-Type".into(), resp.content_type.into()));
    headers.push(("X-Content-Type-Options".into(), "nosniff".into()));
    for (name, value) in headers {
        if let Ok(header) = tiny_http::Header::from_bytes(name.as_bytes(), value.as_bytes()) {
            response.add_header(header);
        }
    }
    let _ = request.respond(response);
}

/// Answers requests on `server` with `workers` threads, forever.
pub fn serve(server: tiny_http::Server, master: Arc<Master>, workers: usize) {
    let server = Arc::new(server);
    let mut threads = Vec::new();
    for i in 0..workers.max(1) {
        let (server, master) = (server.clone(), master.clone());
        threads.push(
            std::thread::Builder::new()
                .name(format!("http {i}"))
                .spawn(move || {
                    while let Ok(mut request) = server.recv() {
                        let ip = request.remote_addr().map_or(Ipv4Addr::LOCALHOST.into(), |a| a.ip());
                        let Some(_admitted) = master.admission.enter(ip) else {
                            respond(request, Resp::error(429, "too many requests from your address; try again shortly"));
                            continue;
                        };
                        let resp = match read_request(&mut request, &master.config) {
                            Ok(req) => match std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| handle(&master, &req))) {
                                Ok(resp) => resp,
                                Err(_) => {
                                    eprintln!("error: a request handler panicked on {} {}", req.path, req.ip);
                                    Resp::error(500, "internal error")
                                }
                            },
                            Err(resp) => resp,
                        };
                        respond(request, resp);
                    }
                })
                .expect("a thread"),
        );
    }
    for thread in threads {
        let _ = thread.join();
    }
}

/// Routes a request.
pub fn handle(master: &Master, req: &Req) -> Resp {
    let result = if let Some(api) = req.path.strip_prefix(game_auth::api::API_PREFIX) {
        crate::api::handle(master, req, api)
    } else {
        crate::web::handle(master, req)
    };
    result.unwrap_or_else(|resp| resp)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn forms_and_urls() {
        assert_eq!(url_decode("a+b%20c%2Fd%zz%"), "a b c/d%zz%");
        let form = parse_form("name=Alice&password=p%26ss+word&empty=&flag");
        assert_eq!(form["name"], "Alice");
        assert_eq!(form["password"], "p&ss word");
        assert_eq!(form["empty"], "");
        assert_eq!(form["flag"], "");
        assert_eq!(url_encode("a b/c"), "a%20b%2Fc");
        let mut req = Req::new(Method::Get, "/players/Bob?sort=kills");
        assert_eq!((req.path.as_str(), req.query["sort"].as_str()), ("/players/Bob", "kills"));
        req.headers.insert("cookie".into(), "x=1; bf2r_session=abc".into());
        assert_eq!(req.cookie("bf2r_session").as_deref(), Some("abc"));
        req.headers.insert("authorization".into(), "Bearer tok".into());
        assert_eq!(req.bearer(), Some("tok"));
    }

    fn tiny_request(remote: &str, headers: &[(&str, &str)]) -> tiny_http::Request {
        let mut test = tiny_http::TestRequest::new()
            .with_method(tiny_http::Method::Post)
            .with_path("/api/v1/login")
            .with_remote_addr(remote.parse().unwrap())
            .with_body("{}");
        for (name, value) in headers {
            test = test.with_header(tiny_http::Header::from_bytes(name.as_bytes(), value.as_bytes()).unwrap());
        }
        test.into()
    }

    #[test]
    fn x_forwarded_for_only_from_a_trusted_proxy() {
        let mut config = Config::default();
        config.trust_proxy = true;
        // Loopback is trusted by default (the documented reverse-proxy setup).
        let mut request = tiny_request("127.0.0.1:9", &[("X-Forwarded-For", "203.0.113.9")]);
        let req = read_request(&mut request, &config).unwrap();
        assert_eq!(req.ip, "203.0.113.9".parse::<IpAddr>().unwrap());
        // A direct client (not the proxy) can't spoof its address this way.
        let mut request = tiny_request("198.51.100.1:9", &[("X-Forwarded-For", "203.0.113.9")]);
        let req = read_request(&mut request, &config).unwrap();
        assert_eq!(req.ip, "198.51.100.1".parse::<IpAddr>().unwrap());
        // Without trust_proxy at all, nobody's X-Forwarded-For is honoured, even loopback's.
        let mut off = Config::default();
        let mut request = tiny_request("127.0.0.1:9", &[("X-Forwarded-For", "203.0.113.9")]);
        assert_eq!(read_request(&mut request, &off).unwrap().ip, "127.0.0.1".parse::<IpAddr>().unwrap());
        // An explicit allow-list can trust a proxy that isn't on this machine.
        off.trust_proxy = true;
        off.trusted_proxies = vec!["10.0.0.5".parse().unwrap()];
        let mut request = tiny_request("10.0.0.5:9", &[("X-Forwarded-For", "203.0.113.9")]);
        assert_eq!(read_request(&mut request, &off).unwrap().ip, "203.0.113.9".parse::<IpAddr>().unwrap());
        let mut request = tiny_request("127.0.0.1:9", &[("X-Forwarded-For", "203.0.113.9")]);
        assert_eq!(read_request(&mut request, &off).unwrap().ip, "127.0.0.1".parse::<IpAddr>().unwrap(), "loopback isn't in the allow-list");
    }

    #[test]
    fn body_too_big_is_rejected() {
        let config = Config::default();
        let big = "x".repeat(MAX_BODY_BYTES + 1);
        let test = tiny_http::TestRequest::new().with_method(tiny_http::Method::Post).with_path("/x").with_body(Box::leak(big.into_boxed_str()));
        let mut request: tiny_http::Request = test.into();
        assert_eq!(read_request(&mut request, &config).unwrap_err().status, 413);
    }

    #[test]
    fn poisoned_mutex_recovers() {
        let mutex = Mutex::new(5);
        let _ = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            let _guard = mutex.lock().unwrap();
            panic!("boom");
        }));
        assert!(mutex.is_poisoned());
        assert_eq!(*lock(&mutex), 5);
    }
}
