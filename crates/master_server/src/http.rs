//! The HTTP side: requests become [`Req`], handlers answer with [`Resp`] (so tests call them
//! without sockets), [`serve`] runs them on worker threads with `tiny_http`.
//!
//! Plain HTTP only: in production the master runs behind a reverse proxy that terminates
//! TLS (see docs/MODDING.md), with `trust_proxy` so rate limits see the players' addresses.

use std::{
    collections::HashMap,
    io::Read,
    net::{IpAddr, Ipv4Addr},
    sync::{Arc, Mutex},
};

use game_auth::{Identity, api::MAX_BODY_BYTES};

use crate::{auth::RateLimits, config::Config, db::Db, list::SharedList};

/// Everything the handlers share.
pub struct Master {
    pub config: Config,
    /// Signs session tokens and tickets.
    pub key: Identity,
    pub db: Mutex<Db>,
    pub list: SharedList,
    pub limits: Mutex<RateLimits>,
}

impl Master {
    pub fn public_key(&self) -> [u8; 32] {
        self.key.public_key()
    }
}

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

/// Reads a `tiny_http` request.
fn read_request(request: &mut tiny_http::Request, trust_proxy: bool) -> Result<Req, Resp> {
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
    if trust_proxy {
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
    let mut body = Vec::new();
    request
        .as_reader()
        .take(MAX_BODY_BYTES as u64 + 1)
        .read_to_end(&mut body)
        .map_err(|_| Resp::error(400, "can't read the request"))?;
    if body.len() > MAX_BODY_BYTES {
        return Err(Resp::error(413, "request too big"));
    }
    req.body = body;
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
                        let resp = match read_request(&mut request, master.config.trust_proxy) {
                            Ok(req) => handle(&master, &req),
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
}
