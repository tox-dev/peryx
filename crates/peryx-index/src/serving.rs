//! The role engine's shared serving primitives: single-flight coalescing and the stale-on-error
//! bound.
//!
//! Every cached (proxy) role does the same two things around an upstream fetch, whatever it caches.
//! It coalesces concurrent misses for one key so a cold page is fetched once, not once per waiter —
//! the difference between a warm cache and a thundering herd on a popular project. And it decides how
//! long a page past its freshness window may still answer while the upstream is unreachable. Both live
//! here so a `PyPI` page and an `OCI` manifest share one implementation rather than drifting apart.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

/// Per-key single-flight locks. Concurrent misses for one key take the same lock, so exactly one
/// fetches from upstream and stores the result while the rest wait and then serve it from cache.
pub type Inflight = Mutex<HashMap<String, Arc<tokio::sync::Mutex<()>>>>;

/// The lock concurrent misses for `key` share.
///
/// # Panics
/// Panics if the inflight map's mutex was poisoned by a thread that panicked while holding it.
#[must_use]
pub fn flight_gate(inflight: &Inflight, key: &str) -> Arc<tokio::sync::Mutex<()>> {
    inflight
        .lock()
        .expect("inflight lock")
        .entry(key.to_owned())
        .or_default()
        .clone()
}

/// Release a single-flight hold: unlock first so a waiter parked on the gate proceeds, then drop the
/// map entry so later requests start fresh.
///
/// # Panics
/// Panics if the inflight map's mutex was poisoned.
pub fn release_flight(inflight: &Inflight, key: &str, guard: tokio::sync::OwnedMutexGuard<()>) {
    drop(guard);
    inflight.lock().expect("inflight lock").remove(key);
}

/// Whether a page past its freshness window may still answer while the upstream cannot be reached.
///
/// Serving something old beats serving nothing while an upstream reboots, but only for a while: a
/// cache that answers with whatever it last saw, forever, has stopped being a cache and become a
/// fork. `max_stale_secs` bounds the outage a stale page papers over; `0` removes the bound, which is
/// what an operator deliberately mirroring an unreliable upstream asks for. `freshness_secs` is the
/// lifetime the page was fresh for — an ecosystem passes the upstream-granted lifetime, or its own
/// fallback.
#[must_use]
pub const fn within_stale_bound(now: i64, max_stale_secs: i64, fetched_at: i64, freshness_secs: i64) -> bool {
    max_stale_secs == 0 || now.saturating_sub(fetched_at) < freshness_secs + max_stale_secs
}

/// The in-memory caches a cached (proxy) role serves from, and the mutation counter that retires them.
///
/// Every warm request is a lookup here; a mutation bumps the epoch so a stale hot page misses by key
/// rather than being hunted down. The store fields are public so a driver can stream directly into
/// them on the serve path; the methods cover the common gestures.
pub struct ServingCache {
    /// Single-flight locks; see [`flight_gate`].
    pub inflight: Inflight,
    /// Transformed page bytes paired with their unix expiry. Keys carry the mutation epoch, so a
    /// mutation invalidates by key miss; the expiry honours each page's upstream lifetime, and moka's
    /// own time-to-live is a coarse eviction backstop.
    pub hot: moka::sync::Cache<String, (bytes::Bytes, i64)>,
    /// Short-lived upstream misses (key → unix expiry), kept apart from stored pages so a `404` adds
    /// no row to the persistent cache.
    pub negative: moka::sync::Cache<String, i64>,
    /// Bumped by every mutation that changes what a page serves, retiring hot-cache keys.
    pub epoch: std::sync::atomic::AtomicU64,
}

impl ServingCache {
    /// Build the caches. `hot_cache_bytes` is the transformed-page budget; `ttl_secs` sets moka's
    /// coarse time-to-live backstop.
    #[must_use]
    pub fn new(hot_cache_bytes: u64, ttl_secs: i64) -> Self {
        Self {
            inflight: Inflight::default(),
            hot: moka::sync::Cache::builder()
                .max_capacity(hot_cache_bytes)
                .weigher(|key: &String, (value, _): &(bytes::Bytes, i64)| {
                    u32::try_from(key.len() + value.len()).unwrap_or(u32::MAX)
                })
                .time_to_live(std::time::Duration::from_secs(
                    u64::try_from(ttl_secs.max(1)).unwrap_or(1800),
                ))
                .build(),
            negative: moka::sync::Cache::builder().max_capacity(65_536).build(),
            epoch: std::sync::atomic::AtomicU64::new(0),
        }
    }

    /// Drop a single-flight entry after a fetch that held no owned guard, so later requests start
    /// fresh.
    ///
    /// # Panics
    /// Panics if the inflight map's mutex was poisoned.
    pub fn forget_flight(&self, key: &str) {
        self.inflight.lock().expect("inflight lock").remove(key);
    }

    /// A hot-cache entry still within its freshness window at `now`; an expired entry misses.
    #[must_use]
    pub fn hot_fresh(&self, key: &str, now: i64) -> Option<bytes::Bytes> {
        let (bytes, expires_at) = self.hot.get(key)?;
        (now < expires_at).then_some(bytes)
    }

    /// Store `bytes` as the hot representation of `key` until `expires_at`.
    pub fn store_hot(&self, key: String, bytes: bytes::Bytes, expires_at: i64) {
        self.hot.insert(key, (bytes, expires_at));
    }

    /// The hot-cache key for one representation of a page as served on `route` right now.
    ///
    /// `variant` separates the representations a page has (PEP 691 JSON, PEP 503 HTML, legacy JSON):
    /// different bytes. The epoch makes a mutation invalidate them all at once.
    #[must_use]
    pub fn hot_key(&self, route: &str, project: &str, variant: &str) -> String {
        let epoch = self.epoch.load(std::sync::atomic::Ordering::Relaxed);
        format!("{route}\u{0}{project}\u{0}{variant}\u{0}{epoch}")
    }

    /// Whether a remembered upstream miss for `key` is still inside its expiry at `now`.
    #[must_use]
    pub fn negative_fresh(&self, key: &str, now: i64) -> bool {
        match self.negative.get(key) {
            Some(expires_at) if now < expires_at => true,
            Some(_) => {
                self.negative.invalidate(key);
                false
            }
            None => false,
        }
    }

    /// Remember an upstream miss for `key` until `expires_at`.
    pub fn remember_negative(&self, key: String, expires_at: i64) {
        self.negative.insert(key, expires_at);
    }

    /// Retire every hot-cache entry after a mutation by advancing the epoch its keys carry.
    pub fn bump_epoch(&self) {
        self.epoch.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    }
}

#[cfg(test)]
mod tests {
    use super::within_stale_bound;

    #[test]
    fn test_zero_max_stale_serves_any_age() {
        assert!(within_stale_bound(1_000_000, 0, 0, 60));
    }

    #[test]
    fn test_stale_within_the_bound_serves_and_past_it_does_not() {
        // fresh for 60s, tolerate 300s past that: servable up to 360s after fetch.
        assert!(within_stale_bound(1_359, 300, 1_000, 60));
        assert!(!within_stale_bound(1_360, 300, 1_000, 60));
    }

    #[test]
    fn test_a_future_fetch_time_does_not_underflow() {
        assert!(within_stale_bound(1_000, 300, 5_000, 60));
    }
}
