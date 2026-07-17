//! The role engine's shared serving primitives: single-flight coalescing and the stale-on-error
//! bound.
//!
//! Every cached (proxy) role does the same two things around an upstream fetch, whatever it caches.
//! It coalesces concurrent misses for one key so a cold page is fetched once, not once per waiter —
//! the difference between a warm cache and a thundering herd on a popular project. And it decides how
//! long a page past its freshness window may still answer while the upstream is unreachable. Both live
//! here so a `PyPI` page and an `OCI` manifest share one implementation rather than drifting apart.

use std::collections::BTreeMap;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

use dashmap::DashMap;
use dashmap::mapref::entry::Entry;

/// Sharded per-key single-flight locks.
#[derive(Clone, Debug, Default)]
pub struct Inflight {
    gates: Arc<DashMap<Arc<str>, Arc<Gate>>>,
}

#[derive(Debug)]
struct Gate {
    mutex: Arc<tokio::sync::Mutex<()>>,
    users: AtomicUsize,
}

impl Gate {
    fn new() -> Self {
        Self {
            mutex: Arc::default(),
            users: AtomicUsize::new(1),
        }
    }
}

/// A registered caller for one single-flight key.
#[derive(Debug)]
pub struct FlightGate {
    inflight: Inflight,
    key: Arc<str>,
    gate: Arc<Gate>,
}

impl FlightGate {
    /// Wait for this key's current producer.
    pub async fn lock(self) -> FlightGuard {
        self.lock_owned().await
    }

    /// Wait for this key's current producer with an owned guard.
    pub async fn lock_owned(self) -> FlightGuard {
        let guard = self.gate.mutex.clone().lock_owned().await;
        FlightGuard {
            _guard: guard,
            flight: self,
        }
    }

    /// Take this key's producer slot without waiting.
    ///
    /// # Errors
    /// Returns Tokio's lock error while another caller holds the slot.
    pub fn try_lock_owned(self) -> Result<FlightGuard, tokio::sync::TryLockError> {
        let guard = self.gate.mutex.clone().try_lock_owned()?;
        Ok(FlightGuard {
            _guard: guard,
            flight: self,
        })
    }
}

impl Drop for FlightGate {
    fn drop(&mut self) {
        if self.gate.users.fetch_sub(1, Ordering::AcqRel) == 1 {
            self.inflight.gates.remove_if(&self.key, |_, gate| {
                Arc::ptr_eq(gate, &self.gate) && gate.users.load(Ordering::Acquire) == 0
            });
        }
    }
}

/// The held producer slot for one single-flight key.
#[derive(Debug)]
pub struct FlightGuard {
    _guard: tokio::sync::OwnedMutexGuard<()>,
    flight: FlightGate,
}

/// The lock concurrent misses for `key` share.
#[must_use]
pub fn flight_gate(inflight: &Inflight, key: &str) -> FlightGate {
    let key = Arc::<str>::from(key);
    let gate = match inflight.gates.entry(key.clone()) {
        Entry::Occupied(entry) => {
            entry.get().users.fetch_add(1, Ordering::Relaxed);
            entry.get().clone()
        }
        Entry::Vacant(entry) => entry.insert(Arc::new(Gate::new())).clone(),
    };
    FlightGate {
        inflight: inflight.clone(),
        key,
        gate,
    }
}

/// Release a single-flight hold.
pub fn release_flight(inflight: &Inflight, key: &str, guard: FlightGuard) {
    debug_assert!(Arc::ptr_eq(&inflight.gates, &guard.flight.inflight.gates));
    debug_assert_eq!(key, guard.flight.key.as_ref());
    drop(guard);
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

/// The in-memory caches a cached (proxy) role serves from, and the per-project epochs that retire
/// them.
///
/// Every warm request is a lookup here; a mutation bumps only the affected project's epoch, so a stale
/// hot page misses by key while every other project keeps serving. The store fields are public so a
/// driver can stream directly into them on the serve path; the methods cover the common gestures.
pub struct ServingCache {
    /// Single-flight locks; see [`flight_gate`].
    pub inflight: Inflight,
    /// Transformed page bytes paired with their unix expiry. Keys carry their project's epoch, so a
    /// mutation to that project invalidates by key miss; the expiry honours each page's upstream
    /// lifetime, and moka's own time-to-live is a coarse eviction backstop.
    pub hot: moka::sync::Cache<String, (bytes::Bytes, i64, Option<u64>)>,
    /// Short-lived upstream misses (key → unix expiry), kept apart from stored pages so a `404` adds
    /// no row to the persistent cache.
    pub negative: moka::sync::Cache<String, i64>,
    /// Per-project hot-cache epochs, bumped by every mutation that changes what one project serves.
    /// Absent means epoch `0`. A `BTreeMap` keeps `hot_key`'s serve-path lookup free of `RandomState`,
    /// so cachegrind instruction counts stay stable.
    pub hot_epochs: Mutex<BTreeMap<String, u64>>,
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
                .weigher(|key: &String, (value, _, _): &(bytes::Bytes, i64, Option<u64>)| {
                    u32::try_from(key.len() + value.len()).unwrap_or(u32::MAX)
                })
                .time_to_live(std::time::Duration::from_secs(
                    u64::try_from(ttl_secs.max(1)).unwrap_or(1800),
                ))
                .build(),
            negative: moka::sync::Cache::builder().max_capacity(65_536).build(),
            hot_epochs: Mutex::new(BTreeMap::new()),
        }
    }

    /// Retire an uncontended flight before its guard returns.
    pub fn forget_flight(&self, key: &str) {
        self.inflight
            .gates
            .remove_if(key, |_, gate| gate.users.load(Ordering::Acquire) == 1);
    }

    /// A hot-cache entry still within its freshness window at `now`; an expired entry misses.
    #[must_use]
    pub fn hot_fresh(&self, key: &str, now: i64) -> Option<bytes::Bytes> {
        let (bytes, expires_at, _) = self.hot.get(key)?;
        (now < expires_at).then_some(bytes)
    }

    /// A fresh hot-cache entry with the source revision attached by its driver.
    #[must_use]
    pub fn hot_fresh_versioned(&self, key: &str, now: i64) -> Option<(bytes::Bytes, Option<u64>)> {
        let (bytes, expires_at, revision) = self.hot.get(key)?;
        (now < expires_at).then_some((bytes, revision))
    }

    /// Store `bytes` as the hot representation of `key` until `expires_at`.
    pub fn store_hot(&self, key: String, bytes: bytes::Bytes, expires_at: i64) {
        self.hot.insert(key, (bytes, expires_at, None));
    }

    /// Store bytes with the source revision that produced them.
    pub fn store_hot_versioned(&self, key: String, bytes: bytes::Bytes, expires_at: i64, revision: Option<u64>) {
        self.hot.insert(key, (bytes, expires_at, revision));
    }

    /// The hot-cache key for one representation of a page as served on `route` right now.
    ///
    /// `variant` separates the representations a page has (PEP 691 JSON, PEP 503 HTML, legacy JSON):
    /// different bytes. The project's epoch makes a mutation to it retire them all at once, while
    /// leaving other projects' keys unchanged.
    ///
    /// # Panics
    /// Panics if the epoch map's mutex was poisoned.
    #[must_use]
    pub fn hot_key(&self, route: &str, project: &str, variant: &str) -> String {
        let epoch = self
            .hot_epochs
            .lock()
            .expect("hot epoch lock")
            .get(project)
            .copied()
            .unwrap_or(0);
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

    /// Retire a project's hot-cache entries after a mutation by advancing the epoch its keys carry.
    /// Every other project's entries stay hittable, so one project's change does not cold-start the
    /// rest.
    ///
    /// # Panics
    /// Panics if the epoch map's mutex was poisoned.
    pub fn invalidate_hot(&self, project: &str) {
        *self
            .hot_epochs
            .lock()
            .expect("hot epoch lock")
            .entry(project.to_owned())
            .or_default() += 1;
    }
}

#[cfg(test)]
mod tests {
    use super::{Inflight, flight_gate, within_stale_bound};

    #[tokio::test]
    async fn test_same_key_waiters_share_one_gate() {
        let inflight = Inflight::default();
        let first = flight_gate(&inflight, "digest").lock_owned().await;
        assert!(flight_gate(&inflight, "digest").try_lock_owned().is_err());

        drop(first);
        drop(flight_gate(&inflight, "digest").try_lock_owned().unwrap());
    }

    #[tokio::test]
    async fn test_distinct_keys_lock_independently() {
        let inflight = Inflight::default();
        let first = flight_gate(&inflight, "first").lock().await;
        let second = flight_gate(&inflight, "second").try_lock_owned().unwrap();

        drop((first, second));
    }

    #[tokio::test]
    async fn test_cancelled_waiter_retires_its_registration() {
        let inflight = Inflight::default();
        let producer = flight_gate(&inflight, "digest").lock_owned().await;
        let mut waiting = tokio::spawn(flight_gate(&inflight, "digest").lock_owned());
        tokio::task::yield_now().await;

        waiting.abort();
        let cancelled = (&mut waiting).await.unwrap_err().is_cancelled();
        drop(waiting);
        assert!(cancelled);

        drop(producer);
        drop(flight_gate(&inflight, "digest").try_lock_owned().unwrap());
    }

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
