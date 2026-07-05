//! Shared application state and index routing.

use std::collections::HashMap;
use std::sync::atomic::AtomicU64;
use std::sync::{Arc, Mutex};

use bytes::Bytes;
use velodex_format::Ecosystem;
use velodex_policy::Policy;
use velodex_storage::blob::BlobStore;
use velodex_storage::meta::MetaStore;
use velodex_upstream::UpstreamClient;

use crate::metrics::Metrics;
use crate::rate_limit::{DEFAULT_UPSTREAM_CONCURRENCY, RateLimitConfig, RateLimiter, UpstreamLimits};
use crate::search::{PackageSearch, SearchError};
use crate::webhook::WebhookRuntime;

/// A source of the current unix time, injectable so cache-freshness logic is deterministic in
/// tests.
pub type Clock = Arc<dyn Fn() -> i64 + Send + Sync>;

/// One resolved index. `layers`/`upload` in a virtual index are indices into [`AppState::indexes`], so
/// resolution is a plain vector walk with no name lookups at request time.
#[derive(Debug)]
pub struct Index {
    pub name: String,
    pub route: String,
    pub ecosystem: Ecosystem,
    pub kind: IndexKind,
    pub policy: Policy,
}

/// The runtime shape of an index by role: a cached index owns its upstream client, a hosted store its
/// upload policy, a virtual index the resolved positions of its members and upload target.
#[derive(Debug)]
pub enum IndexKind {
    Cached {
        client: UpstreamClient,
        offline: bool,
    },
    Hosted {
        upload_token: Option<String>,
        volatile: bool,
    },
    Virtual {
        layers: Vec<usize>,
        upload: Option<usize>,
    },
}

struct StateParts {
    meta: MetaStore,
    blobs: BlobStore,
    ttl_secs: i64,
    indexes: Vec<Index>,
    clock: Clock,
}

/// Runtime controls applied when building [`AppState`].
pub struct RuntimeOptions<I> {
    pub rate_limit: RateLimitConfig,
    pub upstream_concurrency: I,
    pub webhooks: WebhookRuntime,
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
    pub downloads: Mutex<HashMap<String, crate::download::DownloadHandle>>,
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
    /// Per-client HTTP request limits. The bucket cache has a fixed capacity.
    pub rate_limits: RateLimiter,
    /// Per-mirror upstream fetch gates, keyed by configured index name.
    pub upstream_limits: UpstreamLimits,
    /// Signed webhook delivery runtime.
    pub webhooks: WebhookRuntime,
    /// The ecosystem serving driver requests are dispatched to. One per process today (`PyPI`); a
    /// registry keyed by an index's ecosystem once a second ecosystem lands.
    pub serving: Arc<dyn crate::serving::EcosystemServing>,
}

impl AppState {
    /// Build the state with a system clock.
    #[must_use]
    pub fn new(meta: MetaStore, blobs: BlobStore, ttl_secs: i64, indexes: Vec<Index>) -> Self {
        Self::with_clock(meta, blobs, ttl_secs, indexes, Arc::new(system_now))
    }

    /// Build the state with system time plus hosted abuse-control settings.
    #[must_use]
    pub fn with_rate_limits(
        meta: MetaStore,
        blobs: BlobStore,
        ttl_secs: i64,
        indexes: Vec<Index>,
        rate_limit: RateLimitConfig,
        upstream_concurrency: impl IntoIterator<Item = (String, usize)>,
    ) -> Self {
        Self::with_limits(
            meta,
            blobs,
            ttl_secs,
            indexes,
            Arc::new(system_now),
            rate_limit,
            upstream_concurrency,
        )
    }

    /// Build the state with an injected clock.
    #[must_use]
    pub fn with_clock(meta: MetaStore, blobs: BlobStore, ttl_secs: i64, indexes: Vec<Index>, clock: Clock) -> Self {
        Self::with_limits(
            meta,
            blobs,
            ttl_secs,
            indexes,
            clock,
            RateLimitConfig::default(),
            std::iter::empty(),
        )
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
        Self::with_search_path_and_rate_limits(
            meta,
            blobs,
            ttl_secs,
            indexes,
            search_path,
            RateLimitConfig::default(),
            std::iter::empty(),
        )
    }

    /// Build the state with an on-disk search index and hosted abuse-control settings.
    ///
    /// # Errors
    /// Returns an error if the search index cannot be opened.
    pub fn with_search_path_and_rate_limits(
        meta: MetaStore,
        blobs: BlobStore,
        ttl_secs: i64,
        indexes: Vec<Index>,
        search_path: impl AsRef<std::path::Path>,
        rate_limit: RateLimitConfig,
        upstream_concurrency: impl IntoIterator<Item = (String, usize)>,
    ) -> Result<Self, SearchError> {
        Self::with_search_path_and_runtime(
            meta,
            blobs,
            ttl_secs,
            indexes,
            search_path,
            RuntimeOptions {
                rate_limit,
                upstream_concurrency,
                webhooks: WebhookRuntime::disabled(),
            },
        )
    }

    /// Build the state with an on-disk search index and runtime controls.
    ///
    /// # Errors
    /// Returns an error if the search index cannot be opened.
    pub fn with_search_path_and_runtime<I>(
        meta: MetaStore,
        blobs: BlobStore,
        ttl_secs: i64,
        indexes: Vec<Index>,
        search_path: impl AsRef<std::path::Path>,
        runtime: RuntimeOptions<I>,
    ) -> Result<Self, SearchError>
    where
        I: IntoIterator<Item = (String, usize)>,
    {
        Ok(Self::with_limits_and_search(
            StateParts {
                meta,
                blobs,
                ttl_secs,
                indexes,
                clock: Arc::new(system_now),
            },
            PackageSearch::open(search_path)?,
            runtime,
        ))
    }

    /// Build the state with an injected clock plus hosted abuse-control settings.
    #[must_use]
    pub fn with_limits(
        meta: MetaStore,
        blobs: BlobStore,
        ttl_secs: i64,
        indexes: Vec<Index>,
        clock: Clock,
        rate_limit: RateLimitConfig,
        upstream_concurrency: impl IntoIterator<Item = (String, usize)>,
    ) -> Self {
        Self::with_limits_and_search(
            StateParts {
                meta,
                blobs,
                ttl_secs,
                indexes,
                clock,
            },
            PackageSearch::in_memory(),
            RuntimeOptions {
                rate_limit,
                upstream_concurrency,
                webhooks: WebhookRuntime::disabled(),
            },
        )
    }

    /// Build the state with an injected clock and webhook runtime.
    #[must_use]
    pub fn with_clock_and_webhooks(
        meta: MetaStore,
        blobs: BlobStore,
        ttl_secs: i64,
        indexes: Vec<Index>,
        clock: Clock,
        webhooks: WebhookRuntime,
    ) -> Self {
        Self::with_limits_and_search(
            StateParts {
                meta,
                blobs,
                ttl_secs,
                indexes,
                clock,
            },
            PackageSearch::in_memory(),
            RuntimeOptions {
                rate_limit: RateLimitConfig::default(),
                upstream_concurrency: std::iter::empty(),
                webhooks,
            },
        )
    }

    fn with_limits_and_search<I>(parts: StateParts, search: PackageSearch, runtime: RuntimeOptions<I>) -> Self
    where
        I: IntoIterator<Item = (String, usize)>,
    {
        let StateParts {
            meta,
            blobs,
            ttl_secs,
            indexes,
            clock,
        } = parts;
        let RuntimeOptions {
            rate_limit,
            upstream_concurrency,
            webhooks,
        } = runtime;
        let configured: HashMap<_, _> = upstream_concurrency.into_iter().collect();
        let upstream_limits = indexes
            .iter()
            .filter_map(|index| match &index.kind {
                IndexKind::Cached { .. } => Some((
                    index.name.clone(),
                    configured
                        .get(&index.name)
                        .copied()
                        .unwrap_or(DEFAULT_UPSTREAM_CONCURRENCY),
                )),
                IndexKind::Hosted { .. } | IndexKind::Virtual { .. } => None,
            })
            .collect::<Vec<_>>();
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
            rate_limits: RateLimiter::new(rate_limit),
            upstream_limits: UpstreamLimits::new(upstream_limits),
            webhooks,
            serving: default_serving(),
        }
    }

    /// Wire in the ecosystem serving driver and its search indexer. The binary calls this once at
    /// startup with the configured ecosystem's implementations; a state built without it serves the
    /// neutral defaults ([`UnconfiguredServing`](crate::serving::UnconfiguredServing) and
    /// [`EmptyIndexer`](crate::search::EmptyIndexer)).
    pub fn set_ecosystem(
        &mut self,
        serving: Arc<dyn crate::serving::EcosystemServing>,
        indexer: Arc<dyn crate::search::PackageIndexer>,
    ) {
        self.serving = serving;
        self.search.set_indexer(indexer);
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

#[must_use]
pub fn describe_index(indexes: &[Index], position: usize) -> IndexDescription {
    let index = &indexes[position];
    let (kind, layers, uploads, volatile_deletes, upload_to) = match &index.kind {
        IndexKind::Cached { .. } => ("cached", Vec::new(), false, false, None),
        IndexKind::Hosted { upload_token, volatile } => (
            "hosted",
            Vec::new(),
            upload_token.is_some(),
            upload_token.is_some() && *volatile,
            None,
        ),
        IndexKind::Virtual { layers, upload } => {
            let names = layers.iter().map(|&pos| indexes[pos].name.clone()).collect();
            let uploads = upload.is_some_and(|pos| {
                matches!(
                    &indexes[pos].kind,
                    IndexKind::Hosted {
                        upload_token: Some(_),
                        ..
                    }
                )
            });
            let volatile_deletes = upload.is_some_and(|pos| {
                matches!(
                    &indexes[pos].kind,
                    IndexKind::Hosted {
                        upload_token: Some(_),
                        volatile: true,
                    }
                )
            });
            let upload_to = upload.map(|pos| indexes[pos].name.clone());
            ("virtual", names, uploads, volatile_deletes, upload_to)
        }
    };
    let (upstream, hosted) = match &index.kind {
        IndexKind::Cached { client, offline } => (
            Some(UpstreamDescription {
                url: client.redacted_base_url(),
                auth: client.auth_status().as_str(),
                offline: *offline,
            }),
            None,
        ),
        IndexKind::Hosted { upload_token, volatile } => (
            None,
            Some(HostedDescription {
                volatile: *volatile,
                upload_token: SecretDescription::new(upload_token.is_some()),
            }),
        ),
        IndexKind::Virtual { .. } => (None, None),
    };
    IndexDescription {
        name: index.name.clone(),
        route: index.route.clone(),
        ecosystem: index.ecosystem.as_str(),
        kind,
        layers,
        uploads,
        volatile_deletes,
        upload_to,
        upstream,
        hosted,
    }
}

/// A configured index as presented to humans: on the dashboard, in `/+status`, and in discovery.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct IndexDescription {
    pub name: String,
    pub route: String,
    pub ecosystem: &'static str,
    pub kind: &'static str,
    pub layers: Vec<String>,
    pub uploads: bool,
    pub volatile_deletes: bool,
    /// For an overlay: the layer uploads land in, whether or not a token currently enables them.
    pub upload_to: Option<String>,
    pub upstream: Option<UpstreamDescription>,
    pub hosted: Option<HostedDescription>,
}

/// Mirror status data that excludes credential material.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UpstreamDescription {
    pub url: String,
    pub auth: &'static str,
    pub offline: bool,
}

/// Local-store status data that excludes upload-token values.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HostedDescription {
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

fn default_serving() -> Arc<dyn crate::serving::EcosystemServing> {
    Arc::new(crate::serving::UnconfiguredServing)
}
