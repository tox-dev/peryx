use std::collections::BTreeMap;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

use dashmap::DashMap;
use dashmap::mapref::entry::Entry;

#[derive(Clone, Debug, Default)]
pub struct Inflight {
    gates: Arc<DashMap<Arc<str>, Arc<Gate>>>,
}

impl Inflight {
    /// Subscribe to owners joining the active flight for `key`.
    #[must_use]
    pub fn subscribe(&self, key: &str) -> Option<FlightEvents> {
        Some(FlightEvents(self.gates.get(key)?.joins.subscribe()))
    }
}

#[derive(Debug)]
struct Gate {
    mutex: Arc<tokio::sync::Mutex<()>>,
    users: AtomicUsize,
    joins: tokio::sync::watch::Sender<u64>,
}

impl Gate {
    fn new() -> Self {
        Self {
            mutex: Arc::default(),
            users: AtomicUsize::new(1),
            joins: tokio::sync::watch::channel(0).0,
        }
    }
}

#[derive(Debug)]
pub struct FlightGate {
    inflight: Inflight,
    key: Arc<str>,
    gate: Arc<Gate>,
}

impl FlightGate {
    pub async fn lock(self) -> FlightGuard {
        self.lock_owned().await
    }

    pub async fn lock_owned(self) -> FlightGuard {
        let guard = self.gate.mutex.clone().lock_owned().await;
        FlightGuard {
            _guard: guard,
            flight: self,
        }
    }

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
        match self.inflight.gates.entry(self.key.clone()) {
            Entry::Occupied(entry) if Arc::ptr_eq(entry.get(), &self.gate) => {
                if self.gate.users.fetch_sub(1, Ordering::AcqRel) == 1 {
                    entry.remove();
                }
            }
            _ => {
                self.gate.users.fetch_sub(1, Ordering::AcqRel);
            }
        }
    }
}

#[derive(Debug)]
pub struct FlightEvents(tokio::sync::watch::Receiver<u64>);

impl FlightEvents {
    /// Wait for another owner to join the subscribed flight.
    ///
    /// # Errors
    /// Returns when every owner leaves before another joins.
    pub async fn next_join(&mut self) -> Result<(), tokio::sync::watch::error::RecvError> {
        self.0.changed().await
    }
}

#[derive(Debug)]
pub struct FlightGuard {
    _guard: tokio::sync::OwnedMutexGuard<()>,
    flight: FlightGate,
}

#[must_use]
pub fn flight_gate(inflight: &Inflight, key: &str) -> FlightGate {
    let key = Arc::<str>::from(key);
    let gate = match inflight.gates.entry(key.clone()) {
        Entry::Occupied(entry) => {
            let gate = entry.get().clone();
            gate.users.fetch_add(1, Ordering::Relaxed);
            gate.joins.send_modify(|joins| *joins += 1);
            gate
        }
        Entry::Vacant(entry) => entry.insert(Arc::new(Gate::new())).clone(),
    };
    FlightGate {
        inflight: inflight.clone(),
        key,
        gate,
    }
}

pub fn release_flight(inflight: &Inflight, key: &str, guard: FlightGuard) {
    debug_assert!(Arc::ptr_eq(&inflight.gates, &guard.flight.inflight.gates));
    debug_assert_eq!(key, guard.flight.key.as_ref());
    drop(guard);
}

/// Limit stale responses during upstream failure. Zero allows any age.
#[must_use]
pub const fn within_stale_bound(now: i64, max_stale_secs: i64, fetched_at: i64, freshness_secs: i64) -> bool {
    max_stale_secs == 0 || now.saturating_sub(fetched_at) < freshness_secs.saturating_add(max_stale_secs)
}

pub struct ServingCache {
    pub inflight: Inflight,
    pub hot: moka::sync::Cache<String, (bytes::Bytes, i64, Option<u64>)>,
    pub negative: moka::sync::Cache<String, i64>,
    /// A `BTreeMap` keeps benchmark instruction counts deterministic.
    pub resource_epochs: Mutex<BTreeMap<String, u64>>,
}

impl ServingCache {
    #[must_use]
    pub fn new(hot_cache_bytes: u64, ttl_secs: i64) -> Self {
        Self {
            inflight: Inflight::default(),
            hot: moka::sync::Cache::builder()
                .max_capacity(hot_cache_bytes)
                .weigher(|key: &String, (value, _, _): &(bytes::Bytes, i64, Option<u64>)| {
                    u32::try_from(key.len() + value.len()).unwrap_or(u32::MAX)
                })
                .time_to_live(std::time::Duration::from_secs(ttl_secs.max(1).unsigned_abs()))
                .build(),
            negative: moka::sync::Cache::builder().max_capacity(65_536).build(),
            resource_epochs: Mutex::new(BTreeMap::new()),
        }
    }

    pub fn forget_flight(&self, key: &str) {
        self.inflight
            .gates
            .remove_if(key, |_, gate| gate.users.load(Ordering::Acquire) == 1);
    }

    #[must_use]
    pub fn hot_fresh(&self, key: &str, now: i64) -> Option<bytes::Bytes> {
        let (bytes, expires_at, _) = self.hot.get(key)?;
        (now < expires_at).then_some(bytes)
    }

    #[must_use]
    pub fn hot_fresh_versioned(&self, key: &str, now: i64) -> Option<(bytes::Bytes, Option<u64>)> {
        let (bytes, expires_at, revision) = self.hot.get(key)?;
        (now < expires_at).then_some((bytes, revision))
    }

    pub fn store_hot(&self, key: String, bytes: bytes::Bytes, expires_at: i64) {
        self.hot.insert(key, (bytes, expires_at, None));
    }

    pub fn store_hot_versioned(&self, key: String, bytes: bytes::Bytes, expires_at: i64, revision: Option<u64>) {
        self.hot.insert(key, (bytes, expires_at, revision));
    }

    /// # Panics
    /// Panics if the epoch map's mutex was poisoned.
    #[must_use]
    pub fn representation_key(&self, route: &str, resource: &str, representation: &str) -> String {
        let epoch = self
            .resource_epochs
            .lock()
            .expect("hot epoch lock")
            .get(resource)
            .copied()
            .unwrap_or(0);
        format!("{route}\u{0}{resource}\u{0}{representation}\u{0}{epoch}")
    }

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

    pub fn remember_negative(&self, key: String, expires_at: i64) {
        self.negative.insert(key, expires_at);
    }

    /// # Panics
    /// Panics if the epoch map's mutex was poisoned.
    pub fn invalidate_resource(&self, resource: &str) {
        *self
            .resource_epochs
            .lock()
            .expect("hot epoch lock")
            .entry(resource.to_owned())
            .or_default() += 1;
    }
}

#[cfg(test)]
#[path = "../tests/unit/serving/tests.rs"]
mod tests;
