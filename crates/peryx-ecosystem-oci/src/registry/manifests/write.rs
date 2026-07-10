//! The manifest write path: push validation, referrer recording, and delete.

use axum::body::Body;
use axum::http::{HeaderMap, StatusCode, header};
use axum::response::Response;
use std::sync::Arc;

use peryx_driver::AppState;
use peryx_events::webhook::WebhookEventKind;

use crate::error::{ErrorCode, error_response};
use crate::store::{self, Manifest};

use super::*;

/// Store a manifest a client pushed, mapping the tag or verifying the digest reference.
pub(in crate::registry) async fn put_manifest(
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
pub(in crate::registry) fn delete_manifest(
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
