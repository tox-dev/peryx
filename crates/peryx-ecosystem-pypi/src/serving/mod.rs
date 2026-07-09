//! The `PyPI` ecosystem serving driver: the Simple repository API, legacy JSON, release-file and
//! archive-inspection downloads, the multipart upload API, and yank/restore/promote mutations.
//!
//! peryx-http routes a request to a configured index and hands it to that index's
//! [`EcosystemServing`] driver. This module is the `PyPI` implementation; it composes the neutral
//! peryx-http surfaces (state, path safety, metrics, webhooks, security events, search) with this
//! crate's cache, upload, and archive logic.
#![allow(
    clippy::result_large_err,
    reason = "handler helpers carry an axum Response as their error; boxing it everywhere adds noise"
)]

use std::sync::Arc;

use async_trait::async_trait;
use axum::extract::Multipart;
use axum::http::{HeaderMap, StatusCode, Uri, header};
use axum::response::{IntoResponse, Response};
use peryx_format::Ecosystem;
use peryx_http::discovery::BaseUrl;
use peryx_http::handlers::not_found;
use peryx_http::metrics::MetricFamily;
use peryx_http::path_safety::{self, PathSafetyError};
use peryx_http::rate_limit::RouteClass;
use peryx_http::serving::{EcosystemServing, RefreshSweep};
use peryx_http::state::{AppState, Index, IndexDescription, IndexKind, Role};

use crate::cache::{self};
use crate::discovery;

mod get;
mod inspect;
mod mutate;
mod post;
mod response;
mod upload_form;

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
impl EcosystemServing for PypiServing {
    fn ecosystem(&self) -> Ecosystem {
        Ecosystem::Pypi
    }

    async fn get(&self, state: Arc<AppState>, position: usize, rest: String, uri: Uri, headers: HeaderMap) -> Response {
        pypi_dispatch_get(state, position, &rest, uri, headers).await
    }

    async fn post(&self, state: Arc<AppState>, path: String, headers: HeaderMap, multipart: Multipart) -> Response {
        pypi_dispatch_post(state, path, headers, multipart).await
    }

    async fn put(&self, state: Arc<AppState>, uri: Uri, headers: HeaderMap) -> Response {
        pypi_dispatch_put(state, uri, headers).await
    }

    async fn delete(&self, state: Arc<AppState>, uri: Uri, headers: HeaderMap) -> Response {
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

    fn metric_families(&self) -> &'static [MetricFamily] {
        PYPI_FAMILIES
    }

    async fn refresh_stale(&self, state: Arc<AppState>) -> Result<RefreshSweep, String> {
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
    let filename = path_safety::decode_path_segment(raw)?;
    path_safety::validate_filename(&filename)?;
    Ok(filename)
}

fn path_error_response(err: &PathSafetyError) -> Response {
    (StatusCode::BAD_REQUEST, err.to_string()).into_response()
}

/// The writable hosted index behind `index`: itself if hosted, its upload layer if a virtual index.
fn upload_target<'a>(state: &'a AppState, index: &'a Index) -> Option<&'a Index> {
    match &index.kind {
        IndexKind::Hosted { .. } => Some(index),
        IndexKind::Virtual { upload: Some(pos), .. } => Some(state.index_at(*pos)),
        IndexKind::Cached { .. } | IndexKind::Virtual { upload: None, .. } => None,
    }
}

/// Check the Basic-auth token of a hosted index, returning a ready response on any failure.
fn authorize(hosted: &Index, headers: &HeaderMap) -> Result<(), Response> {
    let IndexKind::Hosted { upload_token, .. } = &hosted.kind else {
        return Err(not_found());
    };
    let actor = peryx_http::security::actor(headers);
    let Some(token) = upload_token.as_deref() else {
        security_token_event(
            headers,
            actor.as_deref(),
            &hosted.name,
            "denied",
            "uploads are disabled",
        );
        return Err((StatusCode::FORBIDDEN, "uploads are disabled").into_response());
    };
    let auth = headers.get(header::AUTHORIZATION).and_then(|value| value.to_str().ok());
    if peryx_identity::authorized(auth, token) {
        security_token_event(headers, actor.as_deref(), &hosted.name, "success", "");
        Ok(())
    } else {
        security_token_event(
            headers,
            actor.as_deref(),
            &hosted.name,
            "denied",
            "invalid upload token",
        );
        Err((
            StatusCode::UNAUTHORIZED,
            [(header::WWW_AUTHENTICATE, "Basic realm=\"peryx\"")],
            "unauthorized",
        )
            .into_response())
    }
}

fn request_id(headers: &HeaderMap) -> Option<String> {
    headers
        .get("x-request-id")
        .and_then(|value| value.to_str().ok())
        .map(str::to_owned)
}

fn security_token_event(headers: &HeaderMap, actor: Option<&str>, route: &str, result: &'static str, reason: &str) {
    let event = peryx_http::security::Event::new("token_use", result)
        .actor(actor)
        .index(route)
        .request(headers);
    if reason.is_empty() {
        event.emit();
    } else {
        event.reason(Some(reason)).emit();
    }
}
