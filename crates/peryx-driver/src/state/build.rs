use std::collections::HashMap;
use std::sync::atomic::AtomicU64;
use std::sync::{Arc, Mutex};

use peryx_core::LexiconRegistry;
use peryx_storage::blob::BlobStorage;
use peryx_storage::meta::MetaStore;
use peryx_upstream::UpstreamRouter;

use peryx_index::{Index, IndexKind};

use crate::rate_limit::{DEFAULT_UPSTREAM_CONCURRENCY, RateLimitConfig, RateLimiter, UpstreamLimits};
use peryx_events::metrics::Metrics;
use peryx_events::webhook::WebhookRuntime;
use peryx_search::{SearchError, SearchIndex};

struct StateParts {
    meta: MetaStore,
    blobs: BlobStorage,
    ttl_secs: i64,
    indexes: Vec<Index>,
    clock: Clock,
}

/// Runtime controls applied when building [`AppState`].
pub struct RuntimeOptions<I> {
    pub rate_limit: RateLimitConfig,
    pub upstream_concurrency: I,
    pub upstream_routes: Vec<(String, UpstreamRouter)>,
    pub webhooks: WebhookRuntime,
    /// Byte budget for the transformed-page cache: memory traded against warm-serve speed. Entries
    /// are re-derivable from the cached raw page, so a smaller budget costs hit rate, never
    /// correctness; `0` disables the cache and every warm page pays its transform again.
    pub hot_cache_bytes: u64,
    /// How long past its freshness window a cached page may still answer while the upstream is
    /// unreachable. `0` means without limit: a mirror in front of a flaky upstream can be told to
    /// keep serving whatever it last saw, but that is an operator's explicit choice, not a default.
    pub max_stale_secs: i64,
    /// How many days of daily group-and-source usage buckets to retain; `None` keeps them without
    /// limit. Older buckets expire on the aggregator thread, never on the request path.
    pub usage_retention_days: Option<u32>,
    /// Views that must reach a serial before reads expose it.
    pub required_views: std::sync::Arc<[&'static str]>,
}

/// Keeps transient outages readable while surfacing prolonged stale data.
pub const DEFAULT_MAX_STALE_SECS: i64 = 300;

/// How long a realm token lives when an operator configures no `[auth] token_ttl_secs`.
///
/// One freshness window: long enough for a client transfer, short enough that a
/// revoked ACL takes hold soon after the token that was minted under it expires.
pub const DEFAULT_TOKEN_TTL_SECS: i64 = 300;

/// The transformed-page cache budget when an operator configures none.
///
/// Sized for several transformed index pages at the measured multi-megabyte upper bound.
pub const DEFAULT_HOT_CACHE_BYTES: u64 = 256 * 1024 * 1024;

use super::app::{AppState, Clock};

impl AppState {
    #[must_use]
    pub fn new(meta: MetaStore, blobs: impl Into<BlobStorage>, ttl_secs: i64, indexes: Vec<Index>) -> Self {
        Self::with_clock(meta, blobs, ttl_secs, indexes, Arc::new(system_now))
    }

    #[must_use]
    pub fn with_rate_limits(
        meta: MetaStore,
        blobs: impl Into<BlobStorage>,
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

    #[must_use]
    pub fn with_clock(
        meta: MetaStore,
        blobs: impl Into<BlobStorage>,
        ttl_secs: i64,
        indexes: Vec<Index>,
        clock: Clock,
    ) -> Self {
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

    /// # Errors
    /// Returns an error if the search index cannot be opened.
    pub fn with_search_path(
        meta: MetaStore,
        blobs: impl Into<BlobStorage>,
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

    /// # Errors
    /// Returns an error if the search index cannot be opened.
    pub fn with_search_path_and_rate_limits(
        meta: MetaStore,
        blobs: impl Into<BlobStorage>,
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
                upstream_routes: Vec::new(),
                webhooks: WebhookRuntime::disabled(),
                hot_cache_bytes: DEFAULT_HOT_CACHE_BYTES,
                max_stale_secs: DEFAULT_MAX_STALE_SECS,
                usage_retention_days: None,
                required_views: std::sync::Arc::from(super::derived_views::REQUIRED_VIEWS),
            },
        )
    }

    /// # Errors
    /// Returns an error if the search index cannot be opened.
    pub fn with_search_path_and_runtime<I>(
        meta: MetaStore,
        blobs: impl Into<BlobStorage>,
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
                blobs: blobs.into(),
                ttl_secs,
                indexes,
                clock: Arc::new(system_now),
            },
            SearchIndex::open(search_path)?,
            runtime,
        ))
    }

    #[must_use]
    pub fn with_limits(
        meta: MetaStore,
        blobs: impl Into<BlobStorage>,
        ttl_secs: i64,
        indexes: Vec<Index>,
        clock: Clock,
        rate_limit: RateLimitConfig,
        upstream_concurrency: impl IntoIterator<Item = (String, usize)>,
    ) -> Self {
        Self::with_limits_and_search(
            StateParts {
                meta,
                blobs: blobs.into(),
                ttl_secs,
                indexes,
                clock,
            },
            SearchIndex::in_memory(),
            RuntimeOptions {
                rate_limit,
                upstream_concurrency,
                upstream_routes: Vec::new(),
                webhooks: WebhookRuntime::disabled(),
                hot_cache_bytes: DEFAULT_HOT_CACHE_BYTES,
                max_stale_secs: DEFAULT_MAX_STALE_SECS,
                usage_retention_days: None,
                required_views: std::sync::Arc::from(super::derived_views::REQUIRED_VIEWS),
            },
        )
    }

    #[must_use]
    pub fn with_clock_and_webhooks(
        meta: MetaStore,
        blobs: impl Into<BlobStorage>,
        ttl_secs: i64,
        indexes: Vec<Index>,
        clock: Clock,
        webhooks: WebhookRuntime,
    ) -> Self {
        Self::with_limits_and_search(
            StateParts {
                meta,
                blobs: blobs.into(),
                ttl_secs,
                indexes,
                clock,
            },
            SearchIndex::in_memory(),
            RuntimeOptions {
                rate_limit: RateLimitConfig::default(),
                upstream_concurrency: std::iter::empty(),
                upstream_routes: Vec::new(),
                webhooks,
                hot_cache_bytes: DEFAULT_HOT_CACHE_BYTES,
                max_stale_secs: DEFAULT_MAX_STALE_SECS,
                usage_retention_days: None,
                required_views: std::sync::Arc::from(super::derived_views::REQUIRED_VIEWS),
            },
        )
    }

    fn with_limits_and_search<I>(parts: StateParts, search: SearchIndex, runtime: RuntimeOptions<I>) -> Self
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
            upstream_routes,
            webhooks,
            hot_cache_bytes,
            max_stale_secs,
            usage_retention_days,
            required_views,
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
        let metrics = Metrics::start_durable_or_degraded(meta.analytics(), usage_retention_days, clock.clone());
        let users = crate::users::UserService::new(meta.clone());
        let authorization = crate::authz::AuthorizationService::new(meta.clone());
        let revocations = crate::revocations::RevocationService::new(meta.clone());
        let tokens = crate::tokens::TokenService::new(meta.clone());
        let job_attempts = crate::jobs::JobAttemptControl::new(meta.clone());
        Self {
            serving: std::sync::Arc::new(super::app::ServingState {
                meta,
                users,
                authorization,
                revocations,
                tokens,
                job_attempts,
                blobs,
                ttl_secs,
                max_stale_secs,
                clock,
                requests: AtomicU64::new(0),
                read_only: false,
                availability: super::app::AvailabilityState { distributed: None },
                route_resolver: peryx_index::RouteResolver::new(&indexes),
                indexes,
                cache: peryx_index::ServingCache::new(hot_cache_bytes, ttl_secs),
                downloads: crate::download::DownloadRegistry::default(),
                metrics,
                search,
                required_views,
                rate_limits: RateLimiter::new(rate_limit),
                upstream_limits: UpstreamLimits::new(upstream_limits.clone()),
                metadata_upstream_limits: UpstreamLimits::new(upstream_limits),
                upstream_routes: upstream_routes.into_iter().collect(),
                webhooks,
                signer: None,
                token_ttl_secs: DEFAULT_TOKEN_TTL_SECS,
                plugin_services: HashMap::new(),
                ldap_logins: HashMap::new(),
                retention_gates: crate::retention::RetentionGates::new(RETENTION_PLANS_PER_REPOSITORY),
                oidc_logins: HashMap::new(),
                session_sealer: None,
            }),
            drivers: crate::DriverSet::default(),
            protocols: HashMap::new(),
            idle_reclaimers: HashMap::new(),
            intent_finalizers: HashMap::new(),
            cache_refreshers: HashMap::new(),
            replicated_apply_drivers: HashMap::new(),
            mirror_drivers: HashMap::new(),
            rate_limit_principals: HashMap::new(),
            client_discovery: HashMap::new(),
            absolute_prefixes: Vec::new(),
            lexicons: LexiconRegistry::default(),
            openapi: std::sync::Arc::from(STUB_OPENAPI),
            prometheus: Mutex::new(Vec::new()),
            http_routes: Vec::new(),
        }
    }
}

fn system_now() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| i64::try_from(d.as_secs()).unwrap_or(i64::MAX))
}

/// How many retention-plan previews one repository may compute at once. A preview is a full metadata
/// scan, so a small bound keeps a burst on one repository from starving the others while still letting a
/// dry-run and an export overlap.
const RETENTION_PLANS_PER_REPOSITORY: usize = 2;

/// The minimal `OpenAPI` document a state serves until the binary installs the assembled one. It names
/// no ecosystem; the real per-ecosystem paths are merged in by the binary at startup.
const STUB_OPENAPI: &str = r#"{"openapi":"3.1.0","info":{"title":"peryx","version":"0"},"paths":{}}"#;

#[cfg(test)]
#[path = "../../tests/unit/state/build/tests.rs"]
mod tests;
