//! The ecosystem serving interface.
//!
//! The router is ecosystem-neutral: it resolves a request to a configured index and hands the request
//! to that index's ecosystem driver. Each ecosystem implements [`EcosystemServing`] to serve its own
//! wire protocol (`PyPI`'s Simple API today; an `OCI` `/v2/` or npm registry later). The driver is held on
//! [`AppState`] and dispatched dynamically once per request, so adding an ecosystem is a new driver,
//! not a change to the router.

use std::sync::Arc;

use async_trait::async_trait;
use axum::extract::Multipart;
use axum::http::{HeaderMap, StatusCode, Uri};
use axum::response::{IntoResponse, Response};

use crate::state::AppState;

/// How one ecosystem serves requests routed to an index of its kind.
#[async_trait]
pub trait EcosystemServing: Send + Sync {
    /// Serve a GET (index listing, project detail, file, archive inspection).
    async fn get(&self, state: Arc<AppState>, uri: Uri, headers: HeaderMap) -> Response;

    /// Serve a POST (publish/upload).
    async fn post(&self, state: Arc<AppState>, path: String, headers: HeaderMap, multipart: Multipart) -> Response;

    /// Serve a PUT (yank, restore, promote).
    async fn put(&self, state: Arc<AppState>, uri: Uri, headers: HeaderMap) -> Response;

    /// Serve a DELETE (remove or un-yank).
    async fn delete(&self, state: Arc<AppState>, uri: Uri, headers: HeaderMap) -> Response;

    /// Serve `GET /+api` — API discovery and copyable client configuration for configured indexes.
    async fn discover(&self, state: Arc<AppState>, uri: Uri, headers: HeaderMap) -> Response;

    /// The rate-limit class of a GET inside an index's namespace — a project listing, a metadata
    /// sibling, or an artifact — which depends on this ecosystem's URL scheme. Writes and velodex's
    /// own service endpoints are classified before this by
    /// [`service_route_class`](crate::rate_limit::service_route_class).
    fn classify_route(&self, path: &str) -> crate::rate_limit::RouteClass;

    /// The ecosystem-specific counter families this driver publishes, so the neutral render layer
    /// exposes and scopes them without knowing any ecosystem's vocabulary. Empty by default; a
    /// driver declares its own (`PyPI`'s PEP 658 sibling today).
    fn metric_families(&self) -> &'static [crate::metrics::MetricFamily] {
        &[]
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
    async fn get(&self, _state: Arc<AppState>, _uri: Uri, _headers: HeaderMap) -> Response {
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

    async fn discover(&self, _state: Arc<AppState>, _uri: Uri, _headers: HeaderMap) -> Response {
        Self::unavailable()
    }

    fn classify_route(&self, _path: &str) -> crate::rate_limit::RouteClass {
        crate::rate_limit::RouteClass::Listing
    }
}
