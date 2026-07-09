//! Blob serving: local and proxied reads, range/HEAD, layer-contents browsing, ingest and delete.

use super::uploads::created;
use super::*;
use crate::error::{ErrorCode, error_response, gateway_error};
use crate::store::{self};
use crate::upstream::UpstreamError;
use axum::body::Body;
use axum::http::{HeaderMap, HeaderName, HeaderValue, StatusCode, header};
use axum::response::{IntoResponse, Response};
use futures_util::{Stream, TryStreamExt as _};
use std::path::Path;
use std::sync::Arc;
use peryx_http::metrics::Event;
use peryx_http::webhook::WebhookEventKind;
use peryx_http::{AppState, Index};
use peryx_policy::PolicyAction;
use peryx_storage::archive::{self, ArchiveError, MemberChunk};
use peryx_storage::blob::{BlobError, BlobStore, Digest, PendingBlob};

impl OciRegistry {
    /// Serve a blob from the store, pulling it through the members' online proxies on a miss. Blobs
    /// are content-addressed and shared, so the store hit covers every member at once.
    pub(super) async fn serve_blob(
        &self,
        state: &AppState,
        name: &str,
        digest: &str,
        head: bool,
        range: Option<&str>,
    ) -> Result<Response, ServeError> {
        let Some((index, repo)) = resolve(&state.indexes, name) else {
            return Ok(error_response(ErrorCode::NameUnknown, "repository name unknown"));
        };
        if policy_blocks(index, PolicyAction::Serve, repo) {
            return Ok(error_response(ErrorCode::BlobUnknown, "blob unknown"));
        }
        let Some(storage) = store::blob_digest(digest) else {
            return Ok(error_response(
                ErrorCode::DigestInvalid,
                "only sha256 blob digests are supported",
            ));
        };
        if head {
            return self.head_blob(state, index, repo, digest, &storage, range).await;
        }
        let response = match self.ensure_blob(state, index, repo, digest, &storage).await? {
            BlobFetch::Stored => serve_stored_blob(&state.blobs, &storage, digest, false, range).await?,
            BlobFetch::Absent => error_response(ErrorCode::BlobUnknown, "blob unknown"),
            BlobFetch::Gateway(response) => response,
        };
        // A blob served to a GET is a download (a HEAD returned earlier, above).
        if response.status().is_success() {
            state.metrics.record(Event::Download {
                route: index.route.clone(),
                project: repo.to_owned(),
                filename: digest.to_owned(),
                bytes: served_bytes(&response),
            });
        }
        Ok(response)
    }

    /// Answer a blob `HEAD`: from the store when cached, otherwise a cheap upstream `HEAD` on a proxy
    /// member so a client's pre-flight existence check never downloads the whole layer.
    async fn head_blob(
        &self,
        state: &AppState,
        index: &Index,
        repo: &str,
        digest: &str,
        storage: &Digest,
        range: Option<&str>,
    ) -> Result<Response, ServeError> {
        if state.blobs.exists(storage) {
            return serve_stored_blob(&state.blobs, storage, digest, true, range).await;
        }
        for member in serving_members(state, index) {
            let Some(client) = proxy_client(&member.kind) else {
                continue;
            };
            match self
                .upstream
                .blob_head(client.base_url(), client.auth(), repo, digest)
                .await
            {
                Ok(size) => return Ok(blob_head_response(digest, size)),
                Err(UpstreamError::Status(status)) if absent_upstream(status) => {}
                Err(err) => return Ok(upstream_error_response(&err, "blob")),
            }
        }
        Ok(error_response(ErrorCode::BlobUnknown, "blob unknown"))
    }

    /// Make a blob present in the store, fetching it once through the single-flight gate on a miss.
    /// Concurrent misses for one content-addressed blob share the download: the first waiter fetches
    /// it, the rest wake to find it stored.
    async fn ensure_blob(
        &self,
        state: &AppState,
        index: &Index,
        repo: &str,
        digest: &str,
        storage: &Digest,
    ) -> Result<BlobFetch, ServeError> {
        if state.blobs.exists(storage) {
            return Ok(BlobFetch::Stored);
        }
        let gate_key = format!("oci\0blob\0{digest}");
        let gate = flight_gate(state, &gate_key);
        let _guard = gate.lock().await;
        if state.blobs.exists(storage) {
            return Ok(BlobFetch::Stored);
        }
        let fetched = self.fetch_blob(state, index, repo, digest, storage).await;
        state.inflight.lock().expect("inflight lock").remove(&gate_key);
        fetched
    }

    /// Serve `GET /v2/<name>/blobs/<digest>/contents`: list the tar members of a stored layer, or
    /// preview one text member. The layer is a (usually gzip) tar, so the same neutral archive engine
    /// drives it; the JSON listing and `text/plain` + `x-peryx-member-*` chunk headers follow the
    /// neutral archive-inspect contract, so the web UI's file browser renders a layer verbatim.
    pub(super) async fn serve_layer_contents(
        &self,
        state: &AppState,
        name: &str,
        digest: &str,
        query: &str,
    ) -> Result<Response, ServeError> {
        let Some((index, repo)) = resolve(&state.indexes, name) else {
            return Ok(error_response(ErrorCode::NameUnknown, "repository name unknown"));
        };
        if policy_blocks(index, PolicyAction::Serve, repo) {
            return Ok(error_response(ErrorCode::BlobUnknown, "blob unknown"));
        }
        let Some(storage) = store::blob_digest(digest) else {
            return Ok(error_response(
                ErrorCode::DigestInvalid,
                "only sha256 blob digests are supported",
            ));
        };
        match self.ensure_blob(state, index, repo, digest, &storage).await? {
            BlobFetch::Stored => {}
            BlobFetch::Absent => return Ok(error_response(ErrorCode::BlobUnknown, "blob unknown")),
            BlobFetch::Gateway(response) => return Ok(response),
        }
        let path = state.blobs.path_for(&storage);
        let selected = layer_query_member(query);
        Ok(
            tokio::task::spawn_blocking(move || layer_contents_response(&path, selected))
                .await
                .expect("layer inspection task panicked"),
        )
    }

    /// Fetch a missed blob from the first proxy member that has it, into the store. Called under the
    /// single-flight gate, so only one request per digest reaches an upstream.
    async fn fetch_blob(
        &self,
        state: &AppState,
        index: &Index,
        repo: &str,
        digest: &str,
        storage: &Digest,
    ) -> Result<BlobFetch, ServeError> {
        for member in serving_members(state, index) {
            let Some(client) = proxy_client(&member.kind) else {
                continue;
            };
            match self.upstream.blob(client.base_url(), client.auth(), repo, digest).await {
                Ok(response) => {
                    if let Err(response) = download_blob(&state.blobs, storage, response).await {
                        return Ok(BlobFetch::Gateway(response));
                    }
                    return Ok(BlobFetch::Stored);
                }
                Err(UpstreamError::Status(status)) if absent_upstream(status) => {}
                Err(err) => {
                    return Ok(BlobFetch::Gateway(upstream_error_response(&err, "blob")));
                }
            }
        }
        Ok(BlobFetch::Absent)
    }
}

/// Delete a blob from the content-addressed store. Blobs are shared across indexes, so this removes
/// the bytes globally.
pub(super) fn delete_blob(
    state: &Arc<AppState>,
    headers: &HeaderMap,
    name: &str,
    digest: &str,
) -> Result<Response, ServeError> {
    let (index, repo) = match resolve_writable(state, name, headers) {
        Ok(target) => target,
        Err(response) => return Ok(response),
    };
    let Some(storage) = store::blob_digest(digest) else {
        return Ok(error_response(
            ErrorCode::DigestInvalid,
            "only sha256 blob digests are supported",
        ));
    };
    // Blobs are one global content-addressed pool, so a blob a manifest (in any index) still names is
    // shared: physically removing it would break that manifest. Acknowledge the delete but retain the
    // bytes; only an unreferenced blob is unlinked.
    if store::referenced_blob_digests(&state.meta)?.contains(storage.as_str()) {
        return Ok(accepted());
    }
    let removed = state.blobs.remove(&storage).map_err(blob_fault)?;
    Ok(if removed {
        emit_webhook(
            state,
            headers,
            WebhookEventKind::Delete,
            index,
            &repo,
            None,
            Some(digest.to_owned()),
        );
        accepted()
    } else {
        error_response(ErrorCode::BlobUnknown, "blob unknown")
    })
}

/// The outcome of fetching a missed blob from a virtual index's proxy members.
enum BlobFetch {
    /// The blob was fetched from an upstream and is now in the store.
    Stored,
    /// No proxy member has the blob; the client gets a `404`.
    Absent,
    /// A member erred mid-fetch; this ready gateway response carries the reason.
    Gateway(Response),
}

/// A failed blob ingest: the store rejected it (digest mismatch or io) or the transfer errored.
#[derive(Debug)]
pub enum DownloadError {
    Blob(BlobError),
    Stream(String),
}

impl From<BlobError> for DownloadError {
    fn from(err: BlobError) -> Self {
        Self::Blob(err)
    }
}

#[allow(
    clippy::needless_pass_by_value,
    reason = "passed as a function pointer to `map_err`, which hands over the owned error"
)]
pub(super) fn blob_fault(err: BlobError) -> ServeError {
    ServeError::Transport(err.to_string())
}

/// Stream an upstream blob into the store, verifying its digest on commit.
pub async fn download_blob(blobs: &BlobStore, storage: &Digest, response: reqwest::Response) -> Result<(), Response> {
    let stream = response.bytes_stream().map_err(|err| err.to_string());
    ingest_blob(blobs, storage, stream)
        .await
        .map_err(download_error_response)
}

/// Drain a byte stream into a staged blob and commit it under `storage`. Takes the transfer error
/// pre-stringified so this stays one instantiation a test can drive with a plain-string failure.
async fn ingest_blob(
    blobs: &BlobStore,
    storage: &Digest,
    stream: impl Stream<Item = Result<bytes::Bytes, String>> + Send,
) -> Result<(), DownloadError> {
    let mut pending = blobs.begin()?;
    let mut stream = std::pin::pin!(stream);
    while let Some(chunk) = stream.next().await {
        let chunk = chunk.map_err(DownloadError::Stream)?;
        pending.write(&chunk)?;
    }
    blobs.commit(pending, storage)?;
    Ok(())
}

/// Map a failed ingest to a client response: a digest mismatch is the client's fault, the rest ours.
fn download_error_response(err: DownloadError) -> Response {
    match err {
        DownloadError::Blob(BlobError::DigestMismatch { expected, actual }) => error_response(
            ErrorCode::DigestInvalid,
            &format!("blob digest mismatch: expected {expected}, got {actual}"),
        ),
        DownloadError::Blob(err) => gateway_error(&format!("blob store error: {err}")),
        DownloadError::Stream(err) => gateway_error(&format!("blob body read failed: {err}")),
    }
}

/// Drain a request body into a staged blob, returning the byte count written.
pub(super) async fn stream_into(pending: &mut PendingBlob, body: Body) -> Result<u64, ServeError> {
    let mut written = 0u64;
    let mut stream = body.into_data_stream();
    while let Some(chunk) = stream.next().await {
        let chunk = chunk.map_err(|err| ServeError::Transport(err.to_string()))?;
        pending.write(&chunk).map_err(blob_fault)?;
        written += chunk.len() as u64;
    }
    Ok(written)
}

/// Commit a staged blob under `digest`, verifying its bytes. A mismatch is the client's fault.
pub(super) fn commit_blob(blobs: &BlobStore, pending: PendingBlob, name: &str, digest: &str) -> Response {
    let Some(storage) = store::blob_digest(digest) else {
        return error_response(ErrorCode::DigestInvalid, "only sha256 blob digests are supported");
    };
    match blobs.commit(pending, &storage) {
        Ok(()) => blob_created(name, digest),
        Err(err) => download_error_response(DownloadError::Blob(err)),
    }
}

/// `201 Created` for a stored blob, with its location and digest.
pub(super) fn blob_created(name: &str, digest: &str) -> Response {
    created(&format!("/v2/{name}/blobs/{digest}"), digest)
}

/// Stream a stored blob, honoring a single-range request with `206`/`Content-Range`.
async fn serve_stored_blob(
    blobs: &BlobStore,
    storage: &Digest,
    digest: &str,
    head: bool,
    range: Option<&str>,
) -> Result<Response, ServeError> {
    let path = blobs.path_for(storage);
    let file = tokio::fs::File::open(&path).await?;
    let size = file.metadata().await?.len();
    let common = [
        (header::CONTENT_TYPE, HeaderValue::from_static(OCTET_STREAM)),
        (header::ACCEPT_RANGES, HeaderValue::from_static("bytes")),
        (
            DOCKER_CONTENT_DIGEST,
            HeaderValue::from_str(digest).unwrap_or(HeaderValue::from_static("")),
        ),
    ];
    // A range in a unit we do not speak, or a multi-range we do not serve as multipart, is ignored and
    // the whole blob served, per RFC 7233; only a malformed or unsatisfiable single `bytes` range
    // earns a 416.
    let Some(range) = range
        .filter(|value| value.starts_with("bytes="))
        .filter(|value| !value.contains(','))
    else {
        let mut builder = Response::builder()
            .status(StatusCode::OK)
            .header(header::CONTENT_LENGTH, size);
        for (name, value) in common {
            builder = builder.header(name, value);
        }
        let body = if head {
            Body::empty()
        } else {
            peryx_http::body::pipelined_file(file.into_std().await, 0, size)
        };
        return Ok(builder
            .body(body)
            .expect("blob response builds from validated header parts"));
    };
    let Some((start, end)) = parse_range(range, size) else {
        return Ok(Response::builder()
            .status(StatusCode::RANGE_NOT_SATISFIABLE)
            .header(header::ACCEPT_RANGES, "bytes")
            .header(header::CONTENT_RANGE, format!("bytes */{size}"))
            .body(Body::empty())
            .expect("range response builds from validated header parts"));
    };
    let length = end - start + 1;
    let mut builder = Response::builder()
        .status(StatusCode::PARTIAL_CONTENT)
        .header(header::CONTENT_LENGTH, length)
        .header(header::CONTENT_RANGE, format!("bytes {start}-{end}/{size}"));
    for (name, value) in common {
        builder = builder.header(name, value);
    }
    if head {
        return Ok(builder.body(Body::empty()).expect("range head response builds"));
    }
    Ok(builder
        .body(peryx_http::body::pipelined_file(file.into_std().await, start, length))
        .expect("range response builds from validated header parts"))
}

/// Parse a single-range `Range: bytes=…` header against a known size, inclusive per HTTP semantics.
/// A blob `HEAD` response: the size and digest headers a client needs to decide whether to pull, with
/// no body.
fn blob_head_response(digest: &str, size: u64) -> Response {
    Response::builder()
        .status(StatusCode::OK)
        .header(header::CONTENT_TYPE, OCTET_STREAM)
        .header(header::CONTENT_LENGTH, size)
        .header(header::ACCEPT_RANGES, "bytes")
        .header(
            DOCKER_CONTENT_DIGEST,
            HeaderValue::from_str(digest).unwrap_or(HeaderValue::from_static("")),
        )
        .body(Body::empty())
        .expect("blob head response builds from validated parts")
}

fn parse_range(header: &str, size: u64) -> Option<(u64, u64)> {
    let spec = header.strip_prefix("bytes=")?;
    let (start, end) = spec.split_once('-')?;
    let (start, end) = match (start.is_empty(), end.is_empty()) {
        (true, false) => {
            let suffix: u64 = end.parse().ok()?;
            (size.checked_sub(suffix)?, size.checked_sub(1)?)
        }
        (false, true) => {
            let start: u64 = start.parse().ok()?;
            (start, size.checked_sub(1)?)
        }
        (false, false) => {
            let start: u64 = start.parse().ok()?;
            let end: u64 = end.parse().ok()?;
            (start, end.min(size.checked_sub(1)?))
        }
        (true, true) => return None,
    };
    (start <= end && end < size).then_some((start, end))
}

/// The `member` (and its `offset`) a layer-contents request selects, or `None` to list the layer.
fn layer_query_member(query: &str) -> Option<(String, u64)> {
    let mut member = None;
    let mut offset = 0;
    for (key, value) in url::form_urlencoded::parse(query.as_bytes()) {
        match key.as_ref() {
            "member" => member = Some(value.into_owned()),
            "offset" => offset = value.parse().unwrap_or(0),
            _ => {}
        }
    }
    member.map(|member| (member, offset))
}

/// A synthetic filename that tells the archive engine how the layer blob is framed. The engine picks
/// its decoder by extension, and a content-addressed blob has none, so sniff the gzip magic and name
/// it accordingly.
fn layer_archive_name(path: &Path) -> &'static str {
    let mut magic = [0_u8; 2];
    let gzip = std::fs::File::open(path)
        .and_then(|mut file| file.read_exact(&mut magic))
        .is_ok()
        && magic == [0x1f, 0x8b];
    if gzip { "layer.tar.gz" } else { "layer.tar" }
}

/// List a stored layer's members, or preview one text member, as a response the web UI's archive
/// browser consumes: a `{ "members": [...] }` document, or `text/plain` bytes with the member-size,
/// offset, and next-offset headers of the neutral archive-inspect contract.
fn layer_contents_response(path: &Path, selected: Option<(String, u64)>) -> Response {
    let filename = layer_archive_name(path);
    match selected {
        None => match archive::list_members_path(filename, path) {
            Ok(members) => axum::Json(serde_json::json!({ "members": members })).into_response(),
            Err(err) => layer_error_response(&err),
        },
        Some((member, offset)) => {
            match archive::read_text_member_chunk_nested_path(
                filename,
                path,
                &[],
                &member,
                offset,
                archive::DEFAULT_MEMBER_CHUNK,
            ) {
                Ok(chunk) => member_chunk_response(&chunk),
                Err(err) => layer_error_response(&err),
            }
        }
    }
}

/// A previewed text member: its bytes as `text/plain`, plus the size/offset/next-offset headers the
/// browser reads to page through a large member.
fn member_chunk_response(chunk: &MemberChunk) -> Response {
    let mut response = Response::new(Body::from(chunk.bytes.clone()));
    let headers = response.headers_mut();
    headers.insert(
        header::CONTENT_TYPE,
        HeaderValue::from_static("text/plain; charset=utf-8"),
    );
    insert_member_header(headers, "x-peryx-member-size", chunk.size);
    insert_member_header(headers, "x-peryx-member-offset", chunk.offset);
    if let Some(next) = chunk.next_offset {
        insert_member_header(headers, "x-peryx-next-offset", next);
    }
    response
}

fn insert_member_header(headers: &mut HeaderMap, name: &'static str, value: u64) {
    if let Ok(value) = HeaderValue::from_str(&value.to_string()) {
        headers.insert(HeaderName::from_static(name), value);
    }
}

/// Map an archive engine failure onto a client status for peryx's own layer browser: a missing
/// member is a `404`, a non-text member a `415`, a bad preview range a `416`, and anything else (a
/// corrupt or unreadable layer) a `422`. This is not a distribution-spec route, so it answers with a
/// plain status and message the web UI surfaces, not a coded registry error envelope.
fn layer_error_response(err: &ArchiveError) -> Response {
    let status = match err {
        ArchiveError::MemberNotFound => StatusCode::NOT_FOUND,
        ArchiveError::BinaryMember(_) => StatusCode::UNSUPPORTED_MEDIA_TYPE,
        ArchiveError::InvalidRange { .. } => StatusCode::RANGE_NOT_SATISFIABLE,
        _ => StatusCode::UNPROCESSABLE_ENTITY,
    };
    (status, err.to_string()).into_response()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_download_error_maps_mismatch_to_client_and_the_rest_to_gateway() {
        let mismatch = DownloadError::Blob(BlobError::DigestMismatch {
            expected: "a".to_owned(),
            actual: "b".to_owned(),
        });
        assert_eq!(download_error_response(mismatch).status(), StatusCode::BAD_REQUEST);
        let io = DownloadError::Blob(BlobError::Io(std::io::Error::other("disk")));
        assert_eq!(download_error_response(io).status(), StatusCode::BAD_GATEWAY);
        assert_eq!(
            download_error_response(DownloadError::Stream("reset".to_owned())).status(),
            StatusCode::BAD_GATEWAY
        );
    }

    #[test]
    fn test_blob_fault_is_a_transport_error() {
        assert!(matches!(
            blob_fault(BlobError::NotFound("x".to_owned())),
            ServeError::Transport(_)
        ));
    }

    #[test]
    fn test_parse_range_rejects_multi_range_and_empty_spec() {
        assert_eq!(parse_range("bytes=0-1,2-3", 10), None);
        assert_eq!(parse_range("bytes=-", 10), None);
        assert_eq!(parse_range("bytes=5-2", 10), None);
        assert_eq!(parse_range("bytes=0-3", 10), Some((0, 3)));
    }

    #[tokio::test]
    async fn test_ingest_blob_reports_a_stream_error() {
        let dir = tempfile::tempdir().unwrap();
        let blobs = BlobStore::new(dir.path().join("blobs"));
        let storage = Digest::of(b"x");
        let stream = futures_util::stream::iter(vec![Err("boom".to_owned())]);
        let err = ingest_blob(&blobs, &storage, stream).await.unwrap_err();
        assert!(matches!(err, DownloadError::Stream(message) if message == "boom"));
    }
}
