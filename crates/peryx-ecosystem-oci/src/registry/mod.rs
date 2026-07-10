//! Resolving a `/v2/` request to a configured OCI index and serving the pull path.
//!
//! `<name>` in `/v2/<name>/...` carries the peryx index route as a prefix, so the longest OCI index
//! route that segment-aligns with `<name>` selects the index and the remainder is the upstream
//! repository, the same longest-prefix rule peryx resolves any index by. A proxy pulls through and caches;
//! a hosted index serves only what it stores; a virtual index walks its members hosted-first, so a
//! hosted image shadows the same name upstream (the dependency-confusion defense). Blobs and manifests
//! are content-addressed and immutable, so a cache hit skips the network; a tag is mutable, so an
//! online proxy serves it from cache within a freshness window and revalidates against upstream once
//! that window elapses.
//!
//! Store and blob-io faults propagate through [`ServeError`] so the serving methods read as the happy
//! path, and a single conversion turns a fault into a `502`.
#![allow(
    clippy::result_large_err,
    reason = "write helpers carry an axum Response as their error; boxing it everywhere adds noise"
)]

use crate::error::{ErrorCode, error_response, gateway_error};
use crate::name::{OciRoute, classify};
use crate::upstream::{Upstream, UpstreamError};
use async_trait::async_trait;
use axum::body::Body;
use axum::extract::Request;
use axum::http::{HeaderMap, HeaderName, HeaderValue, Method, StatusCode, header};
use axum::response::{IntoResponse, Response};
use futures_util::StreamExt as _;
use peryx_core::Ecosystem;
use peryx_http::serving::NamespaceServing;
use peryx_http::webhook::{WebhookEvent, WebhookEventKind};
use peryx_http::AppState;
use peryx_index::{Index, IndexKind};
use peryx_policy::PolicyAction;
use peryx_storage::blob::PendingBlob;
use peryx_storage::meta::MetaError;
use std::io::Read as _;
use std::sync::Arc;

mod blobs;
mod discovery;
mod manifests;
mod uploads;
use blobs::delete_blob;
pub use blobs::download_blob;
use discovery::serve_catalog;
use manifests::{delete_manifest, put_manifest};
/// The header a registry returns the canonical content digest in.
const DOCKER_CONTENT_DIGEST: HeaderName = HeaderName::from_static("docker-content-digest");
/// The header carrying an upload session's id.
const DOCKER_UPLOAD_UUID: HeaderName = HeaderName::from_static("docker-upload-uuid");
/// The media type served for blob bytes.
const OCTET_STREAM: &str = "application/octet-stream";
/// The media type assumed when an upstream manifest response omits its content type.
const DEFAULT_MANIFEST_TYPE: &str = "application/vnd.oci.image.manifest.v1+json";
/// The largest manifest body accepted on a push or read through from an upstream. Manifests are small;
/// this only bounds abuse.
const MAX_MANIFEST_BYTES: usize = 4 * 1024 * 1024;
/// The largest tag-list body read from an upstream. Tag lists are text; this bounds a hostile or
/// broken upstream that would otherwise stream an unbounded body into memory.
const MAX_TAGS_BYTES: usize = 4 * 1024 * 1024;
/// The most upstream tag-list pages followed when aggregating, so a broken or hostile upstream whose
/// `Link` chain never ends cannot loop forever. Far more than any real repository paginates into.
const MAX_TAG_PAGES: usize = 100;
/// An internal fault while serving: the metadata store, blob io, or an upstream body transfer failed.
/// Client-visible outcomes (unknown manifest, invalid digest) are ordinary responses, not this.
#[derive(Debug)]
pub enum ServeError {
    Store(MetaError),
    Io(std::io::Error),
    Transport(String),
}
impl From<MetaError> for ServeError {
    fn from(err: MetaError) -> Self {
        Self::Store(err)
    }
}
impl From<std::io::Error> for ServeError {
    fn from(err: std::io::Error) -> Self {
        Self::Io(err)
    }
}
impl From<reqwest::Error> for ServeError {
    fn from(err: reqwest::Error) -> Self {
        Self::Transport(err.to_string())
    }
}
impl ServeError {
    /// Every internal fault is a gateway error to the client; the specifics go to the log context.
    fn into_response(self) -> Response {
        match self {
            Self::Store(err) => gateway_error(&format!("metadata store error: {err}")),
            Self::Io(err) => gateway_error(&format!("blob io error: {err}")),
            Self::Transport(err) => gateway_error(&format!("upstream transfer failed: {err}")),
        }
    }
}
/// The OCI/Docker registry driver: one shared upstream fetcher over the process's stores, plus the
/// in-progress blob uploads a hosted push accumulates across its `POST`/`PATCH`/`PUT` requests.
#[derive(Default)]
pub struct OciRegistry {
    upstream: Upstream,
    uploads: tokio::sync::Mutex<std::collections::HashMap<String, UploadSession>>,
    next_upload: std::sync::atomic::AtomicU64,
}
/// A blob upload between its `POST` (start) and `PUT` (finish): the staged bytes, how many landed, and
/// the index that opened it. The session map is process-global, so the owning index is checked on
/// every follow-up request; otherwise a client authorized for its own index could reach another
/// index's session by its id and disrupt or read it.
struct UploadSession {
    pending: PendingBlob,
    offset: u64,
    index: String,
    created_at: i64,
}
/// How long an open upload session lives before it is reclaimed. Each new upload evicts sessions past
/// this age, so a client that starts uploads and abandons them cannot pin their file descriptors and
/// temp files forever; dropping the session deletes its staged temp file.
const UPLOAD_SESSION_TTL_SECS: i64 = 3600;
impl OciRegistry {
    /// Build the driver with its shared upstream client.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// A fresh, process-unique upload-session id. Sessions are ephemeral, so a counter suffices.
    fn new_session(&self) -> String {
        let next = self.next_upload.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        format!("{next:032x}")
    }
}
#[async_trait]
impl NamespaceServing for OciRegistry {
    fn ecosystem(&self) -> peryx_core::Ecosystem {
        peryx_core::Ecosystem::Oci
    }

    fn prefixes(&self) -> &'static [&'static str] {
        &["/v2/"]
    }

    async fn serve(&self, state: Arc<AppState>, request: Request) -> Response {
        let method = request.method().clone();
        let read = matches!(method, Method::GET | Method::HEAD);
        if read {
            let path = request.uri().path();
            if path == "/v2/" || path == "/v2" {
                return version_check();
            }
        }
        let (parts, body) = request.into_parts();
        let Some(route) = classify(parts.uri.path()) else {
            return error_response(ErrorCode::NameUnknown, "repository name unknown");
        };
        let headers = &parts.headers;
        let query = parts.uri.query().unwrap_or_default();
        let head = method == Method::HEAD;
        let result = match route {
            OciRoute::Manifest { name, reference } if read => {
                self.serve_manifest(&state, &name, &reference, head).await
            }
            OciRoute::Manifest { name, reference } if method == Method::PUT => {
                put_manifest(&state, headers, body, &name, &reference).await
            }
            OciRoute::Manifest { name, reference } if method == Method::DELETE => {
                delete_manifest(&state, headers, &name, &reference)
            }
            OciRoute::Blob { name, digest } if read => {
                let range = headers.get(header::RANGE).and_then(|value| value.to_str().ok());
                self.serve_blob(&state, &name, &digest, head, range).await
            }
            OciRoute::Blob { name, digest } if method == Method::DELETE => delete_blob(&state, headers, &name, &digest),
            OciRoute::BlobContents { name, digest } if method == Method::GET => {
                self.serve_layer_contents(&state, &name, &digest, query).await
            }
            OciRoute::Catalog if method == Method::GET => serve_catalog(&state, query),
            OciRoute::TagsList { name } if method == Method::GET => self.serve_tags(&state, &name, query).await,
            OciRoute::Referrers { name, digest } if read => self.serve_referrers(&state, &name, &digest, query).await,
            OciRoute::UploadStart { name } if method == Method::POST => {
                self.start_upload(&state, headers, query, &name, body).await
            }
            OciRoute::UploadSession { name, session } if method == Method::GET => {
                self.upload_status(&state, headers, &name, &session).await
            }
            OciRoute::UploadSession { name, session } if method == Method::PATCH => {
                self.patch_upload(&state, headers, &name, &session, body).await
            }
            OciRoute::UploadSession { name, session } if method == Method::PUT => {
                self.finish_upload(&state, headers, query, &name, &session, body).await
            }
            _ => Ok(error_response(ErrorCode::Unsupported, "operation not supported")),
        };
        result.unwrap_or_else(ServeError::into_response)
    }

    fn classify_route(&self, path: &str) -> peryx_http::rate_limit::RouteClass {
        use peryx_http::rate_limit::RouteClass;
        // A blob GET streams layer bytes, an artifact download; manifests, tags, referrers, and the
        // layer browser are listings. The version check and writes never reach here.
        match classify(path) {
            Some(OciRoute::Blob { .. }) => RouteClass::Artifact,
            _ => RouteClass::Listing,
        }
    }

    fn discover_index(
        &self,
        index: peryx_http::state::IndexDescription,
        base: Option<&peryx_http::discovery::BaseUrl>,
    ) -> serde_json::Value {
        crate::discovery::index_entry(index, base)
    }
}
/// Resolve the writable hosted index behind `name` and authorize the request, or return a ready error
/// response (unknown name, read-only index, uploads disabled, or bad credentials). A virtual index
/// routes the write to its upload-target member.
fn resolve_writable<'a>(state: &'a AppState, name: &str, headers: &HeaderMap) -> Result<(&'a Index, String), Response> {
    let Some((index, repo)) = resolve(&state.indexes, name) else {
        return Err(error_response(ErrorCode::NameUnknown, "repository name unknown"));
    };
    let target = match &index.kind {
        IndexKind::Hosted { .. } => index,
        IndexKind::Virtual { upload: Some(pos), .. } => state.index_at(*pos),
        _ => return Err(error_response(ErrorCode::Denied, "index is read-only")),
    };
    let IndexKind::Hosted { upload_token, .. } = &target.kind else {
        return Err(error_response(ErrorCode::Denied, "index is read-only"));
    };
    let Some(token) = upload_token.as_deref() else {
        return Err(error_response(ErrorCode::Denied, "uploads are disabled"));
    };
    let auth = headers.get(header::AUTHORIZATION).and_then(|value| value.to_str().ok());
    if peryx_identity::authorized(auth, token) {
        Ok((target, repo.to_owned()))
    } else {
        Err(unauthorized())
    }
}
/// Whether the index's policy blocks this repository name. A blocked image is hidden on reads (served
/// as absent, like any policy-denied artifact) and refused on writes. The image name is the neutral
/// [`check_project`](peryx_policy::Policy::check_project) input, so an OCI index reuses the neutral
/// allow/block-list machinery every ecosystem shares.
fn policy_blocks(index: &Index, action: PolicyAction, repo: &str) -> bool {
    index.policy.check_project(action, repo).is_err()
}
/// Refuse a blob whose repository is blocked or whose size exceeds the index's `max_file_size_bytes`,
/// the neutral upload rules an OCI index shares with every ecosystem. `None` lets the write proceed.
fn policy_size_denial(index: &Index, repo: &str, size: u64) -> Option<Response> {
    index
        .policy
        .check_size(PolicyAction::Upload, repo, size)
        .err()
        .map(|denial| error_response(ErrorCode::Denied, &denial.to_string()))
}
/// The members a request serves from, in shadowing order; any non-virtual index is its own single
/// member. The order comes from the neutral role engine, so an OCI image shadows upstream by the same
/// rule a `PyPI` wheel does.
pub fn serving_members<'a>(state: &'a AppState, index: &'a Index) -> Vec<&'a Index> {
    let IndexKind::Virtual { layers, .. } = &index.kind else {
        return vec![index];
    };
    peryx_index::shadow_order(&state.indexes, layers)
        .into_iter()
        .map(|position| state.index_at(position))
        .collect()
}
/// Enqueue a webhook for an OCI mutation on a hosted index. `version` is the tag when a tagged
/// reference was affected; `digest` is the manifest or blob digest. The webhook subsystem is neutral,
/// so a hosted OCI index delivers push and delete events like any hosted index.
fn emit_webhook(
    state: &Arc<AppState>,
    headers: &HeaderMap,
    kind: WebhookEventKind,
    index: &Index,
    repo: &str,
    version: Option<String>,
    digest: Option<String>,
) {
    peryx_http::webhook::emit(
        Arc::clone(state),
        &WebhookEvent {
            kind,
            created_at_unix: (state.clock)(),
            index: index.name.clone(),
            route: index.route.clone(),
            hosted_index: index.name.clone(),
            project: repo.to_owned(),
            version,
            filename: digest.clone(),
            digest,
            count: 1,
            actor: peryx_http::security::actor(headers),
            request_id: request_id(headers),
        },
    );
}
/// The client-supplied request id, echoed into webhook deliveries for correlation.
fn request_id(headers: &HeaderMap) -> Option<String> {
    headers
        .get("x-request-id")
        .and_then(|value| value.to_str().ok())
        .map(str::to_owned)
}
/// Answer the API version check.
fn version_check() -> Response {
    (
        [(
            HeaderName::from_static("docker-distribution-api-version"),
            HeaderValue::from_static("registry/2.0"),
        )],
        StatusCode::OK,
    )
        .into_response()
}
/// The `Content-Length` a served blob response carries, for the downloaded-bytes counter. A full
/// serve reports the blob size; a range serves the partial length it delivered.
fn served_bytes(response: &Response) -> u64 {
    response
        .headers()
        .get(header::CONTENT_LENGTH)
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.parse().ok())
        .unwrap_or(0)
}
/// The per-blob lock concurrent misses share so a single upstream fetch serves them all, the same
/// single-flight coalescing every cached fetch shares. Keyed in its own namespace on the blob digest.
fn flight_gate(state: &AppState, key: &str) -> Arc<tokio::sync::Mutex<()>> {
    state
        .inflight
        .lock()
        .expect("inflight lock")
        .entry(key.to_owned())
        .or_default()
        .clone()
}
/// Find the OCI index whose route is the longest segment-aligned prefix of `name`, and the upstream
/// repository (the remainder). An empty route matches at the root, losing every tie to a real prefix.
fn resolve<'a, 'b>(indexes: &'a [Index], name: &'b str) -> Option<(&'a Index, &'b str)> {
    let mut best: Option<(&'a Index, &'b str)> = None;
    for index in indexes {
        if index.ecosystem != Ecosystem::Oci {
            continue;
        }
        let repo = if index.route.is_empty() {
            name
        } else {
            match name.strip_prefix(&index.route).and_then(|rest| rest.strip_prefix('/')) {
                Some(rest) if !rest.is_empty() => rest,
                _ => continue,
            }
        };
        if best.is_none_or(|(current, _)| index.route.len() > current.route.len()) {
            best = Some((index, repo));
        }
    }
    best
}
/// Map a blob-store failure to an internal serving fault.
/// Parse a raw query string into its percent-decoded `key=value` pairs. Clients percent-encode the
/// colon in `digest=sha256:…`, so decoding is required, not cosmetic.
fn query_params(query: &str) -> std::collections::HashMap<String, String> {
    url::form_urlencoded::parse(query.as_bytes())
        .map(|(key, value)| (key.into_owned(), value.into_owned()))
        .collect()
}
/// A bare `202 Accepted` for a completed delete.
fn accepted() -> Response {
    (StatusCode::ACCEPTED, Body::empty()).into_response()
}
/// `401 Unauthorized` with the Basic-auth challenge a pushing client expects.
fn unauthorized() -> Response {
    let body =
        serde_json::json!({"errors": [{"code": "UNAUTHORIZED", "message": "authentication required"}]}).to_string();
    Response::builder()
        .status(StatusCode::UNAUTHORIZED)
        .header(header::WWW_AUTHENTICATE, "Basic realm=\"peryx\"")
        .header(header::CONTENT_TYPE, "application/json")
        .body(Body::from(body))
        .expect("unauthorized response builds from validated parts")
}
/// Read an upstream response body into memory, refusing one larger than `max`. A caching proxy holds
/// the whole body to hash or re-serve it, so an unbounded read would let a hostile or broken upstream
/// drive peryx out of memory.
/// Whether something fetched at `fetched_at` may still answer while the upstream cannot confirm it.
///
/// The same bound a stale `PyPI` page gets: serve past the freshness window while an upstream is
/// down, but not without end. `0` removes the bound.
fn within_stale_bound(state: &AppState, fetched_at: i64) -> bool {
    if state.max_stale_secs == 0 {
        return true;
    }
    (state.clock)().saturating_sub(fetched_at) < state.ttl_secs + state.max_stale_secs
}

async fn bounded_body(response: reqwest::Response, max: usize) -> Result<bytes::Bytes, ServeError> {
    let mut stream = response.bytes_stream();
    let mut body: Vec<u8> = Vec::new();
    while let Some(chunk) = stream.next().await {
        let chunk = chunk?;
        if body.len() + chunk.len() > max {
            return Err(ServeError::Transport(format!("upstream body exceeds {max} bytes")));
        }
        body.extend_from_slice(&chunk);
    }
    Ok(bytes::Bytes::from(body))
}
/// Turn an upstream fetch failure into a client response. A rate limit becomes a `429` carrying the
/// upstream's `Retry-After`, so the client backs off instead of hammering; anything else is a `502`,
/// a fault between peryx and its upstream.
fn upstream_error_response(err: &UpstreamError, what: &str) -> Response {
    let UpstreamError::RateLimited(retry_after) = err else {
        return gateway_error(&format!("upstream {what} fetch failed: {err}"));
    };
    let mut response = error_response(ErrorCode::TooManyRequests, "upstream rate limit reached; retry later");
    if let Some(value) = retry_after
        .as_deref()
        .and_then(|value| HeaderValue::from_str(value).ok())
    {
        response.headers_mut().insert(header::RETRY_AFTER, value);
    }
    response
}
/// Whether an upstream status means "this member does not have it" rather than a fault: `404`, and
/// also `401`/`403` because a registry (Docker Hub) answers those for a repository that does not
/// exist or is not anonymously visible, either way the member cannot serve it, so a virtual index
/// walks on and a lone proxy reports the artifact unknown.
fn absent_upstream(status: StatusCode) -> bool {
    matches!(
        status,
        StatusCode::NOT_FOUND | StatusCode::UNAUTHORIZED | StatusCode::FORBIDDEN
    )
}
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_serve_error_maps_every_fault_to_a_gateway_error() {
        let decode = serde_json::from_str::<u8>("nope").unwrap_err();
        assert_eq!(
            ServeError::from(MetaError::Decode(decode)).into_response().status(),
            StatusCode::BAD_GATEWAY
        );
        assert_eq!(
            ServeError::Io(std::io::Error::other("disk")).into_response().status(),
            StatusCode::BAD_GATEWAY
        );
        assert_eq!(
            ServeError::Transport("reset".to_owned()).into_response().status(),
            StatusCode::BAD_GATEWAY
        );
    }

    #[tokio::test]
    async fn test_serve_error_wraps_a_transport_failure() {
        let _ = rustls::crypto::ring::default_provider().install_default();
        let err = reqwest::Client::new()
            .get("http://127.0.0.1:1/")
            .send()
            .await
            .unwrap_err();
        assert_eq!(ServeError::from(err).into_response().status(), StatusCode::BAD_GATEWAY);
    }

    #[test]
    fn test_classify_route_buckets_blob_pulls_as_artifacts() {
        use peryx_http::rate_limit::RouteClass;
        use peryx_http::serving::NamespaceServing as _;
        let registry = OciRegistry::default();
        let digest = "sha256:2222222222222222222222222222222222222222222222222222222222222222";
        assert_eq!(
            registry.classify_route(&format!("/v2/store/app/blobs/{digest}")),
            RouteClass::Artifact
        );
        assert_eq!(
            registry.classify_route("/v2/store/app/manifests/1.0"),
            RouteClass::Listing
        );
        assert_eq!(registry.classify_route("/v2/store/app/tags/list"), RouteClass::Listing);
        assert_eq!(
            registry.classify_route(&format!("/v2/store/app/blobs/{digest}/contents")),
            RouteClass::Listing
        );
    }
}
