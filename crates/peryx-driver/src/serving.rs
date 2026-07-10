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

use crate::state::ServingState;

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

    /// Compile this ecosystem's artifact-policy rules from its slice of an index's `[policy]` table —
    /// the keys the neutral engine did not claim. The neutral binary attaches these to the index's
    /// [`Policy`](peryx_policy::Policy) without knowing any ecosystem's policy vocabulary. Default: an
    /// ecosystem with no artifact policy claims no keys, so any key here is unknown configuration.
    ///
    /// # Errors
    /// Returns a user-visible message when a key is unknown to this ecosystem or a value is invalid.
    fn compile_policy(&self, policy: &toml::Table) -> Result<Vec<Arc<dyn peryx_policy::ArtifactRule>>, String> {
        policy.keys().next().map_or_else(
            || Ok(Vec::new()),
            |key| Err(format!("unknown field `{key}` in `[index.policy]`")),
        )
    }

    /// The stored-blob digests this ecosystem's metadata references, so the neutral orphan-blob
    /// collector keeps them and reclaims the rest. Blobs are content-addressed and shared across
    /// ecosystems, so the collector unions this over every installed driver. Default: none.
    ///
    /// # Errors
    /// Returns a user-visible message when a metadata record cannot be read, so a purge never runs
    /// against a store it cannot fully account for.
    fn referenced_blob_digests(
        &self,
        _meta: &peryx_storage::meta::MetaStore,
    ) -> Result<std::collections::BTreeSet<String>, String> {
        Ok(std::collections::BTreeSet::new())
    }

    /// Validate this ecosystem's metadata records, writing one line per problem to `out` and returning
    /// the count. Blob contents are content-addressed, so the neutral caller verifies them once for
    /// all ecosystems; this checks only that the metadata is internally consistent. Default: none.
    ///
    /// # Errors
    /// Returns a user-visible message when the store cannot be read or `out` cannot be written.
    fn fsck_metadata(
        &self,
        _meta: &peryx_storage::meta::MetaStore,
        _blobs: &peryx_storage::blob::BlobStore,
        _out: &mut dyn std::io::Write,
    ) -> Result<u64, String> {
        Ok(0)
    }

    /// Preview this ecosystem's policy decisions over its cached and uploaded records, writing one
    /// line per denial to `out`. `indexes` is every configured index; `index_filter` and
    /// `project_filter` narrow the scan. The neutral caller writes the header once and runs this over
    /// every driver. Default: an ecosystem with no previewable records writes nothing.
    ///
    /// # Errors
    /// Returns a user-visible message when a record cannot be read or `out` cannot be written.
    fn policy_dry_run(
        &self,
        _meta: &peryx_storage::meta::MetaStore,
        _indexes: &[peryx_index::Index],
        _index_filter: Option<&str>,
        _project_filter: Option<&str>,
        _out: &mut dyn std::io::Write,
    ) -> Result<(), String> {
        Ok(())
    }

    /// Purge one project's cached records from `index`, keeping any blob a still-cached project or a
    /// hosted upload also references. With `apply`, deletes and reports the removed counts; otherwise
    /// counts what a purge would remove. Returns the ecosystem-normalized project name alongside.
    /// Default: an ecosystem without a project cache refuses.
    ///
    /// # Errors
    /// Returns a user-visible message when the store cannot be read or written, or the ecosystem has
    /// no per-project cache to purge.
    fn purge_project(
        &self,
        _meta: &peryx_storage::meta::MetaStore,
        _index: &str,
        _project: &str,
        _apply: bool,
    ) -> Result<(String, peryx_storage::meta::ProjectCachePurgeCounts), String> {
        Err(format!(
            "the {} ecosystem does not support per-project cache purge",
            self.ecosystem().as_str()
        ))
    }

    /// Import every artifact under `dir` into the hosted index `target_name` (reached at
    /// `target_route`), writing per-file progress to `out`. The neutral binary resolves the upload
    /// target from the index topology; how a directory of files becomes stored artifacts is the
    /// ecosystem's. Default: an ecosystem with no bulk-import format refuses.
    ///
    /// # Errors
    /// Returns a user-visible message when the directory cannot be read or the ecosystem does not
    /// support directory import.
    fn import_dir(
        &self,
        _meta: &peryx_storage::meta::MetaStore,
        _blobs: &peryx_storage::blob::BlobStore,
        _target_name: &str,
        _target_route: &str,
        _dir: &std::path::Path,
        _out: &mut dyn std::io::Write,
    ) -> Result<(), String> {
        Err(format!(
            "the {} ecosystem does not support directory import",
            self.ecosystem().as_str()
        ))
    }

    /// Revalidate stale cached pages once, invoked from the server's background sweep. A driver
    /// without a read-through cache sweeps nothing, so the default is a no-op.
    async fn refresh_stale(&self, _state: Arc<ServingState>) -> Result<RefreshSweep, String> {
        Ok(RefreshSweep::default())
    }

    /// The project names of the index at `position`, for the web index listing. The web crate renders
    /// these without knowing the wire protocol they came from. Default: none.
    ///
    /// # Errors
    /// Returns a user-visible message when the index cannot be read.
    fn project_names(&self, _state: &ServingState, _position: usize) -> Result<Vec<String>, String> {
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
        _state: Arc<ServingState>,
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
        _state: Arc<ServingState>,
        _position: usize,
        _digest_hex: String,
        _filename: String,
    ) -> Result<std::path::PathBuf, String> {
        Err("this ecosystem does not serve artifact files".to_owned())
    }

    /// Serve a whole request under one of this driver's [`Absolute`](RouteMount::Absolute) prefixes.
    async fn serve(&self, _state: Arc<ServingState>, _request: Request) -> Response {
        wrong_mount()
    }

    /// Serve a GET for an [`Indexed`](RouteMount::Indexed) wire-protocol path. The router has resolved
    /// the request to index `position`, with `rest` the sub-path after the index route.
    async fn get(
        &self,
        _state: Arc<ServingState>,
        _position: usize,
        _rest: String,
        _uri: Uri,
        _headers: HeaderMap,
    ) -> Response {
        wrong_mount()
    }

    /// Serve a POST (publish/upload) for an [`Indexed`](RouteMount::Indexed) driver.
    async fn post(
        &self,
        _state: Arc<ServingState>,
        _path: String,
        _headers: HeaderMap,
        _multipart: Multipart,
    ) -> Response {
        wrong_mount()
    }

    /// Serve a PUT (yank, restore, promote) for an [`Indexed`](RouteMount::Indexed) driver.
    async fn put(&self, _state: Arc<ServingState>, _uri: Uri, _headers: HeaderMap) -> Response {
        wrong_mount()
    }

    /// Serve a DELETE (remove or un-yank) for an [`Indexed`](RouteMount::Indexed) driver.
    async fn delete(&self, _state: Arc<ServingState>, _uri: Uri, _headers: HeaderMap) -> Response {
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
