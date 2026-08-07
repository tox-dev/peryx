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
        distributed: bool,
    ) -> Result<(), String>;

    fn supports(&self, _capability: EcosystemCapability) -> bool {
        false
    }

    fn openapi_paths(&self, paths: utoipa::openapi::PathsBuilder) -> utoipa::openapi::PathsBuilder {
        paths
    }

    /// # Errors
    ///
    /// Returns an error when `format` is unsupported or snippet generation fails.
    fn snippet_text(
        &self,
        _base: &crate::discovery::BaseUrl,
        _route: &str,
        _uploads: bool,
        format: &str,
    ) -> Result<Option<String>, String> {
        Err(format!(
            "ecosystem {} does not provide client snippet {format:?}",
            self.ecosystem()
        ))
    }
}

/// Maintenance capabilities a driver owns.
#[async_trait]
pub trait MaintenanceDriver: Send + Sync {
    /// The ecosystem this driver serves.
    fn ecosystem(&self) -> Ecosystem;

    /// Drop expired process-local resources once per maintenance sweep.
    async fn reclaim_idle(&self, state: Arc<ServingState>) -> usize;

    /// Finalize this ecosystem's admitted intents once per maintenance sweep.
    async fn finalize_admitted(&self, state: Arc<ServingState>) -> u64;

    /// Revalidate stale cached pages once per maintenance sweep.
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

    /// Build an ecosystem-specific node-local job, or decline a kind owned by another driver.
    fn node_job(&self, _job: &crate::jobs::ScheduledJob) -> Option<Result<Arc<dyn crate::jobs::NodeJob>, String>> {
        None
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

    /// Fold a project key into the form this ecosystem matches against, so policy configuration and
    /// requests use the same identity. The default preserves the name.
    fn normalize_name(&self, name: &str) -> String {
        name.to_owned()
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
        _blobs: &peryx_storage::blob::BlobStorage,
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

    /// Stream one index's retention decisions against `policy` in deterministic order, adapting this
    /// ecosystem's own records into the neutral candidates the planner evaluates. `emit` returns a
    /// message to stop early (a filled page or a disconnected export client), and the scan aborts
    /// without reading further. Returns the plan's identity (policy version and metadata frontier), or
    /// `None` when this ecosystem plans no retention, so the neutral caller reports an unsupported
    /// index rather than an empty plan. The whole path only reads metadata, so an interrupted plan
    /// writes nothing.
    ///
    /// # Errors
    /// Returns a user-visible message when the store cannot be read, a record does not decode, or
    /// `emit` stopped the scan.
    fn plan_retention(
        &self,
        _meta: &peryx_storage::meta::MetaStore,
        _index: &str,
        _policy: &peryx_policy::RetentionPolicy,
        _now: Option<i64>,
        _emit: &mut dyn FnMut(peryx_policy::RetentionDecision) -> Result<(), String>,
    ) -> Result<Option<peryx_policy::RetentionSummary>, String> {
        Ok(None)
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
    ) -> Result<PurgeReport, String> {
        Err(format!(
            "the {} ecosystem does not support per-project cache purge",
            self.ecosystem().as_str()
        ))
    }

    /// Summarize this ecosystem's per-index activity (project/upload counts and recent uploads) for
    /// the status page and dashboard, keyed by index name. The neutral status path groups configured
    /// indexes by ecosystem and dispatches each group here, so no shared code reads a format's tables.
    /// Default: no summary, which reports zeros.
    ///
    /// # Errors
    /// Returns a user-visible message when the store cannot be read.
    fn summarize_indexes(
        &self,
        _meta: &peryx_storage::meta::MetaStore,
        _index_names: &[String],
        _recent_limit: usize,
    ) -> Result<std::collections::HashMap<String, IndexSummary>, String> {
        Ok(std::collections::HashMap::new())
    }

    /// This ecosystem's soft-deleted artifacts across `index_names`, as neutral trash records for the
    /// operator inspection view. Each driver reads its own trash keyspace and tags each record with its
    /// [`ecosystem`](Self::ecosystem); the neutral query merges, filters, and paginates them without
    /// naming a format. Reads only indexed trash entries, never an unbounded catalog scan. Default:
    /// none, so an ecosystem with no soft-delete contributes nothing.
    ///
    /// # Errors
    /// Returns a user-visible message when the store cannot be read.
    fn trash_records(
        &self,
        _meta: &peryx_storage::meta::MetaStore,
        _index_names: &[String],
    ) -> Result<Vec<peryx_core::TrashRecord>, String> {
        Ok(Vec::new())
    }

    /// Explain how the virtual repository at `position` resolves `project`: the selected candidate for
    /// each filename and every candidate a member shadowed, as neutral records. The driver replays its
    /// own precedence and fallback rules over stored records only, never probing a member per row, so
    /// the management query stays bounded. `position` is a resolved index the caller authorized; a
    /// non-virtual index shadows nothing. Default: none, so an ecosystem with no virtual resolution
    /// contributes nothing.
    ///
    /// # Errors
    /// Returns a user-visible message when a member's stored records cannot be read.
    fn shadowed_candidates(
        &self,
        _state: &ServingState,
        _position: usize,
        _project: &str,
    ) -> Result<Vec<peryx_core::ShadowCandidate>, String> {
        Ok(Vec::new())
    }

    /// This ecosystem's cached index pages for the `cache list`/`cache size` command, each split into
    /// `(index, project)` in its own key terms. `index_names` are the configured index names, longest
    /// first, so the driver can split a slash-bearing key against them. Default: none.
    ///
    /// # Errors
    /// Returns a user-visible message when the store cannot be read.
    fn cache_pages(
        &self,
        _meta: &peryx_storage::meta::MetaStore,
        _index_names: &[&str],
    ) -> Result<Vec<CachePage>, String> {
        Ok(Vec::new())
    }

    /// This ecosystem's cached metadata record counts as `(label, count)` pairs for `cache size`. The
    /// driver labels its own record kinds, so the neutral command tabulates them without naming any.
    /// Default: none.
    ///
    /// # Errors
    /// Returns a user-visible message when the store cannot be read.
    fn cache_record_counts(&self, _meta: &peryx_storage::meta::MetaStore) -> Result<Vec<(String, u64)>, String> {
        Ok(Vec::new())
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
        _blobs: &peryx_storage::blob::BlobStorage,
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

    /// Drop expired process-local resources once per server maintenance tick. A driver without idle
    /// resources returns zero, so the default has no work.
    async fn reclaim_idle(&self, _state: Arc<ServingState>) -> usize {
        0
    }

    /// Finalize this ecosystem's admitted-but-unfinalized uploads at their authority's home datacenter,
    /// once per server maintenance tick. An upload is admitted wherever a client reaches, staged as a
    /// pending ingress intent; its authority's home turns it into an authoritative release. This is the
    /// home-side trigger the scheduled maintenance pass invokes, so a node finalizes the backlog it owns
    /// and its own fence turns away an intent it does not home. Returns how many intents it finalized. A
    /// driver without ingress admission finalizes nothing, so the default is a no-op.
    async fn finalize_admitted(&self, _state: Arc<ServingState>) -> u64 {
        0
    }

    /// Rebuild this driver's derived views for the authoritative keys a replica just copied from the
    /// primary, so the views reflect the applied serial before the neutral apply path advances the
    /// readable frontier over it. `changed_keys` are raw store keys spanning every ecosystem; a driver
    /// acts on the ones it owns and ignores the rest, so the neutral replica loop forwards them without
    /// parsing.
    ///
    /// A driver with no replicated derived views does nothing, so the default succeeds without work.
    ///
    /// # Errors
    /// Returns [`ViewBlock`](crate::state::ViewBlock) when a required view could not be rebuilt: the apply
    /// path then holds the frontier at its prior value rather than exposing a serial the view does not
    /// reflect, and the lazy full refresh a later search runs recovers it.
    fn apply_replicated_changes(
        &self,
        _state: &ServingState,
        _changed_keys: &[String],
    ) -> Result<(), crate::state::ViewBlock> {
        Ok(())
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

    /// The client-facing API endpoint one index of this ecosystem is served at — where a user points
    /// their tool. The neutral status document carries this so the web dashboard shows it without
    /// knowing any ecosystem's URL scheme. Default: the index route itself.
    fn client_endpoint(&self, route: &str) -> String {
        let mut url = String::with_capacity(route.len() + 2);
        url.push('/');
        peryx_core::url_encoding::push_path(&mut url, route);
        url.push('/');
        url
    }

    /// A project-level browse view for `project` on the index at `position`: a file listing with
    /// metadata (a file ecosystem) or a list of references (a registry). The web crate dispatches on
    /// the returned shape without naming a format. `None` when the project is absent. Default: none.
    ///
    /// # Errors
    /// Returns a user-visible message when the project or its metadata cannot be read.
    async fn browse_project(
        &self,
        _state: Arc<ServingState>,
        _position: usize,
        _project: String,
    ) -> Result<Option<UiProjectView>, String> {
        Ok(None)
    }

    /// A manifest view for one `reference` of `project` on the index at `position`, produced from this
    /// ecosystem's format so the web crate carries none of that logic. `None` when the reference is not
    /// served. Default: none, which suits an ecosystem with no manifest concept.
    ///
    /// # Errors
    /// Returns a user-visible message when the manifest cannot be read or parsed.
    async fn manifest_view(
        &self,
        _state: Arc<ServingState>,
        _position: usize,
        _project: String,
        _reference: String,
    ) -> Result<Option<UiManifest>, String> {
        Ok(None)
    }

    /// The member listing of the nested content item `digest` under `project` on the index at
    /// `position` (an image layer), for the web layer browser. Default: none.
    ///
    /// # Errors
    /// Returns a user-visible message when the item cannot be found, fetched, or listed.
    async fn artifact_members(
        &self,
        _state: Arc<ServingState>,
        _position: usize,
        _project: String,
        _digest: String,
    ) -> Result<Vec<UiMember>, String> {
        Ok(Vec::new())
    }

    /// One text chunk of `member` inside the nested content item `digest` under `project` on the index
    /// at `position`. Default: empty.
    ///
    /// # Errors
    /// Returns a user-visible message when the member cannot be previewed as text.
    async fn artifact_member_chunk(
        &self,
        _state: Arc<ServingState>,
        _position: usize,
        _project: String,
        _digest: String,
        _member: String,
        _offset: u64,
    ) -> Result<UiMemberChunk, String> {
        Ok(UiMemberChunk::default())
    }

    /// Return a seekable artifact after proving `digest_hex`/`filename` is a member of `project` on the
    /// index at `position`, so a caller cannot borrow another project's digest to reach content it may
    /// not read. The returned lease keeps the local representation alive while the archive engine reads
    /// it. Default: unsupported.
    ///
    /// # Errors
    /// Returns a user-visible message when the file does not belong to the project, or cannot be found
    /// or fetched.
    async fn artifact_path_in_project(
        &self,
        _state: Arc<ServingState>,
        _position: usize,
        _project: String,
        _digest_hex: String,
        _filename: String,
    ) -> Result<peryx_storage::blob::BlobLease, String> {
        Err("this ecosystem does not serve artifact files".to_owned())
    }

    /// Serve a whole request under one of this driver's [`Absolute`](RouteMount::Absolute) prefixes.
    async fn serve(&self, _state: Arc<ServingState>, _request: Request) -> Response {
        wrong_mount()
    }

    /// Classify a process-wide service `POST` this driver owns, or return `None` for normal dispatch.
    fn classify_service_post(&self, _path: &str, _headers: &HeaderMap) -> Option<crate::rate_limit::RouteClass> {
        None
    }

    /// Serve a process-wide endpoint claimed by [`classify_service_post`](Self::classify_service_post).
    async fn service_post(&self, _state: Arc<ServingState>, _request: Request) -> Response {
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
