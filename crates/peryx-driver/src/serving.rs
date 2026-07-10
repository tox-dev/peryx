//! The ecosystem serving interface.
//!
//! The router is ecosystem-neutral: it resolves a request to a configured index and hands it to that
//! index's [`EcosystemDriver`]. Each ecosystem implements one driver; where it mounts is data, not a
//! second trait. A driver held in the registry on [`AppState`] is dispatched once per request, so
//! adding an ecosystem is a new driver rather than a change to the router.

use std::sync::Arc;

use async_trait::async_trait;
use axum::extract::{Multipart, Request};
use axum::http::{HeaderMap, StatusCode, Uri};
use axum::response::{IntoResponse, Response};
use peryx_core::{Ecosystem, UiMeta, UiProject};

use crate::state::AppState;

/// Where an ecosystem's wire protocol mounts in the URL space.
///
/// Most ecosystems are reached through peryx's own per-index route (`{route}/simple/…` for `PyPI`);
/// they are [`Indexed`](Self::Indexed), and the neutral router resolves the index and calls the
/// per-method handlers. `OCI`'s distribution spec instead owns a fixed top-level prefix (`/v2/`) and
/// resolves the index itself from the path, so it is [`Absolute`](Self::Absolute) and serves the whole
/// request. The router and rate limiter read this to reach a driver without naming any ecosystem.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RouteMount {
    /// Reached through peryx's per-index route prefix; the router pre-resolves the index.
    Indexed,
    /// Owns these absolute top-level path prefixes and resolves the index itself.
    Absolute(&'static [&'static str]),
}

/// The outcome of one background refresh sweep: how many cached pages a driver revalidated and how
/// many it found changed upstream.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct RefreshSweep {
    pub checked: usize,
    pub changed: usize,
}

/// How one ecosystem serves its wire protocol.
///
/// The metadata methods ([`ecosystem`](Self::ecosystem), [`mount`](Self::mount),
/// [`classify_route`](Self::classify_route), [`discover_index`](Self::discover_index)) are common to
/// every ecosystem. The serving methods split by [`mount`](Self::mount): an
/// [`Indexed`](RouteMount::Indexed) driver implements
/// [`get`](Self::get)/[`post`](Self::post)/[`put`](Self::put)/[`delete`](Self::delete), which the
/// neutral router calls after resolving the index; an [`Absolute`](RouteMount::Absolute) driver
/// implements [`serve`](Self::serve) and dispatches the whole request itself. Each implements only the
/// half its mount uses; the unused half's default answers `500`, and the router never calls it.
#[async_trait]
pub trait EcosystemDriver: Send + Sync {
    /// The ecosystem this driver serves.
    fn ecosystem(&self) -> Ecosystem;

    /// Where this ecosystem's wire protocol mounts. Indexed by default (`PyPI`'s Simple API).
    fn mount(&self) -> RouteMount {
        RouteMount::Indexed
    }

    /// The rate-limit class of a GET inside this ecosystem's URL space, which depends on its scheme.
    /// Writes and peryx's own service endpoints are classified before this reaches a driver.
    fn classify_route(&self, path: &str) -> crate::rate_limit::RouteClass;

    /// The `GET /+api` entry for one index of this ecosystem: its wire-protocol endpoints,
    /// capabilities, and copyable client configuration. The neutral handler wraps each ecosystem's
    /// entries into one discovery document.
    fn discover_index(
        &self,
        index: crate::state::IndexDescription,
        base: Option<&crate::discovery::BaseUrl>,
    ) -> serde_json::Value;

    /// The ecosystem-specific counter families this driver publishes, so the neutral render layer
    /// exposes and scopes them without knowing any ecosystem's vocabulary. Empty by default.
    fn metric_families(&self) -> &'static [peryx_events::metrics::MetricFamily] {
        &[]
    }

    /// Revalidate stale cached pages once, invoked from the server's background sweep. A driver
    /// without a read-through cache sweeps nothing, so the default is a no-op.
    async fn refresh_stale(&self, _state: Arc<AppState>) -> Result<RefreshSweep, String> {
        Ok(RefreshSweep::default())
    }

    /// The project names of the index at `position`, for the web index listing. The web crate renders
    /// these without knowing the wire protocol they came from. Default: none.
    ///
    /// # Errors
    /// Returns a user-visible message when the index cannot be read.
    fn project_names(&self, _state: &AppState, _position: usize) -> Result<Vec<String>, String> {
        Ok(Vec::new())
    }

    /// The web project page for `project` on the index at `position`: its files and neutral metadata,
    /// produced from this ecosystem's format so the web crate carries none of that logic. `None` when
    /// the project is absent. Default: none.
    ///
    /// # Errors
    /// Returns a user-visible message when the project or its metadata cannot be read.
    async fn project_page(
        &self,
        _state: Arc<AppState>,
        _position: usize,
        _project: String,
    ) -> Result<Option<(UiProject, UiMeta)>, String> {
        Ok(None)
    }

    /// Ensure the artifact `digest_hex`/`filename` on the index at `position` is present locally,
    /// fetching it through the proxy on a miss, and return its path in the blob store. The web archive
    /// browser reads members from this path with the neutral archive engine. Default: unsupported.
    ///
    /// # Errors
    /// Returns a user-visible message when the artifact cannot be found or fetched.
    async fn artifact_path(
        &self,
        _state: Arc<AppState>,
        _position: usize,
        _digest_hex: String,
        _filename: String,
    ) -> Result<std::path::PathBuf, String> {
        Err("this ecosystem does not serve artifact files".to_owned())
    }

    /// Serve a whole request under one of this driver's [`Absolute`](RouteMount::Absolute) prefixes.
    async fn serve(&self, _state: Arc<AppState>, _request: Request) -> Response {
        wrong_mount()
    }

    /// Serve a GET for an [`Indexed`](RouteMount::Indexed) wire-protocol path. The router has resolved
    /// the request to index `position`, with `rest` the sub-path after the index route.
    async fn get(
        &self,
        _state: Arc<AppState>,
        _position: usize,
        _rest: String,
        _uri: Uri,
        _headers: HeaderMap,
    ) -> Response {
        wrong_mount()
    }

    /// Serve a POST (publish/upload) for an [`Indexed`](RouteMount::Indexed) driver.
    async fn post(&self, _state: Arc<AppState>, _path: String, _headers: HeaderMap, _multipart: Multipart) -> Response {
        wrong_mount()
    }

    /// Serve a PUT (yank, restore, promote) for an [`Indexed`](RouteMount::Indexed) driver.
    async fn put(&self, _state: Arc<AppState>, _uri: Uri, _headers: HeaderMap) -> Response {
        wrong_mount()
    }

    /// Serve a DELETE (remove or un-yank) for an [`Indexed`](RouteMount::Indexed) driver.
    async fn delete(&self, _state: Arc<AppState>, _uri: Uri, _headers: HeaderMap) -> Response {
        wrong_mount()
    }
}

/// A driver reached through a method its mount does not serve. The router dispatches by
/// [`mount`](EcosystemDriver::mount), so this is unreachable in a correct build; it fails loudly
/// rather than silently if that invariant ever breaks.
fn wrong_mount() -> Response {
    (
        StatusCode::INTERNAL_SERVER_ERROR,
        "ecosystem driver reached through the wrong route mount",
    )
        .into_response()
}
