//! Assembling an `AppState`: the runtime knobs, their defaults, and the constructors the binary and
//! the tests build one through.

use std::collections::HashMap;
use std::sync::atomic::AtomicU64;
use std::sync::{Arc, Mutex};

use bytes::Bytes;
use peryx_core::LexiconRegistry;
use peryx_storage::blob::BlobStore;
use peryx_storage::meta::MetaStore;

use peryx_index::{Index, IndexKind};

use crate::rate_limit::{DEFAULT_UPSTREAM_CONCURRENCY, RateLimitConfig, RateLimiter, UpstreamLimits};
use peryx_events::metrics::Metrics;
use peryx_events::webhook::WebhookRuntime;
use peryx_search::{PackageSearch, SearchError};

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

use super::app::{AppState, Clock};

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
            drivers: std::array::from_fn(|_| None),
            absolute_prefixes: Vec::new(),
            lexicons: LexiconRegistry::default(),
            openapi: std::sync::Arc::from(STUB_OPENAPI),
        }
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
