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

use serde::{Deserialize, Serialize};

use peryx_core::Role;
use peryx_storage::meta::AnalyticsHandle;

/// One request-path observation.
#[derive(Debug, Clone)]
pub enum Event {
    /// An index listing was served.
    Page { route: String, project: String },
    /// An artifact was served, with its size. `filename` keys the per-file breakdown; `project` is
    /// the pre-normalized owning project (the ecosystem driver derives it, so this stays neutral).
    Download {
        route: String,
        project: String,
        filename: String,
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
    /// The Prometheus metric name, e.g. `peryx_index_metadata_total`.
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

/// The recording half handed to request handlers: a clone-cheap sender plus the shared snapshot.
#[derive(Clone)]
pub struct Metrics {
    sender: Sender<Event>,
    tree: Arc<RwLock<StatsTree>>,
}

impl Metrics {
    /// Start an ephemeral aggregator whose counters live only as long as the process.
    ///
    /// # Panics
    /// Panics if the OS refuses to spawn the aggregator thread.
    #[must_use]
    pub fn start() -> Self {
        Self::spawn(None)
    }

    /// Start an aggregator with durable download usage: restore the persisted snapshot into the
    /// initial tree, and rewrite it after every batch that recorded a download, so download and byte
    /// totals survive a restart. Persistence runs on the aggregator thread, never the request path.
    ///
    /// # Panics
    /// Panics if the OS refuses to spawn the aggregator thread.
    #[must_use]
    pub fn start_durable(store: AnalyticsHandle) -> Self {
        Self::spawn(Some(store))
    }

    fn spawn(store: Option<AnalyticsHandle>) -> Self {
        let (sender, receiver) = channel();
        let mut initial = StatsTree::new();
        if let Some(snapshot) = store
            .as_ref()
            .and_then(|store| store.load().ok().flatten())
            .and_then(|bytes| serde_json::from_slice::<DownloadSnapshot>(&bytes).ok())
        {
            restore_downloads(&mut initial, snapshot);
        }
        let tree = Arc::new(RwLock::new(initial));
        let sink = Arc::clone(&tree);
        std::thread::Builder::new()
            .name("peryx-metrics".to_owned())
            .spawn(move || aggregate(&receiver, &sink, store.as_ref()))
            .expect("spawn metrics thread");
        Self { sender, tree }
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
fn aggregate(receiver: &Receiver<Event>, tree: &Arc<RwLock<StatsTree>>, store: Option<&AnalyticsHandle>) {
    while let Ok(event) = receiver.recv() {
        let mut dirty = matches!(&event, Event::Download { .. });
        let pending = {
            let mut tree = tree.write().expect("metrics lock");
            apply(&mut tree, event);
            // Batch whatever else is already queued under the same lock acquisition.
            while let Ok(event) = receiver.try_recv() {
                dirty |= matches!(&event, Event::Download { .. });
                apply(&mut tree, event);
            }
            (dirty && store.is_some())
                .then(|| serde_json::to_vec(&snapshot_downloads(&tree)).expect("serialize metrics snapshot"))
        };
        if let (Some(store), Some(bytes)) = (store, pending) {
            let _ = store.save(&bytes);
        }
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
    }
}

#[cfg(test)]
mod tests {
    use peryx_storage::meta::{AnalyticsHandle, MetaStore};

    use super::{DownloadSnapshot, Event, Metrics, PackageUsage};

    fn store() -> (tempfile::TempDir, MetaStore) {
        let dir = tempfile::tempdir().unwrap();
        let meta = MetaStore::open(dir.path().join("peryx.redb")).unwrap();
        (dir, meta)
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
            bytes,
        }
    }

    #[test]
    fn test_durable_downloads_survive_a_restart() {
        let (_dir, meta) = store();
        let filename = "pandas-3.0-py3-none-any.whl";
        let metrics = Metrics::start_durable(meta.analytics());
        metrics.record(Event::Page {
            route: "root/pypi".into(),
            project: "pandas".into(),
        });
        metrics.record(download("root/pypi", "pandas", filename, 100));
        metrics.record(download("root/pypi", "pandas", filename, 50));
        settle(|| persisted_downloads(&meta.analytics()) == Some(2));
        drop(metrics);

        let restarted = Metrics::start_durable(meta.analytics());
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
        let metrics = Metrics::start_durable(meta.analytics());
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
