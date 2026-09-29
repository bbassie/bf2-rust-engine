//! A small, dependency-free connection/request admission cap, shared by the master server's
//! and the game server's `tiny_http` endpoints (`master_server::http`,
//! `game_server::content`).
//!
//! `tiny_http` accepts connections on an internal, unbounded thread pool and doesn't expose
//! the underlying sockets, so there is no way to set an OS-level read/write timeout or a hard
//! connection cap from outside the crate. [`Limiter`] is the practical substitute: it bounds
//! how many requests may be worked on at once, in total and per source address, so a handful
//! of slow or malicious connections can only ever occupy a small, fixed slice of the server's
//! capacity instead of starving every other client. It does not (and cannot) forcibly close an
//! already-accepted, idle connection; operators who terminate TLS at a reverse proxy (as
//! production deployments here already must, see docs/MODDING.md) should also set that
//! proxy's read/write timeouts as the real backstop.

use std::{
    collections::HashMap,
    net::IpAddr,
    sync::{
        Mutex,
        atomic::{AtomicU32, Ordering},
    },
};

/// Caps concurrent admissions: a global total and a per-address share of it.
pub struct Limiter {
    max_total: u32,
    max_per_ip: u32,
    total: AtomicU32,
    by_ip: Mutex<HashMap<IpAddr, u32>>,
}

/// Holds one admitted slot; releases it (and the per-address count) on drop.
pub struct Admitted<'a> {
    limiter: &'a Limiter,
    ip: IpAddr,
}

impl Limiter {
    pub fn new(max_total: u32, max_per_ip: u32) -> Self {
        Self { max_total, max_per_ip, total: AtomicU32::new(0), by_ip: Mutex::new(HashMap::new()) }
    }

    /// Admits one request from `ip`, or `None` if the global or per-address cap is already
    /// reached (the caller should answer quickly, e.g. 429, and not do any further work).
    pub fn enter(&self, ip: IpAddr) -> Option<Admitted<'_>> {
        if self.total.fetch_add(1, Ordering::AcqRel) >= self.max_total {
            self.total.fetch_sub(1, Ordering::AcqRel);
            return None;
        }
        let mut by_ip = self.by_ip.lock().unwrap_or_else(|p| p.into_inner());
        let count = by_ip.entry(ip).or_insert(0);
        if *count >= self.max_per_ip {
            drop(by_ip);
            self.total.fetch_sub(1, Ordering::AcqRel);
            return None;
        }
        *count += 1;
        drop(by_ip);
        Some(Admitted { limiter: self, ip })
    }

    /// Requests currently admitted (for tests and diagnostics).
    pub fn current_total(&self) -> u32 {
        self.total.load(Ordering::Acquire)
    }
}

impl Drop for Admitted<'_> {
    fn drop(&mut self) {
        self.limiter.total.fetch_sub(1, Ordering::AcqRel);
        let mut by_ip = self.limiter.by_ip.lock().unwrap_or_else(|p| p.into_inner());
        if let Some(count) = by_ip.get_mut(&self.ip) {
            *count = count.saturating_sub(1);
            if *count == 0 {
                by_ip.remove(&self.ip);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn caps_total_and_per_address() {
        let limiter = Limiter::new(3, 2);
        let a: IpAddr = "10.0.0.1".parse().unwrap();
        let b: IpAddr = "10.0.0.2".parse().unwrap();
        let g1 = limiter.enter(a).expect("first from a");
        let g2 = limiter.enter(a).expect("second from a");
        assert!(limiter.enter(a).is_none(), "per-address cap (2)");
        let g3 = limiter.enter(b).expect("first from b, under the total cap of 3");
        assert!(limiter.enter(b).is_none(), "global cap (3) reached even though b is under its own limit");
        assert_eq!(limiter.current_total(), 3);
        drop(g1);
        assert_eq!(limiter.current_total(), 2);
        assert!(limiter.enter(a).is_some(), "a slot freed up");
        drop((g2, g3));
    }
}
