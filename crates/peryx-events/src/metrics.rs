//! Usage metrics, aggregated off the request path.
//!
//! Handlers record events with one non-blocking channel send; a dedicated OS thread aggregates them
//! into a tree (index → project → file) that the dashboard and `/+stats` read. The request path
//! never takes the aggregation lock for writing.
//!
//! Counters are grouped by the role that owns them: a neutral [`BaseCounters`] every index reports,
//! a [`CachedCounters`] group only a caching index fills, a [`HostedCounters`] group only an upload
//! store fills, and an open [`EcosystemCounters`] map whose keys each ecosystem driver declares
//! through [`MetricFamily`]. The core stays ecosystem-neutral: a driver names and describes its own
//! families supplied by ecosystem adapters, and the render layer scopes each family to the roles
//! and ecosystem that emit it, so a hosted index never reports a caching counter.

use std::collections::{BTreeMap, HashMap};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::mpsc::{Receiver, RecvTimeoutError, SyncSender, TrySendError, sync_channel};
#[cfg(any(test, feature = "test-util"))]
use std::sync::mpsc::{Sender, channel};
use std::sync::{Arc, RwLock};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};

use peryx_core::Role;
use peryx_ha::{AggregateDelta, AggregateKey, AggregateRow, AnalyticsBatch, AuthorityEpoch, IntervalId, ProducerId};
use peryx_storage::meta::AnalyticsHandle;

/// Unix seconds, the shape every peryx clock reports, so the aggregator can date a download's UTC
/// bucket without pulling in a heavier time type.
pub type Clock = Arc<dyn Fn() -> i64 + Send + Sync>;

const SECONDS_PER_DAY: i64 = 86_400;

/// The window a usage query spans when it names no start: the trailing month up to its end day.
const DEFAULT_USAGE_WINDOW_DAYS: i64 = 30;

/// The widest window a single usage query may span, so scan and sort work stays bounded even under
/// unbounded retention.
const MAX_USAGE_WINDOW_DAYS: i64 = 366;

/// The current on-disk shape of the daily-usage snapshot. A snapshot written under any other schema
/// is rebuilt from zero rather than trusted, so a forward-incompatible format never blocks startup.
const DAILY_SCHEMA: u32 = 1;

/// How many unconsumed events the recording channel buffers before [`Metrics::record`] starts
/// dropping. Recording is loss-tolerant, so the queue caps overload at a fixed slot count rather than
/// absorbing it into an unbounded allocation that grows with traffic duration. Each buffered download
/// retains a handful of short strings, so the ceiling holds metrics memory to tens of megabytes even
/// when the aggregator stalls behind slow analytics writes.
const EVENT_QUEUE_CAPACITY: usize = 65_536;

/// Log the running drop total on the first drop and then once per this many further drops, so a
/// sustained overload stays visible in logs without emitting a line per lost event.
const DROP_LOG_INTERVAL: u64 = 1_024;

/// The longest a recorded download waits for its durable checkpoint. The aggregator folds every
/// download into the live tree and daily buckets as it drains them, but serializes and writes the two
/// full snapshots at most once per interval instead of once per drained batch. A lull that hands the
/// aggregator one download at a time therefore no longer pays a whole-state serialization per download;
/// the cost is bounded per unit time, not per download. An orderly shutdown and each test settle
/// barrier flush regardless, so durability never depends on traffic shape.
const FLUSH_INTERVAL: Duration = Duration::from_secs(5);

/// One request-path observation.
#[derive(Debug, Clone)]
pub enum Event {
    /// An index listing was served.
    Page { route: String, project: String },
    /// An artifact was served, with its size. `filename` keys the per-file breakdown; `project` is
    /// the pre-normalized owning project (the ecosystem driver derives it, so this stays neutral).
    ///
    /// `version` and `source` feed the durable daily aggregate: `version` is the distribution version
    /// the driver parsed from the artifact identity (`None` when the ecosystem has no version, as with
    /// content-addressed artifacts), and `source` is the routed upstream a cache miss fetched from
    /// (`None` when the bytes came straight from the local store, so no upstream was routed to). The
    /// driver derives both without touching the store, keeping collection off the request path.
    Download {
        route: String,
        project: String,
        filename: String,
        version: Option<String>,
        source: Option<String>,
        bytes: u64,
    },
    /// An ecosystem-specific counter fired. `family` is a static key the ecosystem driver declares
    /// through [`MetricFamily`]; `filename` keys the
    /// per-file breakdown when the observation is about one artifact.
    Ecosystem {
        route: String,
        project: String,
        filename: Option<String>,
        family: &'static str,
    },
    /// A distribution was uploaded.
    Upload { route: String, project: String },
    /// A revalidation ran against upstream (on demand or from the background refresher);
    /// `changed` marks the upstream page differing from the cached copy.
    Refresh {
        route: String,
        project: String,
        changed: bool,
    },
    /// Upstream was unreachable or errored, and the cached copy was served instead.
    StaleServed { route: String, project: String },
    /// Upstream was unreachable and there was nothing cached to fall back to.
    UpstreamError { route: String, project: String },
    /// A streamed download hashed differently than its registration; the blob was not admitted.
    BlobRejected { route: String, project: String },
    /// A remote root-catalog synchronization completed. This is index-level operational state: it
    /// never creates a project or file node in the metrics tree.
    CatalogSync {
        route: String,
        outcome: CatalogSyncOutcome,
        projects: Option<u64>,
    },
}

/// The bounded outcomes a catalog synchronization reports.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CatalogSyncOutcome {
    Published,
    NotModified,
    Error,
}

/// Counters every index reports, whatever its role or ecosystem.
#[derive(Debug, Default, Clone, Serialize)]
pub struct BaseCounters {
    pub pages: u64,
    pub downloads: u64,
    pub bytes: u64,
    /// Downloads whose bytes failed digest verification and were not cached.
    pub rejected: u64,
}

/// Counters only a caching index fills: everything about revalidating against an upstream.
#[derive(Debug, Default, Clone, Serialize)]
pub struct CachedCounters {
    pub refreshes: u64,
    /// Refreshes that found the upstream page changed.
    pub changed: u64,
    /// Pages served from cache because upstream was unavailable.
    pub stale_served: u64,
    pub upstream_errors: u64,
    pub catalog_syncs: u64,
    pub catalog_published: u64,
    pub catalog_not_modified: u64,
    pub catalog_errors: u64,
    /// Names in the most recently published or revalidated root catalog.
    pub catalog_projects: u64,
}

/// Counters only a hosted index fills.
#[derive(Debug, Default, Clone, Serialize)]
pub struct HostedCounters {
    pub uploads: u64,
}

/// Ecosystem-specific counters, keyed by the family key its driver declares. Open by construction so
/// a new ecosystem adds keys without touching the neutral core.
pub type EcosystemCounters = BTreeMap<&'static str, u64>;

/// One counter family an ecosystem driver publishes: how to store, expose, and scope it.
///
/// The core renders `/metrics`, `/+status`, and the dashboard from these descriptors instead of
/// hardcoding any ecosystem's vocabulary.
#[derive(Debug, Clone, Copy)]
pub struct MetricFamily {
    /// The [`EcosystemCounters`] key this family accumulates under.
    pub key: &'static str,
    /// The Prometheus metric name, e.g. `peryx_metadata_served_total`.
    pub prom_name: &'static str,
    /// The Prometheus `# HELP` line.
    pub help: &'static str,
    /// The dashboard label supplied by the ecosystem adapter.
    pub ui_label: &'static str,
    /// The roles that emit this family; the render layer skips it for any other role.
    pub roles: &'static [Role],
}

/// One ecosystem's activity rolled up across all its indexes, for the `/+status` summary and the
/// dashboard. `families` holds that ecosystem's own counters keyed by family key.
#[derive(Debug, Default, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct EcosystemSummary {
    pub ecosystem: String,
    pub pages: u64,
    pub downloads: u64,
    pub bytes: u64,
    pub rejected: u64,
    pub uploads: u64,
    pub families: BTreeMap<String, u64>,
}

/// Durable download usage for one project in one repository.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct PackageUsage {
    pub repository: String,
    pub project: String,
    pub downloads: u64,
    pub bytes: u64,
}

/// One project version's downloads over the queried window.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct VersionUsage {
    pub repository: String,
    pub project: String,
    /// The distribution version, or `None` when the ecosystem reported none.
    pub version: Option<String>,
    pub downloads: u64,
    pub bytes: u64,
}

/// One project's downloads attributed to one routed source over the queried window. The source
/// dimension is operator-scoped, so only an operator query builds these rows.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct SourceUsage {
    pub repository: String,
    pub project: String,
    /// The routed upstream, or `None` when the bytes were served from the local store.
    pub source: Option<String>,
    pub downloads: u64,
    pub bytes: u64,
}

/// A project with durable lifetime downloads but none inside the queried window.
///
/// `lifetime_downloads` distinguishes a package idle in the window from one whose
/// activity predates the retained interval reported alongside it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct UnusedPackage {
    pub repository: String,
    pub project: String,
    pub lifetime_downloads: u64,
}

/// One UTC-day time bucket with explicit half-open `[start_unix, end_unix)` bounds, so a caller reads
/// the delta aggregation temporality of the OpenTelemetry metrics data model directly off each row.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct TimelineBucket {
    pub day: i64,
    pub start_unix: i64,
    pub end_unix: i64,
    pub downloads: u64,
    pub bytes: u64,
}

/// The resolved day window a usage query ran over.
///
/// `retained_from_day` is the retention floor (absent under unbounded retention);
/// `window_clamped_to_retention` marks a requested start that predated it, so a caller can tell
/// missing rows apart from data aged out of retention.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct UsageInterval {
    pub from_day: i64,
    pub to_day: i64,
    pub retained_from_day: Option<i64>,
    pub window_clamped_to_retention: bool,
}

/// A driver's counter family as the dashboard needs it: the storage key, its human label, and the
/// roles that report it.
///
/// Lets the neutral UI label ecosystem counters without hardcoding any ecosystem's vocabulary.
#[derive(Debug, Default, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FamilyDescriptor {
    pub key: String,
    pub label: String,
    pub roles: Vec<String>,
}

/// Counters at one level of the tree, grouped by the role that owns each group.
#[derive(Debug, Default, Clone, Serialize)]
pub struct Counters {
    pub base: BaseCounters,
    pub cached: CachedCounters,
    pub hosted: HostedCounters,
    pub ecosystem: EcosystemCounters,
}

/// Per-file counters.
#[derive(Debug, Default, Clone, Serialize)]
pub struct FileStats {
    pub downloads: u64,
    pub bytes: u64,
    pub ecosystem: EcosystemCounters,
}

/// Per-project counters plus the files underneath.
#[derive(Debug, Default, Clone, Serialize)]
pub struct ProjectStats {
    pub totals: Counters,
    pub files: HashMap<String, FileStats>,
}

/// Per-index counters plus the projects underneath.
#[derive(Debug, Default, Clone, Serialize)]
pub struct IndexStats {
    pub totals: Counters,
    pub projects: HashMap<String, ProjectStats>,
}

/// The whole tree, index route at the top.
pub type StatsTree = HashMap<String, IndexStats>;

/// One persisted file's usage: enough to rebuild the download and byte totals at every level, since
/// each download increments its file, project, and index together.
#[derive(Debug, Serialize, Deserialize)]
struct FileDownloadRow {
    route: String,
    project: String,
    filename: String,
    downloads: u64,
    bytes: u64,
}

/// The durable slice of the tree: per-file download counts and bytes.
///
/// Only usage data survives a restart. The operational counters (pages, uploads, cache refreshes,
/// upstream errors) are live gauges the process rebuilds as it serves, so persisting them would
/// carry stale operational state across restarts without answering a usage question.
#[derive(Debug, Default, Serialize, Deserialize)]
struct DownloadSnapshot {
    files: Vec<FileDownloadRow>,
}

/// The identity of one daily-usage bucket: a repository/project's downloads of one version, routed
/// from one source, on one UTC day. `day` leads the ordering so retention drops an expired prefix in
/// one `BTreeMap` split. Every field is a bounded server-side label, never a client identity, address,
/// or credential, so the aggregate stays low-cardinality per Prometheus guidance.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
struct DailyKey {
    day: i64,
    repository: String,
    project: String,
    version: String,
    source: String,
}

#[derive(Debug, Default, Clone, Copy)]
struct DailyTotals {
    downloads: u64,
    bytes: u64,
}

/// The live daily aggregate: independent buckets the aggregator folds downloads into and retention
/// prunes. Kept apart from the all-time per-file [`DownloadSnapshot`] so time-bucketed usage evolves
/// without disturbing the totals that rebuild the live tree.
type DailyBuckets = BTreeMap<DailyKey, DailyTotals>;

/// One daily-usage bucket as callers read it: the full dimension tuple plus its totals.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DailyUsage {
    /// The UTC day, in whole days since the Unix epoch.
    pub day: i64,
    pub repository: String,
    pub project: String,
    /// The distribution version, or empty when the ecosystem reported none.
    pub version: String,
    /// The routed upstream, or empty when the bytes were served from the local store.
    pub source: String,
    pub downloads: u64,
    pub bytes: u64,
}

/// The durable daily aggregate: a schema tag guarding the rows, so a future format change is a
/// deliberate migration rather than a silent misread.
#[derive(Debug, Default, Serialize, Deserialize)]
struct DailySnapshot {
    schema: u32,
    buckets: Vec<DailyUsage>,
}

/// Encode daily-usage buckets into the durable snapshot bytes [`AnalyticsHandle::save_daily`] persists.
///
/// A test seeds a producer's sealed-day aggregate directly into its store before the node boots, writing
/// the same schema-tagged snapshot [`Metrics::start_durable`] restores, so the seed folds back through
/// the same path a live download would have written. Gated to `test-util` and this crate's own tests:
/// production writes this snapshot only from the aggregator thread, never from a caller.
///
/// # Panics
/// Panics if the buckets cannot be serialized to JSON.
#[cfg(any(test, feature = "test-util"))]
#[must_use]
pub fn encode_daily_snapshot(buckets: Vec<DailyUsage>) -> Vec<u8> {
    serde_json::to_vec(&DailySnapshot {
        schema: DAILY_SCHEMA,
        buckets,
    })
    .expect("serialize daily usage snapshot")
}

/// The UTC day a Unix-seconds instant falls on, flooring toward the epoch so pre-epoch instants (only
/// a misconfigured clock reaches them) still map to a stable day rather than rounding across zero.
const fn utc_day(unix_secs: i64) -> i64 {
    unix_secs.div_euclid(SECONDS_PER_DAY)
}

/// Present an absent daily dimension (empty version or source) as JSON `null` rather than `""`.
fn non_empty(value: String) -> Option<String> {
    (!value.is_empty()).then_some(value)
}

/// The system-wall-clock source used when no clock is injected: Unix seconds, saturating rather than
/// panicking if the host clock predates the epoch.
fn system_clock() -> Clock {
    Arc::new(|| {
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map_or(0, |elapsed| i64::try_from(elapsed.as_secs()).unwrap_or(i64::MAX))
    })
}

/// What the aggregator thread pulls off its channel: request-path observations, plus a test-only
/// barrier the aggregator acknowledges once it has drained and persisted everything queued ahead of
/// it, so a test can await a deterministic point instead of polling the shared snapshots. The barrier
/// compiles under this crate's own tests and under the `test-util` feature downstream test crates
/// enable to reach [`Metrics::settle`].
enum Message {
    Event(Event),
    #[cfg(any(test, feature = "test-util"))]
    Barrier(Sender<()>),
}

/// The recording half handed to request handlers: a clone-cheap sender plus the shared snapshots.
#[derive(Clone)]
pub struct Metrics {
    sender: SyncSender<Message>,
    tree: Arc<RwLock<StatsTree>>,
    daily: Arc<RwLock<DailyBuckets>>,
    /// Events [`Metrics::record`] dropped because the bounded queue was full, so overload is
    /// countable rather than silently swallowed into unbounded memory.
    dropped: Arc<AtomicU64>,
    /// Dates a usage query's window; the same clock the aggregator buckets downloads by.
    clock: Clock,
    /// The daily-bucket retention bound, so a query can report and clamp to the retained floor.
    retention_days: Option<u32>,
}

impl Metrics {
    /// Start an ephemeral aggregator whose counters live only as long as the process, dating downloads
    /// off the system clock and keeping daily buckets without limit.
    ///
    /// # Panics
    /// Panics if the OS refuses to spawn the aggregator thread.
    #[must_use]
    pub fn start() -> Self {
        Self::spawn(None, None, system_clock(), EVENT_QUEUE_CAPACITY)
    }

    /// Start an aggregator with durable usage: restore the persisted per-file totals and daily buckets,
    /// checkpoint both on the coalescing [`FLUSH_INTERVAL`] once a download has landed, and prune daily
    /// buckets older than `retention_days` (kept without limit when `None`). `clock` dates each download's
    /// UTC bucket. Folding, persistence, and pruning run on the aggregator thread, never the request path.
    ///
    /// # Panics
    /// Panics if the OS refuses to spawn the aggregator thread.
    #[must_use]
    pub fn start_durable(store: AnalyticsHandle, retention_days: Option<u32>, clock: Clock) -> Self {
        Self::spawn(Some(store), retention_days, clock, EVENT_QUEUE_CAPACITY)
    }

    fn spawn(store: Option<AnalyticsHandle>, retention_days: Option<u32>, clock: Clock, capacity: usize) -> Self {
        let (sender, receiver) = sync_channel(capacity);
        let mut initial = StatsTree::new();
        if let Some(snapshot) = store
            .as_ref()
            .and_then(|store| store.load().ok().flatten())
            .and_then(|bytes| serde_json::from_slice::<DownloadSnapshot>(&bytes).ok())
        {
            restore_downloads(&mut initial, snapshot);
        }
        let mut daily_initial = DailyBuckets::new();
        if let Some(snapshot) = store
            .as_ref()
            .and_then(|store| store.load_daily().ok().flatten())
            .and_then(|bytes| serde_json::from_slice::<DailySnapshot>(&bytes).ok())
            .filter(|snapshot| snapshot.schema == DAILY_SCHEMA)
        {
            restore_daily(&mut daily_initial, snapshot);
        }
        if let Some(days) = retention_days {
            expire_daily(&mut daily_initial, clock(), days);
        }
        let tree = Arc::new(RwLock::new(initial));
        let daily = Arc::new(RwLock::new(daily_initial));
        let sink = Arc::clone(&tree);
        let daily_sink = Arc::clone(&daily);
        let query_clock = clock.clone();
        std::thread::Builder::new()
            .name("peryx-metrics".to_owned())
            .spawn(move || {
                aggregate(
                    &receiver,
                    &sink,
                    &daily_sink,
                    store.as_ref(),
                    retention_days,
                    &clock,
                    FLUSH_INTERVAL,
                );
            })
            .expect("spawn metrics thread");
        Self {
            sender,
            tree,
            daily,
            dropped: Arc::new(AtomicU64::new(0)),
            clock: query_clock,
            retention_days,
        }
    }

    /// A snapshot of the daily version-and-source usage buckets, ordered by day then dimension.
    ///
    /// # Panics
    /// Panics if the aggregator thread panicked and poisoned the daily lock.
    #[must_use]
    pub fn daily_usage(&self) -> Vec<DailyUsage> {
        let daily = self.daily.read().expect("metrics lock");
        daily_rows(&daily)
    }

    /// Package this node's daily usage as an idempotent [`AnalyticsBatch`] stamped with `interval`, so
    /// a replica can fold it into its accepted totals exactly once. Each additive bucket becomes one
    /// row carrying only bounded server-side labels and its download and byte totals, never a raw
    /// request or actor history.
    ///
    /// # Panics
    /// Panics if the aggregator thread panicked and poisoned the daily lock.
    #[must_use]
    pub fn export_daily_batch(&self, interval: IntervalId) -> AnalyticsBatch {
        let rows = self
            .daily_usage()
            .into_iter()
            .map(|usage| AggregateRow {
                key: AggregateKey {
                    day: usage.day,
                    repository: usage.repository,
                    project: usage.project,
                    version: usage.version,
                    source: usage.source,
                },
                delta: AggregateDelta {
                    downloads: usage.downloads,
                    bytes: usage.bytes,
                },
            })
            .collect();
        AnalyticsBatch { interval, rows }
    }

    /// Package each sealed UTC day after `after_day` as its own idempotent [`AnalyticsBatch`], one per
    /// day in ascending order, stamped with a per-day [`IntervalId`] so a replica folds each day exactly
    /// once.
    ///
    /// A day is sealed once it is strictly before the current UTC day, so its buckets can no longer grow;
    /// only sealed days are exported, so a re-pull or a restart re-derives the same stable batches. The
    /// day itself is the interval sequence, so the mapping from day to identity never shifts, and
    /// `after_day` skips the days the caller has already acknowledged. `producer` and `epoch` stamp the
    /// generation the sequence belongs to.
    ///
    /// # Panics
    /// Panics if the aggregator thread panicked and poisoned the daily lock.
    #[must_use]
    pub fn export_sealed_day_batches(
        &self,
        producer: &ProducerId,
        epoch: AuthorityEpoch,
        after_day: i64,
    ) -> Vec<AnalyticsBatch> {
        let today = utc_day((self.clock)());
        let mut by_day: BTreeMap<i64, Vec<AggregateRow>> = BTreeMap::new();
        for usage in self.daily_usage() {
            if usage.day <= after_day || usage.day >= today || usage.day < 0 {
                continue;
            }
            by_day.entry(usage.day).or_default().push(AggregateRow {
                key: AggregateKey {
                    day: usage.day,
                    repository: usage.repository,
                    project: usage.project,
                    version: usage.version,
                    source: usage.source,
                },
                delta: AggregateDelta {
                    downloads: usage.downloads,
                    bytes: usage.bytes,
                },
            });
        }
        by_day
            .into_iter()
            .map(|(day, rows)| AnalyticsBatch {
                interval: IntervalId {
                    producer: producer.clone(),
                    epoch,
                    sequence: u64::try_from(day).unwrap_or(0),
                },
                rows,
            })
            .collect()
    }

    /// Record one event; never blocks, and a stopped aggregator is ignored.
    ///
    /// The queue is bounded, so when the aggregator falls behind request traffic the event is dropped
    /// rather than buffered without limit: recording is loss-tolerant, and holding a fixed memory
    /// ceiling matters more than a stray observation. Every drop increments [`Metrics::dropped`] and,
    /// throttled, logs the running total so sustained overload stays visible.
    pub fn record(&self, event: Event) {
        if let Err(TrySendError::Full(_)) = self.sender.try_send(Message::Event(event)) {
            let total = self.dropped.fetch_add(1, Ordering::Relaxed) + 1;
            if total == 1 || total.is_multiple_of(DROP_LOG_INTERVAL) {
                tracing::warn!(target: "peryx::metrics", dropped = total, "metrics event queue full, dropping event");
            }
        }
    }

    /// How many events [`Metrics::record`] has dropped because the bounded queue was full. A non-zero
    /// value means the aggregator fell behind request traffic; the retained work stayed bounded.
    #[must_use]
    pub fn dropped(&self) -> u64 {
        self.dropped.load(Ordering::Relaxed)
    }

    /// Block until the aggregator has drained and applied every event recorded before this call, and
    /// checkpointed the durable snapshots when running durably. The channel is FIFO, so the barrier lands
    /// behind those events; because checkpoints otherwise coalesce on an interval, the barrier forces the
    /// pending checkpoint and is acknowledged only once the totals and snapshots are written, giving a
    /// test a deterministic settle point in place of polling the shared snapshots on a timer.
    ///
    /// Available to this crate's own tests and, through the `test-util` feature, to downstream test
    /// suites that seed events and then read the aggregated view.
    ///
    /// # Panics
    /// Panics if the aggregator thread has stopped before acknowledging the barrier.
    #[cfg(any(test, feature = "test-util"))]
    pub fn settle(&self) {
        let (ack, done) = channel();
        self.sender.send(Message::Barrier(ack)).expect("aggregator alive");
        done.recv().expect("aggregator acknowledged");
    }

    /// A snapshot of one index's totals per route, for the dashboard cards and Prometheus.
    ///
    /// # Panics
    /// Panics if the aggregator thread panicked and poisoned the tree lock.
    #[must_use]
    pub fn index_totals(&self) -> HashMap<String, Counters> {
        let tree = self.tree.read().expect("metrics lock");
        tree.iter()
            .map(|(route, stats)| (route.clone(), stats.totals.clone()))
            .collect()
    }

    /// Snapshot totals for the requested routes in the same order, without copying route values.
    /// Missing routes report zero counters.
    ///
    /// # Panics
    /// Panics if the aggregator thread panicked and poisoned the tree lock.
    #[must_use]
    pub fn totals_for_routes<'a>(&self, routes: impl IntoIterator<Item = &'a str>) -> Vec<Counters> {
        let tree = self.tree.read().expect("metrics lock");
        routes
            .into_iter()
            .map(|route| tree.get(route).map(|stats| stats.totals.clone()).unwrap_or_default())
            .collect()
    }

    /// Today's UTC day off the query clock, in whole days since the Unix epoch. A completeness query
    /// reads it to measure how far the accepted analytics frontier lags the present.
    #[must_use]
    pub fn current_day(&self) -> i64 {
        utc_day((self.clock)())
    }

    /// Resolve a usage query's day window from optional Unix-second bounds. The end defaults to today
    /// and never runs ahead of it; the start defaults to a trailing [`DEFAULT_USAGE_WINDOW_DAYS`], is
    /// capped to [`MAX_USAGE_WINDOW_DAYS`], and is raised to the retention floor when one is set.
    #[must_use]
    pub fn resolve_usage_interval(&self, from_secs: Option<i64>, to_secs: Option<i64>) -> UsageInterval {
        let now_day = utc_day((self.clock)());
        let retained_from_day = self.retention_days.map(|days| now_day - i64::from(days));
        let to_day = to_secs.map_or(now_day, utc_day).min(now_day);
        let requested_from = from_secs.map_or(to_day - (DEFAULT_USAGE_WINDOW_DAYS - 1), utc_day);
        let from_day = requested_from.max(to_day - (MAX_USAGE_WINDOW_DAYS - 1));
        UsageInterval {
            window_clamped_to_retention: retained_from_day.is_some_and(|floor| from_day < floor),
            from_day: retained_from_day.map_or(from_day, |floor| from_day.max(floor)),
            to_day,
            retained_from_day,
        }
    }

    /// Projects by downloads over `interval`, ordered by downloads, bytes, repository, then project.
    ///
    /// # Panics
    /// Panics if the aggregator thread panicked and poisoned the daily lock.
    #[must_use]
    pub fn usage_top(&self, repository: Option<&str>, interval: &UsageInterval) -> Vec<PackageUsage> {
        let mut rows: Vec<_> = self
            .fold_daily(repository, interval, |bucket| {
                (bucket.repository.clone(), bucket.project.clone())
            })
            .into_iter()
            .map(|((repository, project), totals)| PackageUsage {
                repository,
                project,
                downloads: totals.downloads,
                bytes: totals.bytes,
            })
            .collect();
        rows.sort_by(|left, right| {
            right
                .downloads
                .cmp(&left.downloads)
                .then_with(|| right.bytes.cmp(&left.bytes))
                .then_with(|| left.repository.cmp(&right.repository))
                .then_with(|| left.project.cmp(&right.project))
        });
        rows
    }

    /// Project versions by downloads over `interval`, ordered by downloads, bytes, then identity.
    ///
    /// # Panics
    /// Panics if the aggregator thread panicked and poisoned the daily lock.
    #[must_use]
    pub fn usage_versions(&self, repository: Option<&str>, interval: &UsageInterval) -> Vec<VersionUsage> {
        let mut rows: Vec<_> = self
            .fold_daily(repository, interval, |bucket| {
                (
                    bucket.repository.clone(),
                    bucket.project.clone(),
                    bucket.version.clone(),
                )
            })
            .into_iter()
            .map(|((repository, project, version), totals)| VersionUsage {
                repository,
                project,
                version: non_empty(version),
                downloads: totals.downloads,
                bytes: totals.bytes,
            })
            .collect();
        rows.sort_by(|left, right| {
            right
                .downloads
                .cmp(&left.downloads)
                .then_with(|| right.bytes.cmp(&left.bytes))
                .then_with(|| left.repository.cmp(&right.repository))
                .then_with(|| left.project.cmp(&right.project))
                .then_with(|| left.version.cmp(&right.version))
        });
        rows
    }

    /// Project downloads attributed to each routed source over `interval`, ordered by downloads,
    /// bytes, then identity. The caller must have cleared the source dimension for the requester.
    ///
    /// # Panics
    /// Panics if the aggregator thread panicked and poisoned the daily lock.
    #[must_use]
    pub fn usage_sources(&self, repository: Option<&str>, interval: &UsageInterval) -> Vec<SourceUsage> {
        let mut rows: Vec<_> = self
            .fold_daily(repository, interval, |bucket| {
                (bucket.repository.clone(), bucket.project.clone(), bucket.source.clone())
            })
            .into_iter()
            .map(|((repository, project, source), totals)| SourceUsage {
                repository,
                project,
                source: non_empty(source),
                downloads: totals.downloads,
                bytes: totals.bytes,
            })
            .collect();
        rows.sort_by(|left, right| {
            right
                .downloads
                .cmp(&left.downloads)
                .then_with(|| right.bytes.cmp(&left.bytes))
                .then_with(|| left.repository.cmp(&right.repository))
                .then_with(|| left.project.cmp(&right.project))
                .then_with(|| left.source.cmp(&right.source))
        });
        rows
    }

    /// Durable lifetime download and byte totals per project, keyed by repository.
    ///
    /// Reads the all-time tree, where a repository maps directly to its nested projects, so a scoped
    /// read touches only the named repository. This is the indexed accessor the query layer's
    /// `usage.downloads` domain builds on: the join key `(repository, project)` resolves without a
    /// daily-bucket scan. Ordered downloads-desc, then repository and project ascending, for a stable
    /// page.
    ///
    /// # Panics
    /// Panics if the aggregator thread panicked and poisoned the metrics lock.
    #[must_use]
    pub fn usage_totals(&self, repository: Option<&str>) -> Vec<PackageUsage> {
        let tree = self.tree.read().expect("metrics lock");
        let mut rows = Vec::new();
        for (route, index) in tree.iter() {
            if repository.is_some_and(|filter| route != filter) {
                continue;
            }
            for (project, stats) in &index.projects {
                rows.push(PackageUsage {
                    repository: route.clone(),
                    project: project.clone(),
                    downloads: stats.totals.base.downloads,
                    bytes: stats.totals.base.bytes,
                });
            }
        }
        drop(tree);
        rows.sort_by(|left, right| {
            right
                .downloads
                .cmp(&left.downloads)
                .then_with(|| left.repository.cmp(&right.repository))
                .then_with(|| left.project.cmp(&right.project))
        });
        rows
    }

    /// Projects with durable downloads but none inside `interval`, ordered by lifetime downloads,
    /// repository, then project. The universe is every project the retained totals have ever served.
    ///
    /// # Panics
    /// Panics if the aggregator thread panicked and poisoned either lock.
    #[must_use]
    pub fn usage_unused(&self, repository: Option<&str>, interval: &UsageInterval) -> Vec<UnusedPackage> {
        let active: std::collections::BTreeSet<(String, String)> = self
            .fold_daily(repository, interval, |bucket| {
                (bucket.repository.clone(), bucket.project.clone())
            })
            .into_keys()
            .collect();
        let tree = self.tree.read().expect("metrics lock");
        let mut rows = Vec::new();
        for (route, index) in tree.iter() {
            if repository.is_some_and(|filter| route != filter) {
                continue;
            }
            for (project, stats) in &index.projects {
                if stats.totals.base.downloads > 0 && !active.contains(&(route.clone(), project.clone())) {
                    rows.push(UnusedPackage {
                        repository: route.clone(),
                        project: project.clone(),
                        lifetime_downloads: stats.totals.base.downloads,
                    });
                }
            }
        }
        drop(tree);
        rows.sort_by(|left, right| {
            right
                .lifetime_downloads
                .cmp(&left.lifetime_downloads)
                .then_with(|| left.repository.cmp(&right.repository))
                .then_with(|| left.project.cmp(&right.project))
        });
        rows
    }

    /// Downloads bucketed by UTC day over `interval`, ascending by day so the series reads forward.
    ///
    /// # Panics
    /// Panics if the aggregator thread panicked and poisoned the daily lock.
    #[must_use]
    pub fn usage_timeline(&self, repository: Option<&str>, interval: &UsageInterval) -> Vec<TimelineBucket> {
        self.fold_daily(repository, interval, |bucket| bucket.day)
            .into_iter()
            .map(|(day, totals)| TimelineBucket {
                day,
                start_unix: day * SECONDS_PER_DAY,
                end_unix: (day + 1) * SECONDS_PER_DAY,
                downloads: totals.downloads,
                bytes: totals.bytes,
            })
            .collect()
    }

    /// Fold the daily buckets inside `interval` (and one repository, when scoped) under a key the
    /// caller derives, summing downloads and bytes into each group.
    fn fold_daily<K: Ord>(
        &self,
        repository: Option<&str>,
        interval: &UsageInterval,
        key: impl Fn(&DailyKey) -> K,
    ) -> BTreeMap<K, DailyTotals> {
        let daily = self.daily.read().expect("metrics lock");
        let mut folded: BTreeMap<K, DailyTotals> = BTreeMap::new();
        for (bucket, totals) in daily.iter() {
            if bucket.day < interval.from_day
                || bucket.day > interval.to_day
                || repository.is_some_and(|route| bucket.repository != route)
            {
                continue;
            }
            let group = folded.entry(key(bucket)).or_default();
            group.downloads += totals.downloads;
            group.bytes += totals.bytes;
        }
        drop(daily);
        folded
    }

    /// The tree at the requested depth: everything, one index's projects, or one project's files.
    ///
    /// # Panics
    /// Panics if the aggregator thread panicked and poisoned the tree lock.
    #[must_use]
    pub fn drill(&self, route: Option<&str>, project: Option<&str>) -> serde_json::Value {
        let tree = self.tree.read().expect("metrics lock");
        match (route, project) {
            (Some(route), Some(project)) => tree
                .get(route)
                .and_then(|index| index.projects.get(project))
                .map_or_else(|| serde_json::json!({}), |stats| serde_json::json!(stats)),
            (Some(route), None) => tree.get(route).map_or_else(
                || serde_json::json!({}),
                |index| {
                    serde_json::json!({
                        "totals": index.totals,
                        "projects": index.projects.iter()
                            .map(|(name, stats)| (name.clone(), serde_json::json!(stats.totals)))
                            .collect::<HashMap<_, _>>(),
                    })
                },
            ),
            _ => serde_json::json!(
                tree.iter()
                    .map(|(route, index)| (route.clone(), serde_json::json!(index.totals)))
                    .collect::<HashMap<_, _>>()
            ),
        }
    }
}

/// The shared state one aggregator thread folds request-path observations into, gathered so the loop
/// and its steps pass one context instead of five positional arguments.
struct Aggregator<'a> {
    tree: &'a RwLock<StatsTree>,
    daily: &'a RwLock<DailyBuckets>,
    store: Option<&'a AnalyticsHandle>,
    retention_days: Option<u32>,
    clock: &'a Clock,
}

/// How the aggregator paces durable checkpoints: how long an idle-but-dirty loop waits before it wakes
/// to flush, and the same span in whole clock-seconds for the elapsed-since-checkpoint due check.
#[derive(Clone, Copy)]
struct FlushPolicy {
    idle: Duration,
    interval_secs: i64,
}

impl FlushPolicy {
    fn new(interval: Duration) -> Self {
        Self {
            idle: interval,
            interval_secs: i64::try_from(interval.as_secs()).unwrap_or(i64::MAX),
        }
    }
}

/// The aggregator's checkpoint bookkeeping: whether a durable store is attached, whether a download has
/// landed since the last checkpoint, and the clock second that checkpoint ran.
struct FlushState {
    persistent: bool,
    pending: bool,
    last_flush: i64,
}

impl FlushState {
    const fn new(now: i64, persistent: bool) -> Self {
        Self {
            persistent,
            pending: false,
            last_flush: now,
        }
    }

    /// An ephemeral aggregator has nothing to persist, so it never marks work pending and never wakes on
    /// the idle timer to write.
    const fn mark(&mut self, dirty: bool) {
        self.pending |= self.persistent && dirty;
    }

    const fn pending(&self) -> bool {
        self.pending
    }
}

/// What one loop turn pulled off the channel: a message to fold, an idle timeout that woke a dirty loop
/// to flush, or the closed channel that ends the loop after a final flush.
enum Received {
    Batch(Message),
    Idle,
    Closed,
}

/// The aggregator loop: fold each drained batch into the live tree and daily buckets as it arrives, but
/// checkpoint the two durable snapshots on a coalescing interval rather than once per batch.
///
/// Folding stays logarithmic in stored state per download; the two full-state serializations run at most
/// once every [`FLUSH_INTERVAL`], plus once at orderly shutdown, so steady low-concurrency traffic no
/// longer rewrites both snapshots for every download. Serializing runs under a read lock and the write
/// after releasing it, so a slow disk never stalls the aggregator's readers.
fn aggregate(
    receiver: &Receiver<Message>,
    tree: &Arc<RwLock<StatsTree>>,
    daily: &Arc<RwLock<DailyBuckets>>,
    store: Option<&AnalyticsHandle>,
    retention_days: Option<u32>,
    clock: &Clock,
    interval: Duration,
) {
    let ctx = Aggregator {
        tree,
        daily,
        store,
        retention_days,
        clock,
    };
    let policy = FlushPolicy::new(interval);
    let mut state = FlushState::new((clock)(), store.is_some());
    while step(receiver, &ctx, policy, &mut state) {}
}

/// Advance the aggregator one turn: fold a batch and checkpoint if due, flush an idle-but-dirty loop
/// that hit the interval, or flush a last time and stop once the channel has closed. Returns whether the
/// loop should keep running, so a test drives each turn on its own thread where x86 llvm-cov captures it.
fn step(receiver: &Receiver<Message>, ctx: &Aggregator, policy: FlushPolicy, state: &mut FlushState) -> bool {
    match receive(receiver, state.pending(), policy.idle) {
        Received::Batch(first) => {
            let batch = absorb_batch(first, receiver, ctx);
            state.mark(batch.dirty);
            if flush_due(
                (ctx.clock)(),
                state.last_flush,
                state.pending(),
                batch.force,
                policy.interval_secs,
            ) {
                flush(ctx, state);
            }
            #[cfg(any(test, feature = "test-util"))]
            for ack in batch.acks {
                let _ = ack.send(());
            }
            true
        }
        Received::Idle => {
            if flush_due(
                (ctx.clock)(),
                state.last_flush,
                state.pending(),
                false,
                policy.interval_secs,
            ) {
                flush(ctx, state);
            }
            true
        }
        Received::Closed => {
            if state.pending() {
                flush(ctx, state);
            }
            false
        }
    }
}

/// Block for the next message, or wake after `idle` when a dirty loop must checkpoint even though no new
/// event arrived, so durability tracks a fixed interval rather than the traffic shape. A clean loop with
/// nothing pending blocks without a deadline until an event or the closed channel arrives.
fn receive(receiver: &Receiver<Message>, pending: bool, idle: Duration) -> Received {
    if pending {
        match receiver.recv_timeout(idle) {
            Ok(message) => Received::Batch(message),
            Err(RecvTimeoutError::Timeout) => Received::Idle,
            Err(RecvTimeoutError::Disconnected) => Received::Closed,
        }
    } else {
        receiver.recv().map_or(Received::Closed, Received::Batch)
    }
}

/// Drain the first message plus everything already queued behind it under one tree-lock acquisition,
/// then fold the batch's downloads into the daily buckets. Returns what the batch changed: whether a
/// download made the snapshots dirty, whether a barrier forced a checkpoint, and the barriers to ack.
fn absorb_batch(first: Message, receiver: &Receiver<Message>, ctx: &Aggregator) -> Batch {
    let mut batch = Batch::default();
    {
        let mut tree = ctx.tree.write().expect("metrics lock");
        absorb(first, &mut tree, ctx.clock, &mut batch);
        while let Ok(message) = receiver.try_recv() {
            absorb(message, &mut tree, ctx.clock, &mut batch);
        }
    }
    fold_daily_batch(
        std::mem::take(&mut batch.downloads),
        ctx.daily,
        ctx.retention_days,
        ctx.clock,
    );
    batch
}

/// What draining a batch produced: whether the durable snapshots now differ from disk, whether a settle
/// barrier asked to checkpoint immediately, the downloads to fold into the daily buckets, and the
/// barriers to acknowledge once the batch has persisted.
#[derive(Default)]
struct Batch {
    dirty: bool,
    force: bool,
    downloads: Vec<(DailyKey, u64)>,
    #[cfg(any(test, feature = "test-util"))]
    acks: Vec<Sender<()>>,
}

/// Whether a checkpoint is due: only when a download is pending, and then either a barrier forced it or
/// at least the coalescing interval has elapsed on the clock since the last checkpoint.
const fn flush_due(now: i64, last_flush: i64, pending: bool, force: bool, interval_secs: i64) -> bool {
    pending && (force || now - last_flush >= interval_secs)
}

/// Serialize both durable snapshots and write them, then clear the pending mark and stamp the checkpoint
/// clock. Only a pending checkpoint reaches here, and a pending checkpoint implies an attached store, so
/// the store is present by construction. Serializing runs under a read lock, so the aggregator's writers
/// never wait on the disk.
fn flush(ctx: &Aggregator, state: &mut FlushState) {
    let store = ctx.store.expect("a pending checkpoint without a store");
    let downloads = serde_json::to_vec(&snapshot_downloads(&ctx.tree.read().expect("metrics lock")))
        .expect("serialize metrics snapshot");
    let _ = store.save(&downloads);
    let daily = serde_json::to_vec(&snapshot_daily(&ctx.daily.read().expect("metrics lock")))
        .expect("serialize daily usage snapshot");
    let _ = store.save_daily(&daily);
    state.pending = false;
    state.last_flush = (ctx.clock)();
}

/// Fold one message into the batch: apply an event to the tree and note its daily downloads, or park a
/// barrier's acknowledgement for the aggregator to fire after the batch persists.
fn absorb(message: Message, tree: &mut StatsTree, clock: &Clock, batch: &mut Batch) {
    match message {
        Message::Event(event) => {
            batch.dirty |= matches!(&event, Event::Download { .. });
            collect_daily(&event, clock, &mut batch.downloads);
            apply(tree, event);
        }
        #[cfg(any(test, feature = "test-util"))]
        Message::Barrier(ack) => {
            batch.force = true;
            batch.acks.push(ack);
        }
    }
}

/// Pull one download's daily-bucket key and byte count out of an event, dating it on the clock; every
/// other event kind leaves the daily aggregate untouched.
fn collect_daily(event: &Event, clock: &Clock, out: &mut Vec<(DailyKey, u64)>) {
    if let Event::Download {
        route,
        project,
        version,
        source,
        bytes,
        ..
    } = event
    {
        out.push((
            DailyKey {
                day: utc_day(clock()),
                repository: route.clone(),
                project: project.clone(),
                version: version.clone().unwrap_or_default(),
                source: source.clone().unwrap_or_default(),
            },
            *bytes,
        ));
    }
}

/// Fold a batch's daily downloads into the shared buckets in memory and apply retention. Persistence is
/// deferred to [`flush`], so this runs per batch while the durable write coalesces on the interval.
///
/// Split out of [`absorb_batch`] so the retention flush runs on the caller's thread. The aggregator
/// calls it from a spawned thread whose coverage x86 llvm-cov does not capture reliably, so a direct
/// unit test drives this on the test thread instead. Every statement stays a single unconditional
/// region: no embedded closure or `clock()` sub-expression that x86 llvm-cov splits into a flapping dead
/// arm.
fn fold_daily_batch(
    downloads: Vec<(DailyKey, u64)>,
    daily: &RwLock<DailyBuckets>,
    retention_days: Option<u32>,
    clock: &Clock,
) {
    if downloads.is_empty() {
        return;
    }
    let mut daily = daily.write().expect("metrics lock");
    for (key, bytes) in downloads {
        let totals = daily.entry(key).or_default();
        totals.downloads += 1;
        totals.bytes += bytes;
    }
    if let Some(days) = retention_days {
        let now = clock();
        expire_daily(&mut daily, now, days);
    }
    drop(daily);
}

/// Drop every bucket older than `retention_days` days. Buckets order by day first, so the expired
/// prefix leaves in one split and the retained totals are never touched.
fn expire_daily(daily: &mut DailyBuckets, now_secs: i64, retention_days: u32) {
    let floor = DailyKey {
        day: utc_day(now_secs) - i64::from(retention_days),
        repository: String::new(),
        project: String::new(),
        version: String::new(),
        source: String::new(),
    };
    *daily = daily.split_off(&floor);
}

fn daily_rows(daily: &DailyBuckets) -> Vec<DailyUsage> {
    daily
        .iter()
        .map(|(key, totals)| DailyUsage {
            day: key.day,
            repository: key.repository.clone(),
            project: key.project.clone(),
            version: key.version.clone(),
            source: key.source.clone(),
            downloads: totals.downloads,
            bytes: totals.bytes,
        })
        .collect()
}

fn snapshot_daily(daily: &DailyBuckets) -> DailySnapshot {
    DailySnapshot {
        schema: DAILY_SCHEMA,
        buckets: daily_rows(daily),
    }
}

/// Fold a restored daily snapshot back into fresh buckets, summing any rows that share a key.
fn restore_daily(daily: &mut DailyBuckets, snapshot: DailySnapshot) {
    for row in snapshot.buckets {
        let totals = daily
            .entry(DailyKey {
                day: row.day,
                repository: row.repository,
                project: row.project,
                version: row.version,
                source: row.source,
            })
            .or_default();
        totals.downloads += row.downloads;
        totals.bytes += row.bytes;
    }
}

/// Flatten the tree's per-file download counters into a persistable snapshot.
fn snapshot_downloads(tree: &StatsTree) -> DownloadSnapshot {
    let files = tree
        .iter()
        .flat_map(|(route, index)| {
            index.projects.iter().flat_map(move |(project, stats)| {
                stats.files.iter().map(move |(filename, file)| FileDownloadRow {
                    route: route.clone(),
                    project: project.clone(),
                    filename: filename.clone(),
                    downloads: file.downloads,
                    bytes: file.bytes,
                })
            })
        })
        .collect();
    DownloadSnapshot { files }
}

/// Fold a restored snapshot back into a fresh tree, rebuilding every download and byte total.
fn restore_downloads(tree: &mut StatsTree, snapshot: DownloadSnapshot) {
    for row in snapshot.files {
        let index = tree.entry(row.route).or_default();
        index.totals.base.downloads += row.downloads;
        index.totals.base.bytes += row.bytes;
        let project = index.projects.entry(row.project).or_default();
        project.totals.base.downloads += row.downloads;
        project.totals.base.bytes += row.bytes;
        let file = project.files.entry(row.filename).or_default();
        file.downloads += row.downloads;
        file.bytes += row.bytes;
    }
}

fn apply(tree: &mut StatsTree, event: Event) {
    match event {
        Event::Page { route, project } => {
            let index = tree.entry(route).or_default();
            index.totals.base.pages += 1;
            index.projects.entry(project).or_default().totals.base.pages += 1;
        }
        Event::Download {
            route,
            project,
            filename,
            bytes,
            ..
        } => {
            let index = tree.entry(route).or_default();
            index.totals.base.downloads += 1;
            index.totals.base.bytes += bytes;
            let project = index.projects.entry(project).or_default();
            project.totals.base.downloads += 1;
            project.totals.base.bytes += bytes;
            let file = project.files.entry(filename).or_default();
            file.downloads += 1;
            file.bytes += bytes;
        }
        Event::Ecosystem {
            route,
            project,
            filename,
            family,
        } => {
            let index = tree.entry(route).or_default();
            *index.totals.ecosystem.entry(family).or_default() += 1;
            let project = index.projects.entry(project).or_default();
            *project.totals.ecosystem.entry(family).or_default() += 1;
            if let Some(filename) = filename {
                *project
                    .files
                    .entry(filename)
                    .or_default()
                    .ecosystem
                    .entry(family)
                    .or_default() += 1;
            }
        }
        Event::Upload { route, project } => {
            let index = tree.entry(route).or_default();
            index.totals.hosted.uploads += 1;
            index.projects.entry(project).or_default().totals.hosted.uploads += 1;
        }
        Event::Refresh {
            route,
            project,
            changed,
        } => {
            let index = tree.entry(route).or_default();
            index.totals.cached.refreshes += 1;
            let project = index.projects.entry(project).or_default();
            project.totals.cached.refreshes += 1;
            if changed {
                index.totals.cached.changed += 1;
                project.totals.cached.changed += 1;
            }
        }
        Event::StaleServed { route, project } => {
            let index = tree.entry(route).or_default();
            index.totals.cached.stale_served += 1;
            index.projects.entry(project).or_default().totals.cached.stale_served += 1;
        }
        Event::UpstreamError { route, project } => {
            let index = tree.entry(route).or_default();
            index.totals.cached.upstream_errors += 1;
            index.projects.entry(project).or_default().totals.cached.upstream_errors += 1;
        }
        Event::BlobRejected { route, project } => {
            let index = tree.entry(route).or_default();
            index.totals.base.rejected += 1;
            index.projects.entry(project).or_default().totals.base.rejected += 1;
        }
        Event::CatalogSync {
            route,
            outcome,
            projects,
        } => {
            let cached = &mut tree.entry(route).or_default().totals.cached;
            cached.catalog_syncs += 1;
            match outcome {
                CatalogSyncOutcome::Published => cached.catalog_published += 1,
                CatalogSyncOutcome::NotModified => cached.catalog_not_modified += 1,
                CatalogSyncOutcome::Error => cached.catalog_errors += 1,
            }
            if let Some(projects) = projects {
                cached.catalog_projects = projects;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use std::sync::mpsc::channel;
    use std::sync::{Arc, RwLock};
    use std::time::Duration;

    use peryx_ha_distributed::{
        AggregateDelta, AggregateKey, ApplyLimits, ApplyOutcome, ApplyState, AuthorityEpoch, IntervalId, ProducerId,
    };
    use peryx_storage::meta::{AnalyticsHandle, MetaStore};

    use super::{
        Aggregator, Clock, DailyBuckets, DailyKey, DailySnapshot, DailyTotals, DailyUsage, DownloadSnapshot, Event,
        FLUSH_INTERVAL, FlushPolicy, FlushState, Message, Metrics, PackageUsage, SECONDS_PER_DAY, SourceUsage,
        StatsTree, TimelineBucket, UnusedPackage, UsageInterval, VersionUsage, aggregate, daily_rows,
        encode_daily_snapshot, flush_due, fold_daily_batch, step,
    };

    fn store() -> (tempfile::TempDir, MetaStore) {
        let dir = tempfile::tempdir().unwrap();
        let meta = MetaStore::open(dir.path().join("peryx.redb")).unwrap();
        (dir, meta)
    }

    /// A clock frozen at `day`'s noon, so a test dates every download to one deterministic UTC bucket.
    fn clock_on_day(day: i64) -> Clock {
        Arc::new(move || day * SECONDS_PER_DAY + SECONDS_PER_DAY / 2)
    }

    /// A clock the test advances between settled writes, so one running aggregator can date buckets on
    /// different days and cross its own retention window without a restart.
    fn steppable_clock() -> (std::sync::Arc<std::sync::atomic::AtomicI64>, Clock) {
        let day = std::sync::Arc::new(std::sync::atomic::AtomicI64::new(0));
        let handle = std::sync::Arc::clone(&day);
        let clock: Clock =
            Arc::new(move || handle.load(std::sync::atomic::Ordering::SeqCst) * SECONDS_PER_DAY + SECONDS_PER_DAY / 2);
        (day, clock)
    }

    fn settle_and_assert(metrics: &Metrics, done: impl Fn() -> bool) {
        // Drain the aggregator through its barrier, then assert the state it settled on.
        metrics.settle();
        assert!(done(), "metrics settled on an unexpected state");
    }

    fn persisted_downloads(store: &AnalyticsHandle) -> Option<u64> {
        let bytes = store.load().unwrap()?;
        let snapshot: DownloadSnapshot = serde_json::from_slice(&bytes).unwrap();
        Some(snapshot.files.iter().map(|file| file.downloads).sum())
    }

    fn download(route: &str, project: &str, filename: &str, bytes: u64) -> Event {
        Event::Download {
            route: route.into(),
            project: project.into(),
            filename: filename.into(),
            version: None,
            source: None,
            bytes,
        }
    }

    fn download_of(route: &str, project: &str, version: &str, source: Option<&str>, bytes: u64) -> Event {
        Event::Download {
            route: route.into(),
            project: project.into(),
            filename: format!("{project}-{version}.bin"),
            version: Some(version.into()),
            source: source.map(Into::into),
            bytes,
        }
    }

    #[test]
    fn test_record_bounds_the_queue_and_counts_drops_under_overload() {
        // A stalled aggregator must cap retained work at the queue capacity rather than absorb overload
        // into unbounded memory. Hold the write lock the aggregator needs to apply an event, which parks
        // it after it has pulled at most one event off the channel, then flood the recorder well past
        // capacity: the buffer saturates, every further record is dropped, and the drops are counted.
        let capacity = 8;
        let metrics = Metrics::spawn(None, None, clock_on_day(0), capacity);
        let sends = (capacity * 4) as u64;
        let processed = {
            let _parked = metrics.tree.write().unwrap();
            for _ in 0..sends {
                metrics.record(download("alpha", "numpy", "numpy-1.bin", 1));
            }
            assert!(metrics.dropped() > 0, "overload was not reported");
            sends - metrics.dropped()
        };
        assert!(
            processed <= capacity as u64 + 1,
            "retained {processed} events, past the {capacity}-slot bound",
        );
        metrics.settle();
        let aggregated: u64 = metrics
            .index_totals()
            .values()
            .map(|counters| counters.base.downloads)
            .sum();
        assert_eq!(
            aggregated, processed,
            "every event that was not dropped must be aggregated"
        );
    }

    #[test]
    fn test_encode_daily_snapshot_seeds_a_producer_offline() {
        let (_dir, meta) = store();
        let seeded = DailyUsage {
            day: 19_000,
            repository: "hosted".to_owned(),
            project: "veloxdemo".to_owned(),
            version: "1.0.0".to_owned(),
            source: String::new(),
            downloads: 7,
            bytes: 4096,
        };
        meta.analytics()
            .save_daily(&encode_daily_snapshot(vec![seeded.clone()]))
            .unwrap();
        // A durable aggregator restores the seeded bucket on boot through the same path a live download
        // would have written, so an offline seed becomes an exportable sealed day.
        let metrics = Metrics::start_durable(meta.analytics(), None, clock_on_day(seeded.day + 1));
        assert_eq!(metrics.daily_usage(), [seeded]);
    }

    #[test]
    fn test_durable_downloads_survive_a_restart() {
        let (_dir, meta) = store();
        let filename = "pandas-3.0-py3-none-any.bin";
        let metrics = Metrics::start_durable(meta.analytics(), None, clock_on_day(0));
        metrics.record(Event::Page {
            route: "root/alpha".into(),
            project: "pandas".into(),
        });
        metrics.record(download("root/alpha", "pandas", filename, 100));
        metrics.record(download("root/alpha", "pandas", filename, 50));
        settle_and_assert(&metrics, || persisted_downloads(&meta.analytics()) == Some(2));
        drop(metrics);

        let restarted = Metrics::start_durable(meta.analytics(), None, clock_on_day(0));
        let totals = restarted.index_totals();
        let index = &totals["root/alpha"];
        assert_eq!(index.base.downloads, 2);
        assert_eq!(index.base.bytes, 150);
        let files = restarted.drill(Some("root/alpha"), Some("pandas"));
        assert_eq!(files["files"][filename]["downloads"], 2);
        assert_eq!(files["files"][filename]["bytes"], 150);
    }

    #[test]
    fn test_usage_totals_reports_lifetime_by_repository() {
        let (_dir, meta) = store();
        let metrics = Metrics::start_durable(meta.analytics(), None, clock_on_day(0));
        metrics.record(download("alpha", "numpy", "numpy-1.bin", 100));
        metrics.record(download("alpha", "numpy", "numpy-1.bin", 100));
        metrics.record(download("alpha", "scipy", "scipy-1.bin", 50));
        metrics.record(download("alpha", "flask", "flask-1.bin", 40));
        metrics.record(download("other", "django", "django-1.bin", 30));
        settle_and_assert(&metrics, || metrics.usage_totals(None).len() == 4);

        // flask and scipy tie on downloads within the same repository, so ordering falls through to the
        // project tiebreak; django ties on downloads but under a different repository, exercising the
        // repository tiebreak.
        assert_eq!(
            metrics.usage_totals(None),
            [
                PackageUsage {
                    repository: "alpha".into(),
                    project: "numpy".into(),
                    downloads: 2,
                    bytes: 200,
                },
                PackageUsage {
                    repository: "alpha".into(),
                    project: "flask".into(),
                    downloads: 1,
                    bytes: 40,
                },
                PackageUsage {
                    repository: "alpha".into(),
                    project: "scipy".into(),
                    downloads: 1,
                    bytes: 50,
                },
                PackageUsage {
                    repository: "other".into(),
                    project: "django".into(),
                    downloads: 1,
                    bytes: 30,
                },
            ]
        );
        assert_eq!(
            metrics.usage_totals(Some("other")),
            [PackageUsage {
                repository: "other".into(),
                project: "django".into(),
                downloads: 1,
                bytes: 30,
            }]
        );
        assert!(metrics.usage_totals(Some("missing")).is_empty());
    }

    #[test]
    fn test_batches_without_a_download_persist_nothing() {
        let (_dir, meta) = store();
        let metrics = Metrics::start_durable(meta.analytics(), None, clock_on_day(0));
        metrics.record(Event::Page {
            route: "alpha".into(),
            project: "flask".into(),
        });
        settle_and_assert(&metrics, || {
            metrics
                .index_totals()
                .get("alpha")
                .is_some_and(|totals| totals.base.pages == 1)
        });
        assert_eq!(persisted_downloads(&meta.analytics()), None);
        assert!(meta.analytics().load_daily().unwrap().is_none());
    }

    #[test]
    fn test_daily_buckets_split_by_version_source_and_day() {
        let (_dir, meta) = store();
        let metrics = Metrics::start_durable(meta.analytics(), None, clock_on_day(20_000));
        metrics.record(download_of("alpha", "flask", "3.0", Some("upstream"), 10));
        metrics.record(download_of("alpha", "flask", "3.0", Some("upstream"), 40));
        metrics.record(download_of("alpha", "flask", "2.0", Some("upstream"), 5));
        metrics.record(download_of("alpha", "flask", "3.0", None, 7));
        settle_and_assert(&metrics, || metrics.daily_usage().len() == 3);

        assert_eq!(
            metrics.daily_usage(),
            [
                DailyUsage {
                    day: 20_000,
                    repository: "alpha".into(),
                    project: "flask".into(),
                    version: "2.0".into(),
                    source: "upstream".into(),
                    downloads: 1,
                    bytes: 5,
                },
                DailyUsage {
                    day: 20_000,
                    repository: "alpha".into(),
                    project: "flask".into(),
                    version: "3.0".into(),
                    source: String::new(),
                    downloads: 1,
                    bytes: 7,
                },
                DailyUsage {
                    day: 20_000,
                    repository: "alpha".into(),
                    project: "flask".into(),
                    version: "3.0".into(),
                    source: "upstream".into(),
                    downloads: 2,
                    bytes: 50,
                },
            ]
        );
    }

    #[test]
    fn test_retention_drops_expired_days_and_keeps_retained_totals() {
        let (_dir, meta) = store();
        let old = Metrics::start_durable(meta.analytics(), Some(7), clock_on_day(100));
        old.record(download_of("alpha", "flask", "1.0", Some("up"), 3));
        settle_and_assert(&old, || old.daily_usage().len() == 1);
        drop(old);

        // Ten days later a fresh download lands; the day-100 bucket is now beyond the 7-day window.
        let metrics = Metrics::start_durable(meta.analytics(), Some(7), clock_on_day(110));
        metrics.record(download_of("alpha", "flask", "2.0", Some("up"), 9));
        settle_and_assert(&metrics, || metrics.daily_usage().iter().any(|row| row.day == 110));

        assert_eq!(
            metrics.daily_usage(),
            [DailyUsage {
                day: 110,
                repository: "alpha".into(),
                project: "flask".into(),
                version: "2.0".into(),
                source: "up".into(),
                downloads: 1,
                bytes: 9,
            }]
        );
    }

    #[test]
    fn test_the_running_aggregator_expires_a_bucket_that_ages_past_retention() {
        use std::sync::atomic::Ordering;

        let (_dir, meta) = store();
        let (day, clock) = steppable_clock();
        let metrics = Metrics::start_durable(meta.analytics(), Some(2), clock);

        metrics.record(download_of("alpha", "flask", "1.0", Some("up"), 3));
        settle_and_assert(&metrics, || metrics.daily_usage().iter().any(|row| row.day == 0));

        // Advance five days so the day-0 bucket falls outside the two-day window, then record again:
        // the running aggregator applies retention to the live map after startup.
        day.store(5, Ordering::SeqCst);
        metrics.record(download_of("alpha", "flask", "2.0", Some("up"), 9));
        settle_and_assert(&metrics, || metrics.daily_usage().iter().any(|row| row.day == 5));

        let days: Vec<i64> = metrics.daily_usage().iter().map(|row| row.day).collect();
        assert_eq!(
            days,
            [5],
            "the aged day-0 bucket expired during aggregation, leaving only day 5"
        );
    }

    #[test]
    fn test_aggregate_flushes_retention_within_the_batch() {
        // The running aggregator applies retention inside the download batch on its own thread, so the
        // settle-based tests reach that flush only when the batch wins the race with the coverage
        // snapshot. Drive the loop directly over a channel that closes after one batch: the retention
        // flush then runs on every architecture and run.
        let clock = clock_on_day(30);
        let stale = DailyKey {
            day: 10,
            repository: "alpha".into(),
            project: "flask".into(),
            version: "1.0".into(),
            source: "up".into(),
        };
        let mut seeded = DailyBuckets::new();
        seeded.insert(
            stale,
            DailyTotals {
                downloads: 5,
                bytes: 50,
            },
        );
        let daily = Arc::new(RwLock::new(seeded));
        let tree = Arc::new(RwLock::new(StatsTree::new()));

        let (sender, receiver) = channel();
        sender
            .send(Message::Event(download_of("alpha", "flask", "2.0", Some("up"), 9)))
            .unwrap();
        drop(sender);

        aggregate(&receiver, &tree, &daily, None, Some(7), &clock, FLUSH_INTERVAL);

        let rows = daily_rows(&daily.read().unwrap());
        assert_eq!(
            rows.len(),
            1,
            "the day-10 bucket outside the 7-day window survived: {rows:?}"
        );
        assert_eq!(rows[0].day, 30);
        assert_eq!(rows[0].downloads, 1);
        assert_eq!(rows[0].bytes, 9);
    }

    fn bucket(day: i64, version: &str) -> DailyKey {
        DailyKey {
            day,
            repository: "alpha".into(),
            project: "flask".into(),
            version: version.into(),
            source: "up".into(),
        }
    }

    fn seeded_daily(day: i64, version: &str, downloads: u64, bytes: u64) -> RwLock<DailyBuckets> {
        let mut seeded = DailyBuckets::new();
        seeded.insert(bucket(day, version), DailyTotals { downloads, bytes });
        RwLock::new(seeded)
    }

    #[test]
    fn test_fold_daily_batch_expires_aged_buckets() {
        // Drive the retention flush on the test thread, where x86 coverage captures it, instead of only
        // on the aggregator's spawned thread.
        let daily = seeded_daily(10, "1.0", 5, 50);

        fold_daily_batch(vec![(bucket(30, "2.0"), 9)], &daily, Some(7), &clock_on_day(30));

        let rows = daily_rows(&daily.read().unwrap());
        assert_eq!(rows.len(), 1, "the aged day-10 bucket survived retention: {rows:?}");
        assert_eq!((rows[0].day, rows[0].downloads, rows[0].bytes), (30, 1, 9));
    }

    #[test]
    fn test_fold_daily_batch_without_retention_keeps_every_bucket() {
        let daily = seeded_daily(10, "1.0", 5, 50);

        fold_daily_batch(vec![(bucket(30, "2.0"), 9)], &daily, None, &clock_on_day(30));

        // No retention window: both buckets survive untouched.
        assert_eq!(daily_rows(&daily.read().unwrap()).len(), 2);
    }

    #[test]
    fn test_fold_daily_batch_ignores_an_empty_batch() {
        let daily = RwLock::new(DailyBuckets::new());
        fold_daily_batch(Vec::new(), &daily, Some(7), &clock_on_day(30));
        assert!(daily_rows(&daily.read().unwrap()).is_empty());
    }

    #[rstest::rstest]
    #[case::clean_never_checkpoints(false, false, 0, false)]
    #[case::pending_within_interval_waits(true, false, 4, false)]
    #[case::pending_reaching_interval_flushes(true, false, 5, true)]
    #[case::pending_forced_flushes_early(true, true, 0, true)]
    fn test_flush_due(#[case] pending: bool, #[case] force: bool, #[case] elapsed: i64, #[case] expected: bool) {
        assert_eq!(flush_due(100 + elapsed, 100, pending, force, 5), expected);
    }

    /// A clock the test moves in whole seconds, so a checkpoint's elapsed-interval gate can be crossed
    /// without waiting on the wall clock.
    fn seconds_clock() -> (Arc<std::sync::atomic::AtomicI64>, Clock) {
        let secs = Arc::new(std::sync::atomic::AtomicI64::new(0));
        let handle = Arc::clone(&secs);
        let clock: Clock = Arc::new(move || handle.load(std::sync::atomic::Ordering::SeqCst));
        (secs, clock)
    }

    fn context<'a>(
        tree: &'a RwLock<StatsTree>,
        daily: &'a RwLock<DailyBuckets>,
        store: &'a AnalyticsHandle,
        clock: &'a Clock,
    ) -> Aggregator<'a> {
        Aggregator {
            tree,
            daily,
            store: Some(store),
            retention_days: None,
            clock,
        }
    }

    #[test]
    fn test_step_coalesces_isolated_downloads_until_forced() {
        // Each download arrives as its own batch under a frozen clock, so the coalescing interval never
        // elapses: the aggregator folds every one into the live buckets but writes nothing durable until
        // a barrier forces the checkpoint. This is the write-frequency claim the issue asks for, decoupled
        // from download count.
        let (_dir, meta) = store();
        let handle = meta.analytics();
        let (_secs, clock) = seconds_clock();
        let tree = RwLock::new(StatsTree::new());
        let daily = RwLock::new(DailyBuckets::new());
        let ctx = context(&tree, &daily, &handle, &clock);
        let policy = FlushPolicy {
            idle: Duration::from_millis(5),
            interval_secs: 5,
        };
        let mut state = FlushState::new((clock)(), true);

        let (sender, receiver) = channel();
        sender
            .send(Message::Event(download_of("alpha", "flask", "1.0", None, 10)))
            .unwrap();
        assert!(step(&receiver, &ctx, policy, &mut state));
        sender
            .send(Message::Event(download_of("alpha", "flask", "1.0", None, 20)))
            .unwrap();
        assert!(step(&receiver, &ctx, policy, &mut state));

        // Two folded downloads, nothing on disk: the interval swallowed both writes.
        assert_eq!(daily_rows(&daily.read().unwrap())[0].downloads, 2);
        assert!(
            handle.load_daily().unwrap().is_none(),
            "an isolated download wrote through"
        );
        assert!(handle.load().unwrap().is_none());
        assert!(state.pending());

        let (ack, done) = channel();
        sender.send(Message::Barrier(ack)).unwrap();
        assert!(step(&receiver, &ctx, policy, &mut state));
        done.recv().unwrap();

        // The forced checkpoint persists the coalesced totals once and clears the pending mark.
        assert_eq!(persisted_downloads(&handle), Some(2));
        assert_eq!(persisted_daily_downloads(&handle), Some(2));
        assert!(!state.pending());
    }

    fn persisted_daily_downloads(store: &AnalyticsHandle) -> Option<u64> {
        let bytes = store.load_daily().unwrap()?;
        let snapshot: DailySnapshot = serde_json::from_slice(&bytes).unwrap();
        Some(snapshot.buckets.iter().map(|bucket| bucket.downloads).sum())
    }

    #[test]
    fn test_step_flushes_a_dirty_loop_when_the_interval_elapses_while_idle() {
        let (_dir, meta) = store();
        let handle = meta.analytics();
        let (secs, clock) = seconds_clock();
        let tree = RwLock::new(StatsTree::new());
        let daily = RwLock::new(DailyBuckets::new());
        let ctx = context(&tree, &daily, &handle, &clock);
        let policy = FlushPolicy {
            idle: Duration::from_millis(5),
            interval_secs: 5,
        };
        let mut state = FlushState::new((clock)(), true);

        let (sender, receiver) = channel();
        sender
            .send(Message::Event(download_of("alpha", "flask", "1.0", None, 8)))
            .unwrap();
        assert!(step(&receiver, &ctx, policy, &mut state));
        assert!(handle.load_daily().unwrap().is_none());

        // No further events: the idle timer wakes the loop, and by now the interval has elapsed, so the
        // pending download checkpoints without any traffic to trigger it.
        secs.store(5, std::sync::atomic::Ordering::SeqCst);
        assert!(step(&receiver, &ctx, policy, &mut state));

        assert_eq!(persisted_downloads(&handle), Some(1));
        assert!(!state.pending());
        drop(sender);
    }

    #[test]
    fn test_step_flushes_a_pending_loop_on_shutdown() {
        let (_dir, meta) = store();
        let handle = meta.analytics();
        let (_secs, clock) = seconds_clock();
        let tree = RwLock::new(StatsTree::new());
        let daily = RwLock::new(DailyBuckets::new());
        let ctx = context(&tree, &daily, &handle, &clock);
        let policy = FlushPolicy {
            idle: Duration::from_millis(5),
            interval_secs: 5,
        };
        let mut state = FlushState::new((clock)(), true);

        let (sender, receiver) = channel();
        sender
            .send(Message::Event(download_of("alpha", "flask", "1.0", None, 4)))
            .unwrap();
        assert!(step(&receiver, &ctx, policy, &mut state));
        assert!(handle.load_daily().unwrap().is_none());

        // The senders drop; the closing channel drives one last checkpoint so the coalesced tail is
        // never lost to an orderly shutdown.
        drop(sender);
        assert!(!step(&receiver, &ctx, policy, &mut state));

        assert_eq!(persisted_downloads(&handle), Some(1));
        assert!(!state.pending());
    }

    #[test]
    fn test_daily_usage_survives_a_restart() {
        let (_dir, meta) = store();
        let metrics = Metrics::start_durable(meta.analytics(), None, clock_on_day(42));
        metrics.record(download_of("alpha", "flask", "3.0", Some("up"), 12));
        settle_and_assert(&metrics, || meta.analytics().load_daily().unwrap().is_some());
        drop(metrics);

        let restarted = Metrics::start_durable(meta.analytics(), None, clock_on_day(42));
        assert_eq!(
            restarted.daily_usage(),
            [DailyUsage {
                day: 42,
                repository: "alpha".into(),
                project: "flask".into(),
                version: "3.0".into(),
                source: "up".into(),
                downloads: 1,
                bytes: 12,
            }]
        );
    }

    #[test]
    fn test_exported_daily_batch_applies_once_on_a_replica() {
        let (_dir, meta) = store();
        let metrics = Metrics::start_durable(meta.analytics(), None, clock_on_day(20_000));
        metrics.record(download_of("alpha", "flask", "3.0", Some("upstream"), 40));
        metrics.record(download_of("alpha", "flask", "3.0", Some("upstream"), 10));
        settle_and_assert(&metrics, || metrics.daily_usage().len() == 1);

        let interval = IntervalId {
            producer: ProducerId("east".into()),
            epoch: AuthorityEpoch(1),
            sequence: 1,
        };
        let export = metrics.export_daily_batch(interval);
        assert_eq!(export.rows.len(), 1);

        let dimension = AggregateKey {
            day: 20_000,
            repository: "alpha".into(),
            project: "flask".into(),
            version: "3.0".into(),
            source: "upstream".into(),
        };
        let mut replica = ApplyState::new(ApplyLimits::default());
        assert_eq!(replica.apply(&export).unwrap(), ApplyOutcome::Applied);
        assert_eq!(
            replica.total(&dimension),
            AggregateDelta {
                downloads: 2,
                bytes: 50
            }
        );

        assert_eq!(replica.apply(&export).unwrap(), ApplyOutcome::Duplicate);
        assert_eq!(
            replica.total(&dimension),
            AggregateDelta {
                downloads: 2,
                bytes: 50
            }
        );
    }

    #[test]
    fn test_export_sealed_day_batches_emits_one_batch_per_completed_day() {
        use std::sync::atomic::Ordering::SeqCst;

        let (_dir, meta) = store();
        let (day, clock) = steppable_clock();
        let metrics = Metrics::start_durable(meta.analytics(), None, clock);

        day.store(10, SeqCst);
        metrics.record(download_of("alpha", "flask", "1.0", Some("up"), 100));
        settle_and_assert(&metrics, || metrics.daily_usage().iter().any(|usage| usage.day == 10));
        day.store(11, SeqCst);
        metrics.record(download_of("alpha", "flask", "1.0", Some("up"), 200));
        settle_and_assert(&metrics, || metrics.daily_usage().iter().any(|usage| usage.day == 11));
        // The current day has activity too, but it is not yet sealed and must be withheld.
        day.store(12, SeqCst);
        metrics.record(download_of("alpha", "flask", "1.0", Some("up"), 5));
        settle_and_assert(&metrics, || metrics.daily_usage().iter().any(|usage| usage.day == 12));

        let producer = ProducerId("east".to_owned());
        let batches = metrics.export_sealed_day_batches(&producer, AuthorityEpoch(1), -1);

        assert_eq!(
            batches.iter().map(|b| b.interval.sequence).collect::<Vec<_>>(),
            vec![10, 11]
        );
        assert_eq!(batches[0].interval.producer, producer);
        assert_eq!(batches[0].interval.epoch, AuthorityEpoch(1));
        assert_eq!(batches[0].rows[0].delta.downloads, 1);
        assert_eq!(batches[0].rows[0].delta.bytes, 100);

        // A watermark past day 10 withholds it and yields only day 11.
        let after = metrics.export_sealed_day_batches(&producer, AuthorityEpoch(1), 10);
        assert_eq!(after.iter().map(|b| b.interval.sequence).collect::<Vec<_>>(), vec![11]);
    }

    #[test]
    fn test_malformed_daily_snapshot_rebuilds_without_blocking_startup() {
        let (_dir, meta) = store();
        meta.analytics().save_daily(b"{ not valid json").unwrap();
        let metrics = Metrics::start_durable(meta.analytics(), None, clock_on_day(7));
        assert!(metrics.daily_usage().is_empty());

        metrics.record(download_of("alpha", "flask", "3.0", Some("up"), 4));
        settle_and_assert(&metrics, || metrics.daily_usage().len() == 1);
        assert_eq!(metrics.daily_usage()[0].bytes, 4);
    }

    #[test]
    fn test_unknown_daily_schema_rebuilds_from_zero() {
        let (_dir, meta) = store();
        let future = DailySnapshot {
            schema: super::DAILY_SCHEMA + 1,
            buckets: vec![DailyUsage {
                day: 1,
                repository: "alpha".into(),
                project: "flask".into(),
                version: "9.9".into(),
                source: "up".into(),
                downloads: 99,
                bytes: 99,
            }],
        };
        meta.analytics()
            .save_daily(&serde_json::to_vec(&future).unwrap())
            .unwrap();
        let metrics = Metrics::start_durable(meta.analytics(), None, clock_on_day(7));
        assert!(metrics.daily_usage().is_empty());
    }

    #[test]
    fn test_missing_dimensions_restore_as_empty_labels() {
        let (_dir, meta) = store();
        let metrics = Metrics::start_durable(meta.analytics(), None, clock_on_day(3));
        metrics.record(download("alpha", "flask", "flask-3.0.bin", 8));
        settle_and_assert(&metrics, || meta.analytics().load_daily().unwrap().is_some());
        drop(metrics);

        let restarted = Metrics::start_durable(meta.analytics(), None, clock_on_day(3));
        assert_eq!(
            restarted.daily_usage(),
            [DailyUsage {
                day: 3,
                repository: "alpha".into(),
                project: "flask".into(),
                version: String::new(),
                source: String::new(),
                downloads: 1,
                bytes: 8,
            }]
        );
    }

    #[test]
    fn test_totals_for_routes_preserves_order_without_returning_keys() {
        let metrics = Metrics::start();
        metrics.record(Event::Page {
            route: "credential-bearing-route".into(),
            project: "actor-token".into(),
        });
        settle_and_assert(&metrics, || {
            metrics.index_totals().contains_key("credential-bearing-route")
        });

        let totals = metrics.totals_for_routes(["missing", "credential-bearing-route"]);

        assert_eq!(totals.len(), 2);
        assert_eq!(totals[0].base.pages, 0);
        assert_eq!(totals[1].base.pages, 1);
    }

    fn durable_on(day: i64, retention: Option<u32>) -> (tempfile::TempDir, MetaStore, Metrics) {
        let (dir, meta) = store();
        let metrics = Metrics::start_durable(meta.analytics(), retention, clock_on_day(day));
        (dir, meta, metrics)
    }

    #[test]
    fn test_current_day_reads_the_query_clock() {
        let (_dir, _meta, metrics) = durable_on(1_000, None);
        assert_eq!(metrics.current_day(), 1_000);
    }

    #[test]
    fn test_resolve_usage_interval_defaults_to_the_trailing_month() {
        let (_dir, _meta, metrics) = durable_on(1_000, None);
        assert_eq!(
            metrics.resolve_usage_interval(None, None),
            UsageInterval {
                from_day: 971,
                to_day: 1_000,
                retained_from_day: None,
                window_clamped_to_retention: false,
            }
        );
    }

    #[test]
    fn test_resolve_usage_interval_honors_explicit_bounds_and_never_runs_past_today() {
        let (_dir, _meta, metrics) = durable_on(1_000, None);
        let interval = metrics.resolve_usage_interval(Some(950 * SECONDS_PER_DAY), Some(3_000 * SECONDS_PER_DAY));
        assert_eq!(interval.from_day, 950);
        assert_eq!(interval.to_day, 1_000);
    }

    #[test]
    fn test_resolve_usage_interval_caps_span_without_retention() {
        let (_dir, _meta, metrics) = durable_on(1_000, None);
        assert_eq!(
            metrics.resolve_usage_interval(Some(0), None),
            UsageInterval {
                from_day: 635,
                to_day: 1_000,
                retained_from_day: None,
                window_clamped_to_retention: false,
            }
        );
    }

    #[test]
    fn test_resolve_usage_interval_clamps_to_retention_floor() {
        let (_dir, _meta, metrics) = durable_on(1_000, Some(7));
        assert_eq!(
            metrics.resolve_usage_interval(Some(0), None),
            UsageInterval {
                from_day: 993,
                to_day: 1_000,
                retained_from_day: Some(993),
                window_clamped_to_retention: true,
            }
        );
    }

    #[test]
    fn test_usage_top_ranks_window_downloads_and_scopes_by_repository() {
        let (_dir, _meta, metrics) = durable_on(500, None);
        metrics.record(download_of("a", "flask", "1.0", None, 10));
        metrics.record(download_of("a", "flask", "2.0", None, 30));
        metrics.record(download_of("a", "django", "5.0", None, 5));
        metrics.record(download_of("b", "numpy", "1.0", None, 99));
        settle_and_assert(&metrics, || metrics.daily_usage().len() == 4);
        let interval = metrics.resolve_usage_interval(None, None);

        assert_eq!(
            metrics.usage_top(None, &interval),
            [
                PackageUsage {
                    repository: "a".into(),
                    project: "flask".into(),
                    downloads: 2,
                    bytes: 40,
                },
                PackageUsage {
                    repository: "b".into(),
                    project: "numpy".into(),
                    downloads: 1,
                    bytes: 99,
                },
                PackageUsage {
                    repository: "a".into(),
                    project: "django".into(),
                    downloads: 1,
                    bytes: 5,
                },
            ]
        );
        assert_eq!(
            metrics.usage_top(Some("a"), &interval),
            [
                PackageUsage {
                    repository: "a".into(),
                    project: "flask".into(),
                    downloads: 2,
                    bytes: 40,
                },
                PackageUsage {
                    repository: "a".into(),
                    project: "django".into(),
                    downloads: 1,
                    bytes: 5,
                },
            ]
        );
    }

    #[test]
    fn test_usage_top_is_empty_when_the_window_predates_every_bucket() {
        let (_dir, _meta, metrics) = durable_on(500, None);
        metrics.record(download_of("a", "flask", "1.0", None, 10));
        settle_and_assert(&metrics, || metrics.daily_usage().len() == 1);
        let interval = metrics.resolve_usage_interval(Some(100 * SECONDS_PER_DAY), Some(200 * SECONDS_PER_DAY));

        assert!(metrics.usage_top(None, &interval).is_empty());
    }

    #[test]
    fn test_usage_versions_splits_by_version_and_labels_absent_as_null() {
        let (_dir, _meta, metrics) = durable_on(500, None);
        metrics.record(download_of("a", "flask", "3.0", None, 10));
        metrics.record(download_of("a", "flask", "3.0", None, 10));
        metrics.record(download("a", "flask", "flask.bin", 5));
        settle_and_assert(&metrics, || metrics.daily_usage().len() == 2);
        let interval = metrics.resolve_usage_interval(None, None);

        assert_eq!(
            metrics.usage_versions(None, &interval),
            [
                VersionUsage {
                    repository: "a".into(),
                    project: "flask".into(),
                    version: Some("3.0".into()),
                    downloads: 2,
                    bytes: 20,
                },
                VersionUsage {
                    repository: "a".into(),
                    project: "flask".into(),
                    version: None,
                    downloads: 1,
                    bytes: 5,
                },
            ]
        );
    }

    #[test]
    fn test_usage_sources_splits_by_source_and_labels_local_as_null() {
        let (_dir, _meta, metrics) = durable_on(500, None);
        metrics.record(download_of("a", "flask", "1.0", Some("alpha"), 10));
        metrics.record(download_of("a", "flask", "1.0", None, 5));
        settle_and_assert(&metrics, || metrics.daily_usage().len() == 2);
        let interval = metrics.resolve_usage_interval(None, None);

        assert_eq!(
            metrics.usage_sources(None, &interval),
            [
                SourceUsage {
                    repository: "a".into(),
                    project: "flask".into(),
                    source: Some("alpha".into()),
                    downloads: 1,
                    bytes: 10,
                },
                SourceUsage {
                    repository: "a".into(),
                    project: "flask".into(),
                    source: None,
                    downloads: 1,
                    bytes: 5,
                },
            ]
        );
    }

    #[test]
    fn test_usage_timeline_buckets_downloads_by_ascending_day() {
        let (_dir, meta) = store();
        let earlier = Metrics::start_durable(meta.analytics(), None, clock_on_day(500));
        earlier.record(download_of("a", "flask", "1.0", None, 10));
        settle_and_assert(&earlier, || meta.analytics().load_daily().unwrap().is_some());
        drop(earlier);

        let metrics = Metrics::start_durable(meta.analytics(), None, clock_on_day(501));
        metrics.record(download_of("a", "flask", "1.0", None, 20));
        metrics.record(download_of("a", "django", "1.0", None, 3));
        settle_and_assert(&metrics, || metrics.daily_usage().len() == 3);
        let interval = metrics.resolve_usage_interval(None, None);

        assert_eq!(
            metrics.usage_timeline(None, &interval),
            [
                TimelineBucket {
                    day: 500,
                    start_unix: 500 * SECONDS_PER_DAY,
                    end_unix: 501 * SECONDS_PER_DAY,
                    downloads: 1,
                    bytes: 10,
                },
                TimelineBucket {
                    day: 501,
                    start_unix: 501 * SECONDS_PER_DAY,
                    end_unix: 502 * SECONDS_PER_DAY,
                    downloads: 2,
                    bytes: 23,
                },
            ]
        );
    }

    #[test]
    fn test_usage_unused_distinguishes_idle_projects_from_active_and_page_only() {
        let (_dir, meta) = store();
        let past = Metrics::start_durable(meta.analytics(), None, clock_on_day(100));
        past.record(download("a", "old", "old.bin", 7));
        past.record(download("a", "old", "old.bin", 7));
        settle_and_assert(&past, || persisted_downloads(&meta.analytics()) == Some(2));
        drop(past);

        let metrics = Metrics::start_durable(meta.analytics(), None, clock_on_day(500));
        metrics.record(download("a", "flask", "flask.bin", 10));
        metrics.record(Event::Page {
            route: "a".into(),
            project: "page-only".into(),
        });
        let interval = metrics.resolve_usage_interval(None, None);
        settle_and_assert(&metrics, || metrics.usage_top(None, &interval).len() == 1);

        assert_eq!(
            metrics.usage_unused(None, &interval),
            [UnusedPackage {
                repository: "a".into(),
                project: "old".into(),
                lifetime_downloads: 2,
            }]
        );
        assert!(metrics.usage_unused(Some("other"), &interval).is_empty());
    }

    #[test]
    fn test_usage_top_breaks_ties_by_repository_then_project() {
        let (_dir, _meta, metrics) = durable_on(500, None);
        metrics.record(download_of("a", "alpha", "1.0", None, 10));
        metrics.record(download_of("a", "beta", "1.0", None, 10));
        metrics.record(download_of("b", "alpha", "1.0", None, 10));
        settle_and_assert(&metrics, || metrics.daily_usage().len() == 3);
        let interval = metrics.resolve_usage_interval(None, None);

        assert_eq!(
            metrics
                .usage_top(None, &interval)
                .into_iter()
                .map(|row| (row.repository, row.project))
                .collect::<Vec<_>>(),
            [
                ("a".to_owned(), "alpha".to_owned()),
                ("a".to_owned(), "beta".to_owned()),
                ("b".to_owned(), "alpha".to_owned()),
            ]
        );
    }

    #[test]
    fn test_usage_versions_breaks_ties_by_version() {
        let (_dir, _meta, metrics) = durable_on(500, None);
        metrics.record(download_of("a", "flask", "2.0", None, 10));
        metrics.record(download_of("a", "flask", "1.0", None, 10));
        settle_and_assert(&metrics, || metrics.daily_usage().len() == 2);
        let interval = metrics.resolve_usage_interval(None, None);

        assert_eq!(
            metrics
                .usage_versions(None, &interval)
                .into_iter()
                .map(|row| row.version)
                .collect::<Vec<_>>(),
            [Some("1.0".to_owned()), Some("2.0".to_owned())]
        );
    }

    #[test]
    fn test_usage_sources_breaks_ties_by_source() {
        let (_dir, _meta, metrics) = durable_on(500, None);
        metrics.record(download_of("a", "flask", "1.0", Some("beta"), 10));
        metrics.record(download_of("a", "flask", "1.0", Some("alpha"), 10));
        settle_and_assert(&metrics, || metrics.daily_usage().len() == 2);
        let interval = metrics.resolve_usage_interval(None, None);

        assert_eq!(
            metrics
                .usage_sources(None, &interval)
                .into_iter()
                .map(|row| row.source)
                .collect::<Vec<_>>(),
            [Some("alpha".to_owned()), Some("beta".to_owned())]
        );
    }

    #[test]
    fn test_usage_unused_breaks_ties_by_repository_then_project() {
        let (_dir, meta) = store();
        let past = Metrics::start_durable(meta.analytics(), None, clock_on_day(100));
        for (route, project) in [("a", "alpha"), ("a", "beta"), ("b", "alpha")] {
            past.record(download(route, project, "file.bin", 5));
        }
        settle_and_assert(&past, || persisted_downloads(&meta.analytics()) == Some(3));
        drop(past);

        let metrics = Metrics::start_durable(meta.analytics(), None, clock_on_day(500));
        let interval = metrics.resolve_usage_interval(None, None);

        assert_eq!(
            metrics
                .usage_unused(None, &interval)
                .into_iter()
                .map(|row| (row.repository, row.project, row.lifetime_downloads))
                .collect::<Vec<_>>(),
            [
                ("a".to_owned(), "alpha".to_owned(), 1),
                ("a".to_owned(), "beta".to_owned(), 1),
                ("b".to_owned(), "alpha".to_owned(), 1),
            ]
        );
    }
}
