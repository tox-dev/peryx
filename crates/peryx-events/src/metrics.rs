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

/// The system-wall-clock source used when no clock is injected: Unix seconds, saturating rather than
/// panicking if the host clock predates the epoch.
fn system_clock() -> Clock {
    Arc::new(|| {
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map_or(0, |elapsed| i64::try_from(elapsed.as_secs()).unwrap_or(i64::MAX))
    })
}

/// The recording half handed to request handlers: a clone-cheap sender plus the shared snapshots.
#[derive(Clone)]
pub struct Metrics {
    sender: Sender<Event>,
    tree: Arc<RwLock<StatsTree>>,
    daily: Arc<RwLock<DailyBuckets>>,
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
        std::thread::Builder::new()
            .name("peryx-metrics".to_owned())
            .spawn(move || aggregate(&receiver, &sink, &daily_sink, store.as_ref(), retention_days, &clock))
            .expect("spawn metrics thread");
        Self { sender, tree, daily }
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
        let _ = self.sender.send(event);
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

    /// Projects with the most downloads, ordered by count, bytes, repository, then project.
    ///
    /// # Panics
    /// Panics if the aggregator thread panicked and poisoned the tree lock.
    #[must_use]
    pub fn top_packages(&self, limit: usize) -> Vec<PackageUsage> {
        let mut packages: Vec<_> = {
            let tree = self.tree.read().expect("metrics lock");
            tree.iter()
                .flat_map(|(repository, index)| {
                    index
                        .projects
                        .iter()
                        .filter(|(_, stats)| stats.totals.base.downloads > 0)
                        .map(move |(project, stats)| PackageUsage {
                            repository: repository.clone(),
                            project: project.clone(),
                            downloads: stats.totals.base.downloads,
                            bytes: stats.totals.base.bytes,
                        })
                })
                .collect()
        };
        packages.sort_by(|left, right| {
            right
                .downloads
                .cmp(&left.downloads)
                .then_with(|| right.bytes.cmp(&left.bytes))
                .then_with(|| left.repository.cmp(&right.repository))
                .then_with(|| left.project.cmp(&right.project))
        });
        packages.truncate(limit);
        packages
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
    receiver: &Receiver<Event>,
    tree: &Arc<RwLock<StatsTree>>,
    daily: &Arc<RwLock<DailyBuckets>>,
    store: Option<&AnalyticsHandle>,
    retention_days: Option<u32>,
    clock: &Clock,
) {
    while let Ok(event) = receiver.recv() {
        let mut dirty = matches!(&event, Event::Download { .. });
        let mut downloads = Vec::new();
        collect_daily(&event, clock, &mut downloads);
        let pending = {
            let mut tree = tree.write().expect("metrics lock");
            apply(&mut tree, event);
            // Batch whatever else is already queued under the same lock acquisition.
            while let Ok(event) = receiver.try_recv() {
                dirty |= matches!(&event, Event::Download { .. });
                collect_daily(&event, clock, &mut downloads);
                apply(&mut tree, event);
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

    use super::{Clock, DailySnapshot, DailyUsage, DownloadSnapshot, Event, Metrics, PackageUsage, SECONDS_PER_DAY};

    fn store() -> (tempfile::TempDir, MetaStore) {
        let dir = tempfile::tempdir().unwrap();
        let meta = MetaStore::open(dir.path().join("peryx.redb")).unwrap();
        (dir, meta)
    }

    /// A clock frozen at `day`'s noon, so a test dates every download to one deterministic UTC bucket.
    fn clock_on_day(day: i64) -> Clock {
        Arc::new(move || day * SECONDS_PER_DAY + SECONDS_PER_DAY / 2)
    }

    fn settle(done: impl Fn() -> bool) {
        // The aggregator runs on its own thread; poll until the last event lands.
        let settled = (0..500).any(|_| {
            std::thread::sleep(std::time::Duration::from_millis(2));
            done()
        });
        assert!(settled, "metrics aggregator never settled");
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
        settle(|| persisted_downloads(&meta.analytics()) == Some(2));
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
    fn test_batches_without_a_download_persist_nothing() {
        let (_dir, meta) = store();
        let metrics = Metrics::start_durable(meta.analytics(), None, clock_on_day(0));
        metrics.record(Event::Page {
            route: "pypi".into(),
            project: "flask".into(),
        });
        settle(|| {
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
        settle(|| metrics.daily_usage().len() == 3);

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
        settle(|| old.daily_usage().len() == 1);
        drop(old);

        // Ten days later a fresh download lands; the day-100 bucket is now beyond the 7-day window.
        let metrics = Metrics::start_durable(meta.analytics(), Some(7), clock_on_day(110));
        metrics.record(download_of("pypi", "flask", "2.0", Some("up"), 9));
        settle(|| metrics.daily_usage().iter().any(|row| row.day == 110));

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
        settle(|| meta.analytics().load_daily().unwrap().is_some());
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
        settle(|| metrics.daily_usage().len() == 1);
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
        settle(|| meta.analytics().load_daily().unwrap().is_some());
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
        settle(|| metrics.index_totals().contains_key("credential-bearing-route"));

        let totals = metrics.totals_for_routes(["missing", "credential-bearing-route"]);

        assert_eq!(totals.len(), 2);
        assert_eq!(totals[0].base.pages, 0);
        assert_eq!(totals[1].base.pages, 1);
    }

    #[test]
    fn test_top_packages_are_ranked_and_limited() {
        let metrics = Metrics::start();
        metrics.record(Event::Page {
            route: "empty".into(),
            project: "page-only".into(),
        });
        metrics.record(download("b", "large", "large.whl", 30));
        metrics.record(download("a", "small", "small.whl", 20));
        metrics.record(download("a", "small", "small.whl", 20));
        metrics.record(download("a", "alpha", "alpha.whl", 40));
        metrics.record(download("a", "beta", "beta.whl", 40));
        settle(|| metrics.top_packages(4).len() == 4);

        assert_eq!(
            metrics.top_packages(3),
            [
                PackageUsage {
                    repository: "a".into(),
                    project: "small".into(),
                    downloads: 2,
                    bytes: 40,
                },
                PackageUsage {
                    repository: "a".into(),
                    project: "alpha".into(),
                    downloads: 1,
                    bytes: 40,
                },
                PackageUsage {
                    repository: "a".into(),
                    project: "beta".into(),
                    downloads: 1,
                    bytes: 40,
                },
            ]
        );
        assert!(metrics.top_packages(0).is_empty());
    }
}
