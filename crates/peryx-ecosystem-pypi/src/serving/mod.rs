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
use axum::extract::Multipart;
use axum::http::{HeaderMap, StatusCode, Uri, header};
use axum::response::{IntoResponse, Response};
use peryx_core::Ecosystem;
use peryx_core::Role;
use peryx_core::path::{self, PathSafetyError};
use peryx_driver::discovery::BaseUrl;
use peryx_driver::not_found;
use peryx_driver::rate_limit::RouteClass;
use peryx_driver::serving::{EcosystemDriver, RefreshSweep};
use peryx_driver::state::{IndexDescription, ServingState};
use peryx_events::metrics::MetricFamily;
use peryx_identity::{Action, Denial, Identity};
use peryx_index::{Index, IndexKind};
use peryx_storage::blob::Digest;

use crate::cache::{self};
use crate::discovery;

mod get;
mod inspect;
mod mutate;
mod post;
mod response;
mod upload_form;
mod web;

use get::pypi_dispatch_get;
use mutate::{pypi_dispatch_delete, pypi_dispatch_put};
use post::pypi_dispatch_post;

#[cfg(test)]
pub(crate) use response::index_response;

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

/// Pick a response format from the `Accept` header: JSON when it asks for it, HTML otherwise.
#[must_use]
pub fn negotiate(headers: &HeaderMap) -> Format {
    let accept = headers
        .get(header::ACCEPT)
        .and_then(|value| value.to_str().ok())
        .unwrap_or("");
    if accept.contains("json") {
        Format::Json
    } else {
        Format::Html
    }
}

/// The PEP 658 `.metadata` sibling: a resolver reads a distribution's `METADATA` without downloading
/// the whole wheel. Any role that serves files can serve it, so it is not role-scoped.
const METADATA_FAMILY: MetricFamily = MetricFamily {
    key: "metadata",
    prom_name: "peryx_index_metadata_total",
    help: "PEP 658 metadata siblings served.",
    ui_label: "PEP 658 metadata hits",
    roles: &[Role::Cached, Role::Hosted, Role::Virtual],
};

const PYPI_FAMILIES: &[MetricFamily] = &[METADATA_FAMILY];

#[async_trait]
impl EcosystemDriver for PypiServing {
    fn ecosystem(&self) -> Ecosystem {
        Ecosystem::Pypi
    }

    async fn get(
        &self,
        state: Arc<ServingState>,
        position: usize,
        rest: String,
        uri: Uri,
        headers: HeaderMap,
    ) -> Response {
        pypi_dispatch_get(state, position, &rest, uri, headers).await
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
        if path.contains("/files/") && path.ends_with(".metadata") {
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
    }

    fn metric_families(&self) -> &'static [MetricFamily] {
        PYPI_FAMILIES
    }

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

    fn normalize_name(&self, name: &str) -> String {
        crate::normalize_name(name)
    }

    fn referenced_blob_digests(
        &self,
        meta: &peryx_storage::meta::MetaStore,
    ) -> Result<std::collections::BTreeSet<String>, String> {
        crate::admin::referenced_blob_digests(meta)
    }

    fn fsck_metadata(
        &self,
        meta: &peryx_storage::meta::MetaStore,
        blobs: &peryx_storage::blob::BlobStore,
        out: &mut dyn std::io::Write,
    ) -> Result<u64, String> {
        crate::admin::fsck_metadata(meta, blobs, out)
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

    fn purge_project(
        &self,
        meta: &peryx_storage::meta::MetaStore,
        index: &str,
        project: &str,
        apply: bool,
    ) -> Result<peryx_driver::serving::PurgeReport, String> {
        crate::admin::purge_project(meta, index, project, apply)
    }

    fn summarize_indexes(
        &self,
        meta: &peryx_storage::meta::MetaStore,
        index_names: &[String],
        recent_limit: usize,
    ) -> Result<std::collections::HashMap<String, peryx_driver::serving::IndexSummary>, String> {
        crate::store::summarize_indexes(meta, index_names, recent_limit).map_err(crate::error_message)
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

    fn import_dir(
        &self,
        meta: &peryx_storage::meta::MetaStore,
        blobs: &peryx_storage::blob::BlobStore,
        target_name: &str,
        target_route: &str,
        dir: &std::path::Path,
        out: &mut dyn std::io::Write,
    ) -> Result<(), String> {
        crate::import::import_dir(meta, blobs, target_name, target_route, dir, out)
    }

    fn project_names(&self, state: &ServingState, position: usize) -> Result<Vec<String>, String> {
        web::project_names(state, position)
    }

    async fn project_page(
        &self,
        state: Arc<ServingState>,
        position: usize,
        project: String,
    ) -> Result<Option<(peryx_core::UiProject, peryx_core::UiMeta)>, String> {
        web::project_page(state, position, project).await
    }

    fn client_endpoint(&self, route: &str) -> String {
        let mut url = String::with_capacity(route.len() + 9);
        url.push('/');
        peryx_core::url_encoding::push_path(&mut url, route);
        url.push_str("/simple/");
        url
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

    async fn artifact_path(
        &self,
        state: Arc<ServingState>,
        position: usize,
        digest_hex: String,
        filename: String,
    ) -> Result<std::path::PathBuf, String> {
        web::artifact_path(state, position, digest_hex, filename).await
    }

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
fn identify(state: &ServingState, index: &Index, headers: &HeaderMap) -> Identity {
    let header = headers.get(header::AUTHORIZATION).and_then(|value| value.to_str().ok());
    upload_target(state, index)
        .unwrap_or(index)
        .acl
        .identify(header, (state.clock)())
}

/// Check `identity` against the hosted index's ACL, returning a ready response on any refusal.
///
/// `project` is `None` for the pass a `PyPI` upload makes before it has read the multipart body and
/// learned the project name; that pass asks only whether the principal may write anything here, and
/// leaves the success to the named pass, so one upload logs one `token_use`.
fn authorize(
    hosted: &Index,
    identity: &Identity,
    project: Option<&str>,
    action: Action,
    headers: &HeaderMap,
) -> Result<(), Response> {
    if !matches!(hosted.kind, IndexKind::Hosted { .. }) {
        return Err(not_found());
    }
    let actor = peryx_events::security::actor(identity);
    let Err(denial) = peryx_identity::authorize(&identity.principal, &hosted.acl, project, action) else {
        if project.is_some() {
            security_token_event(headers, actor.as_deref(), &hosted.name, "success", "");
        }
        return Ok(());
    };
    security_token_event(headers, actor.as_deref(), &hosted.name, "denied", denial_reason(denial));
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

fn security_token_event(headers: &HeaderMap, actor: Option<&str>, route: &str, result: &'static str, reason: &str) {
    let event = peryx_events::security::Event::new("token_use", result)
        .actor(actor)
        .index(route)
        .request(headers);
    if reason.is_empty() {
        event.emit();
    } else {
        event.reason(Some(reason)).emit();
    }
}
