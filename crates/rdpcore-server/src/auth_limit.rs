//! Per-IP authentication failure limiter: after too many failed
//! handshakes from one address, further attempts are dropped until the
//! lockout expires. Success clears the record. Stale and surplus entries
//! are evicted so a flood of unique source addresses cannot grow the map
//! without bound.

use std::collections::HashMap;
use std::net::IpAddr;
use std::sync::Mutex;
use std::time::{Duration, Instant};

const MAX_FAILURES: u32 = 5;
const WINDOW: Duration = Duration::from_secs(60);
const LOCKOUT: Duration = Duration::from_secs(300);
const MAX_TRACKED: usize = 4096;

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

    fn gc(map: &mut HashMap<IpAddr, FailureState>, now: Instant) {
        map.retain(|_, state| entry_live(state, now));
    }

    fn evict_if_full(map: &mut HashMap<IpAddr, FailureState>, incoming: IpAddr) {
        if map.len() < MAX_TRACKED || map.contains_key(&incoming) {
            return;
        }
        let victim = map
            .iter()
            .filter(|(ip, _)| **ip != incoming)
            .min_by_key(|(_, s)| (s.locked_until.is_some(), s.first))
            .map(|(ip, _)| *ip);
        if let Some(ip) = victim {
            map.remove(&ip);
        }
    }

    /// Whether this address is currently locked out.
    pub fn is_blocked(&self, ip: IpAddr) -> bool {
        let now = Instant::now();
        let mut map = self.lock();
        Self::gc(&mut map, now);
        let Some(state) = map.get(&ip).copied() else {
            return false;
        };
        matches!(state.locked_until, Some(until) if now < until)
    }

    pub fn record_success(&self, ip: IpAddr) {
        self.lock().remove(&ip);
    }

    pub fn record_failure(&self, ip: IpAddr) {
        let now = Instant::now();
        let mut map = self.lock();
        Self::gc(&mut map, now);
        Self::evict_if_full(&mut map, ip);
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

fn entry_live(state: &FailureState, now: Instant) -> bool {
    if let Some(until) = state.locked_until {
        return now < until;
    }
    now.duration_since(state.first) <= WINDOW
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

    #[test]
    fn gc_drops_unlocked_entries_outside_the_window() {
        let mut map = HashMap::new();
        let now = Instant::now();
        map.insert(
            ip(),
            FailureState {
                count: 1,
                first: now - WINDOW - Duration::from_secs(1),
                locked_until: None,
            },
        );
        AuthLimiter::gc(&mut map, now);
        assert!(map.is_empty());
    }

    #[test]
    fn evict_if_full_drops_an_unlocked_entry() {
        let mut map = HashMap::new();
        let now = Instant::now();
        for i in 0..MAX_TRACKED {
            let addr = IpAddr::V4(Ipv4Addr::new(198, 51, (i / 256) as u8, (i % 256) as u8));
            map.insert(
                addr,
                FailureState {
                    count: 1,
                    first: now,
                    locked_until: None,
                },
            );
        }
        let incoming = IpAddr::V4(Ipv4Addr::new(203, 0, 113, 1));
        AuthLimiter::evict_if_full(&mut map, incoming);
        assert_eq!(map.len(), MAX_TRACKED - 1);
        assert!(!map.contains_key(&incoming));
    }
}
