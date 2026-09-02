use std::sync::Arc;
use std::sync::atomic::Ordering;

use axum::extract::Multipart;
use axum::http::{HeaderMap, StatusCode, header};
use axum::response::{IntoResponse, Response};
use peryx_driver::not_found;
use peryx_driver::quota::quota_limit_label;
use peryx_driver::state::ServingState;
use peryx_events::metrics::Observation;
use peryx_events::security::{Attribution, RequestContext};
use peryx_ha::{AuthorityEpoch, CommittedBlob, OperationKind};
use peryx_identity::Action;
use peryx_index::Index;
use peryx_policy::{Policy, PolicyAction, PolicyDenial};
use peryx_storage::blob::Digest;
use peryx_storage::meta::{IntentPhase, OperationClaim, OperationResult, OperationState, QuotaLimit, QuotaLimits};

use crate::cache::{self, CacheError};
use crate::policy::{PypiPolicy, REQUIRED_ATTESTATION_AUDIT_RULE};
use crate::quota::{self, Admission, PendingQuota, QuotaRejection};
use crate::upload::{self, UploadError};
use crate::webhook::{self, PypiWebhook};
use crate::{PYPI_LEXICON, PackageName, ProjectStatus, normalize_name};

use super::{acknowledge, admission};

/// How long a finalized operation's replayable result is retained before the reaper prunes it, bounding
/// the idempotency ledger while covering a client's realistic retry window.
const OPERATION_RETENTION_SECS: i64 = 24 * 3600;
use super::response::{CacheContext, cache_error_response, policy_denial_response};
use super::upload_form::{collect_form, upload_error_message, upload_error_response};
use super::{HttpResult, authorize, identify, request_id, upload_target};

/// `POST /{route}/`, the legacy multipart upload API, used unchanged by twine and `uv publish`.
pub async fn pypi_dispatch_post(
    state: Arc<ServingState>,
    path: String,
    request: RequestContext<'_>,
    multipart: Multipart,
) -> Response {
    state.requests.fetch_add(1, Ordering::Relaxed);
    let browser = match browser_upload(request.headers) {
        Ok(browser) => browser,
        Err(error) => return error.into_response(),
    };
    let Some((index, rest)) = state.resolve(&path) else {
        return not_found();
    };
    let identity = identify(&state, index, request.headers);
    let attribution = Attribution::resolve(&identity);
    if !rest.is_empty() {
        security_upload_event(request, &attribution, &index.route, None, "denied")
            .reason(Some("upload path must target an index root"))
            .emit();
        return not_found();
    }
    let Some(hosted) = upload_target(&state, index) else {
        security_upload_event(request, &attribution, &index.route, None, "denied")
            .reason(Some("index does not accept uploads"))
            .emit();
        return (StatusCode::METHOD_NOT_ALLOWED, "index does not accept uploads").into_response();
    };
    if let Err(error) = authorize(&index.route, hosted, &identity, None, Action::Write, request) {
        return error.into_response();
    }
    let response = accept_upload(
        UploadContext {
            state: &state,
            index,
            hosted,
            identity: &identity,
            request,
            attribution: &attribution,
            browser,
        },
        multipart,
    )
    .await;
    if browser && response.status().is_server_error() {
        (response.status(), "upload storage failed").into_response()
    } else {
        response
    }
}

const BROWSER_CSRF_HEADER: &str = "x-peryx-csrf";

fn browser_upload(headers: &HeaderMap) -> HttpResult<bool> {
    let origin = headers.get(header::ORIGIN);
    let csrf = headers.get(BROWSER_CSRF_HEADER);
    if origin.is_none() && csrf.is_none() {
        return Ok(false);
    }
    let valid = origin.zip(csrf).is_some_and(|(origin, csrf)| {
        origin == csrf
            && headers.get("sec-fetch-site").is_none_or(|site| site == "same-origin")
            && browser_origin_matches_host(origin, headers.get(header::HOST))
    });
    if valid {
        Ok(true)
    } else {
        Err((StatusCode::FORBIDDEN, "browser upload rejected")
            .into_response()
            .into())
    }
}

fn browser_origin_matches_host(origin: &axum::http::HeaderValue, host: Option<&axum::http::HeaderValue>) -> bool {
    let Some((origin, host)) = origin.to_str().ok().zip(host.and_then(|host| host.to_str().ok())) else {
        return false;
    };
    let Ok(origin) = url::Url::parse(origin) else {
        return false;
    };
    let Ok(host) = host.parse::<axum::http::uri::Authority>() else {
        return false;
    };
    let host_name = host.host().trim_start_matches('[').trim_end_matches(']');
    matches!(origin.scheme(), "http" | "https")
        && origin
            .host_str()
            .is_some_and(|origin| origin.eq_ignore_ascii_case(host_name))
        && host.port_u16().map_or_else(
            || origin.port().is_none(),
            |port| origin.port_or_known_default() == Some(port),
        )
}

struct UploadContext<'a> {
    state: &'a Arc<ServingState>,
    index: &'a Index,
    hosted: &'a Index,
    identity: &'a super::UploadIdentity,
    request: RequestContext<'a>,
    attribution: &'a Attribution,
    browser: bool,
}

async fn accept_upload(context: UploadContext<'_>, multipart: Multipart) -> Response {
    let UploadContext {
        state,
        index,
        hosted,
        identity,
        request,
        attribution,
        browser,
    } = context;
    let max_file_size = [index.policy.max_artifact_size(), hosted.policy.max_artifact_size()]
        .into_iter()
        .flatten()
        .min();
    let (form, staged) = match collect_form(multipart, &state.blobs, max_file_size, browser).await {
        Ok(form) => form,
        Err(error) => {
            security_upload_event(request, attribution, &index.route, Some(&hosted.name), "failure")
                .reason(Some("multipart body rejected"))
                .emit();
            return error.into_response();
        }
    };
    let Some(staged) = staged else {
        let err = UploadError::Missing("content");
        let (_, reason) = upload_error_message(&err);
        security_upload_event(request, attribution, &index.route, Some(&hosted.name), "denied")
            .resource(form.name.as_deref().map(normalize_name).as_deref())
            .group(form.version.as_deref())
            .reason(Some(&reason))
            .emit();
        return upload_error_response(&err);
    };
    let form_project = form.name.as_deref().map(normalize_name);
    let form_version = form.version.clone();
    let form_filename = form.filename.clone();
    let upload_time_unix = (state.clock)();
    let prepared = match upload::prepare(form, staged, &index.route, upload_time_unix) {
        Ok(prepared) => prepared,
        Err(err) => {
            let (_, reason) = upload_error_message(&err);
            security_upload_event(request, attribution, &index.route, Some(&hosted.name), "denied")
                .resource(form_project.as_deref())
                .group(form_version.as_deref())
                .artifact(form_filename.as_deref())
                .reason(Some(&reason))
                .emit();
            return upload_error_response(&err);
        }
    };
    let project = prepared.normalized.clone();
    if let Err(error) = authorize(&index.route, hosted, identity, Some(&project), Action::Write, request) {
        return error.into_response();
    }
    let version = prepared.record.version.clone();
    let filename = prepared.filename.clone();
    let digest = prepared.digest.as_str().to_owned();
    let audit = UploadAudit {
        request,
        attribution,
        request_id: request_id(request.headers),
        created_at_unix: upload_time_unix,
        index: &index.name,
        route: &index.route,
        hosted: &hosted.name,
        project: &project,
        version: &version,
        filename: &filename,
        digest: &digest,
    };
    if let Some(block) = upload_policy_response(index, &prepared, &audit) {
        return block;
    }
    if hosted.name != index.name
        && let Some(block) = upload_policy_response(hosted, &prepared, &audit)
    {
        return block;
    }
    if let Some(block) = upload_status_response(
        cache::project_status(state, index, &project).await,
        &index.route,
        &project,
    ) {
        emit_upload_status_event(&audit, &block);
        return block.response;
    }
    admit_and_store(state, index, hosted, prepared, &audit).await
}

/// Durably admit the upload into the ingress datacenter, store it, and acknowledge it against the
/// configured datacenter durability quorum. Admission stages a durable write intent and proves the
/// backend can commit same-DC-durable writes; claiming the operation id makes a retry replay the original
/// result instead of mutating twice. Once stored, the intent advances to admitted and the write's
/// datacenter acknowledgement decides the response: a proven-durable write is `200` and finalizes, while
/// one still short of quorum is reported retry-safe rather than failed. A refused admission returns its
/// response unchanged and nothing is stored.
///
/// Every exit that stores nothing releases the intent this request staged, so a failed upload gives its
/// authority's admission capacity straight back instead of leaving a pending record only the staging
/// deadline could clear. An intent a concurrent identical resend deduplicated onto is left alone: that
/// record belongs to the upload still storing its bytes.
async fn admit_and_store(
    state: &Arc<ServingState>,
    index: &Index,
    hosted: &Index,
    prepared: upload::PreparedUpload,
    audit: &UploadAudit<'_>,
) -> Response {
    let bundle = prepared.provenance.as_deref().map(Digest::of);
    let request = admission::AdmissionRequest {
        tenant: &index.route,
        authority: audit.project,
        filename: &prepared.filename,
        digest: prepared.digest.as_str(),
        size: prepared
            .record
            .file
            .size
            .expect("a prepared upload carries its byte size"),
        ingress_dc: &admission::ingress_dc(state.availability_topology()),
        provenance: bundle.as_ref().map(Digest::as_str),
    };
    let now = (state.clock)();
    let intent = match admission::admit(
        &state.meta,
        state.blobs.durability(),
        admission::STAGING_LIMITS,
        &request,
        now,
    ) {
        admission::Admission::Admitted(intent) => intent,
        admission::Admission::Reject(response) => {
            emit_admission_rejection(audit);
            return *response;
        }
    };
    match store_admitted(state, index, hosted, prepared, audit, &intent, now).await {
        Ok(response) => response,
        Err(response) => {
            if intent.fresh {
                let _ = state.meta.release_intent(&intent.intent_key);
            }
            response
        }
    }
}

/// Store the admitted upload and acknowledge it. `Err` carries the response for every exit that leaves
/// nothing durable behind - a replayed operation, an unreadable claim, an unavailable authority home, a
/// quota block, or a store fault - so the caller reclaims the intent those exits would otherwise strand.
async fn store_admitted(
    state: &Arc<ServingState>,
    index: &Index,
    hosted: &Index,
    prepared: upload::PreparedUpload,
    audit: &UploadAudit<'_>,
    intent: &admission::AdmittedIntent,
    now: i64,
) -> Result<Response, Response> {
    let project = audit.project;
    let digest = prepared.digest.clone();
    let incoming = prepared
        .record
        .file
        .size
        .expect("a prepared upload carries its byte size");
    // Claim the operation before mutating: a retry of a write that already published replays its stored
    // result rather than running the mutation a second time.
    if let Some(response) = claim_short_circuit(state.meta.claim_operation(
        &intent.operation,
        Some(now + OPERATION_RETENTION_SECS),
        now,
    )) {
        return Err(response);
    }
    let authority = crate::name::authority_key(project);
    let fence = first_publish_fence(state, &authority).await?;
    let quota = project_quota_reservation(state, index, hosted, project, audit, incoming).map_err(|block| {
        emit_upload_status_event(audit, &block);
        block.response
    })?;
    let upload_webhook = webhook::prepare(
        state,
        PypiWebhook {
            event: webhook::UPLOAD,
            created_at_unix: audit.created_at_unix,
            index: audit.index,
            route: audit.route,
            hosted_index: audit.hosted,
            project: audit.project,
            version: Some(audit.version),
            filename: Some(audit.filename),
            digest: Some(audit.digest),
            count: 1,
            actor: audit.attribution.actor(),
            request_id: audit.request_id.as_deref(),
        },
    );
    let stored = match cache::store_upload(state, &hosted.name, project, prepared, quota, fence, upload_webhook).await {
        Ok(stored) => stored,
        // A store fault stays a 5xx and leaves the operation pending, so a retry re-drives it.
        Err(err) => return Err(upload_store_error_response(audit, err)),
    };
    // The bytes are durable whether this stored fresh content or deduplicated an identical resend, so the
    // intent advances to admitted either way, which lets the reaper reclaim it.
    let _ = state
        .meta
        .advance_intent(&intent.intent_key, IntentPhase::Admitted, (state.clock)());
    emit_store_side_effects(state, audit, stored.stored);
    let response = acknowledge::ack_response(
        state
            .confirm_blob_write(CommittedBlob::new(
                &digest,
                incoming,
                &authority,
                AuthorityEpoch(state.committed_authority_epoch(&authority).await),
                stored.commit,
                stored.evidence,
                OperationKind::Publish,
            ))
            .await,
        &intent.operation,
    );
    if response.finalize {
        // A finalize fault leaves the operation pending, which a retry re-drives and re-finalizes over the
        // idempotent store, so the terminal result is recorded then rather than failing this proven write.
        let _ = state.meta.finalize_operation(
            &intent.operation,
            OperationResult::Published,
            &response.body,
            (state.clock)(),
        );
    }
    Ok((response.status, response.body).into_response())
}

async fn first_publish_fence(state: &ServingState, authority: &str) -> Result<u64, Response> {
    match state.claim_first_publish_home(authority).await {
        Ok(None) => Ok(0),
        Ok(Some(claim)) if state.availability_topology().local_datacenter() == Some(claim.home.as_str()) => {
            Ok(claim.epoch)
        }
        Ok(Some(_)) => Err(first_publish_unavailable()),
        Err(error) => {
            tracing::warn!(%error, authority, "first-publish home could not be resolved");
            Err(first_publish_unavailable())
        }
    }
}

fn first_publish_unavailable() -> Response {
    (
        StatusCode::SERVICE_UNAVAILABLE,
        [(header::RETRY_AFTER, "1")],
        "project authority is unavailable; retry the upload",
    )
        .into_response()
}

/// Short-circuit an admitted write on its operation claim: replay a write that already published, or fail
/// closed when the claim itself could not be read, since proceeding without a claim risks a second
/// mutation on retry. A fresh claim, or one still pending or failed, proceeds and returns `None`.
fn claim_short_circuit(claim: Result<OperationClaim, peryx_storage::meta::MetaError>) -> Option<Response> {
    match claim {
        Ok(OperationClaim::Existing(record)) if record.state == OperationState::Published => {
            Some(replay_response(&record.response))
        }
        Ok(_) => None,
        Err(err) => {
            tracing::error!(error = ?err, "ingress operation claim failed");
            Some((StatusCode::INTERNAL_SERVER_ERROR, "upload admission failed").into_response())
        }
    }
}

/// Replay a finalized operation's stored response verbatim, so a retry resolves the original result.
fn replay_response(body: &[u8]) -> Response {
    (StatusCode::OK, String::from_utf8_lossy(body).into_owned()).into_response()
}

fn emit_admission_rejection(audit: &UploadAudit<'_>) {
    security_upload_event(
        audit.request,
        audit.attribution,
        audit.route,
        Some(audit.hosted),
        "denied",
    )
    .resource(Some(audit.project))
    .group(Some(audit.version))
    .artifact(Some(audit.filename))
    .digest(Some(audit.digest))
    .reason(Some("ingress admission rejected"))
    .emit();
}

fn project_quota_reservation(
    state: &Arc<ServingState>,
    index: &Index,
    hosted: &Index,
    project: &str,
    audit: &UploadAudit<'_>,
    incoming: u64,
) -> Result<Option<PendingQuota>, Box<UploadStatusBlock>> {
    let Some(quota) = effective_project_quota(index, hosted) else {
        return Ok(None);
    };
    let package = PackageName::new(project);
    let request = quota::quota_reservation(
        &hosted.name,
        &package,
        Some(audit.version),
        audit.digest,
        incoming,
        (state.clock)(),
    );
    let admission = quota::admit_upload(&state.meta, request, quota.limits, quota.max_project_bytes);
    let admission = upload_quota_result(admission, &index.route, project)?;
    match admission {
        Admission::Reserved(reservation) => {
            quota::record_decision(state, hosted, project, false);
            Ok(Some(reservation))
        }
        Admission::Rejected(QuotaRejection::ProjectBytes { total }) => {
            quota::record_decision(state, hosted, project, true);
            Err(Box::new(upload_quota_denial(
                quota
                    .max_project_bytes
                    .expect("a project-byte rejection has a configured limit"),
                project,
                audit.filename,
                total,
            )))
        }
        Admission::Rejected(QuotaRejection::Limits(violations)) => {
            quota::record_decision(state, hosted, project, true);
            Err(Box::new(upload_limits_denial(&violations, project, audit.filename)))
        }
    }
}

#[derive(Clone, Copy)]
struct EffectiveQuota {
    limits: QuotaLimits,
    max_project_bytes: Option<u64>,
}

fn effective_project_quota(index: &Index, hosted: &Index) -> Option<EffectiveQuota> {
    match (
        policy_quota(&index.policy),
        (hosted.name != index.name)
            .then(|| policy_quota(&hosted.policy))
            .flatten(),
    ) {
        (Some(index), Some(hosted)) => Some(merge_quotas(index, hosted)),
        (Some(quota), None) | (None, Some(quota)) => Some(quota),
        (None, None) => None,
    }
}

fn policy_quota(policy: &Policy) -> Option<EffectiveQuota> {
    (policy.enforces_quota() || policy.has_resource_size_limit()).then(|| EffectiveQuota {
        limits: QuotaLimits {
            max_artifact_bytes: policy.max_artifact_size(),
            max_accounted_bytes: policy.max_accounted_bytes(),
            max_resources: policy.max_resources(),
            max_groups_per_resource: policy.max_groups_per_resource(),
            audit: policy.quota_audit(),
        },
        max_project_bytes: policy.max_resource_size(),
    })
}

fn merge_quotas(first: EffectiveQuota, second: EffectiveQuota) -> EffectiveQuota {
    EffectiveQuota {
        limits: QuotaLimits {
            max_artifact_bytes: minimum(first.limits.max_artifact_bytes, second.limits.max_artifact_bytes),
            max_accounted_bytes: minimum(first.limits.max_accounted_bytes, second.limits.max_accounted_bytes),
            max_resources: minimum(first.limits.max_resources, second.limits.max_resources),
            max_groups_per_resource: minimum(
                first.limits.max_groups_per_resource,
                second.limits.max_groups_per_resource,
            ),
            audit: first.limits.audit && second.limits.audit,
        },
        max_project_bytes: minimum(first.max_project_bytes, second.max_project_bytes),
    }
}

fn minimum(first: Option<u64>, second: Option<u64>) -> Option<u64> {
    first.into_iter().chain(second).min()
}

fn upload_policy_response(
    index: &Index,
    prepared: &upload::PreparedUpload,
    audit: &UploadAudit<'_>,
) -> Option<Response> {
    let Err(denial) = index.policy.check_upload(
        PolicyAction::Upload,
        &prepared.normalized,
        &prepared.record.file,
        &prepared.attestation_predicate_types,
    ) else {
        return None;
    };
    // An audit-mode attestation requirement records the unmet rule but does not reject the upload; the
    // decision is already persisted, so the handler emits the observation and lets the file publish.
    let audit_only = denial.rule == REQUIRED_ATTESTATION_AUDIT_RULE;
    security_upload_event(
        audit.request,
        audit.attribution,
        audit.route,
        Some(audit.hosted),
        if audit_only { "audit" } else { "denied" },
    )
    .resource(Some(audit.project))
    .group(Some(audit.version))
    .artifact(Some(audit.filename))
    .digest(Some(audit.digest))
    .reason(Some(&denial.reason))
    .emit();
    (!audit_only).then(|| policy_denial_response(&denial))
}

struct UploadAudit<'a> {
    request: RequestContext<'a>,
    attribution: &'a Attribution,
    request_id: Option<String>,
    created_at_unix: i64,
    index: &'a str,
    route: &'a str,
    hosted: &'a str,
    project: &'a str,
    version: &'a str,
    filename: &'a str,
    digest: &'a str,
}

/// Emit the metrics, webhook, and security observations a successful store produces. A fresh store
/// records the upload metric and fires the upload webhook; a deduplicated resend is a no-op that only
/// reports the security event. The client response is decided separately from the durability acknowledgement.
fn emit_store_side_effects(state: &Arc<ServingState>, audit: &UploadAudit<'_>, stored: bool) {
    if stored {
        state.metrics.record(Observation::Write {
            repository: audit.route.to_owned(),
            resource: audit.project.to_owned(),
        });
        peryx_events::webhook::notify(state.as_ref());
    }
    security_upload_event(
        audit.request,
        audit.attribution,
        audit.route,
        Some(audit.hosted),
        if stored { "success" } else { "noop" },
    )
    .resource(Some(audit.project))
    .group(Some(audit.version))
    .artifact(Some(audit.filename))
    .digest(Some(audit.digest))
    .count(usize::from(stored))
    .reason((!stored).then_some("same content already stored"))
    .emit();
}

/// Map a store failure to its client response and security observation. A collision with what the
/// filename already publishes - different bytes, or different attestations over the same bytes - is a
/// `400`; any other fault is a `500`, and the operation is left pending so a retry re-drives it.
fn upload_store_error_response(audit: &UploadAudit<'_>, err: CacheError) -> Response {
    match err {
        CacheError::FileExists(filename) => upload_denied_response(
            audit,
            &filename,
            "file exists with different content",
            format!("File already exists: {filename:?} has different content; use a different filename"),
        ),
        CacheError::ProvenanceMismatch(filename) => upload_denied_response(
            audit,
            &filename,
            "file exists with different attestations",
            format!(
                "File already exists: {filename:?} was published with different attestations; \
                 a re-upload cannot change them"
            ),
        ),
        err => {
            let reason = err.user_message();
            security_upload_event(
                audit.request,
                audit.attribution,
                audit.route,
                Some(audit.hosted),
                "failure",
            )
            .resource(Some(audit.project))
            .group(Some(audit.version))
            .artifact(Some(audit.filename))
            .digest(Some(audit.digest))
            .reason(Some(&reason))
            .emit();
            tracing::error!(error = ?err, "upload store failed");
            cache_error_response(&err, CacheContext::upload(audit.route, audit.project))
        }
    }
}

/// The `400` for an upload the store refused because the filename already publishes something else,
/// with the denial recorded against the colliding filename rather than the requested one.
fn upload_denied_response(audit: &UploadAudit<'_>, filename: &str, reason: &str, message: String) -> Response {
    security_upload_event(
        audit.request,
        audit.attribution,
        audit.route,
        Some(audit.hosted),
        "denied",
    )
    .resource(Some(audit.project))
    .group(Some(audit.version))
    .artifact(Some(filename))
    .digest(Some(audit.digest))
    .reason(Some(reason))
    .emit();
    (StatusCode::BAD_REQUEST, message).into_response()
}

fn emit_upload_status_event(audit: &UploadAudit<'_>, block: &UploadStatusBlock) {
    security_upload_event(
        audit.request,
        audit.attribution,
        audit.route,
        Some(audit.hosted),
        block.result,
    )
    .resource(Some(audit.project))
    .group(Some(audit.version))
    .artifact(Some(audit.filename))
    .digest(Some(audit.digest))
    .reason(Some(&block.reason))
    .emit();
}

pub(super) struct UploadStatusBlock {
    pub(super) response: Response,
    pub(super) result: &'static str,
    pub(super) reason: String,
}

/// Preserve the existing policy-denial contract for a project-size reservation rejection.
fn upload_quota_denial(limit: u64, project: &str, filename: &str, total: u64) -> UploadStatusBlock {
    let reason = format!("project size {total} would exceed limit {limit}");
    let denial = PolicyDenial::new(
        PolicyAction::Upload,
        project,
        Some(filename),
        None,
        "max-project-size",
        "project_size",
        reason.clone(),
    );
    UploadStatusBlock {
        response: policy_denial_response(&denial),
        result: "denied",
        reason,
    }
}

fn upload_limits_denial(violations: &[QuotaLimit], project: &str, filename: &str) -> UploadStatusBlock {
    let labels = violations
        .iter()
        .map(|limit| quota_limit_label(&PYPI_LEXICON, *limit))
        .collect::<Vec<_>>()
        .join(", ");
    let reason = format!("{} quota exceeded: {labels}", PYPI_LEXICON.repository);
    let denial = PolicyDenial::new(
        PolicyAction::Upload,
        project,
        Some(filename),
        None,
        "index-quota",
        "quota",
        reason.clone(),
    );
    UploadStatusBlock {
        response: policy_denial_response(&denial),
        result: "denied",
        reason,
    }
}

fn upload_quota_failure(err: &CacheError, route: &str, project: &str) -> UploadStatusBlock {
    UploadStatusBlock {
        response: cache_error_response(err, CacheContext::upload(route, project)),
        result: "failure",
        reason: err.user_message(),
    }
}

fn upload_quota_result<T, E: Into<CacheError>>(
    result: Result<T, E>,
    route: &str,
    project: &str,
) -> Result<T, Box<UploadStatusBlock>> {
    result.map_err(|err| Box::new(upload_quota_failure(&err.into(), route, project)))
}

pub(super) fn upload_status_response(
    result: Result<ProjectStatus, CacheError>,
    index: &str,
    project: &str,
) -> Option<UploadStatusBlock> {
    match result {
        Ok(status) if status.allows_uploads() => None,
        Ok(status) => {
            let reason = format!("project {project:?} is {}; uploads are disabled", status.marker());
            Some(UploadStatusBlock {
                response: (StatusCode::FORBIDDEN, reason.clone()).into_response(),
                result: "denied",
                reason,
            })
        }
        Err(err) => {
            let reason = err.user_message();
            Some(UploadStatusBlock {
                response: cache_error_response(&err, CacheContext::upload(index, project)),
                result: "failure",
                reason,
            })
        }
    }
}

fn security_upload_event<'a>(
    request: RequestContext<'a>,
    attribution: &'a Attribution,
    route: &'a str,
    hosted_index: Option<&'a str>,
    result: &'static str,
) -> peryx_events::security::Event<'a> {
    let event = peryx_events::security::Event::new("upload", result)
        .attribution(attribution)
        .index(route)
        .request(request);
    if let Some(hosted_index) = hosted_index {
        event.hosted_index(hosted_index)
    } else {
        event
    }
}

#[cfg(test)]
#[path = "../../tests/unit/serving/post/tests.rs"]
mod tests;
