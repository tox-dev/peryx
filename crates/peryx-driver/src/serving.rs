//! The ecosystem serving interface.
//!
//! The router is ecosystem-neutral: it resolves a request to a configured index and hands it to that
//! index's [`EcosystemDriver`]. Each ecosystem implements one driver; where it mounts is data, not a
//! second trait. A driver held in the registry on [`AppState`] is dispatched once per request, so
//! adding an ecosystem is a new driver rather than a change to the router.

use std::any::Any;
use std::io::Write;
use std::sync::Arc;

use async_trait::async_trait;
use axum::extract::{Multipart, Request};
use axum::http::{HeaderMap, Method, StatusCode, Uri};
use axum::response::{IntoResponse, Response};
use peryx_core::DefaultIndex;
use peryx_core::{Ecosystem, UiManifest, UiMember, UiMemberChunk, UiMeta, UiProject, UiProjectView};

use crate::state::{ServingState, ViewBlock};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EcosystemCapability {
    CatalogSync,
    TrustedPublishing,
}

pub struct CompiledEcosystemSettings {
    ecosystem: Ecosystem,
    value: Box<dyn Any + Send + Sync>,
}

impl std::fmt::Debug for CompiledEcosystemSettings {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("CompiledEcosystemSettings")
            .field("ecosystem", &self.ecosystem)
            .finish_non_exhaustive()
    }
}

impl CompiledEcosystemSettings {
    #[must_use]
    pub fn new<T: Send + Sync + 'static>(ecosystem: Ecosystem, value: T) -> Self {
        Self {
            ecosystem,
            value: Box::new(value),
        }
    }

    #[must_use]
    pub const fn ecosystem(&self) -> Ecosystem {
        self.ecosystem
    }

    #[must_use]
    pub fn value<T: 'static>(&self) -> Option<&T> {
        self.value.downcast_ref()
    }
}

pub trait EcosystemPlugin: Send + Sync {
    fn ecosystem(&self) -> Ecosystem;

    fn default_indexes(&self) -> &'static [DefaultIndex];

    fn driver(&self) -> Arc<dyn EcosystemDriver>;

    /// # Errors
    ///
    /// Returns an error when the plugin rejects its index settings.
    fn compile_index_settings(
        &self,
        name: &str,
        settings: &toml::Table,
    ) -> Result<Option<CompiledEcosystemSettings>, String>;

    /// # Errors
    ///
    /// Returns an error when the plugin cannot install its runtime services.
    fn install(
        &self,
        state: &mut crate::AppState,
        settings: &[(&str, &CompiledEcosystemSettings)],
    ) -> Result<(), String>;

    /// # Errors
    ///
    /// Returns an error when the plugin cannot install services needed by distributed availability.
    fn install_distributed(
        &self,
        state: &mut crate::AppState,
        settings: &[(&str, &CompiledEcosystemSettings)],
    ) -> Result<(), String> {
        self.install(state, settings)
    }

    fn supports(&self, _capability: EcosystemCapability) -> bool {
        false
    }

    fn openapi_paths(&self, paths: utoipa::openapi::PathsBuilder) -> utoipa::openapi::PathsBuilder;

    /// # Errors
    ///
    /// Returns an error when `format` is unsupported or snippet generation fails.
    fn snippet_text(
        &self,
        base: &crate::discovery::BaseUrl,
        route: &str,
        uploads: bool,
        format: &str,
    ) -> Result<Option<String>, String>;
}

#[derive(Default, Clone, Copy)]
pub struct MaintenanceCapabilities<'a> {
    pub idle_reclaimer: Option<&'a dyn IdleReclaimer>,
    pub intent_finalizer: Option<&'a dyn IntentFinalizer>,
    pub cache_refresher: Option<&'a dyn CacheRefresher>,
}

pub trait MaintenanceDriver: Send + Sync {
    fn ecosystem(&self) -> Ecosystem;
    fn maintenance_capabilities(&self) -> MaintenanceCapabilities<'_>;
}

#[async_trait]
pub trait IdleReclaimer: Send + Sync {
    async fn reclaim_idle(&self, state: Arc<ServingState>) -> usize;
}

#[async_trait]
pub trait IntentFinalizer: Send + Sync {
    async fn finalize_admitted(&self, state: Arc<ServingState>) -> u64;
}

#[async_trait]
pub trait CacheRefresher: Send + Sync {
    async fn refresh_stale(&self, state: Arc<ServingState>) -> Result<RefreshSweep, String>;
}

/// Replicated-view rebuild capability for replica followers.
pub trait ReplicatedApplyDriver: Send + Sync {
    /// Rebuild this driver's views for changed keys after a replicated page applies.
    ///
    /// # Errors
    /// Returns the derived view that could not apply the changes.
    fn apply_replicated_changes(&self, state: &ServingState, changed_keys: &[String]) -> Result<(), ViewBlock>;
}

pub trait JobDriver: Send + Sync {
    fn node_job(&self, job: &crate::jobs::ScheduledJob) -> Option<Result<Arc<dyn crate::jobs::NodeJob>, String>>;
}

pub trait MetricsDriver: Send + Sync {
    fn metric_families(&self) -> &'static [peryx_events::metrics::MetricFamily];
}

pub trait NameDriver: Send + Sync {
    fn normalize_name(&self, name: &str) -> String;
}

pub trait PolicyDriver: Send + Sync {
    /// # Errors
    /// Returns the invalid policy setting.
    fn compile_policy(&self, policy: &toml::Table) -> Result<Vec<Arc<dyn peryx_policy::ArtifactRule>>, String>;

    /// # Errors
    /// Returns an error when policy evaluation or report output fails.
    fn policy_dry_run(
        &self,
        meta: &peryx_storage::meta::MetaStore,
        indexes: &[peryx_index::Index],
        index_filter: Option<&str>,
        project_filter: Option<&str>,
        out: &mut dyn Write,
    ) -> Result<(), String>;
}

pub trait BlobReferenceDriver: Send + Sync {
    /// # Errors
    /// Returns an error when stored references cannot be read.
    fn referenced_blob_digests(
        &self,
        meta: &peryx_storage::meta::MetaStore,
    ) -> Result<std::collections::BTreeSet<String>, String>;
}

pub trait FsckDriver: Send + Sync {
    /// # Errors
    /// Returns an error when metadata cannot be checked or repaired.
    fn fsck_metadata(
        &self,
        meta: &peryx_storage::meta::MetaStore,
        blobs: &peryx_storage::blob::BlobStorage,
        out: &mut dyn Write,
    ) -> Result<u64, String>;
}

pub trait RetentionDriver: Send + Sync {
    /// # Errors
    /// Returns an error when the plan cannot be read or emitted.
    fn plan_retention(
        &self,
        meta: &peryx_storage::meta::MetaStore,
        index: &str,
        policy: &peryx_policy::RetentionPolicy,
        now: Option<i64>,
        emit: &mut dyn FnMut(peryx_policy::RetentionDecision) -> Result<(), String>,
    ) -> Result<peryx_policy::RetentionSummary, String>;
}

pub trait CacheDriver: Send + Sync {
    /// # Errors
    /// Returns an error when cached project state cannot be read or removed.
    fn purge_project(
        &self,
        meta: &peryx_storage::meta::MetaStore,
        index: &str,
        project: &str,
        apply: bool,
    ) -> Result<PurgeReport, String>;
    /// # Errors
    /// Returns an error when cached pages cannot be read.
    fn cache_pages(
        &self,
        meta: &peryx_storage::meta::MetaStore,
        index_names: &[&str],
    ) -> Result<Vec<CachePage>, String>;
    /// # Errors
    /// Returns an error when cached records cannot be counted.
    fn cache_record_counts(&self, meta: &peryx_storage::meta::MetaStore) -> Result<Vec<(String, u64)>, String>;
}

pub trait IndexSummaryDriver: Send + Sync {
    /// # Errors
    /// Returns an error when index state cannot be summarized.
    fn summarize_indexes(
        &self,
        meta: &peryx_storage::meta::MetaStore,
        index_names: &[String],
        recent_limit: usize,
    ) -> Result<std::collections::HashMap<String, IndexSummary>, String>;
}

pub trait TrashDriver: Send + Sync {
    /// # Errors
    /// Returns an error when trash records cannot be read.
    fn trash_records(
        &self,
        meta: &peryx_storage::meta::MetaStore,
        index_names: &[String],
    ) -> Result<Vec<peryx_core::TrashRecord>, String>;
}

pub trait ShadowDriver: Send + Sync {
    /// # Errors
    /// Returns an error when candidate state cannot be resolved.
    fn shadowed_candidates(
        &self,
        state: &ServingState,
        position: usize,
        project: &str,
    ) -> Result<Vec<peryx_core::ShadowCandidate>, String>;
}

pub trait ImportDriver: Send + Sync {
    /// # Errors
    /// Returns an error when an input cannot be validated or stored.
    fn import_dir(
        &self,
        meta: &peryx_storage::meta::MetaStore,
        blobs: &peryx_storage::blob::BlobStorage,
        target_name: &str,
        target_route: &str,
        dir: &std::path::Path,
        out: &mut dyn Write,
    ) -> Result<(), String>;
}

#[async_trait]
pub trait ServiceDriver: Send + Sync {
    fn classify_service_post(&self, path: &str, headers: &HeaderMap) -> Option<crate::rate_limit::RouteClass>;
    async fn service_post(&self, state: Arc<ServingState>, request: Request) -> Response;
}

#[async_trait]
pub trait BrowseDriver: Send + Sync {
    /// # Errors
    /// Returns an error when project names cannot be read.
    fn project_names(&self, state: &ServingState, position: usize) -> Result<Vec<String>, String>;
    async fn browse_project(
        &self,
        state: Arc<ServingState>,
        position: usize,
        project: String,
    ) -> Result<Option<UiProjectView>, String>;
}

#[async_trait]
pub trait ProjectPageDriver: Send + Sync {
    async fn project_page(
        &self,
        state: Arc<ServingState>,
        position: usize,
        project: String,
    ) -> Result<Option<(UiProject, UiMeta)>, String>;
}

#[async_trait]
pub trait ManifestDriver: Send + Sync {
    async fn manifest_view(
        &self,
        state: Arc<ServingState>,
        position: usize,
        project: String,
        reference: String,
    ) -> Result<Option<UiManifest>, String>;
}

#[async_trait]
pub trait ArtifactMemberDriver: Send + Sync {
    async fn artifact_members(
        &self,
        state: Arc<ServingState>,
        position: usize,
        project: String,
        digest: String,
    ) -> Result<Vec<UiMember>, String>;
    async fn artifact_member_chunk(
        &self,
        state: Arc<ServingState>,
        position: usize,
        project: String,
        digest: String,
        member: String,
        offset: u64,
    ) -> Result<UiMemberChunk, String>;
}

#[async_trait]
pub trait ArtifactPathDriver: Send + Sync {
    async fn artifact_path_in_project(
        &self,
        state: Arc<ServingState>,
        position: usize,
        project: String,
        digest_hex: String,
        filename: String,
    ) -> Result<peryx_storage::blob::BlobLease, String>;
}

pub struct ArchiveRequest {
    pub state: Arc<ServingState>,
    pub position: usize,
    pub project: String,
    pub digest: String,
    pub filename: String,
    pub containers: Vec<String>,
}

pub struct ArchiveMemberRequest {
    pub archive: ArchiveRequest,
    pub member: String,
    pub offset: u64,
}

#[async_trait]
pub trait ArchiveDriver: Send + Sync {
    async fn archive_members(&self, request: ArchiveRequest) -> Result<Vec<UiMember>, String>;
    async fn archive_member_chunk(&self, request: ArchiveMemberRequest) -> Result<UiMemberChunk, String>;
}

pub trait UploadUiDriver: Send + Sync {
    fn upload_ui(&self, route: &str, enabled: bool) -> Option<peryx_core::UiUploadSpec>;
}

#[derive(Default)]
pub struct DriverCapabilities<'a> {
    pub jobs: Option<&'a dyn JobDriver>,
    pub metrics: Option<&'a dyn MetricsDriver>,
    pub name: Option<&'a dyn NameDriver>,
    pub policy: Option<&'a dyn PolicyDriver>,
    pub blob_references: Option<&'a dyn BlobReferenceDriver>,
    pub fsck: Option<&'a dyn FsckDriver>,
    pub retention: Option<&'a dyn RetentionDriver>,
    pub cache: Option<&'a dyn CacheDriver>,
    pub index_summary: Option<&'a dyn IndexSummaryDriver>,
    pub trash: Option<&'a dyn TrashDriver>,
    pub shadow: Option<&'a dyn ShadowDriver>,
    pub import: Option<&'a dyn ImportDriver>,
    pub service: Option<&'a dyn ServiceDriver>,
    pub browse: Option<&'a dyn BrowseDriver>,
    pub project_page: Option<&'a dyn ProjectPageDriver>,
    pub manifest: Option<&'a dyn ManifestDriver>,
    pub artifact_members: Option<&'a dyn ArtifactMemberDriver>,
    pub artifact_path: Option<&'a dyn ArtifactPathDriver>,
    pub archive: Option<&'a dyn ArchiveDriver>,
    pub upload_ui: Option<&'a dyn UploadUiDriver>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MirrorAction {
    Plan,
    Sync,
    Verify,
}

pub struct MirrorRequest<'a> {
    pub action: MirrorAction,
    pub index: &'a str,
    pub settings: &'a toml::Table,
    pub configured: &'a toml::Table,
    pub overrides: &'a toml::Table,
}

#[async_trait]
pub trait MirrorDriver: Send + Sync {
    async fn mirror(
        &self,
        state: Arc<crate::AppState>,
        request: MirrorRequest<'_>,
        output: &mut (dyn Write + Send),
    ) -> Result<(), String>;
}

/// Where an ecosystem's wire protocol mounts in the URL space.
///
/// Indexed protocols are resolved before dispatch. Absolute protocols own declared top-level prefixes
/// and resolve their indexes from the request path.
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

/// What a per-project cache purge planned or removed.
///
/// The driver owns the category names, so the neutral maintenance command tabulates them without
/// knowing which records a format keeps.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct PurgeReport {
    /// The project name in the ecosystem's own normalized form.
    pub project: String,
    /// Ordered `(category, count)` pairs the command prints as columns.
    pub categories: Vec<(String, u64)>,
}

/// One index's activity counts for the neutral status page and dashboard.
///
/// A driver maps its vocabulary onto these neutral counters. Missing counters remain zero.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct IndexSummary {
    pub project_count: u64,
    pub upload_count: u64,
    pub recent_uploads: Vec<RecentUpload>,
}

/// One recently uploaded artifact, token-free metadata only, for the dashboard's activity list.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RecentUpload {
    pub project: String,
    pub filename: String,
    pub version: String,
    pub uploaded_at: Option<String>,
    pub size: Option<u64>,
}

/// One cached index page for the `cache list`/`cache size` command, produced by the driver that owns
/// the cache. `index` and `project` are the page's storage key split into the driver's own terms.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CachePage {
    pub index: String,
    pub project: String,
    pub fetched_at_unix: i64,
    pub fresh_secs: Option<i64>,
    pub body_bytes: u64,
    pub record_bytes: u64,
    pub key: String,
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

    fn capabilities(&self) -> DriverCapabilities<'_> {
        DriverCapabilities::default()
    }
    /// Where this ecosystem's wire protocol mounts.
    fn mount(&self) -> RouteMount {
        RouteMount::Indexed
    }

    /// The rate-limit class of a GET inside this ecosystem's URL space, which depends on its scheme.
    /// Writes and peryx's own service endpoints are classified before this reaches a driver.
    fn classify_route(&self, path: &str) -> crate::rate_limit::RouteClass;

    /// Resolve credentials before local bucket selection so changing an invalid header cannot allocate
    /// another bucket. Return `Anonymous` when credentials are absent or invalid; the caller uses the client IP.
    fn rate_limit_principal(
        &self,
        _state: &ServingState,
        _position: Option<usize>,
        _headers: &HeaderMap,
    ) -> peryx_identity::Principal {
        peryx_identity::Principal::Anonymous
    }

    /// The `GET /+api` entry for one index of this ecosystem: its wire-protocol endpoints,
    /// capabilities, and copyable client configuration. The neutral handler wraps each ecosystem's
    /// entries into one discovery document.
    fn discover_index(
        &self,
        index: crate::state::IndexDescription,
        base: Option<&crate::discovery::BaseUrl>,
    ) -> serde_json::Value;

    /// The client-facing API endpoint one index of this ecosystem is served at - where a user points
    /// their tool. The neutral status document carries this so the web dashboard shows it without
    /// knowing any ecosystem's URL scheme. Default: the index route itself.
    fn client_endpoint(&self, route: &str) -> String {
        let mut url = String::with_capacity(route.len() + 2);
        url.push('/');
        peryx_core::url_encoding::push_path(&mut url, route);
        url.push('/');
        url
    }

    /// Serve a whole request under one of this driver's [`Absolute`](RouteMount::Absolute) prefixes.
    async fn serve(&self, _state: Arc<ServingState>, _request: Request) -> Response {
        wrong_mount()
    }

    /// Serve a GET or HEAD for an [`Indexed`](RouteMount::Indexed) wire-protocol path. The router has
    /// resolved the request to index `position`, with `rest` the sub-path after the index route.
    ///
    /// Both methods arrive here because a HEAD asks for the headers of the GET it stands for, and only
    /// the driver knows how to produce them without the body: axum strips the body it is handed, so a
    /// driver that ignores `method` still answers correctly, but it pays for bytes no client reads.
    async fn get(
        &self,
        _state: Arc<ServingState>,
        _position: usize,
        _rest: String,
        _uri: Uri,
        _headers: HeaderMap,
        _method: Method,
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

#[cfg(test)]
#[path = "../tests/unit/serving/tests.rs"]
mod tests;
