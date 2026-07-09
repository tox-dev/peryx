//! The ecosystem serving interface.
//!
//! The router is ecosystem-neutral: it resolves a request to a configured index and hands the request
//! to that index's ecosystem driver. Each ecosystem implements [`EcosystemServing`] to serve its own
//! wire protocol (`PyPI`'s Simple API today; an `OCI` `/v2/` or npm registry later). The driver is held on
//! [`AppState`] and dispatched dynamically once per request, so adding an ecosystem is a new driver,
//! not a change to the router.

use std::sync::Arc;

use async_trait::async_trait;
use axum::extract::{Multipart, Request};
use axum::http::{HeaderMap, StatusCode, Uri};
use axum::response::{IntoResponse, Response};
use peryx_format::Ecosystem;

use crate::state::AppState;

/// An ecosystem whose wire protocol owns fixed top-level path prefixes and resolves indexes itself,
/// rather than being reached through peryx's per-index route prefix.
///
/// `OCI`'s distribution spec routes by its own scheme (`/v2/<name>/(manifests|blobs|tags|...)/...`) and
/// its writes need the raw request body, so a namespace driver takes the whole request and dispatches
/// internally. A driver declares the prefixes it owns so the router and rate limiter reach it without
/// naming any ecosystem, and the ecosystem it serves so `/+api` renders its indexes' setup.
#[async_trait]
pub trait NamespaceServing: Send + Sync {
    /// The ecosystem this driver serves.
    fn ecosystem(&self) -> Ecosystem;

    /// The absolute top-level path prefixes this driver owns (`OCI`'s `["/v2/"]`). The router mounts a
    /// catch-all under each; the rate limiter and dispatcher match a request path against them.
    fn prefixes(&self) -> &'static [&'static str];

    /// Serve a request whose path fell under one of [`prefixes`](Self::prefixes).
    async fn serve(&self, state: Arc<AppState>, request: Request) -> Response;

    /// The rate-limit class of a GET under one of this driver's prefixes. A blob pull is a large
    /// artifact download; everything else (manifests, tags, referrers, the layer browser) is a listing.
    fn classify_route(&self, path: &str) -> crate::rate_limit::RouteClass;

    /// The `GET /+api` entry for one of this ecosystem's indexes: its endpoint, capabilities, and
    /// client setup snippet. [`crate::discovery::minimal_entry`] renders the identity-only fallback.
    fn discover_index(
        &self,
        index: crate::state::IndexDescription,
        base: Option<&crate::discovery::BaseUrl>,
    ) -> serde_json::Value;
}

/// The outcome of one background refresh sweep: how many cached pages a driver revalidated and how
/// many it found changed upstream.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct RefreshSweep {
    pub checked: usize,
    pub changed: usize,
}

/// How one ecosystem serves requests routed to an index of its kind through peryx's per-index route
/// prefix (`PyPI`'s Simple API).
///
/// Contrast [`NamespaceServing`], whose wire protocol owns its own top-level path space.
#[async_trait]
pub trait EcosystemServing: Send + Sync {
    /// The ecosystem this driver serves.
    fn ecosystem(&self) -> Ecosystem;

    /// Serve a GET for an ecosystem wire-protocol path (index listing, project detail, file, archive
    /// inspection). The neutral router has already resolved the request to index `position`, with
    /// `rest` the sub-path after the index route; peryx's own `+api`/`+search` routes are handled
    /// before this and never reach a driver.
    async fn get(&self, state: Arc<AppState>, position: usize, rest: String, uri: Uri, headers: HeaderMap) -> Response;

    /// Serve a POST (publish/upload).
    async fn post(&self, state: Arc<AppState>, path: String, headers: HeaderMap, multipart: Multipart) -> Response;

    /// Serve a PUT (yank, restore, promote).
    async fn put(&self, state: Arc<AppState>, uri: Uri, headers: HeaderMap) -> Response;

    /// Serve a DELETE (remove or un-yank).
    async fn delete(&self, state: Arc<AppState>, uri: Uri, headers: HeaderMap) -> Response;

    /// The `GET /+api` entry for one index of this ecosystem: its wire-protocol endpoints,
    /// capabilities, and copyable client configuration. The neutral handler wraps the returned entries
    /// from every ecosystem into one discovery document.
    fn discover_index(
        &self,
        index: crate::state::IndexDescription,
        base: Option<&crate::discovery::BaseUrl>,
    ) -> serde_json::Value;

    /// The rate-limit class of a GET inside an index's namespace (a project listing, a metadata
    /// sibling, or an artifact), which depends on this ecosystem's URL scheme. Writes and peryx's
    /// own service endpoints are classified before this by
    /// [`service_route_class`](crate::rate_limit::service_route_class).
    fn classify_route(&self, path: &str) -> crate::rate_limit::RouteClass;

    /// The ecosystem-specific counter families this driver publishes, so the neutral render layer
    /// exposes and scopes them without knowing any ecosystem's vocabulary. Empty by default; a
    /// driver declares its own (`PyPI`'s PEP 658 sibling today).
    fn metric_families(&self) -> &'static [crate::metrics::MetricFamily] {
        &[]
    }

    /// Revalidate stale cached pages once, invoked from the server's background sweep. A driver
    /// without a read-through cache sweeps nothing, so the default is a no-op.
    async fn refresh_stale(&self, _state: Arc<AppState>) -> Result<RefreshSweep, String> {
        Ok(RefreshSweep::default())
    }
}

/// The driver installed when no ecosystem is wired into [`AppState`]: every request gets a `503`.
///
/// A build that forgot to inject a driver fails loudly rather than silently serving nothing. The
/// binary replaces this with a real driver at startup.
#[derive(Debug, Clone, Copy, Default)]
pub struct UnconfiguredServing;

impl UnconfiguredServing {
    fn unavailable() -> Response {
        (StatusCode::SERVICE_UNAVAILABLE, "no ecosystem driver configured").into_response()
    }
}

#[async_trait]
impl EcosystemServing for UnconfiguredServing {
    fn ecosystem(&self) -> Ecosystem {
        Ecosystem::Pypi
    }

    async fn get(
        &self,
        _state: Arc<AppState>,
        _position: usize,
        _rest: String,
        _uri: Uri,
        _headers: HeaderMap,
    ) -> Response {
        Self::unavailable()
    }

    async fn post(&self, _state: Arc<AppState>, _path: String, _headers: HeaderMap, _multipart: Multipart) -> Response {
        Self::unavailable()
    }

    async fn put(&self, _state: Arc<AppState>, _uri: Uri, _headers: HeaderMap) -> Response {
        Self::unavailable()
    }

    async fn delete(&self, _state: Arc<AppState>, _uri: Uri, _headers: HeaderMap) -> Response {
        Self::unavailable()
    }

    fn discover_index(
        &self,
        index: crate::state::IndexDescription,
        _base: Option<&crate::discovery::BaseUrl>,
    ) -> serde_json::Value {
        crate::discovery::minimal_entry(&index)
    }

    fn classify_route(&self, _path: &str) -> crate::rate_limit::RouteClass {
        crate::rate_limit::RouteClass::Listing
    }
}
