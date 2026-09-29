//! Identities and accounts, shared by the master server, game servers and clients. Nothing
//! here depends on the engine.
//!
//! - [`identity`]: every game server has a persistent Ed25519 key. Clients see its
//!   fingerprint before trusting a server with downloads, and servers prove they hold the key
//!   by signing a client's random challenge.
//! - [`token`]: the master server signs short-lived account tokens with its own key; game
//!   servers check them offline with the master's public key.
//! - [`api`]: the master server's REST API (JSON).
//! - [`ranks`]: the leveling curve (XP and ranks).

pub mod admission;
pub mod api;
pub mod identity;
pub mod ranks;
pub mod token;

pub use identity::{Identity, IdentityProof, fingerprint};

/// `N` random bytes from the operating system.
pub fn random_bytes<const N: usize>() -> [u8; N] {
    let mut bytes = [0u8; N];
    getrandom::fill(&mut bytes).expect("the operating system's random numbers");
    bytes
}

/// Lowercase hex.
pub fn hex(bytes: &[u8]) -> String {
    const DIGITS: &[u8; 16] = b"0123456789abcdef";
    let mut out = String::with_capacity(bytes.len() * 2);
    for b in bytes {
        out.push(DIGITS[(b >> 4) as usize] as char);
        out.push(DIGITS[(b & 15) as usize] as char);
    }
    out
}

/// Hex (either case) to bytes.
pub fn unhex(text: &str) -> Option<Vec<u8>> {
    let text = text.trim();
    if !text.len().is_multiple_of(2) {
        return None;
    }
    let digit = |c: u8| (c as char).to_digit(16).map(|d| d as u8);
    text.as_bytes()
        .chunks(2)
        .map(|pair| Some(digit(pair[0])? << 4 | digit(pair[1])?))
        .collect()
}

/// Hex of exactly `N` bytes.
pub fn unhex_array<const N: usize>(text: &str) -> Option<[u8; N]> {
    unhex(text)?.try_into().ok()
}

/// Seconds since 1970.
pub fn unix_now() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_secs())
}

/// Account names: 3 to 24 letters, digits, `_`, `-` and `.`, starting with a letter or digit.
pub fn validate_account_name(name: &str) -> Result<(), String> {
    let len = name.chars().count();
    if !(3..=24).contains(&len) {
        return Err("Names are 3 to 24 characters long.".into());
    }
    if !name.chars().all(|c| c.is_ascii_alphanumeric() || matches!(c, '_' | '-' | '.')) {
        return Err("Names may only use letters, digits, `_`, `-` and `.`.".into());
    }
    if !name.starts_with(|c: char| c.is_ascii_alphanumeric()) {
        return Err("Names start with a letter or digit.".into());
    }
    Ok(())
}

/// Passwords: 8 to 128 characters.
pub fn validate_password(password: &str) -> Result<(), String> {
    let len = password.chars().count();
    if len < 8 {
        return Err("Passwords have at least 8 characters.".into());
    }
    if len > 128 {
        return Err("Passwords have at most 128 characters.".into());
    }
    Ok(())
}

/// Whether `url` (`scheme://host[:port][/path]`) is safe for account traffic (passwords,
/// tokens, API keys): `https://` always, `http://` only to loopback, for local testing
/// without a reverse proxy. Used by both the game server's and the client's master-server
/// connection so neither silently talks accounts over plain HTTP to a real host.
pub fn https_or_loopback(url: &str) -> bool {
    let url = url.trim();
    if let Some(rest) = url.strip_prefix("https://") {
        return !rest.is_empty();
    }
    let Some(rest) = url.strip_prefix("http://") else {
        return false;
    };
    let host_port = rest.split(['/', '?', '#']).next().unwrap_or("");
    let host = match host_port.strip_prefix('[') {
        Some(inner) => inner.split(']').next().unwrap_or(""),
        None => host_port.split(':').next().unwrap_or(""),
    };
    matches!(host, "localhost" | "127.0.0.1" | "::1") || host.starts_with("127.")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hex_round_trip() {
        let bytes = [0u8, 1, 0xab, 0xff];
        assert_eq!(hex(&bytes), "0001abff");
        assert_eq!(unhex("0001ABff").unwrap(), bytes);
        assert_eq!(unhex("abc"), None);
        assert_eq!(unhex("zz"), None);
        assert_eq!(unhex_array::<2>("0001"), Some([0, 1]));
        assert_eq!(unhex_array::<3>("0001"), None);
    }

    #[test]
    fn names_and_passwords() {
        assert!(validate_account_name("alice").is_ok());
        assert!(validate_account_name("Bob_the-2nd.x").is_ok());
        for bad in ["al", "a very long name that goes on", "bad name", "<script>", "_under", "tab\t", "émile"] {
            assert!(validate_account_name(bad).is_err(), "{bad}");
        }
        assert!(validate_password("correct horse").is_ok());
        assert!(validate_password("short").is_err());
        assert!(validate_password(&"x".repeat(129)).is_err());
    }

    #[test]
    fn url_scheme() {
        assert!(https_or_loopback("https://master.example.com"));
        assert!(https_or_loopback("http://127.0.0.1:16581"));
        assert!(https_or_loopback("http://localhost:16581"));
        assert!(https_or_loopback("http://[::1]:16581"));
        assert!(https_or_loopback("http://127.5.6.7"));
        assert!(!https_or_loopback("http://master.example.com"));
        assert!(!https_or_loopback("http://10.0.0.5"));
        assert!(!https_or_loopback("ftp://x"));
        assert!(!https_or_loopback("https://"));
        assert!(!https_or_loopback(""));
    }
}
