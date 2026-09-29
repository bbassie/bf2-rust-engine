//! Passwords, tokens, API keys and login rate limits.

use std::{
    collections::HashMap,
    net::IpAddr,
    time::{Duration, Instant},
};

use argon2::{
    Argon2, PasswordHash, PasswordHasher, PasswordVerifier,
    password_hash::SaltString,
};
use game_auth::{hex, random_bytes};

/// Argon2id with the crate's defaults (19 MiB, 2 passes): tens of milliseconds per guess.
pub fn hash_password(password: &str) -> String {
    let salt = SaltString::encode_b64(&random_bytes::<16>()).expect("16 bytes make a salt");
    Argon2::default()
        .hash_password(password.as_bytes(), &salt)
        .expect("argon2 hashes any password")
        .to_string()
}

pub fn check_password(password: &str, hash: &str) -> bool {
    PasswordHash::new(hash).is_ok_and(|parsed| Argon2::default().verify_password(password.as_bytes(), &parsed).is_ok())
}

/// A hash to check against when the account doesn't exist, so a failed login takes as long
/// whether or not the name is taken.
pub fn dummy_hash() -> &'static str {
    static HASH: std::sync::OnceLock<String> = std::sync::OnceLock::new();
    HASH.get_or_init(|| hash_password("not a real password"))
}

/// A new random secret: refresh tokens, web sessions.
pub fn new_secret() -> String {
    hex(&random_bytes::<32>())
}

/// A new API key for a ranked server.
pub fn new_api_key() -> String {
    format!("bf2r_{}", hex(&random_bytes::<32>()))
}

/// Limits failed logins per address and per account name, and registrations per address.
#[derive(Default)]
pub struct RateLimits {
    failures_by_ip: HashMap<IpAddr, Vec<Instant>>,
    failures_by_name: HashMap<String, Vec<Instant>>,
    registrations: HashMap<IpAddr, Vec<Instant>>,
}

/// Failed logins within [`FAILURE_WINDOW`] allowed per address and per name.
const FAILURES_PER_IP: usize = 20;
const FAILURES_PER_NAME: usize = 8;
const FAILURE_WINDOW: Duration = Duration::from_secs(15 * 60);
const REGISTRATIONS_PER_IP: usize = 5;
const REGISTRATION_WINDOW: Duration = Duration::from_secs(3600);

fn recent(list: &mut Vec<Instant>, window: Duration) -> usize {
    list.retain(|t| t.elapsed() < window);
    list.len()
}

impl RateLimits {
    /// `Err(minutes to wait)` if this address or name failed too often lately.
    pub fn check_login(&mut self, ip: IpAddr, name: &str) -> Result<(), u64> {
        let by_ip = recent(self.failures_by_ip.entry(ip).or_default(), FAILURE_WINDOW);
        let by_name = recent(self.failures_by_name.entry(name.to_ascii_lowercase()).or_default(), FAILURE_WINDOW);
        if by_ip >= FAILURES_PER_IP || by_name >= FAILURES_PER_NAME {
            Err(FAILURE_WINDOW.as_secs() / 60)
        } else {
            Ok(())
        }
    }

    pub fn login_failed(&mut self, ip: IpAddr, name: &str) {
        self.failures_by_ip.entry(ip).or_default().push(Instant::now());
        self.failures_by_name.entry(name.to_ascii_lowercase()).or_default().push(Instant::now());
    }

    pub fn login_succeeded(&mut self, name: &str) {
        self.failures_by_name.remove(&name.to_ascii_lowercase());
    }

    /// Counts a registration; `false` if this address registered too often lately.
    pub fn register(&mut self, ip: IpAddr) -> bool {
        let list = self.registrations.entry(ip).or_default();
        if recent(list, REGISTRATION_WINDOW) >= REGISTRATIONS_PER_IP {
            return false;
        }
        list.push(Instant::now());
        true
    }

    /// Forgets old entries (called now and then).
    pub fn prune(&mut self) {
        self.failures_by_ip.retain(|_, l| recent(l, FAILURE_WINDOW) > 0);
        self.failures_by_name.retain(|_, l| recent(l, FAILURE_WINDOW) > 0);
        self.registrations.retain(|_, l| recent(l, REGISTRATION_WINDOW) > 0);
    }
}

/// Emails are optional: at most 254 characters, one `@`, no spaces.
pub fn validate_email(email: &str) -> Result<(), String> {
    let ok = email.len() <= 254
        && email.matches('@').count() == 1
        && !email.starts_with('@')
        && !email.ends_with('@')
        && !email.chars().any(|c| c.is_whitespace() || c.is_control() || matches!(c, '<' | '>' | '"'));
    if ok { Ok(()) } else { Err("That doesn't look like an email address.".into()) }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn passwords() {
        let hash = hash_password("correct horse");
        assert!(hash.starts_with("$argon2id$"));
        assert!(!hash.contains("correct horse"));
        assert!(check_password("correct horse", &hash));
        assert!(!check_password("wrong horse", &hash));
        assert!(!check_password("correct horse", "not a hash"));
        assert_ne!(hash_password("correct horse"), hash, "salted");
    }

    #[test]
    fn limits() {
        let mut limits = RateLimits::default();
        let ip: IpAddr = "10.0.0.1".parse().unwrap();
        for _ in 0..FAILURES_PER_NAME {
            assert!(limits.check_login(ip, "alice").is_ok());
            limits.login_failed(ip, "alice");
        }
        assert!(limits.check_login(ip, "Alice").is_err(), "per name, any case");
        assert!(limits.check_login(ip, "bob").is_ok());
        for _ in 0..REGISTRATIONS_PER_IP {
            assert!(limits.register(ip));
        }
        assert!(!limits.register(ip));
        assert!(validate_email("a@b.example").is_ok());
        assert!(validate_email("a@b@c").is_err());
        assert!(validate_email("<a@b>").is_err());
    }
}
