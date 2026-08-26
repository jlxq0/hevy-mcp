//! Per-bearer rate limiting.
//!
//! Keys are the first 16 hex characters of `sha256(bearer)`. The raw Hevy API
//! key is never stored in a limiter map or emitted to logs. Reads and writes
//! have independent quotas configured through environment variables.

use std::collections::HashMap;
use std::num::NonZeroU32;
use std::sync::{Arc, RwLock};
use std::time::Duration;

use governor::clock::DefaultClock;
use governor::state::{InMemoryState, NotKeyed};
use governor::{Quota, RateLimiter};

type Bucket = RateLimiter<NotKeyed, InMemoryState, DefaultClock>;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Category {
    Read,
    Write,
}

#[derive(Debug, Clone, Copy)]
pub struct RateLimited;

#[derive(Debug)]
pub struct Limiter {
    reads_per_min: NonZeroU32,
    writes_per_min: NonZeroU32,
    bearer_read: RwLock<HashMap<String, Arc<Bucket>>>,
    bearer_write: RwLock<HashMap<String, Arc<Bucket>>>,
}

impl Limiter {
    /// Return `None` for zero quotas; callers must configure positive limits.
    #[must_use]
    pub fn new(reads_per_min: u32, writes_per_min: u32) -> Option<Self> {
        Some(Self {
            reads_per_min: NonZeroU32::new(reads_per_min)?,
            writes_per_min: NonZeroU32::new(writes_per_min)?,
            bearer_read: RwLock::new(HashMap::new()),
            bearer_write: RwLock::new(HashMap::new()),
        })
    }

    pub fn check(&self, bearer_hash: &str, category: Category) -> Result<(), RateLimited> {
        let (map, quota) = match category {
            Category::Read => (&self.bearer_read, self.reads_per_min),
            Category::Write => (&self.bearer_write, self.writes_per_min),
        };
        get_or_insert(map, bearer_hash, quota)
            .check()
            .map_err(|_| RateLimited)
    }
}

fn get_or_insert(
    map: &RwLock<HashMap<String, Arc<Bucket>>>,
    key: &str,
    quota: NonZeroU32,
) -> Arc<Bucket> {
    get_or_insert_with_quota(map, key, Quota::per_minute(quota))
}

fn get_or_insert_with_quota(
    map: &RwLock<HashMap<String, Arc<Bucket>>>,
    key: &str,
    quota: Quota,
) -> Arc<Bucket> {
    if let Ok(guard) = map.read()
        && let Some(bucket) = guard.get(key)
    {
        return Arc::clone(bucket);
    }
    let mut guard = match map.write() {
        Ok(guard) => guard,
        Err(poisoned) => poisoned.into_inner(),
    };
    Arc::clone(
        guard
            .entry(key.to_owned())
            .or_insert_with(|| Arc::new(RateLimiter::direct(quota))),
    )
}

/// Rate limiter for fresh MCP `initialize` requests. Tool quotas cannot cover
/// this path because rmcp creates a session before dispatching any tool.
///
/// Burst and replenish period come from [`crate::config::Config`]; see the
/// defaults there for why they are sized the way they are.
#[derive(Debug)]
pub struct InitializeLimiter {
    quota: Quota,
    bearer: RwLock<HashMap<String, Arc<Bucket>>>,
}

impl InitializeLimiter {
    #[must_use]
    pub fn new(replenish_1_per: Duration, burst: u32) -> Self {
        let burst = NonZeroU32::new(burst).unwrap_or(NonZeroU32::MIN);
        let quota = Quota::with_period(replenish_1_per)
            .unwrap_or_else(|| Quota::per_minute(NonZeroU32::MIN))
            .allow_burst(burst);
        Self {
            quota,
            bearer: RwLock::new(HashMap::new()),
        }
    }

    pub fn check(&self, bearer_hash: &str) -> Result<(), RateLimited> {
        get_or_insert_with_quota(&self.bearer, bearer_hash, self.quota)
            .check()
            .map_err(|_| RateLimited)
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;

    #[test]
    fn zero_quota_rejected() {
        assert!(Limiter::new(0, 1).is_none());
        assert!(Limiter::new(1, 0).is_none());
    }

    #[test]
    fn reads_and_writes_have_independent_buckets() {
        let limiter = Limiter::new(2, 2).unwrap();
        limiter.check("hash", Category::Read).unwrap();
        limiter.check("hash", Category::Read).unwrap();
        assert!(limiter.check("hash", Category::Read).is_err());
        limiter.check("hash", Category::Write).unwrap();
        limiter.check("hash", Category::Write).unwrap();
        assert!(limiter.check("hash", Category::Write).is_err());
    }

    #[test]
    fn distinct_bearers_do_not_share_a_bucket() {
        let limiter = Limiter::new(1, 1).unwrap();
        limiter.check("hash-1", Category::Read).unwrap();
        assert!(limiter.check("hash-1", Category::Read).is_err());
        limiter.check("hash-2", Category::Read).unwrap();
    }

    #[test]
    fn initialize_limiter_denies_after_burst() {
        let limiter = InitializeLimiter::new(Duration::from_mins(1), 2);
        limiter.check("hash").unwrap();
        limiter.check("hash").unwrap();
        assert!(limiter.check("hash").is_err());
        limiter.check("other-hash").unwrap();
    }
}
