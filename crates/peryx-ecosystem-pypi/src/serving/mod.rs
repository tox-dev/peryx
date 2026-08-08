//! The `PyPI` ecosystem serving driver: the Simple repository API, legacy JSON, release-file and
//! archive-inspection downloads, the multipart upload API, and yank/restore/promote mutations.
//!
//! peryx-http routes a request to a configured index and hands it to that index's
//! [`EcosystemDriver`]. This module is the `PyPI` implementation; it composes the neutral
//! surfaces peryx offers a driver (state, path safety, metrics, webhooks, security events, search)
//! with this crate's cache, upload, and archive logic.
#![allow(
    clippy::result_large_err,
    reason = "handler helpers carry an axum Response as their error; boxing it everywhere adds noise"
)]

use std::sync::Arc;

use async_trait::async_trait;
use axum::extract::{Multipart, Request};
use axum::http::{HeaderMap, Method, StatusCode, Uri, header};
use axum::response::{IntoResponse, Response};
use peryx_core::Ecosystem;
use peryx_core::Role;
use peryx_core::path::{self, PathSafetyError};
use peryx_driver::discovery::BaseUrl;
use peryx_driver::not_found;
use peryx_driver::rate_limit::RouteClass;
use peryx_driver::serving::{
    ArchiveDriver, ArtifactPathDriver, BlobReferenceDriver, BrowseDriver, CacheDriver, CacheRefresher,
    DriverCapabilities, EcosystemDriver, FsckDriver, ImportDriver, IndexSummaryDriver, IntentFinalizer, JobDriver,
    MaintenanceCapabilities, MaintenanceDriver, MetricsDriver, NameDriver, PolicyDriver, ProjectPageDriver,
    RefreshSweep, ReplicatedApplyDriver, RetentionDriver, ServiceDriver, ShadowDriver, TrashDriver, UploadUiDriver,
};
use peryx_driver::state::{IndexDescription, SEARCH_VIEW, ServingState, ViewBlock};
use peryx_events::metrics::MetricFamily;
use peryx_identity::{
    Action, Denial, Grant, Identity, VerifiedToken, authorize_grants, parse_basic, strip_auth_scheme,
};
use peryx_index::{Index, IndexKind};
use peryx_search::{IndexerCtx, PackageSearch, SearchError, project_key};
use peryx_storage::blob::Digest;

use crate::attestation;
use crate::cache::{self};
use crate::discovery;

mod acknowledge;
mod admission;
mod changelog;
pub mod finalize;
pub(crate) mod finalize_sweep;
mod get;
mod inspect;
mod mutate;
mod post;
mod response;
mod upload_form;
mod web;

use changelog::{is_changelog_path, pypi_changelog};
use get::pypi_dispatch_get;
use mutate::{pypi_dispatch_delete, pypi_dispatch_put};
use post::pypi_dispatch_post;

const MIME_JSON: &str = "application/vnd.pypi.simple.v1+json";

const MIME_LEGACY_JSON: &str = "application/json";

const MIME_HTML: &str = "text/html; charset=utf-8";

/// The `PyPI` ecosystem serving driver.
#[derive(Debug, Clone, Copy, Default)]
pub struct PypiServing;

/// The negotiated wire format for a Simple-API response.
#[derive(Clone, Copy)]
pub enum Format {
    Json,
    Html,
}

#[cfg(test)]
#[path = "../../tests/unit/serving/test_support.rs"]
pub(crate) mod test_support;

/// Pick the supported response format with the highest `Accept` quality, falling back to HTML.
#[must_use]
pub fn negotiate(headers: &HeaderMap) -> Format {
    let mut json = Preference::default();
    let mut html = Preference::default();
    for accept in headers
        .get_all(header::ACCEPT)
        .iter()
        .filter_map(|value| value.to_str().ok())
    {
        for range in accept.split(',').filter_map(parse_accept_range) {
            if !range.utf8
                && let Some(specificity) = specificity(range.media, JSON_MEDIA_TYPES, JSON_MEDIA_RANGES)
            {
                json.consider(specificity * 2, range.quality);
            }
            if let Some(specificity) = specificity(range.media, HTML_MEDIA_TYPES, HTML_MEDIA_RANGES) {
                html.consider(specificity * 2 + u8::from(range.utf8), range.quality);
            }
        }
    }
    if json.quality > html.quality {
        Format::Json
    } else {
        Format::Html
    }
}

#[derive(Default)]
struct Preference {
    specificity: u8,
    quality: u16,
}

impl Preference {
    const fn consider(&mut self, specificity: u8, quality: u16) {
        if specificity > self.specificity || specificity == self.specificity && quality > self.quality {
            self.specificity = specificity;
            self.quality = quality;
        }
    }
}

struct AcceptRange<'a> {
    media: &'a str,
    quality: u16,
    utf8: bool,
}

fn parse_accept_range(range: &str) -> Option<AcceptRange<'_>> {
    let mut parts = range.split(';');
    let media = parts.next()?.trim();
    let mut quality = None;
    let mut utf8 = false;
    for parameter in parts {
        let (name, value) = parameter.split_once('=')?;
        let name = name.trim();
        let value = value.trim();
        if name.eq_ignore_ascii_case("q") {
            if quality.is_some() {
                return None;
            }
            quality = Some(parse_qvalue(value)?);
        } else if name.eq_ignore_ascii_case("charset") {
            if utf8 || !(value.eq_ignore_ascii_case("utf-8") || value.eq_ignore_ascii_case("\"utf-8\"")) {
                return None;
            }
            utf8 = true;
        } else {
            return None;
        }
    }
    (!media.is_empty()).then_some(AcceptRange {
        media,
        quality: quality.unwrap_or(1000),
        utf8,
    })
}

fn parse_qvalue(value: &str) -> Option<u16> {
    let (whole, fraction) = value.split_once('.').unwrap_or((value, ""));
    if fraction.len() > 3 || !fraction.bytes().all(|digit| digit.is_ascii_digit()) {
        return None;
    }
    match whole {
        "0" => Some(
            fraction
                .bytes()
                .fold(0_u16, |quality, digit| quality * 10 + u16::from(digit - b'0'))
                * [1000, 100, 10, 1][fraction.len()],
        ),
        "1" if fraction.bytes().all(|digit| digit == b'0') => Some(1000),
        _ => None,
    }
}

const JSON_MEDIA_TYPES: &[&str] = &[
    "application/json",
    "application/vnd.pypi.simple.v1+json",
    "application/vnd.pypi.simple.latest+json",
];
const JSON_MEDIA_RANGES: &[&str] = &["application/*"];
const HTML_MEDIA_TYPES: &[&str] = &[
    "text/html",
    "application/vnd.pypi.simple.v1+html",
    "application/vnd.pypi.simple.latest+html",
];
const HTML_MEDIA_RANGES: &[&str] = &["text/*", "application/*"];

fn specificity(media: &str, types: &[&str], ranges: &[&str]) -> Option<u8> {
    types
        .iter()
        .any(|candidate| media.eq_ignore_ascii_case(candidate))
        .then_some(2)
        .or_else(|| {
            ranges
                .iter()
                .any(|candidate| media.eq_ignore_ascii_case(candidate))
                .then_some(1)
        })
        .or_else(|| media.eq_ignore_ascii_case("*/*").then_some(0))
}

/// The PEP 658 `.metadata` sibling: a resolver reads a distribution's `METADATA` without downloading
/// the whole wheel. Any role that serves files can serve it, so it is not role-scoped.
const METADATA_FAMILY: MetricFamily = MetricFamily {
    key: "metadata",
    prom_name: "peryx_metadata_served_total",
    help: "PEP 658 metadata siblings served.",
    ui_label: "PEP 658 metadata hits",
    roles: &[Role::Cached, Role::Hosted, Role::Virtual],
};

/// The PEP 740 provenance object: a distribution's uploaded attestations, served alongside the file
/// by any role that serves files, so it is not role-scoped.
const PROVENANCE_FAMILY: MetricFamily = MetricFamily {
    key: "provenance",
    prom_name: "peryx_provenance_served_total",
    help: "PEP 740 provenance objects served.",
    ui_label: "PEP 740 provenance hits",
    roles: &[Role::Cached, Role::Hosted, Role::Virtual],
};

const PYPI_FAMILIES: &[MetricFamily] = &[
    METADATA_FAMILY,
    PROVENANCE_FAMILY,
    crate::quota::QUOTA_FAMILIES[0],
    crate::quota::QUOTA_FAMILIES[1],
];

#[async_trait]
impl EcosystemDriver for PypiServing {
    fn ecosystem(&self) -> Ecosystem {
        crate::ECOSYSTEM
    }

    fn capabilities(&self) -> DriverCapabilities<'_> {
        DriverCapabilities {
            jobs: Some(self),
            metrics: Some(self),
            name: Some(self),
            policy: Some(self),
            blob_references: Some(self),
            fsck: Some(self),
            retention: Some(self),
            cache: Some(self),
            index_summary: Some(self),
            trash: Some(self),
            shadow: Some(self),
            import: Some(self),
            service: Some(self),
            browse: Some(self),
            project_page: Some(self),
            upload_ui: Some(self),
            archive: Some(self),
            artifact_path: Some(self),
            ..DriverCapabilities::default()
        }
    }

    async fn get(
        &self,
        state: Arc<ServingState>,
        position: usize,
        rest: String,
        uri: Uri,
        headers: HeaderMap,
        method: Method,
    ) -> Response {
        pypi_dispatch_get(state, position, &rest, uri, headers, method == Method::HEAD).await
    }

    async fn post(&self, state: Arc<ServingState>, path: String, headers: HeaderMap, multipart: Multipart) -> Response {
        pypi_dispatch_post(state, path, headers, multipart).await
    }

    async fn put(&self, state: Arc<ServingState>, uri: Uri, headers: HeaderMap) -> Response {
        pypi_dispatch_put(state, uri, headers).await
    }

    async fn delete(&self, state: Arc<ServingState>, uri: Uri, headers: HeaderMap) -> Response {
        pypi_dispatch_delete(state, uri, headers).await
    }

    fn discover_index(&self, index: IndexDescription, base: Option<&BaseUrl>) -> serde_json::Value {
        discovery::index_entry(index, base)
    }

    /// `PyPI`'s Simple-API URL scheme: `.../files/*.metadata` is a PEP 658 metadata sibling, any other
    /// `files/` or `inspect/` path is an artifact, and everything else is a project listing.
    fn classify_route(&self, path: &str) -> RouteClass {
        let path = path.trim_start_matches('/');
        if path.contains("/files/") && (path.ends_with(".metadata") || path.ends_with(attestation::PROVENANCE_SUFFIX)) {
            RouteClass::Metadata
        } else if path.contains("/files/") || path.contains("/inspect/") {
            RouteClass::Artifact
        } else {
            RouteClass::Listing
        }
    }

    fn rate_limit_principal(
        &self,
        state: &ServingState,
        position: Option<usize>,
        headers: &HeaderMap,
    ) -> peryx_identity::Principal {
        identify(
            state,
            state.index_at(position.expect("an indexed driver receives a resolved index position")),
            headers,
        )
        .principal
        .clone()
    }

    fn client_endpoint(&self, route: &str) -> String {
        let mut url = String::with_capacity(route.len() + 9);
        url.push('/');
        peryx_core::url_encoding::push_path(&mut url, route);
        url.push_str("/simple/");
        url
    }
}

impl JobDriver for PypiServing {
    fn node_job(
        &self,
        job: &peryx_driver::jobs::ScheduledJob,
    ) -> Option<Result<Arc<dyn peryx_driver::jobs::NodeJob>, String>> {
        crate::catalog_job::catalog_job(job)
    }
}

#[async_trait]
impl ServiceDriver for PypiServing {
    fn classify_service_post(&self, path: &str, headers: &HeaderMap) -> Option<RouteClass> {
        is_changelog_path(path, headers).then_some(RouteClass::Listing)
    }

    async fn service_post(&self, state: Arc<ServingState>, request: Request) -> Response {
        pypi_changelog(state, request).await
    }
}

impl MetricsDriver for PypiServing {
    fn metric_families(&self) -> &'static [MetricFamily] {
        PYPI_FAMILIES
    }
}

impl NameDriver for PypiServing {
    fn normalize_name(&self, name: &str) -> String {
        crate::normalize_name(name)
    }
}

impl PolicyDriver for PypiServing {
    fn compile_policy(&self, policy: &toml::Table) -> Result<Vec<Arc<dyn peryx_policy::ArtifactRule>>, String> {
        if let Some(key) = policy
            .keys()
            .find(|key| !crate::policy::PypiPolicyConfig::KEYS.contains(&key.as_str()))
        {
            return Err(format!("unknown field `{key}` in `[index.policy]`"));
        }
        let config = toml::Value::Table(policy.clone())
            .try_into()
            .map_err(crate::error_message)?;
        crate::policy::compile_rules(&config).map_err(crate::error_message)
    }

    fn policy_dry_run(
        &self,
        meta: &peryx_storage::meta::MetaStore,
        indexes: &[Index],
        index_filter: Option<&str>,
        project_filter: Option<&str>,
        out: &mut dyn std::io::Write,
    ) -> Result<(), String> {
        crate::admin::policy_dry_run(meta, indexes, index_filter, project_filter, out)
    }
}

impl BlobReferenceDriver for PypiServing {
    fn referenced_blob_digests(
        &self,
        meta: &peryx_storage::meta::MetaStore,
    ) -> Result<std::collections::BTreeSet<String>, String> {
        crate::admin::referenced_blob_digests(meta)
    }
}

impl FsckDriver for PypiServing {
    fn fsck_metadata(
        &self,
        meta: &peryx_storage::meta::MetaStore,
        blobs: &peryx_storage::blob::BlobStorage,
        out: &mut dyn std::io::Write,
    ) -> Result<u64, String> {
        crate::admin::fsck_metadata(meta, blobs, out)
    }
}

impl RetentionDriver for PypiServing {
    fn plan_retention(
        &self,
        meta: &peryx_storage::meta::MetaStore,
        index: &str,
        policy: &peryx_policy::RetentionPolicy,
        now: Option<i64>,
        emit: &mut dyn FnMut(peryx_policy::RetentionDecision) -> Result<(), String>,
    ) -> Result<peryx_policy::RetentionSummary, String> {
        crate::retention::evaluate_retention(
            meta,
            index,
            policy,
            now,
            crate::retention::RETENTION_PROJECT_BUDGET_BYTES,
            emit,
        )
    }
}

impl CacheDriver for PypiServing {
    fn purge_project(
        &self,
        meta: &peryx_storage::meta::MetaStore,
        index: &str,
        project: &str,
        apply: bool,
    ) -> Result<peryx_driver::serving::PurgeReport, String> {
        crate::admin::purge_project(meta, index, project, apply)
    }

    fn cache_pages(
        &self,
        meta: &peryx_storage::meta::MetaStore,
        index_names: &[&str],
    ) -> Result<Vec<peryx_driver::serving::CachePage>, String> {
        crate::admin::cache_pages(meta, index_names)
    }

    fn cache_record_counts(&self, meta: &peryx_storage::meta::MetaStore) -> Result<Vec<(String, u64)>, String> {
        crate::admin::cache_record_counts(meta)
    }
}

impl IndexSummaryDriver for PypiServing {
    fn summarize_indexes(
        &self,
        meta: &peryx_storage::meta::MetaStore,
        index_names: &[String],
        recent_limit: usize,
    ) -> Result<std::collections::HashMap<String, peryx_driver::serving::IndexSummary>, String> {
        crate::store::summarize_indexes(meta, index_names, recent_limit).map_err(crate::error_message)
    }
}

impl TrashDriver for PypiServing {
    fn trash_records(
        &self,
        meta: &peryx_storage::meta::MetaStore,
        index_names: &[String],
    ) -> Result<Vec<peryx_core::TrashRecord>, String> {
        let mut records = Vec::new();
        for index in index_names {
            records.extend(crate::trash::trash_records(meta, index)?);
        }
        Ok(records)
    }
}

impl ShadowDriver for PypiServing {
    fn shadowed_candidates(
        &self,
        state: &ServingState,
        position: usize,
        project: &str,
    ) -> Result<Vec<peryx_core::ShadowCandidate>, String> {
        cache::shadowed_candidates(state, position, project)
    }
}

impl ImportDriver for PypiServing {
    fn import_dir(
        &self,
        meta: &peryx_storage::meta::MetaStore,
        blobs: &peryx_storage::blob::BlobStorage,
        target_name: &str,
        target_route: &str,
        dir: &std::path::Path,
        out: &mut dyn std::io::Write,
    ) -> Result<(), String> {
        crate::import::import_dir(meta, blobs, target_name, target_route, dir, out)
    }
}

#[async_trait]
impl BrowseDriver for PypiServing {
    fn project_names(&self, state: &ServingState, position: usize) -> Result<Vec<String>, String> {
        web::project_names(state, position)
    }

    async fn browse_project(
        &self,
        state: Arc<ServingState>,
        position: usize,
        project: String,
    ) -> Result<Option<peryx_core::UiProjectView>, String> {
        Ok(web::project_page(state, position, project)
            .await?
            .map(|(project, meta)| peryx_core::UiProjectView::Files { project, meta }))
    }
}

#[async_trait]
impl ProjectPageDriver for PypiServing {
    async fn project_page(
        &self,
        state: Arc<ServingState>,
        position: usize,
        project: String,
    ) -> Result<Option<(peryx_core::UiProject, peryx_core::UiMeta)>, String> {
        web::project_page(state, position, project).await
    }
}

#[async_trait]
impl ArtifactPathDriver for PypiServing {
    async fn artifact_path_in_project(
        &self,
        state: Arc<ServingState>,
        position: usize,
        project: String,
        digest_hex: String,
        filename: String,
    ) -> Result<peryx_storage::blob::BlobLease, String> {
        web::artifact_path_in_project(state, position, project, digest_hex, filename).await
    }
}

impl UploadUiDriver for PypiServing {
    fn upload_ui(&self, route: &str, enabled: bool) -> Option<peryx_core::UiUploadSpec> {
        enabled.then(|| peryx_core::UiUploadSpec {
            endpoint: format!("/{}/", route.trim_matches('/')),
            form_field: "content".to_owned(),
            authorization_username: Some("__token__".to_owned()),
            token_label: "Upload token".to_owned(),
            file_label: "Distribution".to_owned(),
            accept: ".whl,.tar.gz".to_owned(),
            help: "Choose one wheel or .tar.gz source distribution. The index's configured size limit applies."
                .to_owned(),
            allowed_suffixes: vec![".whl".to_owned(), ".tar.gz".to_owned()],
        })
    }
}

#[async_trait]
impl ArchiveDriver for PypiServing {
    async fn archive_members(
        &self,
        request: peryx_driver::serving::ArchiveRequest,
    ) -> Result<Vec<peryx_core::UiMember>, String> {
        let lease = web::artifact_path_in_project(
            request.state,
            request.position,
            request.project,
            request.digest,
            request.filename.clone(),
        )
        .await?;
        tokio::task::spawn_blocking(move || {
            crate::archive::list_members_nested_path(&request.filename, lease.path(), &request.containers)
        })
        .await
        .map_err(archive_listing_task_error)?
        .map(|members| {
            members
                .into_iter()
                .map(|member| peryx_core::UiMember {
                    path: member.path,
                    size: member.size,
                    kind: member.kind.as_str().to_owned(),
                    previewable: member.previewable,
                })
                .collect()
        })
        .map_err(crate::error_message)
    }

    async fn archive_member_chunk(
        &self,
        request: peryx_driver::serving::ArchiveMemberRequest,
    ) -> Result<peryx_core::UiMemberChunk, String> {
        let lease = web::artifact_path_in_project(
            request.archive.state,
            request.archive.position,
            request.archive.project,
            request.archive.digest,
            request.archive.filename.clone(),
        )
        .await?;
        let selected = request.member.clone();
        let chunk = tokio::task::spawn_blocking(move || {
            crate::archive::read_text_member_chunk_nested_path(
                &request.archive.filename,
                lease.path(),
                &request.archive.containers,
                &selected,
                request.offset,
                crate::archive::DEFAULT_MEMBER_CHUNK,
            )
        })
        .await
        .map_err(archive_member_task_error)?
        .map_err(crate::error_message)?;
        Ok(peryx_core::UiMemberChunk {
            text: member_text(&request.member, chunk.bytes)?,
            size: Some(chunk.size),
            offset: chunk.offset,
            next_offset: chunk.next_offset,
        })
    }
}

fn archive_listing_task_error(error: impl std::fmt::Display) -> String {
    format!("archive listing task failed: {error}")
}

fn archive_member_task_error(error: impl std::fmt::Display) -> String {
    format!("archive member task failed: {error}")
}

fn member_text(member: &str, bytes: Vec<u8>) -> Result<String, String> {
    String::from_utf8(bytes).map_err(|error| format!("archive member {member:?} is not UTF-8: {error}"))
}

#[cfg(test)]
#[path = "../../tests/unit/serving/archive_boundary_tests.rs"]
mod archive_boundary_tests;

impl PypiServing {
    fn apply_replicated_changes_impl(state: &ServingState, changed_keys: &[String]) -> Result<(), ViewBlock> {
        let ctx = state.indexer_ctx();
        let mut views: std::collections::BTreeSet<(usize, &str)> = std::collections::BTreeSet::new();
        for key in changed_keys {
            if let Some((index, normalized)) = crate::store::project_of_key(key) {
                for position in serving_positions(ctx.indexes, index) {
                    views.insert((position, normalized));
                }
            }
        }
        let mut hot_retired = std::collections::BTreeSet::new();
        let mut block = None;
        for (position, normalized) in views {
            let index = &ctx.indexes[position];
            if let Err(error) = rebuild_project_view(&ctx, &state.search, index, normalized) {
                tracing::error!(%error, index = %index.name, project = normalized, view = SEARCH_VIEW, "replica search-view rebuild failed; holding the readable frontier");
                block = Some(ViewBlock {
                    view: SEARCH_VIEW.to_owned(),
                });
            }
            if hot_retired.insert(normalized) {
                state.invalidate_hot_pages(normalized);
            }
        }
        block.map_or(Ok(()), Err)
    }
}

#[async_trait]
impl MaintenanceDriver for PypiServing {
    fn ecosystem(&self) -> Ecosystem {
        crate::ECOSYSTEM
    }

    fn maintenance_capabilities(&self) -> MaintenanceCapabilities<'_> {
        MaintenanceCapabilities {
            intent_finalizer: Some(self),
            cache_refresher: Some(self),
            ..MaintenanceCapabilities::default()
        }
    }
}

#[async_trait]
impl IntentFinalizer for PypiServing {
    async fn finalize_admitted(&self, state: Arc<ServingState>) -> u64 {
        finalize_sweep::finalize_admitted(&state).await
    }
}

#[async_trait]
impl CacheRefresher for PypiServing {
    async fn refresh_stale(&self, state: Arc<ServingState>) -> Result<RefreshSweep, String> {
        cache::refresh_stale_pages(&state)
            .await
            .map(|summary| RefreshSweep {
                checked: summary.checked,
                changed: summary.changed,
            })
            .map_err(|err| err.user_message())
    }
}

#[async_trait]
impl ReplicatedApplyDriver for PypiServing {
    fn apply_replicated_changes(&self, state: &ServingState, changed_keys: &[String]) -> Result<(), ViewBlock> {
        Self::apply_replicated_changes_impl(state, changed_keys)
    }
}

/// The positions of the indexes whose search document for a project on `base` a replica must rebuild:
/// `base` itself and every virtual index that reaches it, because a virtual page merges the member it
/// layers, so the member's change shows through the virtual index's own document. An unknown index name
/// resolves to nothing.
fn serving_positions(indexes: &[Index], base: &str) -> Vec<usize> {
    let Some(base_position) = indexes.iter().position(|index| index.name == base) else {
        return Vec::new();
    };
    let mut positions = vec![base_position];
    for (position, index) in indexes.iter().enumerate() {
        if let IndexKind::Virtual { layers, .. } = &index.kind
            && layers_reach(indexes, layers, base_position)
        {
            positions.push(position);
        }
    }
    positions
}

/// Whether `layers` reach the index at `target`, directly or through a nested virtual member.
fn layers_reach(indexes: &[Index], layers: &[usize], target: usize) -> bool {
    layers.iter().any(|&position| {
        position == target
            || matches!(&indexes[position].kind, IndexKind::Virtual { layers, .. } if layers_reach(indexes, layers, target))
    })
}

/// Re-derive `normalized`'s document as it appears on `index` and replace only that document in the
/// search index, retiring it when the project no longer has files.
fn rebuild_project_view(
    ctx: &IndexerCtx<'_>,
    search: &PackageSearch,
    index: &Index,
    normalized: &str,
) -> Result<(), SearchError> {
    let docs: Vec<_> = crate::search_pypi::package_document(ctx, index, normalized)?
        .into_iter()
        .collect();
    search.update_project(&docs, &project_key(&index.route, normalized))
}

fn safe_filename(raw: &str) -> Result<String, PathSafetyError> {
    let filename = path::decode_path_segment(raw)?;
    path::validate_filename(&filename)?;
    Ok(filename.into_owned())
}

fn path_error_response(err: &PathSafetyError) -> Response {
    (StatusCode::BAD_REQUEST, err.to_string()).into_response()
}

/// Parse a sha256 digest out of a route parameter. `PyPI` addresses a stored artifact by its digest,
/// so a bad one is a client error, not a missing file.
///
/// # Errors
/// Returns [`PathSafetyError::InvalidDigest`] unless `hex` is exactly 64 lowercase hex characters.
pub(crate) fn parse_digest(hex: &str) -> Result<Digest, PathSafetyError> {
    Digest::from_hex(hex).ok_or_else(|| PathSafetyError::InvalidDigest(hex.to_owned()))
}

/// The writable hosted index behind `index`: itself if hosted, its upload layer if a virtual index.
fn upload_target<'a>(state: &'a ServingState, index: &'a Index) -> Option<&'a Index> {
    match &index.kind {
        IndexKind::Hosted { .. } => Some(index),
        IndexKind::Virtual { upload: Some(pos), .. } => Some(state.index_at(*pos)),
        IndexKind::Cached { .. } | IndexKind::Virtual { upload: None, .. } => None,
    }
}

/// Resolve the request's credential against the ACL that decides its writes: the hosted target's when
/// the index has one, else the index's own, which grants nothing but still names the actor for audit.
fn identify(state: &ServingState, index: &Index, headers: &HeaderMap) -> UploadIdentity {
    let header = headers.get(header::AUTHORIZATION).and_then(|value| value.to_str().ok());
    if state.trusted_publishing.is_some()
        && let Some(token) = header.and_then(|header| {
            strip_auth_scheme(header, "Bearer").map(str::to_owned).or_else(|| {
                let basic = parse_basic(header)?;
                (basic.user == "__token__").then_some(basic.password)
            })
        })
        && let Some(signer) = &state.signer
        && let Ok(token) = signer.verify_trusted(&token)
    {
        return UploadIdentity {
            identity: Identity {
                principal: token.principal.clone(),
                user: None,
            },
            bearer: Some(token),
        };
    }
    UploadIdentity {
        identity: upload_target(state, index)
            .unwrap_or(index)
            .acl
            .identify(header, (state.clock)()),
        bearer: None,
    }
}

struct UploadIdentity {
    identity: Identity,
    bearer: Option<VerifiedToken>,
}

impl std::ops::Deref for UploadIdentity {
    type Target = Identity;

    fn deref(&self) -> &Identity {
        &self.identity
    }
}

/// Check `identity` against the hosted index's ACL, returning a ready response on any refusal.
///
/// `project` is `None` for the pass a `PyPI` upload makes before it has read the multipart body and
/// learned the project name; that pass asks only whether the principal may write anything here, and
/// leaves the success to the named pass, so one upload logs one `token_use`.
fn authorize(
    route: &str,
    hosted: &Index,
    identity: &UploadIdentity,
    project: Option<&str>,
    action: Action,
    headers: &HeaderMap,
) -> Result<(), Response> {
    if !matches!(hosted.kind, IndexKind::Hosted { .. }) {
        return Err(not_found());
    }
    let actor = peryx_events::security::actor(identity);
    let denial = identity.bearer.as_ref().map_or_else(
        || peryx_identity::authorize(&identity.principal, &hosted.acl, project, action),
        |token| authorize_bearer(&token.grants, route, project, action),
    );
    let Err(denial) = denial else {
        if project.is_some() {
            security_token_event(
                headers,
                actor.as_deref(),
                identity.bearer.as_ref().map(|token| token.id.as_str()),
                &hosted.name,
                "success",
                "",
            );
        }
        return Ok(());
    };
    security_token_event(
        headers,
        actor.as_deref(),
        identity.bearer.as_ref().map(|token| token.id.as_str()),
        &hosted.name,
        "denied",
        denial_reason(denial),
    );
    Err(match denial {
        Denial::Unauthenticated => (
            StatusCode::UNAUTHORIZED,
            [(header::WWW_AUTHENTICATE, "Basic realm=\"peryx\"")],
            "unauthorized",
        )
            .into_response(),
        denial => (StatusCode::FORBIDDEN, denial_reason(denial)).into_response(),
    })
}

fn authorize_bearer(grants: &[Grant], route: &str, project: Option<&str>, action: Action) -> Result<(), Denial> {
    let prefix = (!route.is_empty()).then(|| format!("{route}/"));
    if let Some(project) = project {
        return authorize_grants(
            grants,
            Some(&prefix.map_or_else(|| project.to_owned(), |prefix| format!("{prefix}{project}"))),
            action,
        );
    }
    grants
        .iter()
        .any(|grant| {
            grant.actions.contains(&action)
                && grant
                    .projects
                    .iter()
                    .any(|project| prefix.as_deref().is_none_or(|prefix| project.matches_prefix(prefix)))
        })
        .then_some(())
        .ok_or(Denial::Forbidden)
}

/// What an audit record says about a refusal, and what a client that may retry with better credentials
/// is told. A `401` says only "unauthorized", so the presented token is never echoed back.
const fn denial_reason(denial: Denial) -> &'static str {
    match denial {
        Denial::Unavailable => "uploads are disabled",
        Denial::Unauthenticated => "invalid upload token",
        Denial::Forbidden => "token does not grant this action",
    }
}

fn request_id(headers: &HeaderMap) -> Option<String> {
    headers
        .get("x-request-id")
        .and_then(|value| value.to_str().ok())
        .map(str::to_owned)
}

fn security_token_event(
    headers: &HeaderMap,
    actor: Option<&str>,
    token_id: Option<&str>,
    route: &str,
    result: &'static str,
    reason: &str,
) {
    let mut event = peryx_events::security::Event::new("token_use", result)
        .actor(actor)
        .index(route)
        .request(headers);
    if let Some(token_id) = token_id {
        event = event.token_id(token_id);
    }
    if reason.is_empty() {
        event.emit();
    } else {
        event.reason(Some(reason)).emit();
    }
}

#[cfg(test)]
#[path = "../../tests/unit/serving/maintenance_contract_tests.rs"]
mod maintenance_contract_tests;
