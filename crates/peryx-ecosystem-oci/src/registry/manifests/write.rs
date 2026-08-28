use axum::body::Body;
use axum::http::{HeaderMap, StatusCode, header};
use axum::response::Response;
use std::sync::Arc;

use peryx_driver::ServingState;

use crate::error::{ErrorCode, error_response, error_response_with_status};
use crate::registry::authority::{
    authority_moved, claim_repository_home, epoch_admits, release_reservation, repository_epoch,
};
use crate::store::{self, Manifest};

use super::*;

pub(in crate::registry) async fn put_manifest(
    state: &Arc<ServingState>,
    headers: &HeaderMap,
    body: Body,
    name: &str,
    reference: &Reference,
    journal: crate::outbox::Outbox,
) -> Result<Response, ServeError> {
    let (index, repo, identity) = match resolve_writable(state, name, headers, Action::Write) {
        Ok(target) => target,
        Err(rejection) => return Ok(rejection.into_response()),
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
    if let Some(response) = policy_size_denial(index, &repo, bytes.len() as u64) {
        return Ok(response);
    }
    let canonical = format!("sha256:{}", Digest::of(&bytes).as_str());
    if let Reference::Digest(digest) = reference
        && *digest != canonical
    {
        return Ok(error_response(
            ErrorCode::DigestInvalid,
            "manifest bytes do not match the digest",
        ));
    }
    if let Some(response) = missing_manifest_reference(state, index, &repo, &bytes).await? {
        return Ok(response);
    }
    let fence = match claim_repository_home(state, &repo).await {
        Ok(fence) => fence,
        Err(response) => return Ok(response),
    };
    let reservation = match reserve_push(state, index, &repo, reference, &canonical, bytes.len() as u64)? {
        PushReservation::Rejected(response) => return Ok(response),
        PushReservation::Admitted(reservation) => reservation,
    };
    // Re-admit the leased epoch immediately before the tag and manifest commit. A superseded push
    // returns the momentary reservation and lands nothing, so a stale home cannot expose a manifest or
    // repoint a tag; the digest bytes it uploaded stay content-addressed and unpublished.
    if !epoch_admits(state, &repo, fence).await {
        release_reservation(state, reservation)?;
        return Ok(authority_moved());
    }
    if crate::quota::publish_manifest(
        &state.meta,
        crate::quota::ManifestCommit {
            index: &index.name,
            repo: &repo,
            canonical: &canonical,
            manifest: &Manifest {
                media_type: media_type.clone(),
                bytes: bytes.to_vec(),
            },
            reference,
            reservation,
            journal,
        },
    )? {
        state.bump_search_epoch();
    }
    store::record_content_placement(&state.meta, &canonical, store::OciArtifactOrigin::Pushed, true)?;
    let subject = record_referrer(state, &index.name, &repo, &canonical, &media_type, &bytes)?;
    let location = format!("/v2/{name}/manifests/{canonical}");
    state.metrics.record(Observation::Write {
        repository: index.route.clone(),
        resource: repo.clone(),
    });
    let version = reference_tag(reference).map(str::to_owned);
    emit_webhook(
        state,
        &Requester {
            headers,
            identity: &identity,
        },
        crate::webhook::MANIFEST_PUSH,
        index,
        &repo,
        version.as_deref(),
        Some(canonical.as_str()),
    );
    Ok(manifest_created(&location, &canonical, subject.as_deref()))
}
/// The outcome of reserving quota for a manifest push: the quota rejection response to return, or the
/// reservation to commit with the manifest (`None` when the push is a re-push or the index is unmetered).
enum PushReservation {
    Rejected(Response),
    Admitted(Option<peryx_storage::meta::QuotaReservationRecord>),
}

/// Reserve quota for a manifest push. A re-push of the same manifest under the same reference is already
/// accounted, so it reserves nothing; an unmetered index reserves nothing; a push that crosses a quota
/// yields the rejection response the caller returns unchanged.
fn reserve_push(
    state: &ServingState,
    index: &Index,
    repo: &str,
    reference: &Reference,
    canonical: &str,
    bytes: u64,
) -> Result<PushReservation, ServeError> {
    if crate::quota::manifest_already_published(&state.meta, &index.name, repo, canonical, reference)? {
        return Ok(PushReservation::Admitted(None));
    }
    Ok(
        match crate::quota::admit_push(state, index, repo, reference_tag(reference), canonical, bytes)? {
            crate::quota::Admission::Rejected(response) => PushReservation::Rejected(response),
            crate::quota::Admission::Unmetered => PushReservation::Admitted(None),
            crate::quota::Admission::Reserved(record) => PushReservation::Admitted(Some(record)),
        },
    )
}
fn reference_tag(reference: &Reference) -> Option<&str> {
    match reference {
        Reference::Tag(tag) => Some(tag),
        Reference::Digest(_) => None,
    }
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
pub(in crate::registry) async fn delete_manifest(
    state: &Arc<ServingState>,
    headers: &HeaderMap,
    name: &str,
    reference: &Reference,
    query: &str,
    journal: crate::outbox::Outbox,
) -> Result<Response, ServeError> {
    let (index, repo, identity) = match resolve_writable(state, name, headers, Action::Delete) {
        Ok(target) => target,
        Err(rejection) => return Ok(rejection.into_response()),
    };
    // A delete is a metadata mutation under the repository home, so it is fenced by the committed epoch:
    // a stale home cannot trash a tag or manifest the surviving home still serves.
    let fence = repository_epoch(state, &repo).await;
    if !epoch_admits(state, &repo, fence).await {
        return Ok(authority_moved());
    }
    let info = store::TrashInfo {
        deleted_at_unix: (state.clock)(),
        actor: peryx_events::security::actor(&identity),
        reason: query_params(query).remove("reason"),
    };
    let (removed, version, digest) = match reference {
        Reference::Tag(tag) => {
            let digest = store::trash_tag(&state.meta, &index.name, &repo, tag, &info, journal)?;
            if digest.is_some() {
                state.bump_search_epoch();
            }
            (digest.is_some(), Some(tag.clone()), digest)
        }
        Reference::Digest(digest) => {
            let removed = store::trash_manifest(&state.meta, &index.name, &repo, digest, &info, journal)?;
            if removed.is_some_and(|tags| tags > 0) {
                state.bump_search_epoch();
            }
            (removed.is_some(), None, Some(digest.clone()))
        }
    };
    Ok(if removed {
        emit_webhook(
            state,
            &Requester {
                headers,
                identity: &identity,
            },
            crate::webhook::MANIFEST_DELETE,
            index,
            &repo,
            version.as_deref(),
            digest.as_deref(),
        );
        accepted()
    } else {
        error_response(ErrorCode::ManifestUnknown, "manifest unknown")
    })
}

/// Restore a retained manifest reference without overwriting a tag concurrently pushed elsewhere.
pub(in crate::registry) async fn restore_manifest(
    state: &Arc<ServingState>,
    headers: &HeaderMap,
    name: &str,
    reference: &Reference,
    journal: crate::outbox::Outbox,
) -> Result<Response, ServeError> {
    let (index, repo, identity) = match resolve_writable(state, name, headers, Action::Delete) {
        Ok(target) => target,
        Err(rejection) => return Ok(rejection.into_response()),
    };
    // Restoring re-exposes a trashed manifest or tag, so it is fenced like a publish: a stale home
    // cannot bring back a reference the surviving home has moved past.
    let fence = repository_epoch(state, &repo).await;
    if !epoch_admits(state, &repo, fence).await {
        return Ok(authority_moved());
    }
    let (version, digest, restored, conflicts) = match reference {
        Reference::Tag(tag) => match store::restore_tag(&state.meta, &index.name, &repo, tag, journal)? {
            store::RestoreTagOutcome::Missing => {
                return Ok(error_response(ErrorCode::ManifestUnknown, "manifest unknown"));
            }
            store::RestoreTagOutcome::Restored { digest } => (Some(tag.clone()), digest, 1, Vec::new()),
        },
        Reference::Digest(digest) => match store::restore_manifest(&state.meta, &index.name, &repo, digest, journal)? {
            store::RestoreManifestOutcome::Missing => {
                return Ok(error_response(ErrorCode::ManifestUnknown, "manifest unknown"));
            }
            store::RestoreManifestOutcome::Restored { restored, conflicts } => {
                (None, digest.clone(), restored.len(), conflicts)
            }
        },
    };
    if restored > 0 {
        state.bump_search_epoch();
    }
    emit_webhook(
        state,
        &Requester {
            headers,
            identity: &identity,
        },
        crate::webhook::MANIFEST_RESTORE,
        index,
        &repo,
        version.as_deref(),
        Some(digest.as_str()),
    );
    let mut builder = Response::builder()
        .status(StatusCode::ACCEPTED)
        .header(DOCKER_CONTENT_DIGEST, digest)
        .header("oci-restored-tags", restored);
    if !conflicts.is_empty() {
        builder = builder.header("oci-tag-conflicts", conflicts.join(","));
    }
    Ok(builder
        .body(Body::empty())
        .expect("restore response builds from validated parts"))
}
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
    index: &Index,
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
                .map_err(super::super::ServeError::from)?
                .is_some()
        } else {
            false
        };
        if !present || !store::blob_is_member(&state.meta, &index.name, repo, &blob)? {
            return Ok(Some(error_response(
                ErrorCode::ManifestBlobUnknown,
                &format!("referenced blob {blob} is not present"),
            )));
        }
    }
    for child in children {
        if store::get_manifest(&state.meta, &child)?.is_none()
            || index.policy.enforces_quota() && !store::manifest_is_member(&state.meta, &index.name, repo, &child)?
        {
            return Ok(Some(error_response(
                ErrorCode::ManifestBlobUnknown,
                &format!("referenced manifest {child} is not present"),
            )));
        }
    }
    Ok(None)
}
