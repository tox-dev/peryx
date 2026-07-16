//! The shared application state and its request-time index routing.

use std::collections::HashMap;
use std::sync::atomic::AtomicU64;
use std::sync::{Arc, Mutex};

use peryx_core::{Ecosystem, LexiconRegistry};
use peryx_storage::blob::BlobStore;
use peryx_storage::meta::MetaStore;
use peryx_upstream::UpstreamRouter;

use peryx_index::Index;

use super::describe::{IndexDescription, describe_indexes};
use crate::rate_limit::{RateLimiter, UpstreamLimits};
use peryx_events::metrics::Metrics;
use peryx_events::webhook::WebhookRuntime;
use peryx_search::PackageSearch;

/// A source of the current unix time, injectable so cache-freshness logic is deterministic in
/// tests.
pub type Clock = Arc<dyn Fn() -> i64 + Send + Sync>;

/// A process-level component that contributes Prometheus text exposition.
pub trait PrometheusSource: Send + Sync {
    /// Append complete metric families to `body`.
    fn write_metrics(&self, body: &mut String);
}

/// Everything a serving handler needs, and nothing about *which* ecosystems are installed.
///
/// An ecosystem driver receives an `Arc<ServingState>`, so it can read the stores, the caches and the
/// configured indexes and spawn background work over them — but it holds no driver registry, so it
/// cannot reach another ecosystem's driver or enumerate them. The registry lives one level up on
/// [`AppState`], which the router and rate limiter hold; a driver reaching for it is a compile error,
/// not a convention.
pub struct ServingState {
    pub meta: MetaStore,
    pub blobs: BlobStore,
    /// Fallback freshness for cached simple pages, in seconds: applies only when upstream's
    /// `Cache-Control` granted no usable lifetime.
    pub ttl_secs: i64,
    /// The bound on stale-on-error serving; see [`RuntimeOptions::max_stale_secs`].
    pub max_stale_secs: i64,
    pub clock: Clock,
    pub requests: AtomicU64,
    /// Whether this process serves as a replica and rejects client mutations.
    pub read_only: bool,
    pub indexes: Vec<Index>,
    /// The role engine's caches for a cached (proxy) index: the single-flight map, the transformed-page
    /// cache, the negative cache, and the mutation epoch that retires them.
    pub cache: peryx_index::ServingCache,
    /// One live download per blob digest: concurrent cold requests for the same file all tail the
    /// one upstream transfer as it lands instead of waiting for it to finish.
    pub downloads: Mutex<HashMap<String, crate::download::DownloadHandle>>,
    /// Off-thread usage aggregation: index → project → file counters for the dashboard.
    pub metrics: Metrics,
    /// Derived package search index, refreshed from storage when the mutation epoch advances.
    pub search: PackageSearch,
    /// Per-client HTTP request limits. The bucket cache has a fixed capacity.
    pub rate_limits: RateLimiter,
    /// Per-cached-index upstream fetch gates, keyed by configured index name.
    pub upstream_limits: UpstreamLimits,
    /// Multi-source routes keyed by cached index name. Legacy cached indexes are absent.
    pub upstream_routes: HashMap<String, UpstreamRouter>,
    /// Signed webhook delivery runtime.
    pub webhooks: WebhookRuntime,
    /// The token realm's signing key, or `None` when no signing key is configured. Without it an
    /// ecosystem's token endpoint cannot mint a JWT, so an OCI index falls back to Basic-only auth and
    /// never challenges with the Bearer scheme.
    pub signer: Option<peryx_identity::Signer>,
    /// How long a token the realm mints stays valid, in seconds.
    pub token_ttl_secs: i64,
}

/// The whole process state: the serving data every handler needs, plus the driver registry only the
/// router and rate limiter reach.
///
/// Shared as `Arc<AppState>`; it [`Deref`](std::ops::Deref)s to [`ServingState`], so `app.meta` and
/// the rest read through unchanged.
pub struct AppState {
    /// The serving data, separately `Arc`-shared so a driver receives it without the registry and
    /// background tasks can own a clone.
    pub serving: Arc<ServingState>,
    /// The ecosystem serving drivers, one slot per [`Ecosystem`]. A request is dispatched to the driver
    /// of the index it resolved to (or of the absolute prefix it fell under), so several ecosystems
    /// coexist; a slot stays `None` for an ecosystem nobody installed. Each driver's
    /// [`mount`](crate::serving::EcosystemDriver::mount) tells the router and rate limiter how to reach
    /// it, so neither names an ecosystem.
    pub(super) drivers: [Option<Arc<dyn crate::serving::EcosystemDriver>>; Ecosystem::COUNT],
    /// The absolute top-level prefixes of the [`Absolute`](crate::serving::RouteMount::Absolute)-mount
    /// drivers, each paired with its slot, precomputed at registration. The rate limiter classifies a
    /// request through this on every call, so it must not walk every driver and dispatch `mount()`
    /// dynamically: this list holds only the few absolute prefixes, whatever the ecosystem count.
    pub(super) absolute_prefixes: Vec<(&'static str, usize)>,
    /// Each ecosystem's user-facing vocabulary, registered by its driver at install time so surfaces
    /// localize a label by an index's ecosystem without the neutral core naming any ecosystem's words.
    pub(super) lexicons: LexiconRegistry,
    /// The `OpenAPI` document served at `/api-docs/openapi.json`. The binary assembles it from each
    /// ecosystem driver's paths at startup and installs it here, so this neutral crate carries no
    /// format-specific API description, only a minimal stub until the binary sets the real one.
    pub(super) openapi: std::sync::Arc<str>,
    pub(super) prometheus: Mutex<Vec<Arc<dyn PrometheusSource>>>,
}

impl AppState {
    /// Register process metrics that are not owned by an ecosystem driver.
    pub fn register_prometheus(&self, source: Arc<dyn PrometheusSource>) {
        self.prometheus
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .push(source);
    }

    /// Append every registered process metric family to `body`.
    pub fn write_process_metrics(&self, body: &mut String) {
        for source in self
            .prometheus
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .iter()
        {
            source.write_metrics(body);
        }
    }
}

impl std::ops::Deref for AppState {
    type Target = ServingState;

    fn deref(&self) -> &ServingState {
        &self.serving
    }
}

impl std::ops::DerefMut for AppState {
    /// Mutable access to the serving state, sound only while its `Arc` is uniquely owned — during
    /// build and install, before any handler holds a clone. The router shares the state afterwards,
    /// so a mutation then is a bug, and this panics rather than silently splitting the state.
    fn deref_mut(&mut self) -> &mut ServingState {
        Arc::get_mut(&mut self.serving).expect("serving state is mutated only before it is served")
    }
}

impl ServingState {
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

    /// Describe every configured index for presentation: kind name, virtual-index layer names, upload
    /// access, and delete policy. Shared by `/+status` and the web UI.
    #[must_use]
    pub fn describe_indexes(&self) -> Vec<IndexDescription> {
        describe_indexes(&self.indexes)
    }
}

/// Signed webhook delivery borrows exactly three things from the process — the configured targets,
/// the queue's store, and the clock — and reaches them through this trait rather than the whole state.
impl peryx_events::webhook::WebhookHost for ServingState {
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
