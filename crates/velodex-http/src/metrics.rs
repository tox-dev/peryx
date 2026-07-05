//! Usage metrics, aggregated off the request path.
//!
//! Handlers record events with one non-blocking channel send; a dedicated OS thread aggregates them
//! into a tree (index → project → file) that the dashboard and `/+stats` read. The request path
//! never takes the aggregation lock for writing.

use std::collections::HashMap;
use std::sync::mpsc::{Receiver, Sender, channel};
use std::sync::{Arc, RwLock};

use serde::Serialize;

/// One request-path observation.
#[derive(Debug, Clone)]
pub enum Event {
    /// A simple page was served.
    Page { route: String, project: String },
    /// An artifact was served, with its size. `filename` keys the per-file breakdown; `project` is
    /// the pre-normalized owning project (the ecosystem driver derives it, so this stays neutral).
    Download {
        route: String,
        project: String,
        filename: String,
        bytes: u64,
    },
    /// A PEP 658 sibling was served.
    Metadata {
        route: String,
        project: String,
        filename: String,
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

/// Counters at one level of the tree.
#[derive(Debug, Default, Clone, Serialize)]
pub struct Counters {
    pub pages: u64,
    pub downloads: u64,
    pub metadata: u64,
    pub uploads: u64,
    pub bytes: u64,
    pub refreshes: u64,
    /// Refreshes that found the upstream page changed.
    pub changed: u64,
    /// Pages served from cache because upstream was unavailable.
    pub stale_served: u64,
    pub upstream_errors: u64,
    /// Downloads whose bytes failed digest verification and were not cached.
    pub rejected: u64,
}

/// Per-file counters.
#[derive(Debug, Default, Clone, Serialize)]
pub struct FileStats {
    pub downloads: u64,
    pub metadata: u64,
    pub bytes: u64,
}

/// Per-project counters plus the files underneath.
#[derive(Debug, Default, Clone, Serialize)]
pub struct ProjectStats {
    #[serde(flatten)]
    pub totals: Counters,
    pub files: HashMap<String, FileStats>,
}

/// Per-index counters plus the projects underneath.
#[derive(Debug, Default, Clone, Serialize)]
pub struct IndexStats {
    #[serde(flatten)]
    pub totals: Counters,
    pub projects: HashMap<String, ProjectStats>,
}

/// The whole tree, index route at the top.
pub type StatsTree = HashMap<String, IndexStats>;

/// The recording half handed to request handlers: a clone-cheap sender plus the shared snapshot.
#[derive(Clone)]
pub struct Metrics {
    sender: Sender<Event>,
    tree: Arc<RwLock<StatsTree>>,
}

impl Metrics {
    /// Start the aggregator thread and return the recorder.
    ///
    /// # Panics
    /// Panics if the OS refuses to spawn the aggregator thread.
    #[must_use]
    pub fn start() -> Self {
        let (sender, receiver) = channel();
        let tree = Arc::new(RwLock::new(StatsTree::new()));
        let sink = Arc::clone(&tree);
        std::thread::Builder::new()
            .name("velodex-metrics".to_owned())
            .spawn(move || aggregate(&receiver, &sink))
            .expect("spawn metrics thread");
        Self { sender, tree }
    }

    /// Record one event; never blocks, and a stopped aggregator is ignored.
    pub fn record(&self, event: Event) {
        let _ = self.sender.send(event);
    }

    /// A snapshot of one index's totals per route, for the dashboard cards.
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

/// The aggregator loop: drain events until every sender is gone.
fn aggregate(receiver: &Receiver<Event>, tree: &Arc<RwLock<StatsTree>>) {
    while let Ok(event) = receiver.recv() {
        let mut tree = tree.write().expect("metrics lock");
        apply(&mut tree, event);
        // Batch whatever else is already queued under the same lock acquisition.
        while let Ok(event) = receiver.try_recv() {
            apply(&mut tree, event);
        }
    }
}

fn apply(tree: &mut StatsTree, event: Event) {
    match event {
        Event::Page { route, project } => {
            let index = tree.entry(route).or_default();
            index.totals.pages += 1;
            index.projects.entry(project).or_default().totals.pages += 1;
        }
        Event::Download {
            route,
            project,
            filename,
            bytes,
        } => {
            let index = tree.entry(route).or_default();
            index.totals.downloads += 1;
            index.totals.bytes += bytes;
            let project = index.projects.entry(project).or_default();
            project.totals.downloads += 1;
            project.totals.bytes += bytes;
            let file = project.files.entry(filename).or_default();
            file.downloads += 1;
            file.bytes += bytes;
        }
        Event::Metadata {
            route,
            project,
            filename,
        } => {
            let index = tree.entry(route).or_default();
            index.totals.metadata += 1;
            let project = index.projects.entry(project).or_default();
            project.totals.metadata += 1;
            project.files.entry(filename).or_default().metadata += 1;
        }
        Event::Upload { route, project } => {
            let index = tree.entry(route).or_default();
            index.totals.uploads += 1;
            index.projects.entry(project).or_default().totals.uploads += 1;
        }
        Event::Refresh {
            route,
            project,
            changed,
        } => {
            let index = tree.entry(route).or_default();
            index.totals.refreshes += 1;
            let project = index.projects.entry(project).or_default();
            project.totals.refreshes += 1;
            if changed {
                index.totals.changed += 1;
                project.totals.changed += 1;
            }
        }
        Event::StaleServed { route, project } => {
            let index = tree.entry(route).or_default();
            index.totals.stale_served += 1;
            index.projects.entry(project).or_default().totals.stale_served += 1;
        }
        Event::UpstreamError { route, project } => {
            let index = tree.entry(route).or_default();
            index.totals.upstream_errors += 1;
            index.projects.entry(project).or_default().totals.upstream_errors += 1;
        }
        Event::BlobRejected { route, project } => {
            let index = tree.entry(route).or_default();
            index.totals.rejected += 1;
            index.projects.entry(project).or_default().totals.rejected += 1;
        }
    }
}
