use axum::body::Body;
use axum::http::{HeaderMap, StatusCode, header};
use axum::response::Response;
use std::sync::Arc;

use peryx_driver::ServingState;

use crate::error::{ErrorCode, error_response, error_response_with_status};
use crate::registry::authority::{
    EpochCommit, authority_moved, claim_repository_home, commit_epoch, release_reservation, repository_epoch,
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
    let media_type = match manifest_media_type(headers) {
        Ok(media_type) => media_type,
        Err(response) => return Ok(*response),
    };
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
    let version = reference_tag(reference).map(str::to_owned);
    let webhook = prepare_webhook(
        state,
        &Requester {
            headers,
            identity: &identity,
        },
        crate::webhook::MANIFEST_PUSH,
        index,
        &repo,
        version.as_deref(),
        Some(&canonical),
    );
    let mutation = commit_manifest(
        state,
        fence,
        ManifestWrite {
            index: &index.name,
            repo: &repo,
            canonical: &canonical,
            media_type: &media_type,
            bytes: &bytes,
            reference,
            reservation: &reservation,
            journal,
            webhook,
        },
    )
    .await?;
    let subject = match mutation {
        EpochCommit::Committed(committed) => committed,
        EpochCommit::Fenced => {
            release_reservation(state, reservation)?;
            return Ok(authority_moved());
        }
    };
    let location = format!("/v2/{name}/manifests/{canonical}");
    state.metrics.record(Observation::Write {
        repository: index.route.clone(),
        resource: repo.clone(),
    });
    peryx_events::webhook::notify(state.as_ref());
    Ok(manifest_created(&location, &canonical, subject.as_deref()))
}

fn manifest_media_type(headers: &HeaderMap) -> Result<String, Box<Response>> {
    let declared = headers
        .get(header::CONTENT_TYPE)
        .and_then(|value| value.to_str().ok())
        .unwrap_or(DEFAULT_MANIFEST_TYPE);
    // OCI treats Content-Type parameters as metadata on the same manifest media type.
    let media_type = declared
        .split_once(';')
        .map_or(declared, |(base, _)| base)
        .trim()
        .to_owned();
    if is_supported_manifest_type(&media_type) {
        Ok(media_type)
    } else {
        Err(Box::new(error_response(
            ErrorCode::ManifestInvalid,
            &format!("unsupported manifest media type {media_type}"),
        )))
    }
}

struct ManifestWrite<'a> {
    index: &'a str,
    repo: &'a str,
    canonical: &'a str,
    media_type: &'a str,
    bytes: &'a [u8],
    reference: &'a Reference,
    reservation: &'a Option<peryx_storage::meta::QuotaReservationRecord>,
    journal: crate::outbox::Outbox,
    webhook: Option<peryx_storage::meta::WebhookEventIntent>,
}

async fn commit_manifest(
    state: &ServingState,
    fence: u64,
    write: ManifestWrite<'_>,
) -> Result<EpochCommit<Option<String>>, ServeError> {
    commit_epoch(state, write.repo, fence, |lease| {
        lease.guard()?;
        crate::quota::publish_manifest(
            &state.meta,
            crate::quota::ManifestCommit {
                index: write.index,
                repo: write.repo,
                canonical: write.canonical,
                manifest: &Manifest {
                    media_type: write.media_type.to_owned(),
                    bytes: write.bytes.to_vec(),
                },
                reference: write.reference,
                reservation: write.reservation.clone(),
                journal: write.journal,
                webhook: write.webhook,
            },
        )?;
        lease.guard()?;
        let search_invalidation = crate::search_oci::SearchInvalidationGuard::arm(state, write.repo);
        store::record_content_placement(&state.meta, write.canonical, store::OciArtifactOrigin::Pushed, true)?;
        drop(search_invalidation);
        lease.guard()?;
        record_referrer(
            state,
            write.index,
            write.repo,
            write.canonical,
            write.media_type,
            write.bytes,
        )
    })
    .await
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
    let info = store::TrashInfo {
        deleted_at_unix: (state.clock)(),
        actor: peryx_events::security::actor(&identity),
        reason: query_params(query).remove("reason"),
    };
    let requester = Requester {
        headers,
        identity: &identity,
    };
    let mutation = commit_epoch(state, &repo, fence, |lease| {
        lease.guard()?;
        Ok(match reference {
            Reference::Tag(tag) => {
                let digest = store::trash_tag(&state.meta, &index.name, &repo, tag, &info, journal, |digest| {
                    prepare_webhook(
                        state,
                        &requester,
                        crate::webhook::MANIFEST_DELETE,
                        index,
                        &repo,
                        Some(tag),
                        Some(digest),
                    )
                })?;
                (digest.is_some(), digest.is_some())
            }
            Reference::Digest(digest) => {
                let webhook = prepare_webhook(
                    state,
                    &requester,
                    crate::webhook::MANIFEST_DELETE,
                    index,
                    &repo,
                    None,
                    Some(digest),
                );
                let removed = store::trash_manifest(&state.meta, &index.name, &repo, digest, &info, journal, webhook)?;
                (removed.is_some(), removed.is_some_and(|tags| tags > 0))
            }
        })
    })
    .await?;
    let EpochCommit::Committed((removed, search_changed)) = mutation else {
        return Ok(authority_moved());
    };
    if search_changed {
        state.invalidate_search_resource(&repo);
    }
    Ok(if removed {
        peryx_events::webhook::notify(state.as_ref());
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
    let requester = Requester {
        headers,
        identity: &identity,
    };
    let mutation = commit_epoch(state, &repo, fence, |lease| {
        lease.guard()?;
        Ok(match reference {
            Reference::Tag(tag) => match store::restore_tag(&state.meta, &index.name, &repo, tag, journal, |digest| {
                prepare_webhook(
                    state,
                    &requester,
                    crate::webhook::MANIFEST_RESTORE,
                    index,
                    &repo,
                    Some(tag),
                    Some(digest),
                )
            })? {
                store::RestoreTagOutcome::Missing => None,
                store::RestoreTagOutcome::Restored { digest } => Some((digest, 1, Vec::new())),
            },
            Reference::Digest(digest) => {
                let outcome = store::restore_manifest(
                    &state.meta,
                    &index.name,
                    &repo,
                    digest,
                    journal,
                    prepare_webhook(
                        state,
                        &requester,
                        crate::webhook::MANIFEST_RESTORE,
                        index,
                        &repo,
                        None,
                        Some(digest),
                    ),
                )?;
                match outcome {
                    store::RestoreManifestOutcome::Missing => None,
                    store::RestoreManifestOutcome::Restored { restored, conflicts } => {
                        Some((digest.clone(), restored.len(), conflicts))
                    }
                }
            }
        })
    })
    .await?;
    let (digest, restored, conflicts) = match mutation {
        EpochCommit::Committed(Some(restored)) => restored,
        EpochCommit::Committed(None) => {
            return Ok(error_response(ErrorCode::ManifestUnknown, "manifest unknown"));
        }
        EpochCommit::Fenced => return Ok(authority_moved()),
    };
    if restored > 0 {
        state.invalidate_search_resource(&repo);
    }
    peryx_events::webhook::notify(state.as_ref());
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
/// The error response for a pushed manifest that names content this repository does not hold: a
/// config or layer blob, or an image index's child manifest. A resolver would 404 on the missing piece
/// after the push "succeeded", so the push is rejected up front with `MANIFEST_BLOB_UNKNOWN`.
///
/// Blobs and manifests are content-addressed across every repository, so presence in the store is not
/// membership in this one. A writer who can push here but cannot read the repository that owns a
/// digest must not be able to name it and have the index resolve to those bytes, whatever the index's
/// quota policy.
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
            || !store::manifest_is_member(&state.meta, &index.name, repo, &child)?
        {
            return Ok(Some(error_response(
                ErrorCode::ManifestBlobUnknown,
                &format!("referenced manifest {child} is not present"),
            )));
        }
    }
    Ok(None)
}
