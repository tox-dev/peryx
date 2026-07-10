//! The shared application state and its request-time index routing.

use std::collections::HashMap;
use std::sync::atomic::AtomicU64;
use std::sync::{Arc, Mutex};

use bytes::Bytes;
use peryx_core::{Ecosystem, LexiconRegistry};
use peryx_storage::blob::BlobStore;
use peryx_storage::meta::MetaStore;

use peryx_index::{Index, IndexKind};

use super::describe::{IndexDescription, describe_indexes};
use crate::rate_limit::{DEFAULT_UPSTREAM_CONCURRENCY, RateLimitConfig, RateLimiter, UpstreamLimits};
use peryx_events::metrics::Metrics;
use peryx_events::webhook::WebhookRuntime;
use peryx_search::{IndexerCtx, PackageSearch, SearchCtx, SearchError};

/// A source of the current unix time, injectable so cache-freshness logic is deterministic in
/// tests.
pub type Clock = Arc<dyn Fn() -> i64 + Send + Sync>;

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
    /// Byte budget for the transformed-page cache: memory traded against warm-serve speed. Entries
    /// are re-derivable from the cached raw page, so a smaller budget costs hit rate, never
    /// correctness; `0` disables the cache and every warm page pays its transform again.
    pub hot_cache_bytes: u64,
    /// How long past its freshness window a cached page may still answer while the upstream is
    /// unreachable. `0` means without limit: a mirror in front of a flaky upstream can be told to
    /// keep serving whatever it last saw, but that is an operator's explicit choice, not a default.
    pub max_stale_secs: i64,
}

/// How long an outage may be papered over with a stale page, when an operator configures no bound.
///
/// One further freshness window: long enough to ride out an upstream blip or a redeploy, short enough
/// that a lasting outage surfaces as an error rather than as quietly ancient data.
pub const DEFAULT_MAX_STALE_SECS: i64 = 300;

/// The transformed-page cache budget when an operator configures none.
///
/// Sized for the working set of a busy `PyPI` index, whose transformed pages are the large ones
/// (`boto3` and `numpy` run to megabytes of JSON). Today the `PyPI` driver is the only ecosystem that
/// populates this cache; when a second one does, this becomes a budget per ecosystem, keyed like the
/// lexicon and serving registries already are.
pub const DEFAULT_HOT_CACHE_BYTES: u64 = 256 * 1024 * 1024;

/// Everything a request handler needs. Shared as `Arc<AppState>`.
pub struct AppState {
    pub meta: MetaStore,
    pub blobs: BlobStore,
    /// Fallback freshness for cached simple pages, in seconds: applies only when upstream's
    /// `Cache-Control` granted no usable lifetime.
    pub ttl_secs: i64,
    /// The bound on stale-on-error serving; see [`RuntimeOptions::max_stale_secs`].
    pub max_stale_secs: i64,
    pub clock: Clock,
    pub requests: AtomicU64,
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
    /// Per-cached-index upstream fetch gates, keyed by configured index name.
    pub upstream_limits: UpstreamLimits,
    /// Signed webhook delivery runtime.
    pub webhooks: WebhookRuntime,
    /// The route-mounted serving drivers, one slot per [`Ecosystem`]. A request is dispatched to the
    /// driver of the index it resolved to, so several route-mounted ecosystems coexist; a slot stays
    /// `None` for an ecosystem nobody installed, and for one whose wire protocol owns a top-level
    /// prefix instead (see `namespaces`).
    serving: [Option<Arc<dyn crate::serving::EcosystemServing>>; Ecosystem::COUNT],
    /// Ecosystems whose wire protocol owns top-level path prefixes and resolves indexes itself (`OCI`'s
    /// `/v2/`). Empty until a namespace ecosystem is installed; the router and rate limiter read the
    /// prefixes each declares so neither names an ecosystem.
    pub namespaces: Vec<Arc<dyn crate::serving::NamespaceServing>>,
    /// Each ecosystem's user-facing vocabulary, registered by its driver at install time so surfaces
    /// localize a label by an index's ecosystem without the neutral core naming any ecosystem's words.
    lexicons: LexiconRegistry,
    /// The `OpenAPI` document served at `/api-docs/openapi.json`. The binary assembles it from each
    /// ecosystem driver's paths at startup and installs it here, so this neutral crate carries no
    /// format-specific API description, only a minimal stub until the binary sets the real one.
    openapi: std::sync::Arc<str>,
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
                hot_cache_bytes: DEFAULT_HOT_CACHE_BYTES,
                max_stale_secs: DEFAULT_MAX_STALE_SECS,
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
                hot_cache_bytes: DEFAULT_HOT_CACHE_BYTES,
                max_stale_secs: DEFAULT_MAX_STALE_SECS,
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
                hot_cache_bytes: DEFAULT_HOT_CACHE_BYTES,
                max_stale_secs: DEFAULT_MAX_STALE_SECS,
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
            hot_cache_bytes,
            max_stale_secs,
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
            max_stale_secs,
            clock,
            requests: AtomicU64::new(0),
            indexes,
            inflight: Mutex::new(HashMap::new()),
            downloads: Mutex::new(HashMap::new()),
            hot: moka::sync::Cache::builder()
                .max_capacity(hot_cache_bytes)
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
            serving: std::array::from_fn(|_| None),
            namespaces: Vec::new(),
            lexicons: LexiconRegistry::default(),
            openapi: std::sync::Arc::from(STUB_OPENAPI),
        }
    }

    /// Register an ecosystem's user-facing vocabulary; its driver calls this at install time.
    pub fn register_lexicon(&mut self, ecosystem: Ecosystem, lexicon: &'static peryx_core::Lexicon) {
        self.lexicons.register(ecosystem, lexicon);
    }

    /// The user-facing vocabulary for `ecosystem`, or peryx's neutral words if none is registered.
    #[must_use]
    pub fn lexicon(&self, ecosystem: Ecosystem) -> &'static peryx_core::Lexicon {
        self.lexicons.get(ecosystem)
    }

    /// The stores and indexes an ecosystem's search indexer walks.
    #[must_use]
    pub fn indexer_ctx(&self) -> IndexerCtx<'_> {
        IndexerCtx {
            indexes: &self.indexes,
            meta: &self.meta,
            blobs: &self.blobs,
        }
    }

    /// What one search request reads from this state: the indexers' stores, the mutation epoch that
    /// decides whether the derived index is stale, and the registered vocabularies.
    #[must_use]
    pub fn search_ctx(&self) -> SearchCtx<'_> {
        SearchCtx {
            indexer: self.indexer_ctx(),
            epoch: self.epoch.load(std::sync::atomic::Ordering::Relaxed),
            lexicons: &self.lexicons,
        }
    }

    /// Register a route-mounted ecosystem's serving driver and its search indexer. Each driver's own
    /// [`ecosystem`](crate::serving::EcosystemServing::ecosystem) picks its slot, so installing one
    /// never displaces another.
    pub fn register_ecosystem(
        &mut self,
        serving: Arc<dyn crate::serving::EcosystemServing>,
        indexer: Arc<dyn peryx_search::PackageIndexer>,
    ) {
        let slot = serving.ecosystem().slot();
        self.serving[slot] = Some(serving);
        self.search.add_indexer(indexer);
    }

    /// The route-mounted driver serving `ecosystem`, or `None` when none is installed for it.
    #[must_use]
    pub fn serving_for(&self, ecosystem: Ecosystem) -> Option<&Arc<dyn crate::serving::EcosystemServing>> {
        self.serving[ecosystem.slot()].as_ref()
    }

    /// The route-mounted driver that would serve `path`, found by resolving the index it addresses.
    ///
    /// `path` is a request URI path, so it carries a leading slash; index routes do not.
    #[must_use]
    pub fn serving_for_path(&self, path: &str) -> Option<&Arc<dyn crate::serving::EcosystemServing>> {
        let (position, _) = self.resolve_position(path.trim_start_matches('/'))?;
        self.serving_for(self.index_at(position).ecosystem)
    }

    /// Every installed route-mounted driver, in ecosystem declaration order.
    pub fn servings(&self) -> impl Iterator<Item = &Arc<dyn crate::serving::EcosystemServing>> {
        self.serving.iter().flatten()
    }

    /// Whether any ecosystem driver at all has been wired in. A process with none serves `503` rather
    /// than quietly answering nothing.
    #[must_use]
    pub fn has_any_driver(&self) -> bool {
        self.serving.iter().any(Option::is_some) || !self.namespaces.is_empty()
    }

    /// Add another ecosystem's search indexer, composing with any already installed. An ecosystem
    /// whose serving lives in its own slot (OCI) uses this to make its packages searchable too.
    pub fn add_search_indexer(&mut self, indexer: Arc<dyn peryx_search::PackageIndexer>) {
        self.search.add_indexer(indexer);
    }

    /// Wire in a namespace ecosystem's serving driver. The binary calls this once at startup for each
    /// namespace ecosystem (OCI's `/v2/` registry) whose indexes are configured.
    pub fn register_namespace(&mut self, driver: Arc<dyn crate::serving::NamespaceServing>) {
        self.namespaces.push(driver);
    }

    /// The namespace driver that owns `path`, or `None` when the path falls under no namespace (the
    /// per-index router handles it). The first registered driver whose prefix matches wins.
    #[must_use]
    pub fn namespace_for_path(&self, path: &str) -> Option<&Arc<dyn crate::serving::NamespaceServing>> {
        self.namespaces
            .iter()
            .find(|driver| driver.prefixes().iter().any(|prefix| path.starts_with(prefix)))
    }

    /// The namespace driver serving `ecosystem`, so `/+api` renders that index's setup through it.
    #[must_use]
    pub fn namespace_for_ecosystem(&self, ecosystem: &str) -> Option<&Arc<dyn crate::serving::NamespaceServing>> {
        self.namespaces
            .iter()
            .find(|driver| driver.ecosystem().as_str() == ecosystem)
    }

    /// Install the assembled `OpenAPI` document the `/api-docs/openapi.json` endpoint serves. The
    /// binary builds it from each ecosystem driver's paths and calls this once at startup.
    pub fn set_openapi(&mut self, openapi: impl Into<Arc<str>>) {
        self.openapi = openapi.into();
    }

    /// The installed `OpenAPI` document served at `/api-docs/openapi.json`.
    #[must_use]
    pub fn openapi(&self) -> &str {
        &self.openapi
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
        peryx_index::resolve_position(&self.indexes, path)
    }

    /// The index at position `pos` (a virtual-index layer or upload target).
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

    /// The hot-cache key for one representation of a page as served on `route` right now.
    ///
    /// `variant` separates the representations a page has: the same project answers with PEP 691 JSON,
    /// PEP 503 HTML, or the legacy release JSON, and they are different bytes. The epoch is what makes
    /// a mutation invalidate them all at once, since every mutation bumps it.
    #[must_use]
    pub fn hot_key(&self, route: &str, project: &str, variant: &str) -> String {
        let epoch = self.epoch.load(std::sync::atomic::Ordering::Relaxed);
        format!("{route}\u{0}{project}\u{0}{variant}\u{0}{epoch}")
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

    /// Describe every configured index for presentation: kind name, virtual-index layer names, upload
    /// access, and delete policy. Shared by `/+status` and the web UI.
    #[must_use]
    pub fn describe_indexes(&self) -> Vec<IndexDescription> {
        describe_indexes(&self.indexes)
    }
}

/// Signed webhook delivery borrows exactly three things from the process — the configured targets,
/// the queue's store, and the clock — and reaches them through this trait rather than the whole state.
impl peryx_events::webhook::WebhookHost for AppState {
    fn webhooks(&self) -> &WebhookRuntime {
        &self.webhooks
    }

    fn meta(&self) -> &MetaStore {
        &self.meta
    }

    fn now(&self) -> i64 {
        (self.clock)()
    }
}

fn system_now() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| i64::try_from(d.as_secs()).unwrap_or(i64::MAX))
}

/// The minimal `OpenAPI` document a state serves until the binary installs the assembled one. It names
/// no ecosystem; the real per-ecosystem paths are merged in by the binary at startup.
const STUB_OPENAPI: &str = r#"{"openapi":"3.1.0","info":{"title":"peryx","version":"0"},"paths":{}}"#;
