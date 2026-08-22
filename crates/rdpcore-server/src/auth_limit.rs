//! Per-IP authentication failure limiter: after too many failed
//! handshakes from one address, further attempts are dropped until the
//! lockout expires. Success clears the record.

use std::collections::HashMap;
use std::net::IpAddr;
use std::sync::Mutex;
use std::time::{Duration, Instant};

const MAX_FAILURES: u32 = 5;
const WINDOW: Duration = Duration::from_secs(60);
const LOCKOUT: Duration = Duration::from_secs(60);

#[derive(Clone, Copy)]
struct FailureState {
    count: u32,
    first: Instant,
    locked_until: Option<Instant>,
}

pub struct AuthLimiter {
    inner: Mutex<HashMap<IpAddr, FailureState>>,
}

impl AuthLimiter {
    pub fn new() -> Self {
        Self {
            inner: Mutex::new(HashMap::new()),
        }
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, HashMap<IpAddr, FailureState>> {
        self.inner.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// Whether this address is currently locked out.
    pub fn is_blocked(&self, ip: IpAddr) -> bool {
        let now = Instant::now();
        let mut map = self.lock();
        let Some(state) = map.get(&ip).copied() else {
            return false;
        };
        if let Some(until) = state.locked_until {
            if now < until {
                return true;
            }
            map.remove(&ip);
        }
        false
    }

    pub fn record_success(&self, ip: IpAddr) {
        self.lock().remove(&ip);
    }

    pub fn record_failure(&self, ip: IpAddr) {
        let now = Instant::now();
        let mut map = self.lock();
        let state = map.entry(ip).or_insert(FailureState {
            count: 0,
            first: now,
            locked_until: None,
        });
        if let Some(until) = state.locked_until
            && now < until
        {
            return;
        }
        if now.duration_since(state.first) > WINDOW {
            state.count = 0;
            state.first = now;
            state.locked_until = None;
        }
        state.count = state.count.saturating_add(1);
        if state.count >= MAX_FAILURES {
            state.locked_until = Some(now + LOCKOUT);
        }
    }
}

impl Default for AuthLimiter {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::net::{IpAddr, Ipv4Addr};

    fn ip() -> IpAddr {
        IpAddr::V4(Ipv4Addr::new(192, 0, 2, 10))
    }

    #[test]
    fn five_failures_lock_the_address() {
        let limiter = AuthLimiter::new();
        let ip = ip();
        assert!(!limiter.is_blocked(ip));
        for _ in 0..MAX_FAILURES {
            limiter.record_failure(ip);
        }
        assert!(limiter.is_blocked(ip));
    }

    #[test]
    fn success_clears_failures() {
        let limiter = AuthLimiter::new();
        let ip = ip();
        for _ in 0..(MAX_FAILURES - 1) {
            limiter.record_failure(ip);
        }
        limiter.record_success(ip);
        limiter.record_failure(ip);
        assert!(!limiter.is_blocked(ip));
    }

    #[test]
    fn other_addresses_are_unaffected() {
        let limiter = AuthLimiter::new();
        let a = ip();
        let b = IpAddr::V4(Ipv4Addr::new(192, 0, 2, 11));
        for _ in 0..MAX_FAILURES {
            limiter.record_failure(a);
        }
        assert!(limiter.is_blocked(a));
        assert!(!limiter.is_blocked(b));
    }
}
