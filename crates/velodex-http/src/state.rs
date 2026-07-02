//! Shared application state and index routing.

use std::collections::HashMap;
use std::sync::atomic::AtomicU64;
use std::sync::{Arc, Mutex};

use bytes::Bytes;
use velodex_storage::blob::BlobStore;
use velodex_storage::meta::MetaStore;
use velodex_upstream::UpstreamClient;

use crate::metrics::Metrics;

/// A source of the current unix time, injectable so cache-freshness logic is deterministic in
/// tests.
pub type Clock = Arc<dyn Fn() -> i64 + Send + Sync>;

/// One resolved index. `layers`/`upload` in an overlay are indices into [`AppState::indexes`], so
/// resolution is a plain vector walk with no name lookups at request time.
#[derive(Debug)]
pub struct Index {
    pub name: String,
    pub route: String,
    pub kind: IndexKind,
}

/// The runtime shape of an index: a mirror owns its upstream client, a local store its upload
/// policy, an overlay the resolved positions of its layers and upload target.
#[derive(Debug)]
pub enum IndexKind {
    Mirror(UpstreamClient),
    Local {
        upload_token: Option<String>,
        volatile: bool,
    },
    Overlay {
        layers: Vec<usize>,
        upload: Option<usize>,
    },
}

/// Everything a request handler needs. Shared as `Arc<AppState>`.
pub struct AppState {
    pub meta: MetaStore,
    pub blobs: BlobStore,
    /// Fallback freshness for cached simple pages, in seconds: applies only when upstream's
    /// `Cache-Control` granted no usable lifetime.
    pub ttl_secs: i64,
    pub clock: Clock,
    pub requests: AtomicU64,
    /// PEP 658/714 `.metadata` sibling requests served, exposed via `/metrics`. Downstream clients
    /// only hit this when they take the metadata-only resolution fast path, so it is the server-side
    /// proof that pip and uv resolve through velodex without downloading whole wheels.
    pub metadata_requests: AtomicU64,
    pub indexes: Vec<Index>,
    /// One async lock per project being fetched from upstream, so concurrent cache misses for the
    /// same page share a single upstream fetch instead of each downloading and storing it.
    pub inflight: Mutex<HashMap<String, Arc<tokio::sync::Mutex<()>>>>,
    /// Transformed page bytes ready to serve, paired with their unix expiry: warm requests are a
    /// lookup, an expiry check, and a memcpy. Entries carry the mutation epoch in their key, so
    /// uploads and overrides invalidate by key miss; the expiry honors each page's upstream
    /// `Cache-Control` lifetime, and moka's own time-to-live is a coarse eviction backstop.
    pub hot: moka::sync::Cache<String, (i64, Bytes)>,
    /// Bumped by every mutation that changes what a page serves (persisted fetches, uploads,
    /// yank/hide/restore), retiring hot-cache keys.
    pub epoch: AtomicU64,
    /// Off-thread usage aggregation: index → project → file counters for the dashboard.
    pub metrics: Metrics,
}

impl AppState {
    /// Build the state with a system clock.
    #[must_use]
    pub fn new(meta: MetaStore, blobs: BlobStore, ttl_secs: i64, indexes: Vec<Index>) -> Self {
        Self::with_clock(meta, blobs, ttl_secs, indexes, Arc::new(system_now))
    }

    /// Build the state with an injected clock.
    #[must_use]
    pub fn with_clock(meta: MetaStore, blobs: BlobStore, ttl_secs: i64, indexes: Vec<Index>, clock: Clock) -> Self {
        Self {
            meta,
            blobs,
            ttl_secs,
            clock,
            requests: AtomicU64::new(0),
            metadata_requests: AtomicU64::new(0),
            indexes,
            inflight: Mutex::new(HashMap::new()),
            hot: moka::sync::Cache::builder()
                .max_capacity(256 * 1024 * 1024)
                .weigher(|key: &String, (_, value): &(i64, Bytes)| {
                    u32::try_from(key.len() + value.len()).unwrap_or(u32::MAX)
                })
                .time_to_live(std::time::Duration::from_secs(
                    u64::try_from(ttl_secs.max(1)).unwrap_or(1800),
                ))
                .build(),
            epoch: AtomicU64::new(0),
            metrics: Metrics::start(),
        }
    }

    /// Find the index whose route is the longest segment-aligned prefix of `path` (which has no
    /// leading slash), and the path remainder after `route/`. Returns `None` if no route matches.
    #[must_use]
    pub fn resolve<'a>(&'a self, path: &'a str) -> Option<(&'a Index, &'a str)> {
        self.resolve_position(path)
            .map(|(position, rest)| (&self.indexes[position], rest))
    }

    /// Like [`Self::resolve`], returning the index position instead of a borrow.
    #[must_use]
    pub fn resolve_position<'a>(&self, path: &'a str) -> Option<(usize, &'a str)> {
        let mut best: Option<(usize, &str)> = None;
        for (position, index) in self.indexes.iter().enumerate() {
            let Some(rest) = remainder(path, &index.route) else {
                continue;
            };
            if best.is_none_or(|(current, _)| index.route.len() > self.indexes[current].route.len()) {
                best = Some((position, rest));
            }
        }
        best
    }

    /// The index at position `pos` (an overlay layer or upload target).
    #[must_use]
    pub fn index_at(&self, pos: usize) -> &Index {
        &self.indexes[pos]
    }

    /// A hot-cache entry that is still within its freshness window; expired entries miss.
    #[must_use]
    pub fn hot_fresh(&self, key: &str) -> Option<Bytes> {
        let (expires_at, bytes) = self.hot.get(key)?;
        ((self.clock)() < expires_at).then_some(bytes)
    }

    /// The hot-cache key for a page as served on `route` right now.
    #[must_use]
    pub fn hot_key(&self, route: &str, project: &str) -> String {
        let epoch = self.epoch.load(std::sync::atomic::Ordering::Relaxed);
        format!("{route}\u{0}{project}\u{0}{epoch}")
    }

    /// Retire every hot-cache entry after a mutation (upload, yank, hide, restore, or a fresh
    /// upstream page).
    pub fn bump_epoch(&self) {
        self.epoch.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    }

    /// Describe every configured index for presentation: kind name, overlay layer names, and
    /// whether uploads are enabled. Shared by `/+status` and the web UI.
    #[must_use]
    pub fn describe_indexes(&self) -> Vec<IndexDescription> {
        self.indexes
            .iter()
            .map(|index| {
                let (kind, layers, uploads, upload_to) = match &index.kind {
                    IndexKind::Mirror(_) => ("mirror", Vec::new(), false, None),
                    IndexKind::Local { upload_token, .. } => ("local", Vec::new(), upload_token.is_some(), None),
                    IndexKind::Overlay { layers, upload } => {
                        let names = layers.iter().map(|&pos| self.index_at(pos).name.clone()).collect();
                        let uploads = upload.is_some_and(|pos| {
                            matches!(
                                &self.index_at(pos).kind,
                                IndexKind::Local {
                                    upload_token: Some(_),
                                    ..
                                }
                            )
                        });
                        let upload_to = upload.map(|pos| self.index_at(pos).name.clone());
                        ("overlay", names, uploads, upload_to)
                    }
                };
                IndexDescription {
                    name: index.name.clone(),
                    route: index.route.clone(),
                    kind,
                    layers,
                    uploads,
                    upload_to,
                }
            })
            .collect()
    }
}

/// A configured index as presented to humans: on the dashboard and in `/+status`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct IndexDescription {
    pub name: String,
    pub route: String,
    pub kind: &'static str,
    pub layers: Vec<String>,
    pub uploads: bool,
    /// For an overlay: the layer uploads land in, whether or not a token currently enables them.
    pub upload_to: Option<String>,
}

/// The part of `path` after `route`, requiring a segment boundary so `team/dev` does not match
/// `team/development`. `""` means the index root itself.
fn remainder<'a>(path: &'a str, route: &str) -> Option<&'a str> {
    if path == route {
        return Some("");
    }
    path.strip_prefix(route)?.strip_prefix('/')
}

fn system_now() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| i64::try_from(d.as_secs()).unwrap_or(i64::MAX))
}
