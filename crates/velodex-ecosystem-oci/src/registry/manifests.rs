//! Manifest pull/push/delete: tag revalidation, digest fetch, and the upload-validated store.

use super::*;
use crate::error::{ErrorCode, error_response};
use crate::name::Reference;
use crate::store::{self, Manifest};
use crate::upstream::UpstreamError;
use axum::body::Body;
use axum::http::{HeaderMap, StatusCode, header};
use axum::response::Response;
use std::sync::Arc;
use velodex_http::metrics::Event;
use velodex_http::webhook::WebhookEventKind;
use velodex_http::{AppState, Index};
use velodex_policy::PolicyAction;
use velodex_storage::blob::Digest;
use velodex_upstream::UpstreamClient;

impl OciRegistry {
    /// Serve a manifest by tag or digest. A virtual index walks its members hosted-first, so a hosted
    /// image shadows the same name upstream; a single hosted or proxy index is the one-member case.
    pub(super) async fn serve_manifest(
        &self,
        state: &AppState,
        name: &str,
        reference: &Reference,
        head: bool,
    ) -> Result<Response, ServeError> {
        let Some((index, repo)) = resolve(&state.indexes, name) else {
            return Ok(error_response(ErrorCode::NameUnknown, "repository name unknown"));
        };
        if policy_blocks(index, PolicyAction::Serve, repo) {
            return Ok(error_response(ErrorCode::ManifestUnknown, "manifest unknown"));
        }
        let response = match reference {
            Reference::Digest(digest) => {
                let mut served = store::get_manifest(&state.meta, digest)?
                    .map(|manifest| manifest_response(&manifest, digest, head));
                for member in serving_members(state, index) {
                    if served.is_some() {
                        break;
                    }
                    if let Some(client) = proxy_client(&member.kind) {
                        served = self
                            .pull_manifest_by_digest(state, client, &member.name, repo, digest, head)
                            .await?;
                    }
                }
                served.unwrap_or_else(|| error_response(ErrorCode::ManifestUnknown, "manifest unknown"))
            }
            Reference::Tag(tag) => {
                let mut served = None;
                for member in serving_members(state, index) {
                    if served.is_some() {
                        break;
                    }
                    served = self.member_tag(state, member, repo, tag, head).await?;
                }
                served.unwrap_or_else(|| error_response(ErrorCode::ManifestUnknown, "manifest unknown"))
            }
        };
        // A served manifest is this ecosystem's index document, so it counts as a page like a Simple
        // page does; a HEAD is a metadata check and carries no body, so it does not.
        if !head && response.status() == StatusCode::OK {
            state.metrics.record(Event::Page {
                route: index.route.clone(),
                project: repo.to_owned(),
            });
        }
        Ok(response)
    }

    /// Try one member for a manifest by digest. `None` means this member does not have it (a `404`),
    /// so the caller moves to the next; `Some` is a served manifest or a real error to surface.
    ///
    /// A manifest by digest is immutable, so concurrent pulls of the same digest (the fan-out into an
    /// image index's per-platform children) single-flight through the gate: the first fetches and
    /// stores it, the rest re-read the store and skip the upstream round trip.
    async fn pull_manifest_by_digest(
        &self,
        state: &AppState,
        client: &UpstreamClient,
        index: &str,
        repo: &str,
        digest: &str,
        head: bool,
    ) -> Result<Option<Response>, ServeError> {
        let gate_key = format!("oci\u{0}manifest\u{0}{digest}");
        let gate = flight_gate(state, &gate_key);
        let _guard = gate.lock().await;
        if let Some(manifest) = store::get_manifest(&state.meta, digest)? {
            return Ok(Some(manifest_response(&manifest, digest, head)));
        }
        let fetched = self
            .fetch_manifest_by_digest(state, client, index, repo, digest, head)
            .await;
        state.inflight.lock().expect("inflight lock").remove(&gate_key);
        fetched
    }

    /// Fetch a manifest by digest from one member and store it, verifying the upstream bytes hash to
    /// the requested digest. The single-flight gate around this lives in [`Self::pull_manifest_by_digest`].
    async fn fetch_manifest_by_digest(
        &self,
        state: &AppState,
        client: &UpstreamClient,
        index: &str,
        repo: &str,
        digest: &str,
        head: bool,
    ) -> Result<Option<Response>, ServeError> {
        let response = match self
            .upstream
            .manifest(client.base_url(), client.auth(), repo, digest)
            .await
        {
            Ok(response) => response,
            Err(UpstreamError::Status(status)) if absent_upstream(status) => return Ok(None),
            Err(err) => return Ok(Some(upstream_manifest_error(&err))),
        };
        let (manifest, canonical) = store_manifest(state, index, repo, None, response).await?;
        Ok(Some(if canonical == digest {
            manifest_response(&manifest, digest, head)
        } else {
            error_response(
                ErrorCode::ManifestInvalid,
                &format!("upstream digest {canonical} does not match requested {digest}"),
            )
        }))
    }

    /// Try one member for a manifest by tag. A hosted member reads its cached tag; an online proxy
    /// serves the tag from cache while it is fresh and revalidates once the freshness window elapses.
    /// `None` means a miss, so the caller tries the next member.
    async fn member_tag(
        &self,
        state: &AppState,
        member: &Index,
        repo: &str,
        tag: &str,
        head: bool,
    ) -> Result<Option<Response>, ServeError> {
        let Some(client) = proxy_client(&member.kind) else {
            return Ok(match store::get_tag(&state.meta, &member.name, repo, tag)? {
                Some(digest) => store::get_manifest(&state.meta, &digest)?
                    .map(|manifest| manifest_response(&manifest, &digest, head)),
                None => None,
            });
        };
        if let Some(response) = fresh_tag(state, &member.name, repo, tag, head)? {
            return Ok(Some(response));
        }
        // Single-flight the revalidation: a burst of pulls of the same stale tag makes one upstream
        // request, and the followers re-read the tag the leader just refreshed.
        let gate_key = format!("oci\u{0}tag\u{0}{}\u{0}{repo}\u{0}{tag}", member.name);
        let gate = flight_gate(state, &gate_key);
        let _guard = gate.lock().await;
        if let Some(response) = fresh_tag(state, &member.name, repo, tag, head)? {
            return Ok(Some(response));
        }
        let fetched = self.revalidate_tag(state, client, &member.name, repo, tag, head).await;
        state.inflight.lock().expect("inflight lock").remove(&gate_key);
        fetched
    }

    /// Fetch a proxy tag from upstream and store it, returning the served manifest.
    async fn revalidate_tag(
        &self,
        state: &AppState,
        client: &UpstreamClient,
        index: &str,
        repo: &str,
        tag: &str,
        head: bool,
    ) -> Result<Option<Response>, ServeError> {
        match self
            .upstream
            .manifest(client.base_url(), client.auth(), repo, tag)
            .await
        {
            Ok(response) => {
                let (manifest, canonical) = store_manifest(state, index, repo, Some(tag), response).await?;
                Ok(Some(manifest_response(&manifest, &canonical, head)))
            }
            Err(UpstreamError::Status(status)) if absent_upstream(status) => Ok(None),
            Err(err) => Ok(Some(upstream_manifest_error(&err))),
        }
    }
}

/// Store a manifest a client pushed, mapping the tag or verifying the digest reference.
pub(super) async fn put_manifest(
    state: &Arc<AppState>,
    headers: &HeaderMap,
    body: Body,
    name: &str,
    reference: &Reference,
) -> Result<Response, ServeError> {
    let (index, repo) = match resolve_writable(state, name, headers) {
        Ok(target) => target,
        Err(response) => return Ok(response),
    };
    if policy_blocks(index, PolicyAction::Upload, &repo) {
        return Ok(error_response(ErrorCode::Denied, "image name is blocked by policy"));
    }
    let media_type = headers
        .get(header::CONTENT_TYPE)
        .and_then(|value| value.to_str().ok())
        .unwrap_or(DEFAULT_MANIFEST_TYPE)
        .to_owned();
    if !is_supported_manifest_type(&media_type) {
        return Ok(error_response(
            ErrorCode::ManifestInvalid,
            &format!("unsupported manifest media type {media_type}"),
        ));
    }
    let bytes = axum::body::to_bytes(body, MAX_MANIFEST_BYTES)
        .await
        .map_err(|err| ServeError::Transport(err.to_string()))?;
    let canonical = format!("sha256:{}", Digest::of(&bytes).as_str());
    if let Reference::Digest(digest) = reference
        && *digest != canonical
    {
        return Ok(error_response(
            ErrorCode::DigestInvalid,
            "manifest bytes do not match the digest",
        ));
    }
    if let Some(response) = missing_manifest_reference(state, &bytes)? {
        return Ok(response);
    }
    let manifest = Manifest {
        media_type: media_type.clone(),
        bytes: bytes.to_vec(),
    };
    store::put_manifest(&state.meta, &canonical, &manifest)?;
    if let Reference::Tag(tag) = reference {
        store::put_tag(&state.meta, &index.name, &repo, tag, &canonical)?;
    }
    let subject = record_referrer(state, &index.name, &repo, &canonical, &media_type, &bytes)?;
    let location = format!("/v2/{name}/manifests/{canonical}");
    // A pushed manifest is a published image, the OCI analogue of a distribution upload; blob pushes
    // are its layer bytes and are not counted separately.
    state.metrics.record(Event::Upload {
        route: index.route.clone(),
        project: repo.clone(),
    });
    let version = match reference {
        Reference::Tag(tag) => Some(tag.clone()),
        Reference::Digest(_) => None,
    };
    emit_webhook(
        state,
        headers,
        WebhookEventKind::Upload,
        index,
        &repo,
        version,
        Some(canonical.clone()),
    );
    Ok(manifest_created(&location, &canonical, subject.as_deref()))
}

/// If a pushed manifest declares a subject, store its descriptor under that subject for the referrers
/// API and return the subject digest so the response can echo it in `OCI-Subject`.
fn record_referrer(
    state: &AppState,
    index: &str,
    repo: &str,
    canonical: &str,
    media_type: &str,
    bytes: &[u8],
) -> Result<Option<String>, ServeError> {
    let Ok(document) = serde_json::from_slice::<serde_json::Value>(bytes) else {
        return Ok(None);
    };
    let Some(subject) = document["subject"]["digest"].as_str() else {
        return Ok(None);
    };
    let mut descriptor = serde_json::json!({
        "mediaType": media_type,
        "digest": canonical,
        "size": bytes.len(),
    });
    let artifact_type = document["artifactType"]
        .as_str()
        .or_else(|| document["config"]["mediaType"].as_str());
    if let Some(artifact_type) = artifact_type {
        descriptor["artifactType"] = serde_json::Value::from(artifact_type);
    }
    if let Some(annotations) = document.get("annotations").filter(|value| value.is_object()) {
        descriptor["annotations"] = annotations.clone();
    }
    let descriptor = descriptor.to_string();
    store::put_referrer(&state.meta, index, repo, subject, canonical, descriptor.as_bytes())?;
    Ok(Some(subject.to_owned()))
}

/// Delete a manifest by tag or digest.
pub(super) fn delete_manifest(
    state: &Arc<AppState>,
    headers: &HeaderMap,
    name: &str,
    reference: &Reference,
) -> Result<Response, ServeError> {
    let (index, repo) = match resolve_writable(state, name, headers) {
        Ok(target) => target,
        Err(response) => return Ok(response),
    };
    let (removed, version, digest) = match reference {
        Reference::Tag(tag) => (
            store::delete_tag(&state.meta, &index.name, &repo, tag)?,
            Some(tag.clone()),
            None,
        ),
        Reference::Digest(digest) => (store::delete_manifest(&state.meta, digest)?, None, Some(digest.clone())),
    };
    Ok(if removed {
        emit_webhook(state, headers, WebhookEventKind::Delete, index, &repo, version, digest);
        accepted()
    } else {
        error_response(ErrorCode::ManifestUnknown, "manifest unknown")
    })
}

/// Read an upstream manifest response into storage, keyed by the sha256 of its exact bytes, updating
/// the tag mapping when the pull was by tag. Returns the stored manifest and its canonical digest.
pub async fn store_manifest(
    state: &AppState,
    index: &str,
    repo: &str,
    tag: Option<&str>,
    response: reqwest::Response,
) -> Result<(Manifest, String), ServeError> {
    let media_type = response
        .headers()
        .get(header::CONTENT_TYPE)
        .and_then(|value| value.to_str().ok())
        .unwrap_or(DEFAULT_MANIFEST_TYPE)
        .to_owned();
    let advertised = response
        .headers()
        .get(DOCKER_CONTENT_DIGEST)
        .and_then(|value| value.to_str().ok())
        .map(str::to_owned);
    let bytes = bounded_body(response, MAX_MANIFEST_BYTES).await?;
    let canonical = format!("sha256:{}", Digest::of(&bytes).as_str());
    // A corrupting proxy or CDN between velodex and upstream could return altered bytes; if upstream
    // advertised a digest, the bytes must hash to it, or the manifest is not stored.
    if let Some(advertised) = advertised
        && advertised != canonical
    {
        return Err(ServeError::Transport(format!(
            "upstream digest {advertised} does not match manifest content {canonical}"
        )));
    }
    let manifest = Manifest {
        media_type,
        bytes: bytes.to_vec(),
    };
    store::put_manifest(&state.meta, &canonical, &manifest)?;
    if let Some(tag) = tag {
        store::put_tag(&state.meta, index, repo, tag, &canonical)?;
        store::set_tag_freshness(&state.meta, index, repo, tag, &canonical, (state.clock)())?;
    }
    Ok((manifest, canonical))
}

/// A `201 Created` for a stored manifest, echoing `OCI-Subject` when the manifest declared a subject.
fn manifest_created(location: &str, digest: &str, subject: Option<&str>) -> Response {
    let mut builder = Response::builder()
        .status(StatusCode::CREATED)
        .header(header::LOCATION, location)
        .header(DOCKER_CONTENT_DIGEST, digest);
    if let Some(subject) = subject {
        builder = builder.header("oci-subject", subject);
    }
    builder
        .body(Body::empty())
        .expect("created response builds from validated parts")
}

/// Build the response for a stored manifest, headers-only for a `HEAD`. The content length is set the
/// same either way, so a `HEAD` reports the size a `GET` would return.
fn manifest_response(manifest: &Manifest, digest: &str, head: bool) -> Response {
    let builder = Response::builder()
        .status(StatusCode::OK)
        .header(header::CONTENT_TYPE, &manifest.media_type)
        .header(DOCKER_CONTENT_DIGEST, digest)
        .header(header::CONTENT_LENGTH, manifest.bytes.len());
    let body = if head {
        Body::empty()
    } else {
        Body::from(manifest.bytes.clone())
    };
    builder
        .body(body)
        .expect("manifest response builds from validated header parts")
}

/// Whether a hosted push may store bytes under this media type: the OCI image manifest and index and
/// the Docker v2 schema-2 manifest and manifest list. A proxy stores whatever an upstream sends
/// verbatim, but an authoritative push rejects anything else rather than serving it back as a manifest.
fn is_supported_manifest_type(media_type: &str) -> bool {
    matches!(
        media_type,
        "application/vnd.oci.image.manifest.v1+json"
            | "application/vnd.oci.image.index.v1+json"
            | "application/vnd.docker.distribution.manifest.v2+json"
            | "application/vnd.docker.distribution.manifest.list.v2+json"
    )
}

/// The error response for a pushed manifest that names content the store does not hold: a config or
/// layer blob, or an image index's child manifest. A resolver would 404 on the missing piece after the
/// push "succeeded", so the push is rejected up front with `MANIFEST_BLOB_UNKNOWN`.
///
/// # Errors
/// Returns a store error if a child-manifest lookup fails.
fn missing_manifest_reference(state: &AppState, bytes: &[u8]) -> Result<Option<Response>, ServeError> {
    let (children, blobs) = store::manifest_descriptors(bytes);
    for blob in blobs {
        if !store::blob_digest(&blob).is_some_and(|storage| state.blobs.exists(&storage)) {
            return Ok(Some(error_response(
                ErrorCode::ManifestBlobUnknown,
                &format!("referenced blob {blob} is not present"),
            )));
        }
    }
    for child in children {
        if store::get_manifest(&state.meta, &child)?.is_none() {
            return Ok(Some(error_response(
                ErrorCode::ManifestBlobUnknown,
                &format!("referenced manifest {child} is not present"),
            )));
        }
    }
    Ok(None)
}

/// Serve a proxy tag from cache while its recorded fetch is still within the freshness window, or
/// `None` to force a revalidation. A tag is mutable upstream, so it is trusted only for `ttl_secs`
/// after the last fetch; a manifest missing under a still-fresh record also forces a revalidation.
fn fresh_tag(state: &AppState, index: &str, repo: &str, tag: &str, head: bool) -> Result<Option<Response>, ServeError> {
    let Some((fetched_at, digest)) = store::tag_freshness(&state.meta, index, repo, tag)? else {
        return Ok(None);
    };
    if (state.clock)().saturating_sub(fetched_at) >= state.ttl_secs {
        return Ok(None);
    }
    Ok(store::get_manifest(&state.meta, &digest)?.map(|manifest| manifest_response(&manifest, &digest, head)))
}

/// A gateway fault for an upstream manifest failure. Callers treat an "absent" status as a member
/// miss before reaching here, so anything left is a real transport, server, or rate-limit error.
fn upstream_manifest_error(err: &UpstreamError) -> Response {
    upstream_error_response(err, "manifest")
}
