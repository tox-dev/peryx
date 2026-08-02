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
//! families (`PyPI`'s PEP 658 sibling today), and the render layer scopes each family to the roles
//! and ecosystem that emit it, so a hosted index never reports a caching counter.

use std::collections::{BTreeMap, HashMap};
use std::sync::mpsc::{Receiver, Sender, channel};
use std::sync::{Arc, RwLock};
use std::time::{SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};

use peryx_core::Role;
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
    /// content-addressed OCI layers), and `source` is the routed upstream a cache miss fetched from
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
    /// through [`MetricFamily`] (`PyPI`'s `metadata` PEP 658 sibling today); `filename` keys the
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
    /// The dashboard label, e.g. `PEP 658 metadata hits`.
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
/// `lifetime_downloads` distinguishes a package that was simply idle in the window from one whose
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
/// it, so a test can await a deterministic point instead of polling the shared snapshots.
enum Message {
    Event(Event),
    #[cfg(test)]
    Barrier(Sender<()>),
}

/// The recording half handed to request handlers: a clone-cheap sender plus the shared snapshots.
#[derive(Clone)]
pub struct Metrics {
    sender: Sender<Message>,
    tree: Arc<RwLock<StatsTree>>,
    daily: Arc<RwLock<DailyBuckets>>,
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
        Self::spawn(None, None, system_clock())
    }

    /// Start an aggregator with durable usage: restore the persisted per-file totals and daily buckets,
    /// rewrite each after every batch that recorded a download, and prune daily buckets older than
    /// `retention_days` (kept without limit when `None`). `clock` dates each download's UTC bucket.
    /// Persistence and pruning run on the aggregator thread, never the request path.
    ///
    /// # Panics
    /// Panics if the OS refuses to spawn the aggregator thread.
    #[must_use]
    pub fn start_durable(store: AnalyticsHandle, retention_days: Option<u32>, clock: Clock) -> Self {
        Self::spawn(Some(store), retention_days, clock)
    }

    fn spawn(store: Option<AnalyticsHandle>, retention_days: Option<u32>, clock: Clock) -> Self {
        let (sender, receiver) = channel();
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
            .spawn(move || aggregate(&receiver, &sink, &daily_sink, store.as_ref(), retention_days, &clock))
            .expect("spawn metrics thread");
        Self {
            sender,
            tree,
            daily,
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

    /// Record one event; never blocks, and a stopped aggregator is ignored.
    pub fn record(&self, event: Event) {
        let _ = self.sender.send(Message::Event(event));
    }

    /// Block until the aggregator has drained and persisted every event recorded before this call.
    /// The channel is FIFO, so the barrier lands behind those events; the aggregator acknowledges it
    /// only after their snapshots are written, giving tests a deterministic settle point.
    #[cfg(test)]
    fn sync(&self) {
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

/// The aggregator loop: drain events until every sender is gone, persisting the download snapshot
/// after each batch that changed it. Serializing happens under the lock (cheap); the durable write
/// happens after releasing it, so a slow disk never stalls the aggregator's readers.
fn aggregate(
    receiver: &Receiver<Message>,
    tree: &Arc<RwLock<StatsTree>>,
    daily: &Arc<RwLock<DailyBuckets>>,
    store: Option<&AnalyticsHandle>,
    retention_days: Option<u32>,
    clock: &Clock,
) {
    while let Ok(first) = receiver.recv() {
        let mut dirty = false;
        let mut downloads = Vec::new();
        #[cfg(test)]
        let mut acks = Vec::new();
        let pending = {
            let mut tree = tree.write().expect("metrics lock");
            absorb(
                first,
                &mut tree,
                clock,
                &mut dirty,
                &mut downloads,
                #[cfg(test)]
                &mut acks,
            );
            // Batch whatever else is already queued under the same lock acquisition.
            while let Ok(message) = receiver.try_recv() {
                absorb(
                    message,
                    &mut tree,
                    clock,
                    &mut dirty,
                    &mut downloads,
                    #[cfg(test)]
                    &mut acks,
                );
            }
            (dirty && store.is_some())
                .then(|| serde_json::to_vec(&snapshot_downloads(&tree)).expect("serialize metrics snapshot"))
        };
        if let (Some(store), Some(bytes)) = (store, pending) {
            let _ = store.save(&bytes);
        }
        if !downloads.is_empty() {
            let mut daily = daily.write().expect("metrics lock");
            for (key, bytes) in downloads {
                let totals = daily.entry(key).or_default();
                totals.downloads += 1;
                totals.bytes += bytes;
            }
            if let Some(days) = retention_days {
                expire_daily(&mut daily, clock(), days);
            }
            let pending = store.is_some().then(|| snapshot_daily(&daily));
            drop(daily);
            if let (Some(store), Some(snapshot)) = (store, pending) {
                let _ = store.save_daily(&serde_json::to_vec(&snapshot).expect("serialize daily usage snapshot"));
            }
        }
        // Acknowledge barriers only once this batch's snapshots are on disk, so a synced test observes
        // the durable state, not just the in-memory tree.
        #[cfg(test)]
        for ack in acks {
            let _ = ack.send(());
        }
    }
}

/// Fold one message into the batch: apply an event to the tree and note its daily downloads, or park a
/// barrier's acknowledgement for the aggregator to fire after the batch persists.
fn absorb(
    message: Message,
    tree: &mut StatsTree,
    clock: &Clock,
    dirty: &mut bool,
    downloads: &mut Vec<(DailyKey, u64)>,
    #[cfg(test)] acks: &mut Vec<Sender<()>>,
) {
    match message {
        Message::Event(event) => {
            *dirty |= matches!(&event, Event::Download { .. });
            collect_daily(&event, clock, downloads);
            apply(tree, event);
        }
        #[cfg(test)]
        Message::Barrier(ack) => acks.push(ack),
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
    use std::sync::Arc;

    use peryx_storage::meta::{AnalyticsHandle, MetaStore};

    use super::{
        Clock, DailySnapshot, DailyUsage, DownloadSnapshot, Event, Metrics, PackageUsage, SECONDS_PER_DAY, SourceUsage,
        TimelineBucket, UnusedPackage, UsageInterval, VersionUsage,
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

    fn settle(metrics: &Metrics, done: impl Fn() -> bool) {
        // Drain the aggregator through its barrier, then assert the state it settled on.
        metrics.sync();
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
            filename: format!("{project}-{version}.whl"),
            version: Some(version.into()),
            source: source.map(Into::into),
            bytes,
        }
    }

    #[test]
    fn test_durable_downloads_survive_a_restart() {
        let (_dir, meta) = store();
        let filename = "pandas-3.0-py3-none-any.whl";
        let metrics = Metrics::start_durable(meta.analytics(), None, clock_on_day(0));
        metrics.record(Event::Page {
            route: "root/pypi".into(),
            project: "pandas".into(),
        });
        metrics.record(download("root/pypi", "pandas", filename, 100));
        metrics.record(download("root/pypi", "pandas", filename, 50));
        settle(&metrics, || persisted_downloads(&meta.analytics()) == Some(2));
        drop(metrics);

        let restarted = Metrics::start_durable(meta.analytics(), None, clock_on_day(0));
        let totals = restarted.index_totals();
        let index = &totals["root/pypi"];
        assert_eq!(index.base.downloads, 2);
        assert_eq!(index.base.bytes, 150);
        let files = restarted.drill(Some("root/pypi"), Some("pandas"));
        assert_eq!(files["files"][filename]["downloads"], 2);
        assert_eq!(files["files"][filename]["bytes"], 150);
    }

    #[test]
    fn test_usage_totals_reports_lifetime_by_repository() {
        let (_dir, meta) = store();
        let metrics = Metrics::start_durable(meta.analytics(), None, clock_on_day(0));
        metrics.record(download("pypi", "numpy", "numpy-1.whl", 100));
        metrics.record(download("pypi", "numpy", "numpy-1.whl", 100));
        metrics.record(download("pypi", "scipy", "scipy-1.whl", 50));
        metrics.record(download("other", "django", "django-1.whl", 30));
        settle(&metrics, || metrics.usage_totals(None).len() == 3);

        assert_eq!(
            metrics.usage_totals(None),
            [
                PackageUsage {
                    repository: "pypi".into(),
                    project: "numpy".into(),
                    downloads: 2,
                    bytes: 200,
                },
                PackageUsage {
                    repository: "other".into(),
                    project: "django".into(),
                    downloads: 1,
                    bytes: 30,
                },
                PackageUsage {
                    repository: "pypi".into(),
                    project: "scipy".into(),
                    downloads: 1,
                    bytes: 50,
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
            route: "pypi".into(),
            project: "flask".into(),
        });
        settle(&metrics, || {
            metrics
                .index_totals()
                .get("pypi")
                .is_some_and(|totals| totals.base.pages == 1)
        });
        assert_eq!(persisted_downloads(&meta.analytics()), None);
        assert!(meta.analytics().load_daily().unwrap().is_none());
    }

    #[test]
    fn test_daily_buckets_split_by_version_source_and_day() {
        let (_dir, meta) = store();
        let metrics = Metrics::start_durable(meta.analytics(), None, clock_on_day(20_000));
        metrics.record(download_of("pypi", "flask", "3.0", Some("pypi-org"), 10));
        metrics.record(download_of("pypi", "flask", "3.0", Some("pypi-org"), 40));
        metrics.record(download_of("pypi", "flask", "2.0", Some("pypi-org"), 5));
        metrics.record(download_of("pypi", "flask", "3.0", None, 7));
        settle(&metrics, || metrics.daily_usage().len() == 3);

        assert_eq!(
            metrics.daily_usage(),
            [
                DailyUsage {
                    day: 20_000,
                    repository: "pypi".into(),
                    project: "flask".into(),
                    version: "2.0".into(),
                    source: "pypi-org".into(),
                    downloads: 1,
                    bytes: 5,
                },
                DailyUsage {
                    day: 20_000,
                    repository: "pypi".into(),
                    project: "flask".into(),
                    version: "3.0".into(),
                    source: String::new(),
                    downloads: 1,
                    bytes: 7,
                },
                DailyUsage {
                    day: 20_000,
                    repository: "pypi".into(),
                    project: "flask".into(),
                    version: "3.0".into(),
                    source: "pypi-org".into(),
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
        old.record(download_of("pypi", "flask", "1.0", Some("up"), 3));
        settle(&old, || old.daily_usage().len() == 1);
        drop(old);

        // Ten days later a fresh download lands; the day-100 bucket is now beyond the 7-day window.
        let metrics = Metrics::start_durable(meta.analytics(), Some(7), clock_on_day(110));
        metrics.record(download_of("pypi", "flask", "2.0", Some("up"), 9));
        settle(&metrics, || metrics.daily_usage().iter().any(|row| row.day == 110));

        assert_eq!(
            metrics.daily_usage(),
            [DailyUsage {
                day: 110,
                repository: "pypi".into(),
                project: "flask".into(),
                version: "2.0".into(),
                source: "up".into(),
                downloads: 1,
                bytes: 9,
            }]
        );
    }

    #[test]
    fn test_daily_usage_survives_a_restart() {
        let (_dir, meta) = store();
        let metrics = Metrics::start_durable(meta.analytics(), None, clock_on_day(42));
        metrics.record(download_of("pypi", "flask", "3.0", Some("up"), 12));
        settle(&metrics, || meta.analytics().load_daily().unwrap().is_some());
        drop(metrics);

        let restarted = Metrics::start_durable(meta.analytics(), None, clock_on_day(42));
        assert_eq!(
            restarted.daily_usage(),
            [DailyUsage {
                day: 42,
                repository: "pypi".into(),
                project: "flask".into(),
                version: "3.0".into(),
                source: "up".into(),
                downloads: 1,
                bytes: 12,
            }]
        );
    }

    #[test]
    fn test_malformed_daily_snapshot_rebuilds_without_blocking_startup() {
        let (_dir, meta) = store();
        meta.analytics().save_daily(b"{ not valid json").unwrap();
        let metrics = Metrics::start_durable(meta.analytics(), None, clock_on_day(7));
        assert!(metrics.daily_usage().is_empty());

        metrics.record(download_of("pypi", "flask", "3.0", Some("up"), 4));
        settle(&metrics, || metrics.daily_usage().len() == 1);
        assert_eq!(metrics.daily_usage()[0].bytes, 4);
    }

    #[test]
    fn test_unknown_daily_schema_rebuilds_from_zero() {
        let (_dir, meta) = store();
        let future = DailySnapshot {
            schema: super::DAILY_SCHEMA + 1,
            buckets: vec![DailyUsage {
                day: 1,
                repository: "pypi".into(),
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
        metrics.record(download("pypi", "flask", "flask-3.0.whl", 8));
        settle(&metrics, || meta.analytics().load_daily().unwrap().is_some());
        drop(metrics);

        let restarted = Metrics::start_durable(meta.analytics(), None, clock_on_day(3));
        assert_eq!(
            restarted.daily_usage(),
            [DailyUsage {
                day: 3,
                repository: "pypi".into(),
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
        settle(&metrics, || {
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
        settle(&metrics, || metrics.daily_usage().len() == 4);
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
        settle(&metrics, || metrics.daily_usage().len() == 1);
        let interval = metrics.resolve_usage_interval(Some(100 * SECONDS_PER_DAY), Some(200 * SECONDS_PER_DAY));

        assert!(metrics.usage_top(None, &interval).is_empty());
    }

    #[test]
    fn test_usage_versions_splits_by_version_and_labels_absent_as_null() {
        let (_dir, _meta, metrics) = durable_on(500, None);
        metrics.record(download_of("a", "flask", "3.0", None, 10));
        metrics.record(download_of("a", "flask", "3.0", None, 10));
        metrics.record(download("a", "flask", "flask.whl", 5));
        settle(&metrics, || metrics.daily_usage().len() == 2);
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
        metrics.record(download_of("a", "flask", "1.0", Some("pypi"), 10));
        metrics.record(download_of("a", "flask", "1.0", None, 5));
        settle(&metrics, || metrics.daily_usage().len() == 2);
        let interval = metrics.resolve_usage_interval(None, None);

        assert_eq!(
            metrics.usage_sources(None, &interval),
            [
                SourceUsage {
                    repository: "a".into(),
                    project: "flask".into(),
                    source: Some("pypi".into()),
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
        settle(&earlier, || meta.analytics().load_daily().unwrap().is_some());
        drop(earlier);

        let metrics = Metrics::start_durable(meta.analytics(), None, clock_on_day(501));
        metrics.record(download_of("a", "flask", "1.0", None, 20));
        metrics.record(download_of("a", "django", "1.0", None, 3));
        settle(&metrics, || metrics.daily_usage().len() == 3);
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
        past.record(download("a", "old", "old.whl", 7));
        past.record(download("a", "old", "old.whl", 7));
        settle(&past, || persisted_downloads(&meta.analytics()) == Some(2));
        drop(past);

        let metrics = Metrics::start_durable(meta.analytics(), None, clock_on_day(500));
        metrics.record(download("a", "flask", "flask.whl", 10));
        metrics.record(Event::Page {
            route: "a".into(),
            project: "page-only".into(),
        });
        let interval = metrics.resolve_usage_interval(None, None);
        settle(&metrics, || metrics.usage_top(None, &interval).len() == 1);

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
        settle(&metrics, || metrics.daily_usage().len() == 3);
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
        settle(&metrics, || metrics.daily_usage().len() == 2);
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
        settle(&metrics, || metrics.daily_usage().len() == 2);
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
            past.record(download(route, project, "file.whl", 5));
        }
        settle(&past, || persisted_downloads(&meta.analytics()) == Some(3));
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
