use std::collections::{BTreeMap, HashMap};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::mpsc::{Receiver, RecvTimeoutError, Sender, SyncSender, TrySendError, channel, sync_channel};
use std::sync::{Arc, RwLock};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};

use peryx_core::Role;
use peryx_ha::{AggregateDelta, AggregateKey, AggregateRow, AnalyticsBatch, AuthorityEpoch, IntervalId, ProducerId};
use peryx_storage::meta::{AnalyticsHandle, MetaError};

/// Unix seconds supplied by the process clock.
pub type Clock = Arc<dyn Fn() -> i64 + Send + Sync>;

const SECONDS_PER_DAY: i64 = 86_400;

const DEFAULT_USAGE_WINDOW_DAYS: i64 = 30;

/// Bounds query work when retention is unlimited.
const MAX_USAGE_WINDOW_DAYS: i64 = 366;

const DAILY_SCHEMA: u32 = 1;

/// Bounds memory when analytics writes lag traffic.
const EVENT_QUEUE_CAPACITY: usize = 65_536;

const MAX_BATCH_MESSAGES: usize = 1_024;

/// Reports sustained overload without logging every dropped observation.
const DROP_LOG_INTERVAL: u64 = 1_024;

const FLUSH_INTERVAL: Duration = Duration::from_secs(5);

#[derive(Debug, Clone)]
pub enum Observation {
    Page {
        repository: String,
        resource: String,
    },
    Read {
        repository: String,
        resource: String,
        artifact: String,
        group: Option<String>,
        source: Option<String>,
        bytes: u64,
    },
    Ecosystem {
        repository: String,
        resource: String,
        artifact: Option<String>,
        family: &'static str,
    },
    Write {
        repository: String,
        resource: String,
    },
    Refresh {
        repository: String,
        resource: String,
        changed: bool,
    },
    StaleServed {
        repository: String,
        resource: String,
    },
    UpstreamError {
        repository: String,
        resource: String,
    },
    BlobRejected {
        repository: String,
        resource: String,
    },
    Extension {
        repository: String,
        family: &'static str,
        update: MetricUpdate,
    },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MetricUpdate {
    Increment(u64),
    Set(u64),
}

#[derive(Debug, Default, Clone, Serialize)]
pub struct BaseCounters {
    pub pages: u64,
    pub reads: u64,
    pub bytes: u64,
    pub rejected: u64,
}

#[derive(Debug, Default, Clone, Serialize)]
pub struct CachedCounters {
    pub refreshes: u64,
    pub changed: u64,
    pub stale_served: u64,
    pub upstream_errors: u64,
}

#[derive(Debug, Default, Clone, Serialize)]
pub struct HostedCounters {
    pub writes: u64,
}

/// New ecosystems add counters without changing this crate.
pub type EcosystemCounters = BTreeMap<&'static str, u64>;

/// Describes an owner-defined counter without adding its vocabulary here.
#[derive(Debug, Clone, Copy)]
pub struct MetricFamily {
    pub key: &'static str,
    pub prom_name: &'static str,
    pub help: &'static str,
    pub ui_label: &'static str,
    pub roles: &'static [Role],
    pub json_name: Option<&'static str>,
    pub kind: MetricKind,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MetricKind {
    Counter,
    Gauge,
}

impl MetricKind {
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Counter => "counter",
            Self::Gauge => "gauge",
        }
    }
}

#[derive(Debug, Default, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct EcosystemSummary {
    pub ecosystem: String,
    pub pages: u64,
    pub reads: u64,
    pub bytes: u64,
    pub rejected: u64,
    pub writes: u64,
    pub families: BTreeMap<String, u64>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct ResourceUsage {
    pub repository: String,
    pub resource: String,
    pub reads: u64,
    pub bytes: u64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct GroupUsage {
    pub repository: String,
    pub resource: String,
    pub group: Option<String>,
    pub reads: u64,
    pub bytes: u64,
}

/// Source attribution stays operator-scoped to avoid exposing routing details.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct SourceUsage {
    pub repository: String,
    pub resource: String,
    pub source: Option<String>,
    pub reads: u64,
    pub bytes: u64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct UnusedResource {
    pub repository: String,
    pub resource: String,
    pub lifetime_reads: u64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct TimelineBucket {
    pub day: i64,
    pub start_unix: i64,
    pub end_unix: i64,
    pub reads: u64,
    pub bytes: u64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct UsageInterval {
    pub from_day: i64,
    pub to_day: i64,
    pub retained_from_day: Option<i64>,
    pub window_clamped_to_retention: bool,
}

#[derive(Debug, Default, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FamilyDescriptor {
    pub ecosystem: String,
    pub key: String,
    pub label: String,
    pub roles: Vec<String>,
}

#[derive(Debug, Default, Clone, Serialize)]
pub struct Counters {
    pub base: BaseCounters,
    pub cached: CachedCounters,
    pub hosted: HostedCounters,
    pub ecosystem: EcosystemCounters,
    #[serde(skip)]
    pub extensions: EcosystemCounters,
}

#[derive(Debug, Default, Clone, Serialize)]
pub struct ArtifactStats {
    pub reads: u64,
    pub bytes: u64,
    pub ecosystem: EcosystemCounters,
}

#[derive(Debug, Default, Clone, Serialize)]
pub struct ResourceStats {
    pub totals: Counters,
    pub artifacts: HashMap<String, ArtifactStats>,
}

#[derive(Debug, Default, Clone, Serialize)]
pub struct RepositoryStats {
    pub totals: Counters,
    pub resources: HashMap<String, ResourceStats>,
}

pub type StatsTree = HashMap<String, RepositoryStats>;

#[derive(Debug, PartialEq, Eq, Serialize, Deserialize)]
struct ArtifactUsageRow {
    repository: String,
    resource: String,
    artifact: String,
    reads: u64,
    bytes: u64,
}

/// Operational counters restart with the process; only usage persists.
#[derive(Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
struct ReadSnapshot {
    artifacts: Vec<ArtifactUsageRow>,
}

/// `day` leads ordering so retention removes an expired prefix in one split.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
struct DailyKey {
    day: i64,
    repository: String,
    resource: String,
    group: String,
    source: String,
}

#[derive(Debug, Default, Clone, Copy)]
struct DailyTotals {
    reads: u64,
    bytes: u64,
}

type DailyBuckets = BTreeMap<DailyKey, DailyTotals>;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DailyUsage {
    pub day: i64,
    pub repository: String,
    pub resource: String,
    pub group: String,
    pub source: String,
    pub reads: u64,
    pub bytes: u64,
}

#[derive(Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
struct DailySnapshot {
    schema: u32,
    buckets: Vec<DailyUsage>,
}

const fn utc_day(unix_secs: i64) -> i64 {
    unix_secs.div_euclid(SECONDS_PER_DAY)
}

fn non_empty(value: String) -> Option<String> {
    (!value.is_empty()).then_some(value)
}

/// A bad host clock must not stop metrics startup.
fn system_clock() -> Clock {
    Arc::new(|| {
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map_or(0, |elapsed| i64::try_from(elapsed.as_secs()).unwrap_or(i64::MAX))
    })
}

enum Message {
    Observation { event: Observation, recorded_at: i64 },
    Drain(Sender<Result<(), MetricsError>>),
    Flush(Sender<Result<(), MetricsError>>),
    Shutdown(Sender<Result<(), MetricsError>>),
}

#[derive(Debug, thiserror::Error)]
pub enum MetricsError {
    #[error("metrics aggregator stopped")]
    Stopped,
    #[error("invalid metrics snapshot: {0}")]
    ReadSnapshot(serde_json::Error),
    #[error("invalid daily metrics snapshot: {0}")]
    DailySnapshot(serde_json::Error),
    #[error("unsupported daily metrics schema {0}")]
    DailySchema(u32),
    #[error("metrics persistence failed: {0}")]
    Persistence(String),
    #[error("cannot start metrics aggregator: {0}")]
    Thread(#[source] std::io::Error),
    #[error(transparent)]
    Store(#[from] MetaError),
}

pub trait MetricsStore: Send + Sync + 'static {
    /// # Errors
    /// Returns an error when the snapshot cannot be read.
    fn load(&self) -> Result<Option<Vec<u8>>, MetricsError>;

    /// # Errors
    /// Returns an error when the snapshot cannot be written.
    fn save(&self, snapshot: &[u8]) -> Result<(), MetricsError>;

    /// # Errors
    /// Returns an error when the daily snapshot cannot be read.
    fn load_daily(&self) -> Result<Option<Vec<u8>>, MetricsError>;

    /// # Errors
    /// Returns an error when the daily snapshot cannot be written.
    fn save_daily(&self, snapshot: &[u8]) -> Result<(), MetricsError>;
}

impl MetricsStore for AnalyticsHandle {
    fn load(&self) -> Result<Option<Vec<u8>>, MetricsError> {
        Ok(Self::load(self)?)
    }

    fn save(&self, snapshot: &[u8]) -> Result<(), MetricsError> {
        Ok(Self::save(self, snapshot)?)
    }

    fn load_daily(&self) -> Result<Option<Vec<u8>>, MetricsError> {
        Ok(Self::load_daily(self)?)
    }

    fn save_daily(&self, snapshot: &[u8]) -> Result<(), MetricsError> {
        Ok(Self::save_daily(self, snapshot)?)
    }
}

#[derive(Clone)]
pub struct Metrics {
    sender: SyncSender<Message>,
    tree: Arc<RwLock<StatsTree>>,
    daily: Arc<RwLock<DailyBuckets>>,
    dropped: Arc<AtomicU64>,
    clock: Clock,
    retention_days: Option<u32>,
    durability_failure: Arc<RwLock<Option<String>>>,
}

impl Metrics {
    /// # Panics
    /// Panics if the OS refuses to spawn the aggregator thread.
    #[must_use]
    pub fn start() -> Self {
        Self::spawn(None, None, system_clock(), EVENT_QUEUE_CAPACITY, FLUSH_INTERVAL)
            .expect("spawn in-memory metrics aggregator")
    }

    /// # Errors
    /// Returns an error if persisted accounting cannot be loaded or the aggregator cannot start.
    pub fn start_durable(
        store: impl MetricsStore,
        retention_days: Option<u32>,
        clock: Clock,
    ) -> Result<Self, MetricsError> {
        Self::start_durable_inner(Arc::new(store), retention_days, clock)
    }

    fn start_durable_inner(
        store: Arc<dyn MetricsStore>,
        retention_days: Option<u32>,
        clock: Clock,
    ) -> Result<Self, MetricsError> {
        Self::spawn(Some(store), retention_days, clock, EVENT_QUEUE_CAPACITY, FLUSH_INTERVAL)
    }

    #[must_use]
    pub fn start_durable_or_degraded(store: impl MetricsStore, retention_days: Option<u32>, clock: Clock) -> Self {
        Self::start_durable_or_degraded_inner(Arc::new(store), retention_days, clock)
    }

    fn start_durable_or_degraded_inner(
        store: Arc<dyn MetricsStore>,
        retention_days: Option<u32>,
        clock: Clock,
    ) -> Self {
        Self::start_durable_inner(store, retention_days, clock.clone()).unwrap_or_else(|error| {
            let (sender, receiver) = sync_channel(1);
            drop(receiver);
            Self {
                sender,
                tree: Arc::new(RwLock::new(StatsTree::new())),
                daily: Arc::new(RwLock::new(DailyBuckets::new())),
                dropped: Arc::new(AtomicU64::new(0)),
                clock,
                retention_days,
                durability_failure: Arc::new(RwLock::new(Some(error.to_string()))),
            }
        })
    }

    fn spawn(
        store: Option<Arc<dyn MetricsStore>>,
        retention_days: Option<u32>,
        clock: Clock,
        capacity: usize,
        flush_interval: Duration,
    ) -> Result<Self, MetricsError> {
        let (sender, receiver) = sync_channel(capacity);
        let mut initial = StatsTree::new();
        if let Some(bytes) = store.as_ref().map(|store| store.load()).transpose()?.flatten() {
            restore_reads(
                &mut initial,
                serde_json::from_slice(&bytes).map_err(MetricsError::ReadSnapshot)?,
            );
        }
        let mut daily_initial = DailyBuckets::new();
        if let Some(bytes) = store.as_ref().map(|store| store.load_daily()).transpose()?.flatten() {
            let snapshot: DailySnapshot = serde_json::from_slice(&bytes).map_err(MetricsError::DailySnapshot)?;
            if snapshot.schema != DAILY_SCHEMA {
                return Err(MetricsError::DailySchema(snapshot.schema));
            }
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
        let durability_failure = Arc::new(RwLock::new(None));
        let failure_sink = Arc::clone(&durability_failure);
        std::thread::Builder::new()
            .name("peryx-metrics".to_owned())
            .spawn(move || {
                let ctx = Aggregator {
                    tree: &sink,
                    daily: &daily_sink,
                    store: store.as_deref(),
                    retention_days,
                    clock: &clock,
                };
                aggregate(&receiver, &ctx, flush_interval, failure_sink);
            })
            .map_err(MetricsError::Thread)?;
        Ok(Self {
            sender,
            tree,
            daily,
            dropped: Arc::new(AtomicU64::new(0)),
            clock: query_clock,
            retention_days,
            durability_failure,
        })
    }

    /// A failed checkpoint remains visible until a later checkpoint succeeds.
    #[must_use]
    pub fn durability_failure(&self) -> Option<String> {
        self.durability_failure
            .read()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone()
    }

    /// A snapshot of the daily group-and-source usage buckets, ordered by day then dimension.
    ///
    /// # Panics
    /// Panics if the aggregator thread panicked and poisoned the daily lock.
    #[must_use]
    pub fn daily_usage(&self) -> Vec<DailyUsage> {
        daily_rows(&self.daily.read().expect("metrics lock"))
    }

    /// Exports bounded aggregates without request or actor history.
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
                    resource: usage.resource,
                    group: usage.group,
                    source: usage.source,
                },
                delta: AggregateDelta {
                    downloads: usage.reads,
                    bytes: usage.bytes,
                },
            })
            .collect();
        AnalyticsBatch { interval, rows }
    }

    /// Exports stable batches only after a UTC day closes.
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
        let mut by_day: BTreeMap<u64, Vec<AggregateRow>> = BTreeMap::new();
        for usage in self.daily_usage() {
            if usage.day <= after_day || usage.day >= today {
                continue;
            }
            let Ok(sequence) = u64::try_from(usage.day) else {
                continue;
            };
            by_day.entry(sequence).or_default().push(AggregateRow {
                key: AggregateKey {
                    day: usage.day,
                    repository: usage.repository,
                    resource: usage.resource,
                    group: usage.group,
                    source: usage.source,
                },
                delta: AggregateDelta {
                    downloads: usage.reads,
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
                    sequence: day,
                },
                rows,
            })
            .collect()
    }

    /// Drops observations under load to keep request-path memory bounded.
    ///
    /// Every drop increments [`Metrics::dropped`]; throttled logs expose sustained overload.
    pub fn record(&self, event: Observation) {
        if let Err(TrySendError::Full(_)) = self.sender.try_send(Message::Observation {
            event,
            recorded_at: (self.clock)(),
        }) {
            let total = self.dropped.fetch_add(1, Ordering::Relaxed) + 1;
            if total == 1 || total.is_multiple_of(DROP_LOG_INTERVAL) {
                tracing::warn!(target: "peryx::metrics", dropped = total, "metrics event queue full, dropping event");
            }
        }
    }

    pub fn increment(&self, repository: &str, family: &MetricFamily, value: u64) {
        self.record(Observation::Extension {
            repository: repository.to_owned(),
            family: family.key,
            update: MetricUpdate::Increment(value),
        });
    }

    pub fn set(&self, repository: &str, family: &MetricFamily, value: u64) {
        self.record(Observation::Extension {
            repository: repository.to_owned(),
            family: family.key,
            update: MetricUpdate::Set(value),
        });
    }

    #[must_use]
    pub fn dropped(&self) -> u64 {
        self.dropped.load(Ordering::Relaxed)
    }

    /// # Errors
    /// Returns an error if the aggregator stopped or persistence failed.
    pub fn flush(&self) -> Result<(), MetricsError> {
        let (completion, done) = channel();
        self.sender
            .send(Message::Flush(completion))
            .map_err(|_| MetricsError::Stopped)?;
        done.recv().unwrap_or(Err(MetricsError::Stopped))
    }

    /// # Errors
    /// Returns an error if the aggregator stopped.
    pub fn drain(&self) -> Result<(), MetricsError> {
        let (completion, done) = channel();
        self.sender
            .send(Message::Drain(completion))
            .map_err(|_| MetricsError::Stopped)?;
        done.recv().unwrap_or(Err(MetricsError::Stopped))
    }

    /// # Errors
    /// Returns an error if the aggregator stopped or persistence failed.
    pub fn shutdown(self) -> Result<(), MetricsError> {
        let (completion, done) = channel();
        self.sender
            .send(Message::Shutdown(completion))
            .map_err(|_| MetricsError::Stopped)?;
        done.recv().unwrap_or(Err(MetricsError::Stopped))
    }

    /// A snapshot of one index's totals per repository, for the dashboard cards and Prometheus.
    ///
    /// # Panics
    /// Panics if the aggregator thread panicked and poisoned the tree lock.
    #[must_use]
    pub fn index_totals(&self) -> HashMap<String, Counters> {
        self.tree
            .read()
            .expect("metrics lock")
            .iter()
            .map(|(repository, stats)| (repository.clone(), stats.totals.clone()))
            .collect()
    }

    /// Snapshot totals for the requested repositories in the same order, without copying repository values.
    /// Missing repositories report zero counters.
    ///
    /// # Panics
    /// Panics if the aggregator thread panicked and poisoned the tree lock.
    #[must_use]
    pub fn totals_for_routes<'a>(&self, repositories: impl IntoIterator<Item = &'a str>) -> Vec<Counters> {
        let tree = self.tree.read().expect("metrics lock");
        repositories
            .into_iter()
            .map(|repository| {
                tree.get(repository)
                    .map(|stats| stats.totals.clone())
                    .unwrap_or_default()
            })
            .collect()
    }

    /// Today's UTC day off the query clock, in whole days since the Unix epoch. A completeness query
    /// reads it to measure how far the accepted analytics frontier lags the present.
    #[must_use]
    pub fn current_day(&self) -> i64 {
        utc_day((self.clock)())
    }

    /// Resolves optional Unix-second bounds to an inclusive UTC-day interval. The end defaults to today
    /// and cannot exceed it. The interval defaults to 30 days, cannot exceed 366 days, and cannot start
    /// before the configured retention floor.
    #[must_use]
    pub fn resolve_usage_interval(&self, from_secs: Option<i64>, to_secs: Option<i64>) -> UsageInterval {
        let now_day = utc_day((self.clock)());
        let retained_from_day = self.retention_days.map(|days| now_day - i64::from(days));
        let to_day = to_secs.map_or(now_day, utc_day).min(now_day);
        let from_day = from_secs
            .map_or(to_day - (DEFAULT_USAGE_WINDOW_DAYS - 1), utc_day)
            .max(to_day - (MAX_USAGE_WINDOW_DAYS - 1));
        UsageInterval {
            window_clamped_to_retention: retained_from_day.is_some_and(|floor| from_day < floor),
            from_day: retained_from_day.map_or(from_day, |floor| from_day.max(floor)),
            to_day,
            retained_from_day,
        }
    }

    /// Resources by reads over `interval`, ordered by reads, bytes, repository, then resource.
    ///
    /// # Panics
    /// Panics if the aggregator thread panicked and poisoned the daily lock.
    #[must_use]
    pub fn usage_top(&self, repository: Option<&str>, interval: &UsageInterval) -> Vec<ResourceUsage> {
        let mut rows: Vec<_> = self
            .fold_daily(repository, interval, |bucket| {
                (bucket.repository.clone(), bucket.resource.clone())
            })
            .into_iter()
            .map(|((repository, resource), totals)| ResourceUsage {
                repository,
                resource,
                reads: totals.reads,
                bytes: totals.bytes,
            })
            .collect();
        rows.sort_by(|left, right| {
            right
                .reads
                .cmp(&left.reads)
                .then_with(|| right.bytes.cmp(&left.bytes))
                .then_with(|| left.repository.cmp(&right.repository))
                .then_with(|| left.resource.cmp(&right.resource))
        });
        rows
    }

    /// Resource groups by reads over `interval`, ordered by reads, bytes, then identity.
    ///
    /// # Panics
    /// Panics if the aggregator thread panicked and poisoned the daily lock.
    #[must_use]
    pub fn usage_groups(&self, repository: Option<&str>, interval: &UsageInterval) -> Vec<GroupUsage> {
        let mut rows: Vec<_> = self
            .fold_daily(repository, interval, |bucket| {
                (bucket.repository.clone(), bucket.resource.clone(), bucket.group.clone())
            })
            .into_iter()
            .map(|((repository, resource, group), totals)| GroupUsage {
                repository,
                resource,
                group: non_empty(group),
                reads: totals.reads,
                bytes: totals.bytes,
            })
            .collect();
        rows.sort_by(|left, right| {
            right
                .reads
                .cmp(&left.reads)
                .then_with(|| right.bytes.cmp(&left.bytes))
                .then_with(|| left.repository.cmp(&right.repository))
                .then_with(|| left.resource.cmp(&right.resource))
                .then_with(|| left.group.cmp(&right.group))
        });
        rows
    }

    /// Resource reads attributed to each routed source over `interval`, ordered by reads,
    /// bytes, then identity. The caller must have cleared the source dimension for the requester.
    ///
    /// # Panics
    /// Panics if the aggregator thread panicked and poisoned the daily lock.
    #[must_use]
    pub fn usage_sources(&self, repository: Option<&str>, interval: &UsageInterval) -> Vec<SourceUsage> {
        let mut rows: Vec<_> = self
            .fold_daily(repository, interval, |bucket| {
                (
                    bucket.repository.clone(),
                    bucket.resource.clone(),
                    bucket.source.clone(),
                )
            })
            .into_iter()
            .map(|((repository, resource, source), totals)| SourceUsage {
                repository,
                resource,
                source: non_empty(source),
                reads: totals.reads,
                bytes: totals.bytes,
            })
            .collect();
        rows.sort_by(|left, right| {
            right
                .reads
                .cmp(&left.reads)
                .then_with(|| right.bytes.cmp(&left.bytes))
                .then_with(|| left.repository.cmp(&right.repository))
                .then_with(|| left.resource.cmp(&right.resource))
                .then_with(|| left.source.cmp(&right.source))
        });
        rows
    }

    /// Uses lifetime totals so scoped reads avoid scanning daily buckets.
    ///
    /// Orders by reads descending, then repository and resource ascending.
    ///
    /// # Panics
    /// Panics if the aggregator thread panicked and poisoned the metrics lock.
    #[must_use]
    pub fn usage_totals(&self, repository_filter: Option<&str>) -> Vec<ResourceUsage> {
        let tree = self.tree.read().expect("metrics lock");
        let mut rows = Vec::new();
        for (repository, index) in tree.iter() {
            if repository_filter.is_some_and(|filter| repository != filter) {
                continue;
            }
            for (resource, stats) in &index.resources {
                rows.push(ResourceUsage {
                    repository: repository.clone(),
                    resource: resource.clone(),
                    reads: stats.totals.base.reads,
                    bytes: stats.totals.base.bytes,
                });
            }
        }
        drop(tree);
        rows.sort_by(|left, right| {
            right
                .reads
                .cmp(&left.reads)
                .then_with(|| left.repository.cmp(&right.repository))
                .then_with(|| left.resource.cmp(&right.resource))
        });
        rows
    }

    /// Resources with durable reads but none inside `interval`, ordered by lifetime reads,
    /// repository, then resource. The universe is every resource the retained totals have ever served.
    ///
    /// # Panics
    /// Panics if the aggregator thread panicked and poisoned either lock.
    #[must_use]
    pub fn usage_unused(&self, repository_filter: Option<&str>, interval: &UsageInterval) -> Vec<UnusedResource> {
        let active: std::collections::BTreeSet<(String, String)> = self
            .fold_daily(repository_filter, interval, |bucket| {
                (bucket.repository.clone(), bucket.resource.clone())
            })
            .into_keys()
            .collect();
        let tree = self.tree.read().expect("metrics lock");
        let mut rows = Vec::new();
        for (repository, index) in tree.iter() {
            if repository_filter.is_some_and(|filter| repository != filter) {
                continue;
            }
            for (resource, stats) in &index.resources {
                if stats.totals.base.reads > 0 && !active.contains(&(repository.clone(), resource.clone())) {
                    rows.push(UnusedResource {
                        repository: repository.clone(),
                        resource: resource.clone(),
                        lifetime_reads: stats.totals.base.reads,
                    });
                }
            }
        }
        drop(tree);
        rows.sort_by(|left, right| {
            right
                .lifetime_reads
                .cmp(&left.lifetime_reads)
                .then_with(|| left.repository.cmp(&right.repository))
                .then_with(|| left.resource.cmp(&right.resource))
        });
        rows
    }

    /// Reads bucketed by UTC day over `interval`, ascending by day so the series reads forward.
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
                reads: totals.reads,
                bytes: totals.bytes,
            })
            .collect()
    }

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
                || repository.is_some_and(|repository| bucket.repository != repository)
            {
                continue;
            }
            let group = folded.entry(key(bucket)).or_default();
            group.reads += totals.reads;
            group.bytes += totals.bytes;
        }
        drop(daily);
        folded
    }

    /// The tree at the requested depth: everything, one index's resources, or one resource's artifacts.
    ///
    /// # Panics
    /// Panics if the aggregator thread panicked and poisoned the tree lock.
    #[must_use]
    pub fn drill(&self, repository: Option<&str>, resource: Option<&str>) -> serde_json::Value {
        let tree = self.tree.read().expect("metrics lock");
        match (repository, resource) {
            (Some(repository), Some(resource)) => tree
                .get(repository)
                .and_then(|index| index.resources.get(resource))
                .map_or_else(|| serde_json::json!({}), |stats| serde_json::json!(stats)),
            (Some(repository), None) => tree.get(repository).map_or_else(
                || serde_json::json!({}),
                |index| {
                    serde_json::json!({
                        "totals": index.totals,
                        "resources": index.resources.iter()
                            .map(|(name, stats)| (name.clone(), serde_json::json!(stats.totals)))
                            .collect::<HashMap<_, _>>(),
                    })
                },
            ),
            _ => serde_json::json!(
                tree.iter()
                    .map(|(repository, index)| (repository.clone(), serde_json::json!(index.totals)))
                    .collect::<HashMap<_, _>>()
            ),
        }
    }
}

impl peryx_ha::AnalyticsBatchSource for Metrics {
    fn sealed_batches(&self, producer: &ProducerId, epoch: AuthorityEpoch, after_day: i64) -> Vec<AnalyticsBatch> {
        self.export_sealed_day_batches(producer, epoch, after_day)
    }
}

struct Aggregator<'a> {
    tree: &'a RwLock<StatsTree>,
    daily: &'a RwLock<DailyBuckets>,
    store: Option<&'a dyn MetricsStore>,
    retention_days: Option<u32>,
    clock: &'a Clock,
}

struct FlushState {
    persistent: bool,
    pending: bool,
    interval_started: Instant,
    durability_failure: Arc<RwLock<Option<String>>>,
}

impl FlushState {
    fn durable(persistent: bool, durability_failure: Arc<RwLock<Option<String>>>) -> Self {
        Self {
            persistent,
            pending: false,
            interval_started: Instant::now(),
            durability_failure,
        }
    }

    const fn mark(&mut self, dirty: bool) {
        self.pending |= self.persistent && dirty;
    }

    const fn pending(&self) -> bool {
        self.pending
    }

    fn wake_in(&self, interval: Duration, retention: bool) -> Option<Duration> {
        (self.pending || retention).then(|| interval.saturating_sub(self.interval_started.elapsed()))
    }

    fn checkpoint_due(&self, interval: Duration) -> bool {
        self.pending && self.interval_started.elapsed() >= interval
    }

    fn reset_interval(&mut self) {
        self.interval_started = Instant::now();
    }
}

enum Received {
    Batch(Message),
    Idle,
    Closed,
}

fn aggregate(
    receiver: &Receiver<Message>,
    ctx: &Aggregator,
    interval: Duration,
    durability_failure: Arc<RwLock<Option<String>>>,
) {
    let mut state = FlushState::durable(ctx.store.is_some(), durability_failure);
    while step(receiver, ctx, interval, &mut state) {}
}

fn step(receiver: &Receiver<Message>, ctx: &Aggregator, interval: Duration, state: &mut FlushState) -> bool {
    match receive(receiver, state.wake_in(interval, ctx.retention_days.is_some())) {
        Received::Batch(first) => {
            let batch = absorb_batch(first, receiver, ctx);
            state.mark(batch.dirty);
            if let Some(control) = batch.control {
                let result = if matches!(control, Control::Drain(_)) || !state.pending() {
                    Ok(())
                } else {
                    persist(ctx, state)
                };
                let stop = matches!(control, Control::Shutdown(_));
                control.complete(result);
                if stop {
                    return false;
                }
            } else if state.checkpoint_due(interval)
                && let Err(error) = persist(ctx, state)
            {
                tracing::error!(target: "peryx::metrics", %error, "metrics checkpoint failed");
            }
            true
        }
        Received::Idle => {
            state.mark(expire_retained(ctx.daily, ctx.retention_days, ctx.clock));
            if state.pending() {
                if let Err(error) = persist(ctx, state) {
                    tracing::error!(target: "peryx::metrics", %error, "metrics checkpoint failed");
                }
            } else {
                state.reset_interval();
            }
            true
        }
        Received::Closed => false,
    }
}

/// Use one deadline so idle traffic cannot delay persistence or retention.
fn receive(receiver: &Receiver<Message>, wake_in: Option<Duration>) -> Received {
    wake_in.map_or_else(
        || receiver.recv().map_or(Received::Closed, Received::Batch),
        |wait| match receiver.recv_timeout(wait) {
            Ok(message) => Received::Batch(message),
            Err(RecvTimeoutError::Timeout) => Received::Idle,
            Err(RecvTimeoutError::Disconnected) => Received::Closed,
        },
    )
}

fn absorb_batch(first: Message, receiver: &Receiver<Message>, ctx: &Aggregator) -> Batch {
    let mut batch = Batch::default();
    {
        let mut tree = ctx.tree.write().expect("metrics lock");
        for message in std::iter::once(first)
            .chain(receiver.try_iter())
            .take(MAX_BATCH_MESSAGES)
        {
            absorb(message, &mut tree, &mut batch);
            if batch.control.is_some() {
                break;
            }
        }
    }
    batch.dirty |= fold_daily_batch(
        std::mem::take(&mut batch.reads),
        ctx.daily,
        ctx.retention_days,
        ctx.clock,
    );
    batch
}

#[derive(Default)]
struct Batch {
    dirty: bool,
    reads: Vec<(DailyKey, u64)>,
    control: Option<Control>,
}

enum Control {
    Drain(Sender<Result<(), MetricsError>>),
    Flush(Sender<Result<(), MetricsError>>),
    Shutdown(Sender<Result<(), MetricsError>>),
}

impl Control {
    fn complete(self, result: Result<(), MetricsError>) {
        let sender = match self {
            Self::Drain(sender) | Self::Flush(sender) | Self::Shutdown(sender) => sender,
        };
        let _ = sender.send(result);
    }
}

fn persist(ctx: &Aggregator, state: &mut FlushState) -> Result<(), MetricsError> {
    let store = ctx.store.expect("a pending checkpoint without a store");
    let reads = serde_json::to_vec(&snapshot_reads(&ctx.tree.read().expect("metrics lock")))
        .expect("serialize metrics snapshot");
    let daily = serde_json::to_vec(&snapshot_daily(&ctx.daily.read().expect("metrics lock")))
        .expect("serialize daily usage snapshot");
    let result = store.save(&reads).and_then(|()| store.save_daily(&daily));
    state.reset_interval();
    match result {
        Ok(()) => {
            state.pending = false;
            *state.durability_failure.write().expect("metrics lock") = None;
            Ok(())
        }
        Err(error) => {
            *state.durability_failure.write().expect("metrics lock") = Some(error.to_string());
            Err(error)
        }
    }
}

fn absorb(message: Message, tree: &mut StatsTree, batch: &mut Batch) {
    match message {
        Message::Observation { event, recorded_at } => {
            batch.dirty |= matches!(&event, Observation::Read { .. });
            collect_daily(&event, recorded_at, &mut batch.reads);
            apply(tree, event);
        }
        Message::Drain(completion) => batch.control = Some(Control::Drain(completion)),
        Message::Flush(completion) => batch.control = Some(Control::Flush(completion)),
        Message::Shutdown(completion) => batch.control = Some(Control::Shutdown(completion)),
    }
}

fn collect_daily(event: &Observation, recorded_at: i64, out: &mut Vec<(DailyKey, u64)>) {
    if let Observation::Read {
        repository,
        resource,
        group,
        source,
        bytes,
        ..
    } = event
    {
        out.push((
            DailyKey {
                day: utc_day(recorded_at),
                repository: repository.clone(),
                resource: resource.clone(),
                group: group.clone().unwrap_or_default(),
                source: source.clone().unwrap_or_default(),
            },
            *bytes,
        ));
    }
}

/// Applying retention per batch also expires buckets in long-running processes.
fn fold_daily_batch(
    reads: Vec<(DailyKey, u64)>,
    daily: &RwLock<DailyBuckets>,
    retention_days: Option<u32>,
    clock: &Clock,
) -> bool {
    if reads.is_empty() {
        return expire_retained(daily, retention_days, clock);
    }
    let mut daily = daily.write().expect("metrics lock");
    for (key, bytes) in reads {
        let totals = daily.entry(key).or_default();
        totals.reads += 1;
        totals.bytes += bytes;
    }
    let expired = retention_days.is_some_and(|days| expire_daily(&mut daily, clock(), days));
    drop(daily);
    expired
}

fn expire_retained(daily: &RwLock<DailyBuckets>, retention_days: Option<u32>, clock: &Clock) -> bool {
    retention_days.is_some_and(|days| expire_daily(&mut daily.write().expect("metrics lock"), clock(), days))
}

/// Drop every bucket older than `retention_days` days. Buckets order by day first, so the expired
/// prefix leaves in one split and the retained totals are never touched.
fn expire_daily(daily: &mut DailyBuckets, now_secs: i64, retention_days: u32) -> bool {
    let floor = DailyKey {
        day: utc_day(now_secs) - i64::from(retention_days),
        repository: String::new(),
        resource: String::new(),
        group: String::new(),
        source: String::new(),
    };
    let previous_len = daily.len();
    *daily = daily.split_off(&floor);
    daily.len() != previous_len
}

fn daily_rows(daily: &DailyBuckets) -> Vec<DailyUsage> {
    daily
        .iter()
        .map(|(key, totals)| DailyUsage {
            day: key.day,
            repository: key.repository.clone(),
            resource: key.resource.clone(),
            group: key.group.clone(),
            source: key.source.clone(),
            reads: totals.reads,
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

fn restore_daily(daily: &mut DailyBuckets, snapshot: DailySnapshot) {
    for row in snapshot.buckets {
        let totals = daily
            .entry(DailyKey {
                day: row.day,
                repository: row.repository,
                resource: row.resource,
                group: row.group,
                source: row.source,
            })
            .or_default();
        totals.reads += row.reads;
        totals.bytes += row.bytes;
    }
}

fn snapshot_reads(tree: &StatsTree) -> ReadSnapshot {
    let artifacts = tree
        .iter()
        .flat_map(|(repository, index)| {
            index.resources.iter().flat_map(move |(resource, stats)| {
                stats
                    .artifacts
                    .iter()
                    .map(move |(artifact, artifact_stats)| ArtifactUsageRow {
                        repository: repository.clone(),
                        resource: resource.clone(),
                        artifact: artifact.clone(),
                        reads: artifact_stats.reads,
                        bytes: artifact_stats.bytes,
                    })
            })
        })
        .collect();
    ReadSnapshot { artifacts }
}

fn restore_reads(tree: &mut StatsTree, snapshot: ReadSnapshot) {
    for row in snapshot.artifacts {
        let index = tree.entry(row.repository).or_default();
        index.totals.base.reads += row.reads;
        index.totals.base.bytes += row.bytes;
        let resource = index.resources.entry(row.resource).or_default();
        resource.totals.base.reads += row.reads;
        resource.totals.base.bytes += row.bytes;
        let artifact = resource.artifacts.entry(row.artifact).or_default();
        artifact.reads += row.reads;
        artifact.bytes += row.bytes;
    }
}

fn apply(tree: &mut StatsTree, event: Observation) {
    match event {
        Observation::Page { repository, resource } => {
            let index = tree.entry(repository).or_default();
            index.totals.base.pages += 1;
            index.resources.entry(resource).or_default().totals.base.pages += 1;
        }
        Observation::Read {
            repository,
            resource,
            artifact,
            bytes,
            ..
        } => {
            let index = tree.entry(repository).or_default();
            index.totals.base.reads += 1;
            index.totals.base.bytes += bytes;
            let resource = index.resources.entry(resource).or_default();
            resource.totals.base.reads += 1;
            resource.totals.base.bytes += bytes;
            let artifact = resource.artifacts.entry(artifact).or_default();
            artifact.reads += 1;
            artifact.bytes += bytes;
        }
        Observation::Ecosystem {
            repository,
            resource,
            artifact,
            family,
        } => apply_ecosystem(tree, repository, resource, artifact, family),
        Observation::Write { repository, resource } => {
            let index = tree.entry(repository).or_default();
            index.totals.hosted.writes += 1;
            index.resources.entry(resource).or_default().totals.hosted.writes += 1;
        }
        Observation::Refresh {
            repository,
            resource,
            changed,
        } => {
            let index = tree.entry(repository).or_default();
            index.totals.cached.refreshes += 1;
            let resource = index.resources.entry(resource).or_default();
            resource.totals.cached.refreshes += 1;
            if changed {
                index.totals.cached.changed += 1;
                resource.totals.cached.changed += 1;
            }
        }
        Observation::StaleServed { repository, resource } => {
            let index = tree.entry(repository).or_default();
            index.totals.cached.stale_served += 1;
            index.resources.entry(resource).or_default().totals.cached.stale_served += 1;
        }
        Observation::UpstreamError { repository, resource } => {
            let index = tree.entry(repository).or_default();
            index.totals.cached.upstream_errors += 1;
            index
                .resources
                .entry(resource)
                .or_default()
                .totals
                .cached
                .upstream_errors += 1;
        }
        Observation::BlobRejected { repository, resource } => {
            let index = tree.entry(repository).or_default();
            index.totals.base.rejected += 1;
            index.resources.entry(resource).or_default().totals.base.rejected += 1;
        }
        Observation::Extension {
            repository,
            family,
            update,
        } => apply_extension(tree, repository, family, update),
    }
}

fn apply_ecosystem(
    tree: &mut StatsTree,
    repository: String,
    resource: String,
    artifact: Option<String>,
    family: &'static str,
) {
    let index = tree.entry(repository).or_default();
    *index.totals.ecosystem.entry(family).or_default() += 1;
    let resource = index.resources.entry(resource).or_default();
    *resource.totals.ecosystem.entry(family).or_default() += 1;
    if let Some(artifact) = artifact {
        *resource
            .artifacts
            .entry(artifact)
            .or_default()
            .ecosystem
            .entry(family)
            .or_default() += 1;
    }
}

fn apply_extension(tree: &mut StatsTree, repository: String, family: &'static str, update: MetricUpdate) {
    let value = tree
        .entry(repository)
        .or_default()
        .totals
        .extensions
        .entry(family)
        .or_default();
    match update {
        MetricUpdate::Increment(delta) => *value += delta,
        MetricUpdate::Set(next) => *value = next,
    }
}

#[cfg(test)]
#[path = "../tests/unit/metrics/tests.rs"]
mod tests;
