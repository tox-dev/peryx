//! Shared application state and index routing.

use std::collections::HashMap;
use std::sync::atomic::AtomicU64;
use std::sync::{Arc, Mutex};

use bytes::Bytes;
use velodex_storage::blob::BlobStore;
use velodex_storage::meta::MetaStore;
use velodex_upstream::UpstreamClient;

use crate::metrics::Metrics;
use crate::search::{PackageSearch, SearchError};

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
    /// One live download per blob digest: concurrent cold requests for the same file all tail the
    /// one upstream transfer as it lands instead of waiting for it to finish.
    pub downloads: Mutex<HashMap<String, crate::cache::DownloadHandle>>,
    /// Transformed page bytes ready to serve, paired with their unix expiry: warm requests are a
    /// lookup, an expiry check, and a memcpy. Entries carry the mutation epoch in their key, so
    /// uploads and overrides invalidate by key miss; the expiry honors each page's upstream
    /// `Cache-Control` lifetime, and moka's own time-to-live is a coarse eviction backstop.
    pub hot: moka::sync::Cache<String, (i64, Bytes)>,
    /// Short-lived misses from upstream, keyed separately from stored pages and artifacts so 404s
    /// do not add fake rows to the persistent cache.
    pub negative: moka::sync::Cache<String, i64>,
    /// Bumped by every mutation that changes what a page serves (persisted fetches, uploads,
    /// yank/hide/restore), retiring hot-cache keys.
    pub epoch: AtomicU64,
    /// Off-thread usage aggregation: index → project → file counters for the dashboard.
    pub metrics: Metrics,
    /// Derived package search index, refreshed from storage when the mutation epoch advances.
    pub search: PackageSearch,
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
        Self::with_clock_and_search(meta, blobs, ttl_secs, indexes, clock, PackageSearch::in_memory())
    }

    /// Build the state with an on-disk search index.
    ///
    /// # Errors
    /// Returns an error if the search index cannot be opened.
    pub fn with_search_path(
        meta: MetaStore,
        blobs: BlobStore,
        ttl_secs: i64,
        indexes: Vec<Index>,
        search_path: impl AsRef<std::path::Path>,
    ) -> Result<Self, SearchError> {
        Ok(Self::with_clock_and_search(
            meta,
            blobs,
            ttl_secs,
            indexes,
            Arc::new(system_now),
            PackageSearch::open(search_path)?,
        ))
    }

    fn with_clock_and_search(
        meta: MetaStore,
        blobs: BlobStore,
        ttl_secs: i64,
        indexes: Vec<Index>,
        clock: Clock,
        search: PackageSearch,
    ) -> Self {
        Self {
            meta,
            blobs,
            ttl_secs,
            clock,
            requests: AtomicU64::new(0),
            metadata_requests: AtomicU64::new(0),
            indexes,
            inflight: Mutex::new(HashMap::new()),
            downloads: Mutex::new(HashMap::new()),
            hot: moka::sync::Cache::builder()
                .max_capacity(256 * 1024 * 1024)
                .weigher(|key: &String, (_, value): &(i64, Bytes)| {
                    u32::try_from(key.len() + value.len()).unwrap_or(u32::MAX)
                })
                .time_to_live(std::time::Duration::from_secs(
                    u64::try_from(ttl_secs.max(1)).unwrap_or(1800),
                ))
                .build(),
            negative: moka::sync::Cache::builder().max_capacity(65_536).build(),
            epoch: AtomicU64::new(0),
            metrics: Metrics::start(),
            search,
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

    /// Whether a remembered upstream miss is still inside its injected-clock expiry.
    #[must_use]
    pub fn negative_fresh(&self, key: &str) -> bool {
        match self.negative.get(key) {
            Some(expires_at) if (self.clock)() < expires_at => true,
            Some(_) => {
                self.negative.invalidate(key);
                false
            }
            None => false,
        }
    }

    /// Remember an upstream miss for `ttl_secs` according to the injected clock.
    pub fn remember_negative(&self, key: String, ttl_secs: i64) {
        self.negative.insert(key, (self.clock)() + ttl_secs);
    }

    /// Retire every hot-cache entry after a mutation (upload, yank, hide, restore, or a fresh
    /// upstream page).
    pub fn bump_epoch(&self) {
        self.epoch.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    }

    /// Describe every configured index for presentation: kind name, overlay layer names, upload
    /// access, and delete policy. Shared by `/+status` and the web UI.
    #[must_use]
    pub fn describe_indexes(&self) -> Vec<IndexDescription> {
        describe_indexes(&self.indexes)
    }
}

/// Describe every runtime index without touching storage or upstream state.
#[must_use]
pub fn describe_indexes(indexes: &[Index]) -> Vec<IndexDescription> {
    (0..indexes.len())
        .map(|position| describe_index(indexes, position))
        .collect()
}

pub(crate) fn describe_index(indexes: &[Index], position: usize) -> IndexDescription {
    let index = &indexes[position];
    let (kind, layers, uploads, volatile_deletes, upload_to) = match &index.kind {
        IndexKind::Mirror(_) => ("mirror", Vec::new(), false, false, None),
        IndexKind::Local { upload_token, volatile } => (
            "local",
            Vec::new(),
            upload_token.is_some(),
            upload_token.is_some() && *volatile,
            None,
        ),
        IndexKind::Overlay { layers, upload } => {
            let names = layers.iter().map(|&pos| indexes[pos].name.clone()).collect();
            let uploads = upload.is_some_and(|pos| {
                matches!(
                    &indexes[pos].kind,
                    IndexKind::Local {
                        upload_token: Some(_),
                        ..
                    }
                )
            });
            let volatile_deletes = upload.is_some_and(|pos| {
                matches!(
                    &indexes[pos].kind,
                    IndexKind::Local {
                        upload_token: Some(_),
                        volatile: true,
                    }
                )
            });
            let upload_to = upload.map(|pos| indexes[pos].name.clone());
            ("overlay", names, uploads, volatile_deletes, upload_to)
        }
    };
    let (upstream, local) = match &index.kind {
        IndexKind::Mirror(client) => (
            Some(UpstreamDescription {
                url: client.redacted_base_url(),
                auth: client.auth_status().as_str(),
            }),
            None,
        ),
        IndexKind::Local { upload_token, volatile } => (
            None,
            Some(LocalDescription {
                volatile: *volatile,
                upload_token: SecretDescription::new(upload_token.is_some()),
            }),
        ),
        IndexKind::Overlay { .. } => (None, None),
    };
    IndexDescription {
        name: index.name.clone(),
        route: index.route.clone(),
        kind,
        layers,
        uploads,
        volatile_deletes,
        upload_to,
        upstream,
        local,
    }
}

/// A configured index as presented to humans: on the dashboard, in `/+status`, and in discovery.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct IndexDescription {
    pub name: String,
    pub route: String,
    pub kind: &'static str,
    pub layers: Vec<String>,
    pub uploads: bool,
    pub volatile_deletes: bool,
    /// For an overlay: the layer uploads land in, whether or not a token currently enables them.
    pub upload_to: Option<String>,
    pub upstream: Option<UpstreamDescription>,
    pub local: Option<LocalDescription>,
}

/// Mirror status data that excludes credential material.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UpstreamDescription {
    pub url: String,
    pub auth: &'static str,
}

/// Local-store status data that excludes upload-token values.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LocalDescription {
    pub volatile: bool,
    pub upload_token: SecretDescription,
}

/// Redacted secret metadata for status surfaces.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SecretDescription {
    pub configured: bool,
    pub redacted: Option<&'static str>,
}

impl SecretDescription {
    #[must_use]
    pub fn new(configured: bool) -> Self {
        Self {
            configured,
            redacted: configured.then_some("<redacted>"),
        }
    }
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
