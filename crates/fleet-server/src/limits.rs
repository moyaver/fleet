// SPDX-License-Identifier: AGPL-3.0-only
//! Token-bucket rate limits (research 03 §11; issue #35).
//!
//! [`Limiter`] keeps one bucket per key (a client IP, or an agent credential) and forgets keys
//! whose bucket has refilled, so its memory is bounded by `rate_limits.max_tracked_keys`.
//! [`Bucket::spend`] paces one agent socket's events without refusing any of them (invariant I5).
//! A refusal is `429 Too Many Requests` with `Retry-After` in whole seconds.

use std::{
    collections::HashMap,
    hash::Hash,
    net::IpAddr,
    sync::{Mutex, PoisonError},
    time::{Duration, Instant},
};

use axum::{
    Json,
    http::{StatusCode, header},
    response::{IntoResponse, Response},
};
use fleet_store::auth::Credential;
use serde_json::json;

use crate::config::RateLimits;

/// How often a [`Limiter`] forgets the keys that have refilled.
const SWEEP_EVERY: Duration = Duration::from_secs(1);
/// What a new key is told to wait while the table is full of busy keys.
const FULL_RETRY: Duration = Duration::from_secs(1);

/// Refill rate and capacity of a bucket.
#[derive(Debug, Clone, Copy, PartialEq)]
pub(crate) struct Quota {
    /// Tokens returned per second.
    per_second: f64,
    /// Most tokens a bucket holds.
    burst: f64,
}

impl Quota {
    /// `per_minute` tokens a minute, `burst` at once.
    pub(crate) fn per_minute(per_minute: u32, burst: u32) -> Self {
        Self {
            per_second: f64::from(per_minute) / 60.0,
            burst: f64::from(burst),
        }
    }

    /// `per_second` tokens a second, `burst` at once.
    pub(crate) fn per_second(per_second: u32, burst: u32) -> Self {
        Self {
            per_second: f64::from(per_second),
            burst: f64::from(burst),
        }
    }
}

/// One bucket: the tokens it held when it was last touched.
#[derive(Debug, Clone, Copy, PartialEq)]
pub(crate) struct Bucket {
    tokens: f64,
    at: Instant,
}

impl Bucket {
    /// A full bucket.
    pub(crate) fn new(quota: Quota, now: Instant) -> Self {
        Self {
            tokens: quota.burst,
            at: now,
        }
    }

    /// The tokens held at `now`, refilled.
    fn level(&self, quota: Quota, now: Instant) -> f64 {
        let elapsed = now.saturating_duration_since(self.at).as_secs_f64();
        (self.tokens + elapsed * quota.per_second).min(quota.burst)
    }

    /// Takes `n` tokens if there are that many. Otherwise takes nothing and returns how long
    /// until there are.
    pub(crate) fn take(&mut self, quota: Quota, n: f64, now: Instant) -> Result<(), Duration> {
        let level = self.level(quota, now);
        self.tokens = level;
        self.at = now;
        if level >= n {
            self.tokens -= n;
            Ok(())
        } else {
            Err(Duration::from_secs_f64((n - level) / quota.per_second))
        }
    }

    /// Spends `n` tokens even when fewer are held (the bucket goes into debt), and returns how
    /// long the caller should wait before the debt is repaid. Nothing is refused: the caller
    /// paces itself.
    pub(crate) fn spend(&mut self, quota: Quota, n: f64, now: Instant) -> Duration {
        let level = self.level(quota, now) - n;
        self.tokens = level;
        self.at = now;
        if level >= 0.0 {
            Duration::ZERO
        } else {
            Duration::from_secs_f64(-level / quota.per_second)
        }
    }

    /// Whether the bucket is back at full capacity, so forgetting it changes nothing.
    fn is_full(&self, quota: Quota, now: Instant) -> bool {
        self.level(quota, now) >= quota.burst
    }
}

/// One bucket per key, for a single kind of request.
#[derive(Debug)]
pub(crate) struct Limiter<K> {
    quota: Quota,
    max_keys: usize,
    table: Mutex<Table<K>>,
}

#[derive(Debug)]
struct Table<K> {
    buckets: HashMap<K, Bucket>,
    swept: Instant,
}

impl<K: Hash + Eq + Copy> Limiter<K> {
    pub(crate) fn new(quota: Quota, max_keys: usize, now: Instant) -> Self {
        Self {
            quota,
            max_keys,
            table: Mutex::new(Table {
                buckets: HashMap::new(),
                swept: now,
            }),
        }
    }

    /// Takes one token for `key`, or returns how long until `key` may try again.
    pub(crate) fn check(&self, key: K, now: Instant) -> Result<(), Duration> {
        let mut table = self.table.lock().unwrap_or_else(PoisonError::into_inner);
        if now.saturating_duration_since(table.swept) >= SWEEP_EVERY {
            let quota = self.quota;
            table.buckets.retain(|_, b| !b.is_full(quota, now));
            table.swept = now;
        }
        if !table.buckets.contains_key(&key) && table.buckets.len() >= self.max_keys {
            // Every tracked key is busy. Refusing a stranger keeps memory bounded.
            return Err(FULL_RETRY);
        }
        let quota = self.quota;
        table
            .buckets
            .entry(key)
            .or_insert_with(|| Bucket::new(quota, now))
            .take(quota, 1.0, now)
    }
}

/// The limiters the server runs.
#[derive(Debug)]
pub(crate) struct Limits {
    /// Pre-auth requests per client IP.
    pub(crate) ip: Limiter<IpAddr>,
    /// Agent connections per credential.
    pub(crate) credential: Limiter<Credential>,
    /// Events per second on one agent socket (each socket holds its own bucket).
    pub(crate) event: Quota,
}

impl Limits {
    pub(crate) fn new(config: &RateLimits, now: Instant) -> Self {
        let max = config.max_tracked_keys;
        Self {
            ip: Limiter::new(
                Quota::per_minute(config.ip_requests_per_minute, config.ip_burst),
                max,
                now,
            ),
            credential: Limiter::new(
                Quota::per_minute(
                    config.credential_requests_per_minute,
                    config.credential_burst,
                ),
                max,
                now,
            ),
            event: Quota::per_second(config.events_per_second, config.event_burst),
        }
    }
}

/// `Retry-After` in whole seconds: at least one, rounded up.
fn retry_after_secs(wait: Duration) -> u64 {
    (wait.as_secs() + u64::from(wait.subsec_nanos() > 0)).max(1)
}

/// `429 Too Many Requests` with `Retry-After` for a refused request.
pub(crate) fn too_many_requests(wait: Duration) -> Response {
    (
        StatusCode::TOO_MANY_REQUESTS,
        [(header::RETRY_AFTER, retry_after_secs(wait).to_string())],
        Json(json!({ "error": "too many requests; wait for the Retry-After delay" })),
    )
        .into_response()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn at(base: Instant, secs: f64) -> Instant {
        base + Duration::from_secs_f64(secs)
    }

    #[test]
    fn a_bucket_spends_its_burst_then_refills_at_its_rate() {
        let quota = Quota::per_second(2, 4);
        let t0 = Instant::now();
        let mut b = Bucket::new(quota, t0);
        for _ in 0..4 {
            assert!(b.take(quota, 1.0, t0).is_ok());
        }
        let wait = b.take(quota, 1.0, t0).unwrap_err();
        assert_eq!(wait, Duration::from_millis(500), "one token at 2 a second");
        assert!(
            b.take(quota, 1.0, at(t0, 0.5)).is_ok(),
            "refilled after the wait"
        );
        assert!(
            b.take(quota, 1.0, at(t0, 100.0)).is_ok(),
            "refill never exceeds the burst"
        );
        assert!(b.take(quota, 4.0, at(t0, 100.0)).is_err());
    }

    #[test]
    fn a_paced_bucket_goes_into_debt_and_says_how_long_to_wait() {
        let quota = Quota::per_second(100, 10);
        let t0 = Instant::now();
        let mut b = Bucket::new(quota, t0);
        assert_eq!(
            b.spend(quota, 10.0, t0),
            Duration::ZERO,
            "the burst is free"
        );
        // 64 more events at 100 a second: the reader waits 0.64 s before it reads again.
        assert_eq!(b.spend(quota, 64.0, t0), Duration::from_millis(640));
        // Half the debt is repaid in 0.32 s, so the next frame waits for the rest.
        assert_eq!(
            b.spend(quota, 0.0, at(t0, 0.32)),
            Duration::from_millis(320)
        );
    }

    #[test]
    fn keys_are_independent_and_a_busy_table_refuses_strangers_only() {
        let t0 = Instant::now();
        let limiter = Limiter::new(Quota::per_minute(60, 2), 2, t0);
        let a: IpAddr = "192.0.2.1".parse().unwrap();
        let b: IpAddr = "192.0.2.2".parse().unwrap();
        let c: IpAddr = "192.0.2.3".parse().unwrap();
        assert!(limiter.check(a, t0).is_ok());
        assert!(limiter.check(a, t0).is_ok());
        let wait = limiter.check(a, t0).unwrap_err();
        assert_eq!(wait, Duration::from_secs(1), "one token a second");
        assert!(
            limiter.check(b, t0).is_ok(),
            "another key has its own bucket"
        );
        // The table holds two busy keys: a third is refused until one ages out.
        assert_eq!(limiter.check(c, t0), Err(FULL_RETRY));
        assert!(
            limiter.check(a, at(t0, 1.0)).is_ok(),
            "known keys still pass"
        );
    }

    #[test]
    fn idle_keys_are_forgotten_so_memory_stays_bounded() {
        let t0 = Instant::now();
        let limiter = Limiter::new(Quota::per_minute(60, 2), 2, t0);
        let a: IpAddr = "192.0.2.1".parse().unwrap();
        let b: IpAddr = "192.0.2.2".parse().unwrap();
        let c: IpAddr = "192.0.2.3".parse().unwrap();
        limiter.check(a, t0).unwrap();
        limiter.check(b, t0).unwrap();
        // Both have refilled to full after two minutes; a new key fits and the table shrinks.
        let later = at(t0, 120.0);
        assert!(limiter.check(c, later).is_ok());
        assert_eq!(limiter.table.lock().unwrap().buckets.len(), 1);
    }

    #[test]
    fn retry_after_is_whole_seconds_and_at_least_one() {
        assert_eq!(retry_after_secs(Duration::from_millis(1)), 1);
        assert_eq!(retry_after_secs(Duration::from_secs(3)), 3);
        assert_eq!(retry_after_secs(Duration::from_millis(3001)), 4);
    }

    #[test]
    fn a_refusal_is_429_with_retry_after() {
        let res = too_many_requests(Duration::from_millis(2500));
        assert_eq!(res.status(), StatusCode::TOO_MANY_REQUESTS);
        assert_eq!(res.headers()[header::RETRY_AFTER], "3");
    }
}
