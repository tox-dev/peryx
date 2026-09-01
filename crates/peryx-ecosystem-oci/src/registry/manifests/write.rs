use axum::body::Body;
use axum::http::{HeaderMap, StatusCode, header};
use axum::response::Response;
use std::sync::Arc;

use peryx_driver::ServingState;

use crate::error::{ErrorCode, error_response, error_response_with_status};
use crate::registry::acknowledge::{MetadataAck, acknowledge_metadata};
use crate::registry::authority::{
    EpochCommit, authority_moved, claim_repository_home, commit_epoch, release_reservation, repository_epoch,
};
use crate::store::{self, Manifest, ManifestSchema};

use super::*;

pub(in crate::registry) async fn put_manifest(
    state: &Arc<ServingState>,
    headers: &HeaderMap,
    body: Body,
    name: &str,
    reference: &Reference,
    journal: crate::outbox::Outbox,
) -> Result<Response, ServeError> {
    let (index, repo, identity) = match resolve_uploadable(state, name, headers) {
        Ok(target) => target,
        Err(rejection) => return Ok(rejection.into_response()),
    };
    let manifest = match read_manifest(headers, body, index, &repo, reference).await {
        Ok(manifest) => manifest,
        Err(ManifestInputError::Rejected(response)) => return Ok(*response),
        Err(ManifestInputError::Transport(error)) => return Err(error),
    };
    let reference_key = manifest_reference(reference);
    let operation = journal.then(|| manifest_operation(&index.name, &repo, &manifest.canonical));
    let _guard = if let Some(operation) = &operation {
        Some(flight_gate(state, operation).lock().await)
    } else {
        None
    };
    let fence = match claim_repository_home(state, &repo).await {
        Ok(fence) => fence,
        Err(response) => return Ok(response),
    };
    let referrer = referrer_of(
        &manifest.canonical,
        &manifest.media_type,
        manifest.bytes.len(),
        &manifest.document,
    );
    if let Some(operation) = &operation
        && let Some(response) = resume_manifest(
            state,
            ManifestCompletion::new(
                index,
                &repo,
                name,
                &manifest.canonical,
                referrer.as_ref().map(|referrer| referrer.subject.as_str()),
                operation,
            ),
            fence,
            &reference_key,
        )
        .await?
    {
        return Ok(response);
    }
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
        Some(&manifest.canonical),
    );
    publish_and_acknowledge_manifest(
        state,
        ManifestPublication {
            index,
            repo: &repo,
            name,
            reference,
            journal,
            manifest,
            referrer,
            operation,
            reference_key,
            fence,
            webhook,
        },
    )
    .await
}

async fn publish_and_acknowledge_manifest(
    state: &ServingState,
    publication: ManifestPublication<'_>,
) -> Result<Response, ServeError> {
    let ManifestPublication {
        index,
        repo,
        name,
        reference,
        journal,
        manifest,
        referrer,
        operation,
        reference_key,
        fence,
        webhook,
    } = publication;
    if let Some(response) = missing_manifest_reference(state, index, repo, &manifest.document).await? {
        return Ok(response);
    }
    let manifest_size = manifest.bytes.len() as u64;
    let admission = reserve_push(state, index, repo, reference, &manifest.canonical, manifest_size)?;
    let reservation = match admission {
        PushReservation::Rejected(response) => return Ok(response),
        PushReservation::Admitted(reservation) => reservation,
    };
    let mutation = commit_manifest(
        state,
        fence,
        ManifestWrite {
            index: &index.name,
            repo,
            canonical: &manifest.canonical,
            media_type: &manifest.media_type,
            bytes: &manifest.bytes,
            referrer,
            reference,
            reservation: &reservation,
            journal,
            webhook,
            operation: operation.as_deref(),
            reference_key: &reference_key,
            now: (state.clock)(),
        },
    )
    .await?;
    let committed = match mutation {
        EpochCommit::Committed(committed) => committed,
        EpochCommit::Fenced => {
            release_reservation(state, reservation)?;
            return Ok(authority_moved());
        }
    };
    let Some(operation) = operation else {
        let location = format!("/v2/{name}/manifests/{}", manifest.canonical);
        record_manifest_success(state, index, repo);
        return Ok(manifest_created(
            &location,
            &manifest.canonical,
            committed.subject.as_deref(),
        ));
    };
    let commit = committed.commit.expect("manifest durability enables the journal");
    finish_manifest(
        state,
        ManifestCompletion::new(
            index,
            repo,
            name,
            &manifest.canonical,
            committed.subject.as_deref(),
            &operation,
        ),
        crate::quota::ManifestCheckpoint {
            reference: reference_key,
            epoch: fence,
            serial: commit.serial(),
        },
    )
    .await
}

struct ManifestPublication<'a> {
    index: &'a Index,
    repo: &'a str,
    name: &'a str,
    reference: &'a Reference,
    journal: crate::outbox::Outbox,
    manifest: ManifestInput,
    referrer: Option<store::Referrer>,
    operation: Option<String>,
    reference_key: String,
    fence: u64,
    webhook: Option<peryx_storage::meta::WebhookEventIntent>,
}

async fn read_manifest(
    headers: &HeaderMap,
    body: Body,
    index: &Index,
    repo: &str,
    reference: &Reference,
) -> Result<ManifestInput, ManifestInputError> {
    let (media_type, schema) = manifest_media_type(headers).map_err(ManifestInputError::Rejected)?;
    let bytes = match axum::body::to_bytes(body, MAX_MANIFEST_BYTES).await {
        Ok(bytes) => bytes,
        Err(error) if is_length_limit(&error) => {
            return Err(ManifestInputError::Rejected(Box::new(error_response_with_status(
                StatusCode::PAYLOAD_TOO_LARGE,
                ErrorCode::SizeInvalid,
                &format!("manifest exceeds the {MAX_MANIFEST_BYTES}-byte limit"),
            ))));
        }
        Err(error) => return Err(ManifestInputError::Transport(ServeError::Transport(error.to_string()))),
    };
    if let Some(response) = policy_size_denial(index, repo, bytes.len() as u64) {
        return Err(ManifestInputError::Rejected(Box::new(response)));
    }
    let canonical = format!("sha256:{}", Digest::of(&bytes).as_str());
    if let Reference::Digest(digest) = reference
        && *digest != canonical
    {
        return Err(ManifestInputError::Rejected(Box::new(error_response(
            ErrorCode::DigestInvalid,
            "manifest bytes do not match the digest",
        ))));
    }
    let document = schema.validate(&media_type, &bytes).map_err(|fault| {
        ManifestInputError::Rejected(Box::new(error_response(ErrorCode::ManifestInvalid, &fault.to_string())))
    })?;
    Ok(ManifestInput {
        media_type,
        bytes,
        canonical,
        document,
    })
}

struct ManifestInput {
    media_type: String,
    bytes: bytes::Bytes,
    canonical: String,
    document: serde_json::Value,
}

enum ManifestInputError {
    Rejected(Box<Response>),
    Transport(ServeError),
}

async fn resume_manifest(
    state: &ServingState,
    completion: ManifestCompletion<'_>,
    fence: u64,
    reference: &str,
) -> Result<Option<Response>, ServeError> {
    let Some(record) = state.begin_retryable_write(completion.operation)? else {
        return Ok(None);
    };
    if record.response.is_empty() {
        return Ok(None);
    }
    let checkpoint = crate::quota::ManifestCheckpoint::decode(&record.response)?;
    if checkpoint.epoch != fence || checkpoint.reference != reference {
        return Ok(None);
    }
    finish_manifest(state, completion, checkpoint).await.map(Some)
}

struct ManifestCompletion<'a> {
    index: &'a Index,
    repo: &'a str,
    name: &'a str,
    canonical: &'a str,
    subject: Option<&'a str>,
    operation: &'a str,
}

impl<'a> ManifestCompletion<'a> {
    const fn new(
        index: &'a Index,
        repo: &'a str,
        name: &'a str,
        canonical: &'a str,
        subject: Option<&'a str>,
        operation: &'a str,
    ) -> Self {
        Self {
            index,
            repo,
            name,
            canonical,
            subject,
            operation,
        }
    }
}

async fn finish_manifest(
    state: &ServingState,
    completion: ManifestCompletion<'_>,
    checkpoint: crate::quota::ManifestCheckpoint,
) -> Result<Response, ServeError> {
    let ManifestCompletion {
        index,
        repo,
        name,
        canonical,
        subject,
        operation,
    } = completion;
    if let Err(response) = acknowledge_metadata(
        state,
        MetadataAck {
            repo,
            epoch: checkpoint.epoch,
            commit: peryx_storage::meta::JournalCommit::new(checkpoint.serial),
        },
    )
    .await
    {
        return Ok(response);
    }
    state.finalize_admitted_write(operation, peryx_storage::meta::OperationResult::Published, &[]);
    state.record_operation_trace(peryx_driver::state::OperationKind::Publish, checkpoint.epoch);
    record_manifest_success(state, index, repo);
    Ok(manifest_created(
        &format!("/v2/{name}/manifests/{canonical}"),
        canonical,
        subject,
    ))
}

fn record_manifest_success(state: &ServingState, index: &Index, repo: &str) {
    state.metrics.record(Observation::Write {
        repository: index.route.clone(),
        resource: repo.to_owned(),
    });
    peryx_events::webhook::notify(state);
}

fn manifest_media_type(headers: &HeaderMap) -> Result<(String, ManifestSchema), Box<Response>> {
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
    let Some(schema) = ManifestSchema::of(&media_type) else {
        return Err(Box::new(error_response(
            ErrorCode::ManifestInvalid,
            &format!("unsupported manifest media type {media_type}"),
        )));
    };
    Ok((media_type, schema))
}

struct ManifestWrite<'a> {
    index: &'a str,
    repo: &'a str,
    canonical: &'a str,
    media_type: &'a str,
    bytes: &'a [u8],
    referrer: Option<store::Referrer>,
    reference: &'a Reference,
    reservation: &'a Option<peryx_storage::meta::QuotaReservationRecord>,
    journal: crate::outbox::Outbox,
    webhook: Option<peryx_storage::meta::WebhookEventIntent>,
    operation: Option<&'a str>,
    reference_key: &'a str,
    now: i64,
}

struct CommittedManifest {
    subject: Option<String>,
    commit: Option<peryx_storage::meta::JournalCommit>,
}

/// Publish the manifest and everything derived from it under one lease check, and return the subject
/// digest the response echoes in `OCI-Subject`.
///
/// The publication, the manifest's placement and its referrer row commit together, so a fault leaves
/// the repository exactly as it was: there is no point at which a client can read the manifest while
/// search cannot report its bytes or the subject cannot list it.
async fn commit_manifest(
    state: &ServingState,
    fence: u64,
    write: ManifestWrite<'_>,
) -> Result<EpochCommit<CommittedManifest>, ServeError> {
    let subject = write.referrer.as_ref().map(|referrer| referrer.subject.clone());
    commit_epoch(state, write.repo, fence, |lease| {
        lease.guard()?;
        let search_invalidation = crate::search_oci::SearchInvalidationGuard::arm(state, write.repo);
        let committed = crate::quota::publish_manifest(
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
                referrer: write.referrer.as_ref(),
                reservation: write.reservation.clone(),
                journal: write.journal,
                webhook: write.webhook,
                operation: write.operation.map(|id| crate::quota::ManifestOperation {
                    id,
                    reference: write.reference_key,
                    epoch: fence,
                    now: write.now,
                }),
            },
        )?;
        drop(search_invalidation);
        Ok(CommittedManifest {
            subject,
            commit: committed.journal,
        })
    })
    .await
}

fn manifest_operation(index: &str, repo: &str, digest: &str) -> String {
    format!("oci:manifest:{index}:{repo}:{digest}")
}

fn manifest_reference(reference: &Reference) -> String {
    match reference {
        Reference::Tag(tag) => format!("tag:{tag}"),
        Reference::Digest(digest) => format!("digest:{digest}"),
    }
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
/// The referrers-API row a pushed manifest contributes, or `None` when it declares no subject.
///
/// Derived from the parsed document before the publication transaction opens, so the row the subject
/// will list is decided while the push can still be rejected outright, and committing it costs the
/// transaction nothing but a write.
fn referrer_of(
    canonical: &str,
    media_type: &str,
    size: usize,
    document: &serde_json::Value,
) -> Option<store::Referrer> {
    let subject = document["subject"]["digest"].as_str()?;
    let mut descriptor = serde_json::json!({
        "mediaType": media_type,
        "digest": canonical,
        "size": size,
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
    Some(store::Referrer {
        subject: subject.to_owned(),
        descriptor: descriptor.to_string().into_bytes(),
    })
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
    // Restoring re-exposes a trashed manifest or tag under the repository's name, so it admits like a
    // publish: a name the policy now blocks cannot be brought back through the trash.
    if policy_blocks(index, PolicyAction::Upload, &repo) {
        return Ok(name_blocked());
    }
    // It is fenced like a publish too: a stale home cannot bring back a reference the surviving home has
    // moved past.
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
    document: &serde_json::Value,
) -> Result<Option<Response>, ServeError> {
    let (children, blobs) = store::document_descriptors(document);
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
