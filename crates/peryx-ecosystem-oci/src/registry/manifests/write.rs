//! The manifest write path: push validation, referrer recording, and delete.

use axum::body::Body;
use axum::http::{HeaderMap, StatusCode, header};
use axum::response::Response;
use std::sync::Arc;

use peryx_driver::ServingState;
use peryx_events::webhook::WebhookEventKind;

use crate::error::{ErrorCode, error_response, error_response_with_status};
use crate::store::{self, Manifest};

use super::*;

/// Store a manifest a client pushed, mapping the tag or verifying the digest reference.
pub(in crate::registry) async fn put_manifest(
    state: &Arc<ServingState>,
    headers: &HeaderMap,
    body: Body,
    name: &str,
    reference: &Reference,
) -> Result<Response, ServeError> {
    let (index, repo, identity) = match resolve_writable(state, name, headers, Action::Write) {
        Ok(target) => target,
        Err(response) => return Ok(response),
    };
    if policy_blocks(index, PolicyAction::Upload, &repo) {
        return Ok(error_response(ErrorCode::Denied, "image name is blocked by policy"));
    }
    let declared = headers
        .get(header::CONTENT_TYPE)
        .and_then(|value| value.to_str().ok())
        .unwrap_or(DEFAULT_MANIFEST_TYPE);
    // The spec says a registry SHOULD ignore Content-Type parameters, so match and store the base
    // media type only: `application/…+json; charset=utf-8` is the same manifest type as the bare form.
    let media_type = declared
        .split_once(';')
        .map_or(declared, |(base, _)| base)
        .trim()
        .to_owned();
    if !is_supported_manifest_type(&media_type) {
        return Ok(error_response(
            ErrorCode::ManifestInvalid,
            &format!("unsupported manifest media type {media_type}"),
        ));
    }
    let bytes = match axum::body::to_bytes(body, MAX_MANIFEST_BYTES).await {
        Ok(bytes) => bytes,
        Err(err) if is_length_limit(&err) => {
            return Ok(error_response_with_status(
                StatusCode::PAYLOAD_TOO_LARGE,
                ErrorCode::SizeInvalid,
                &format!("manifest exceeds the {MAX_MANIFEST_BYTES}-byte limit"),
            ));
        }
        Err(err) => return Err(ServeError::Transport(err.to_string())),
    };
    let canonical = format!("sha256:{}", Digest::of(&bytes).as_str());
    if let Reference::Digest(digest) = reference
        && *digest != canonical
    {
        return Ok(error_response(
            ErrorCode::DigestInvalid,
            "manifest bytes do not match the digest",
        ));
    }
    if let Some(response) = missing_manifest_reference(state, &index.name, &repo, &bytes).await? {
        return Ok(response);
    }
    let manifest = Manifest {
        media_type: media_type.clone(),
        bytes: bytes.to_vec(),
    };
    let version = match reference {
        Reference::Tag(tag) => Some(tag.as_str()),
        Reference::Digest(_) => None,
    };
    // A re-push of the same manifest under the same reference is already accounted, so it must not
    // reserve a fresh version or byte allocation.
    let reservation =
        if crate::quota::manifest_already_published(&state.meta, &index.name, &repo, &canonical, reference)? {
            None
        } else {
            match crate::quota::admit_push(state, index, &repo, version, &canonical, bytes.len() as u64)? {
                crate::quota::Admission::Rejected(response) => return Ok(response),
                crate::quota::Admission::Unmetered => None,
                crate::quota::Admission::Reserved(record) => Some(record),
            }
        };
    if crate::quota::publish_manifest(
        &state.meta,
        &index.name,
        &repo,
        &canonical,
        &manifest,
        reference,
        reservation,
    )? {
        state.bump_search_epoch();
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
        &Requester {
            headers,
            identity: &identity,
        },
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
    state: &ServingState,
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
    state: &Arc<ServingState>,
    headers: &HeaderMap,
    name: &str,
    reference: &Reference,
) -> Result<Response, ServeError> {
    let (index, repo, identity) = match resolve_writable(state, name, headers, Action::Delete) {
        Ok(target) => target,
        Err(response) => return Ok(response),
    };
    let (removed, version, digest) = match reference {
        Reference::Tag(tag) => {
            let removed = store::delete_tag(&state.meta, &index.name, &repo, tag)?;
            if removed {
                state.bump_search_epoch();
            }
            (removed, Some(tag.clone()), None)
        }
        Reference::Digest(digest) => (
            delete_manifest_by_digest(state, &index.name, &repo, digest)?,
            None,
            Some(digest.clone()),
        ),
    };
    Ok(if removed {
        emit_webhook(
            state,
            &Requester {
                headers,
                identity: &identity,
            },
            WebhookEventKind::Delete,
            index,
            &repo,
            version,
            digest,
        );
        accepted()
    } else {
        error_response(ErrorCode::ManifestUnknown, "manifest unknown")
    })
}
/// Delete a manifest by digest, mirroring blob retention. Manifests are one global content-addressed
/// pool shared across indexes, so clean this repo's own tags and referrers to the digest, drop its
/// record that this repo serves the digest, then unlink the global record only when nothing else still
/// references it. Reports whether anything changed, so an untouched absent digest still answers
/// `404 MANIFEST_UNKNOWN`.
fn delete_manifest_by_digest(state: &ServingState, index: &str, repo: &str, digest: &str) -> Result<bool, ServeError> {
    let present = store::get_manifest(&state.meta, digest)?.is_some();
    let (removed_tags, removed_referrers) = store::delete_repo_tags_to(&state.meta, index, repo, digest)?;
    if removed_tags > 0 {
        state.bump_search_epoch();
    }
    if present && !store::referenced_manifest_digests(&state.meta)?.contains(digest) {
        store::delete_manifest(&state.meta, digest)?;
    }
    store::prune_manifest_membership(&state.meta, index, repo, digest)?;
    Ok(present || removed_tags > 0 || removed_referrers > 0)
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
/// Whether a body-read failure is axum's length-limit rejection rather than a transport fault, so an
/// oversize manifest answers `413` while a broken transfer stays a gateway error.
fn is_length_limit(err: &axum::Error) -> bool {
    let mut source: Option<&(dyn std::error::Error + 'static)> = Some(err);
    while let Some(err) = source {
        if err.is::<http_body_util::LengthLimitError>() {
            return true;
        }
        source = err.source();
    }
    false
}
/// The error response for a pushed manifest that names content the store does not hold: a config or
/// layer blob, or an image index's child manifest. A resolver would 404 on the missing piece after the
/// push "succeeded", so the push is rejected up front with `MANIFEST_BLOB_UNKNOWN`.
///
/// # Errors
/// Returns a store error if a membership lookup fails.
async fn missing_manifest_reference(
    state: &ServingState,
    index: &str,
    repo: &str,
    bytes: &[u8],
) -> Result<Option<Response>, ServeError> {
    let (children, blobs) = store::manifest_descriptors(bytes);
    for blob in blobs {
        let present = if let Some(storage) = store::blob_digest(&blob) {
            state
                .blobs
                .head(&storage)
                .await
                .map_err(super::super::blobs::blob_fault)?
                .is_some()
        } else {
            false
        };
        if !present || !store::blob_is_member(&state.meta, index, repo, &blob)? {
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
