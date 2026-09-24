use std::any::Any;
use std::collections::BTreeMap;
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
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

/// What the last completed flight on a gate produced, for the callers queued behind it.
type FlightOutcome = Option<Arc<dyn Any + Send + Sync>>;

#[derive(Debug)]
struct Gate {
    mutex: Arc<tokio::sync::Mutex<FlightOutcome>>,
    /// Flights completed so far. A caller reads it before queueing, so a later value proves a flight finished
    /// while it waited.
    completions: AtomicU64,
    users: AtomicUsize,
    /// Subscribers wait for the next join, so the channel carries the event and no count.
    joins: tokio::sync::watch::Sender<()>,
}

impl Gate {
    fn new() -> Self {
        Self {
            mutex: Arc::default(),
            completions: AtomicU64::new(0),
            users: AtomicUsize::new(1),
            joins: tokio::sync::watch::channel(()).0,
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
        FlightGuard { guard, flight: self }
    }

    /// Wait for the gate, and answer with the outcome of a flight that completed meanwhile.
    ///
    /// A caller that finds no such outcome leads the next flight, including when the flight ahead of it
    /// was dropped before it completed. The outcome may come from a request sent before this caller
    /// arrived, which is what joining a flight in progress means.
    pub async fn lock_or_join<T: Clone + 'static>(self) -> Turn<T> {
        let queued_at = self.gate.completions.load(Ordering::Acquire);
        let guard = self.lock_owned().await;
        let completed = guard.flight.gate.completions.load(Ordering::Acquire) != queued_at;
        match guard.guard.as_deref().and_then(|outcome| outcome.downcast_ref::<T>()) {
            Some(outcome) if completed => Turn::Joined(outcome.clone()),
            _ => Turn::Lead(guard),
        }
    }

    /// # Errors
    /// Returns Tokio's lock error while another caller holds the slot.
    pub fn try_lock_owned(self) -> Result<FlightGuard, tokio::sync::TryLockError> {
        let guard = self.gate.mutex.clone().try_lock_owned()?;
        Ok(FlightGuard { guard, flight: self })
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
pub struct FlightEvents(tokio::sync::watch::Receiver<()>);

impl FlightEvents {
    /// Wait for another owner to join the subscribed flight.
    ///
    /// # Errors
    /// Returns when every owner leaves before another joins.
    pub async fn next_join(&mut self) -> Result<(), tokio::sync::watch::error::RecvError> {
        self.0.changed().await
    }
}

/// Whether a caller joined a completed flight or leads the next one.
#[derive(Debug)]
pub enum Turn<T> {
    Joined(T),
    Lead(FlightGuard),
}

#[derive(Debug)]
pub struct FlightGuard {
    guard: tokio::sync::OwnedMutexGuard<FlightOutcome>,
    flight: FlightGate,
}

impl FlightGuard {
    /// Release the gate with `outcome` for the callers queued behind this flight.
    pub fn complete<T: Send + Sync + 'static>(mut self, outcome: T) {
        *self.guard = Some(Arc::new(outcome));
        self.flight.gate.completions.fetch_add(1, Ordering::Release);
    }
}

#[must_use]
pub fn flight_gate(inflight: &Inflight, key: &str) -> FlightGate {
    let key = Arc::<str>::from(key);
    let gate = match inflight.gates.entry(key.clone()) {
        Entry::Occupied(entry) => {
            let gate = entry.get().clone();
            gate.users.fetch_add(1, Ordering::Relaxed);
            gate.joins.send_replace(());
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

const NEGATIVE_CACHE_BYTES: u64 = 8 * 1024 * 1024;
// Moka does not expose allocation size, so this covers its Arc, table slot, and entry metadata.
const NEGATIVE_CACHE_ENTRY_OVERHEAD_BYTES: usize = 128;
const RESOURCE_TICKET_CACHE_BYTES: u64 = 8 * 1024 * 1024;
const RESOURCE_TICKET_CACHE_ENTRY_OVERHEAD_BYTES: usize = 128;

struct ResourceTickets {
    entries: BTreeMap<String, u64>,
    charged_bytes: usize,
    next: u64,
}

impl ResourceTickets {
    const fn new() -> Self {
        Self {
            entries: BTreeMap::new(),
            charged_bytes: 0,
            next: 0,
        }
    }

    fn get_or_insert(&mut self, key: String) -> u64 {
        if let Some(&ticket) = self.entries.get(&key) {
            return ticket;
        }
        let ticket = self.next;
        self.next = self.next.checked_add(1).expect("resource ticket overflow");
        let charge = resource_ticket_weight(&key);
        if u64::from(charge) <= RESOURCE_TICKET_CACHE_BYTES {
            while self.charged_bytes.saturating_add(usize::try_from(charge).unwrap())
                > usize::try_from(RESOURCE_TICKET_CACHE_BYTES).unwrap()
            {
                let (key, _) = self.entries.pop_first().expect("ticket cache is charged");
                self.charged_bytes -= usize::try_from(resource_ticket_weight(&key)).unwrap();
            }
            self.charged_bytes += usize::try_from(charge).unwrap();
            self.entries.insert(key, ticket);
        }
        ticket
    }

    fn invalidate(&mut self, key: &str) {
        if let Some((key, _)) = self.entries.remove_entry(key) {
            self.charged_bytes -= usize::try_from(resource_ticket_weight(&key)).unwrap();
        }
    }

    fn clear(&mut self) {
        self.entries.clear();
        self.charged_bytes = 0;
    }
}

pub struct ServingCache {
    pub inflight: Inflight,
    pub hot: moka::sync::Cache<String, (bytes::Bytes, i64, Option<u64>)>,
    pub negative: moka::sync::Cache<String, i64>,
    resource_tickets: Mutex<ResourceTickets>,
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
            negative: moka::sync::Cache::builder()
                .max_capacity(NEGATIVE_CACHE_BYTES)
                .weigher(|key: &String, _: &i64| negative_weight(key))
                .support_invalidation_closures()
                .build(),
            resource_tickets: Mutex::new(ResourceTickets::new()),
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
    /// Panics if the resource ticket mutex was poisoned or the ticket counter exhausted `u64`.
    #[must_use]
    pub fn representation_key(&self, route: &str, resource: &str, representation: &str) -> String {
        let key = format!("{route}\u{0}{resource}");
        let ticket = self
            .resource_tickets
            .lock()
            .expect("resource ticket lock")
            .get_or_insert(key);
        format!("{route}\u{0}{resource}\u{0}{representation}\u{0}{ticket}")
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
        self.remember_negative_at(key, expires_at, system_now());
    }

    pub fn remember_negative_at(&self, key: String, expires_at: i64, now: i64) {
        if u64::from(negative_weight(&key)) > NEGATIVE_CACHE_BYTES {
            self.maintain_negative(now);
            return;
        }
        self.negative.insert(key, expires_at);
        self.maintain_negative(now);
    }

    fn maintain_negative(&self, now: i64) {
        // Moka's internal clock cannot share the serving owner's injected epoch clock.
        self.negative
            .invalidate_entries_if(move |_, expires_at| *expires_at <= now)
            .expect("negative invalidation is enabled");
        self.negative.run_pending_tasks();
    }

    /// # Panics
    /// Panics if the resource ticket mutex was poisoned.
    pub fn invalidate_resource(&self, route: &str, resource: &str) {
        self.resource_tickets
            .lock()
            .expect("resource ticket lock")
            .invalidate(&format!("{route}\u{0}{resource}"));
    }

    /// # Panics
    /// Panics if the resource ticket mutex was poisoned.
    pub fn invalidate_all(&self) {
        self.hot.invalidate_all();
        self.negative.invalidate_all();
        self.resource_tickets.lock().expect("resource ticket lock").clear();
    }
}

fn negative_weight(key: &String) -> u32 {
    let bytes = std::mem::size_of::<String>()
        .saturating_add(key.capacity())
        .saturating_add(std::mem::size_of::<i64>())
        .saturating_add(NEGATIVE_CACHE_ENTRY_OVERHEAD_BYTES);
    u32::try_from(bytes).unwrap_or(u32::MAX)
}

fn resource_ticket_weight(key: &String) -> u32 {
    let bytes = std::mem::size_of::<String>()
        .saturating_add(key.capacity())
        .saturating_add(std::mem::size_of::<u64>())
        .saturating_add(RESOURCE_TICKET_CACHE_ENTRY_OVERHEAD_BYTES)
        .saturating_add(128);
    u32::try_from(bytes).unwrap_or(u32::MAX)
}

fn system_now() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |duration| i64::try_from(duration.as_secs()).unwrap_or(i64::MAX))
}

#[cfg(test)]
#[path = "../tests/unit/serving/tests.rs"]
mod tests;
