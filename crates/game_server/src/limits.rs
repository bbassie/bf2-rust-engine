//! Rate limits and login backoff for what clients (and remote consoles, and anyone who can
//! send a UDP packet) ask of the server, shared by the handlers instead of each keeping its
//! own counters:
//!
//! - [`RateLimiter`]: a token bucket per sender (a player, a client connection or an address)
//!   for command-like messages: chat lines, admin commands, commander requests, content
//!   reports, discovery queries. Honest players never notice the limits.
//! - [`LoginBackoff`]: failed password attempts per sender; after a few, the sender is locked
//!   out for a while, doubling with each further failure.
//! - [`constant_time_eq`] for comparing secrets.

use std::{collections::HashMap, hash::Hash, net::IpAddr};

use bevy_replicon::shared::backend::connected_client::NetworkId;
use bevy_replicon_renet::netcode::NetcodeServerTransport;

/// How many messages a sender may send at once (`burst`) and keep sending (`per_second`).
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Rate {
    pub burst: f32,
    pub per_second: f32,
}

impl Rate {
    /// Chat lines (about BF2's flood limit: five lines, then one a second).
    pub const CHAT: Rate = Rate { burst: 5.0, per_second: 1.0 };
    /// Admin commands typed into the chat, `/login` included.
    pub const CHAT_COMMANDS: Rate = Rate { burst: 5.0, per_second: 1.0 };
    /// Commander requests from a client (orders, assets, spots, votes): each one plays a
    /// radio line to the team, so a commander clicking fast is fine, a stream is not.
    pub const COMMANDER: Rate = Rate { burst: 10.0, per_second: 2.0 };
    /// Content reports while joining: a handful per check (digest, hashes, after each
    /// download), each comparing the whole required list.
    pub const CONTENT_REPORTS: Rate = Rate { burst: 8.0, per_second: 0.5 };
    /// Server browser queries from one address.
    pub const DISCOVERY_PER_ADDRESS: Rate = Rate { burst: 16.0, per_second: 4.0 };
    /// Server browser queries from everyone together (spoofed senders come from anywhere).
    pub const DISCOVERY_TOTAL: Rate = Rate { burst: 128.0, per_second: 64.0 };
    /// Remote console connections from one address.
    pub const RCON_CONNECTIONS: Rate = Rate { burst: 8.0, per_second: 0.5 };
}

/// A token bucket: holds up to `burst` tokens, refilled at `per_second`; each message takes
/// one. Times are seconds on any monotonic clock.
#[derive(Clone, Copy, Debug)]
pub struct TokenBucket {
    tokens: f32,
    at: f64,
}

impl TokenBucket {
    pub fn full(rate: Rate, now: f64) -> Self {
        Self { tokens: rate.burst, at: now }
    }

    fn level(&self, rate: Rate, now: f64) -> f32 {
        let elapsed = (now - self.at).max(0.0) as f32;
        (self.tokens + elapsed * rate.per_second).min(rate.burst)
    }

    /// Takes a token if there is one.
    pub fn take(&mut self, rate: Rate, now: f64) -> bool {
        self.tokens = self.level(rate, now);
        self.at = now.max(self.at);
        if self.tokens >= 1.0 {
            self.tokens -= 1.0;
            true
        } else {
            false
        }
    }
}

/// A [`TokenBucket`] per sender. Senders whose bucket has filled up again are forgotten (a
/// full bucket is the same as a new one), so the map stays as small as the set of senders
/// that sent something lately, whoever they are.
#[derive(Debug)]
pub struct RateLimiter<K> {
    buckets: HashMap<K, TokenBucket>,
    calls: u32,
}

impl<K> Default for RateLimiter<K> {
    fn default() -> Self {
        Self { buckets: HashMap::new(), calls: 0 }
    }
}

impl<K: Eq + Hash + Copy> RateLimiter<K> {
    /// Whether `key` may send another message now (and counts it if so).
    pub fn allow(&mut self, key: K, rate: Rate, now: f64) -> bool {
        self.calls = self.calls.wrapping_add(1);
        if self.calls % 256 == 0 || self.buckets.len() > 4096 {
            self.buckets.retain(|_, bucket| bucket.level(rate, now) < rate.burst);
        }
        self.buckets.entry(key).or_insert_with(|| TokenBucket::full(rate, now)).take(rate, now)
    }

    /// Senders being tracked.
    pub fn len(&self) -> usize {
        self.buckets.len()
    }

    pub fn is_empty(&self) -> bool {
        self.buckets.is_empty()
    }
}

/// Failed logins per sender (an address, or a player), and the lockouts that follow.
#[derive(Debug)]
pub struct LoginBackoff<K> {
    entries: HashMap<K, Failures>,
}

impl<K> Default for LoginBackoff<K> {
    fn default() -> Self {
        Self { entries: HashMap::new() }
    }
}

#[derive(Clone, Copy, Debug, Default)]
struct Failures {
    count: u32,
    last: f64,
    locked_until: f64,
}

/// What a failed attempt led to.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Failed {
    /// Tries left before a lockout.
    TriesLeft(u32),
    /// Locked out for this many seconds.
    LockedOut(f64),
}

impl<K: Eq + Hash + Copy> LoginBackoff<K> {
    /// Attempts allowed before the first lockout.
    pub const FREE_ATTEMPTS: u32 = 3;
    /// The first lockout; each further failure doubles it, up to [`Self::MAX_LOCKOUT`].
    pub const LOCKOUT: f64 = 30.0;
    pub const MAX_LOCKOUT: f64 = 15.0 * 60.0;
    /// A sender that stays quiet this long starts over.
    pub const FORGET_AFTER: f64 = 60.0 * 60.0;

    /// Seconds `key` is still locked out for, if it is.
    pub fn locked(&self, key: K, now: f64) -> Option<f64> {
        self.entries
            .get(&key)
            .map(|f| f.locked_until - now)
            .filter(|left| *left > 0.0)
    }

    /// Counts a failed attempt.
    pub fn fail(&mut self, key: K, now: f64) -> Failed {
        self.entries.retain(|_, f| now - f.last < Self::FORGET_AFTER || f.locked_until > now);
        let failures = self.entries.entry(key).or_default();
        failures.count += 1;
        failures.last = now;
        if failures.count < Self::FREE_ATTEMPTS {
            return Failed::TriesLeft(Self::FREE_ATTEMPTS - failures.count);
        }
        let doublings = (failures.count - Self::FREE_ATTEMPTS).min(16);
        let lockout = (Self::LOCKOUT * f64::from(1u32 << doublings)).min(Self::MAX_LOCKOUT);
        failures.locked_until = now + lockout;
        Failed::LockedOut(lockout)
    }

    /// A successful login: the sender starts over.
    pub fn succeed(&mut self, key: K) {
        self.entries.remove(&key);
    }
}

/// Compares two secrets in time that depends only on their lengths, not on where they first
/// differ.
pub fn constant_time_eq(a: &[u8], b: &[u8]) -> bool {
    if a.len() != b.len() {
        return false;
    }
    let difference = a.iter().zip(b).fold(0u8, |acc, (x, y)| acc | (x ^ y));
    std::hint::black_box(difference) == 0
}

/// The IP address of a connected client, from its [`NetworkId`] (`None` for the host, or
/// without networking).
pub fn client_ip(id: &NetworkId, transport: Option<&NetcodeServerTransport>) -> Option<IpAddr> {
    transport?.client_addr(id.get()).map(|a| a.ip())
}

#[cfg(test)]
mod tests {
    use super::*;

    const RATE: Rate = Rate { burst: 3.0, per_second: 1.0 };

    #[test]
    fn a_bucket_allows_a_burst_then_its_rate() {
        let mut limiter = RateLimiter::default();
        assert!((0..3).all(|_| limiter.allow(1, RATE, 10.0)));
        assert!(!limiter.allow(1, RATE, 10.0), "burst used up");
        assert!(limiter.allow(2, RATE, 10.0), "others have their own bucket");
        assert!(!limiter.allow(1, RATE, 10.5));
        assert!(limiter.allow(1, RATE, 11.01), "one token a second");
        assert!(!limiter.allow(1, RATE, 11.02));
        // A clock going backwards gives nothing.
        assert!(!limiter.allow(1, RATE, 5.0));
    }

    #[test]
    fn refilled_senders_are_forgotten() {
        let mut limiter = RateLimiter::default();
        for key in 0..1000 {
            limiter.allow(key, RATE, 0.0);
        }
        // Long after, every bucket is full again: the next sweep drops them.
        for _ in 0..256 {
            limiter.allow(5000, RATE, 100.0);
        }
        assert!(limiter.len() <= 2, "{}", limiter.len());
    }

    #[test]
    fn failed_logins_lock_out_for_longer_and_longer() {
        type Backoff = LoginBackoff<u8>;
        let mut backoff = Backoff::default();
        assert_eq!(backoff.fail(1, 0.0), Failed::TriesLeft(2));
        assert_eq!(backoff.fail(1, 1.0), Failed::TriesLeft(1));
        assert_eq!(backoff.locked(1, 1.0), None);
        assert_eq!(backoff.fail(1, 2.0), Failed::LockedOut(Backoff::LOCKOUT));
        assert!(backoff.locked(1, 3.0).is_some());
        assert_eq!(backoff.locked(2, 3.0), None, "per sender");
        assert_eq!(backoff.fail(1, 40.0), Failed::LockedOut(2.0 * Backoff::LOCKOUT));
        for i in 0..20 {
            backoff.fail(1, 100.0 + i as f64);
        }
        assert!(backoff.locked(1, 120.0).is_some_and(|left| left <= Backoff::MAX_LOCKOUT));
        backoff.succeed(1);
        assert_eq!(backoff.locked(1, 120.0), None);
        assert_eq!(backoff.fail(1, 121.0), Failed::TriesLeft(2));
    }

    #[test]
    fn secrets_compare_equal_only_when_equal() {
        assert!(constant_time_eq(b"secret", b"secret"));
        assert!(!constant_time_eq(b"secret", b"secreT"));
        assert!(!constant_time_eq(b"secret", b"secret2"));
        assert!(!constant_time_eq(b"", b"x"));
        assert!(constant_time_eq(b"", b""));
    }
}
