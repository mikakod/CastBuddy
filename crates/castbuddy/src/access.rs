//! Access logging + abuse protection for the public share listener.
//!
//! Design goals (deliberately low-noise, useful for troubleshooting):
//! - first request of a viewer session is logged at info with the client IP;
//! - steady-state media requests (playlist/segment polling) stay at debug;
//! - downloads and denied (invalid/expired) share keys are always visible;
//! - a per-IP token bucket keeps floods from drowning the logs or the box.

use std::collections::HashMap;
use std::net::SocketAddr;
use std::sync::Arc;
use std::time::{Duration, Instant};

use axum::http::HeaderMap;
use tokio::sync::Mutex;

/// Sustained requests/second per IP on the share listener. HLS viewers sit
/// around 2-5 req/s; the burst covers a player's startup fan-out of segment
/// fetches. Anything faster than this is either a broken player or an abuser.
pub const SHARE_RATE_PER_SEC: f64 = 25.0;
pub const SHARE_BURST: f64 = 60.0;

/// Client IP: first `X-Forwarded-For` hop when a reverse proxy (Tailscale
/// Serve, Caddy) fronts the listener, else the socket peer.
pub fn client_ip(headers: &HeaderMap, remote: Option<SocketAddr>) -> String {
    if let Some(xff) = headers.get("x-forwarded-for").and_then(|v| v.to_str().ok()) {
        if let Some(first) = xff.split(',').next() {
            let t = first.trim();
            if !t.is_empty() && t.len() <= 64 {
                return t.to_string();
            }
        }
    }
    remote
        .map(|a| a.ip().to_string())
        .unwrap_or_else(|| "unknown".into())
}

/// Dedup helper: "log the first event per key, suppress repeats for a while".
/// Keys are `"<kind>|<ip>|<subject>"`, so one noisy viewer can never drown
/// out other IPs in the log.
pub struct AccessLog {
    seen: Mutex<HashMap<String, Instant>>,
}

impl AccessLog {
    pub fn new() -> Self {
        Self {
            seen: Mutex::new(HashMap::new()),
        }
    }

    /// True if this (kind, ip, subject) hasn't been logged within `cooldown`.
    pub async fn first(&self, kind: &str, ip: &str, subject: &str, cooldown: Duration) -> bool {
        let key = format!("{kind}|{ip}|{subject}");
        let now = Instant::now();
        let mut map = self.seen.lock().await;
        if map.len() > 4096 {
            map.retain(|_, t| now.duration_since(*t) < Duration::from_secs(3600));
        }
        match map.get(&key) {
            Some(t) if now.duration_since(*t) < cooldown => false,
            _ => {
                map.insert(key, now);
                true
            }
        }
    }
}

impl Default for AccessLog {
    fn default() -> Self {
        Self::new()
    }
}

/// Per-IP token bucket. Shared across tasks behind an Arc.
pub struct RateLimiter {
    inner: Arc<Mutex<HashMap<String, (f64, Instant)>>>,
    rate: f64,
    burst: f64,
}

impl RateLimiter {
    pub fn new(rate: f64, burst: f64) -> Self {
        Self {
            inner: Arc::new(Mutex::new(HashMap::new())),
            rate,
            burst,
        }
    }

    /// Consume one token for `ip`; false = caller should reject with 429.
    pub async fn allow(&self, ip: &str) -> bool {
        let now = Instant::now();
        let mut map = self.inner.lock().await;
        if map.len() > 10_000 {
            // Idle IPs (no traffic for 5 minutes) drop out entirely.
            map.retain(|_, (_, t)| now.duration_since(*t) < Duration::from_secs(300));
        }
        let entry = map.entry(ip.to_string()).or_insert((self.burst, now));
        let dt = now.duration_since(entry.1).as_secs_f64();
        entry.0 = (entry.0 + dt * self.rate).min(self.burst);
        entry.1 = now;
        if entry.0 >= 1.0 {
            entry.0 -= 1.0;
            true
        } else {
            false
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn rate_limiter_bursts_then_throttles() {
        let rl = RateLimiter::new(1.0, 3.0);
        assert!(rl.allow("a").await);
        assert!(rl.allow("a").await);
        assert!(rl.allow("a").await);
        assert!(!rl.allow("a").await, "burst of 3 must be exhausted");
        assert!(rl.allow("b").await, "other IPs are unaffected");
    }

    #[tokio::test]
    async fn access_log_dedups_within_cooldown() {
        let log = AccessLog::new();
        let cd = Duration::from_secs(60);
        assert!(log.first("watch", "1.2.3.4", "share1", cd).await);
        assert!(!log.first("watch", "1.2.3.4", "share1", cd).await);
        assert!(log.first("watch", "1.2.3.4", "share2", cd).await);
        assert!(log.first("watch", "5.6.7.8", "share1", cd).await);
    }

    #[test]
    fn xff_wins_over_socket() {
        let mut h = HeaderMap::new();
        h.insert("x-forwarded-for", "203.0.113.9, 10.0.0.1".parse().unwrap());
        let addr: SocketAddr = "127.0.0.1:5555".parse().unwrap();
        assert_eq!(client_ip(&h, Some(addr)), "203.0.113.9");
        assert_eq!(client_ip(&HeaderMap::new(), Some(addr)), "127.0.0.1");
    }
}
