//! The multipart upload handler: authorization, policy and status checks, and storage.
#![allow(
    clippy::result_large_err,
    reason = "handler helpers carry an axum Response as their error; boxing it everywhere adds noise"
)]

use std::sync::Arc;
use std::sync::atomic::Ordering;

use axum::extract::Multipart;
use axum::http::{HeaderMap, StatusCode, header};
use axum::response::{IntoResponse, Response};
use peryx_driver::not_found;
use peryx_driver::state::ServingState;
use peryx_events::metrics::Event;
use peryx_events::webhook::{WebhookEvent, WebhookEventKind};
use peryx_ha_distributed::AckDecision;
use peryx_identity::Action;
use peryx_index::Index;
use peryx_policy::{PolicyAction, PolicyDenial};
use peryx_storage::meta::{IntentPhase, OperationClaim, OperationResult, OperationState};

use crate::cache::{self, CacheError};
use crate::policy::{PypiPolicy, REQUIRED_ATTESTATION_AUDIT_RULE};
use crate::quota::{self, Admission, PendingQuota};
use crate::upload::{self, UploadError};
use crate::{PackageName, ProjectStatus, normalize_name};

use super::{acknowledge, admission};

/// How long a finalized operation's replayable result is retained before the reaper prunes it, bounding
/// the idempotency ledger while covering a client's realistic retry window.
const OPERATION_RETENTION_SECS: i64 = 24 * 3600;
use super::response::{CacheContext, cache_error_response, policy_denial_response};
use super::upload_form::{collect_form, upload_error_message, upload_error_response};
use super::{authorize, identify, request_id, upload_target};

/// `POST /{route}/`, the legacy multipart upload API, used unchanged by twine and `uv publish`.
pub async fn pypi_dispatch_post(
    state: Arc<ServingState>,
    path: String,
    headers: HeaderMap,
    multipart: Multipart,
) -> Response {
    state.requests.fetch_add(1, Ordering::Relaxed);
    let browser = match browser_upload(&headers) {
        Ok(browser) => browser,
        Err(response) => return response,
    };
    let Some((index, rest)) = state.resolve(&path) else {
        return not_found();
    };
    let identity = identify(&state, index, &headers);
    let actor = peryx_events::security::actor(&identity);
    if !rest.is_empty() {
        security_upload_event(&headers, actor.as_deref(), &index.route, None, "denied")
            .reason(Some("upload path must target an index root"))
            .emit();
        return not_found();
    }
    let Some(hosted) = upload_target(&state, index) else {
        security_upload_event(&headers, actor.as_deref(), &index.route, None, "denied")
            .reason(Some("index does not accept uploads"))
            .emit();
        return (StatusCode::METHOD_NOT_ALLOWED, "index does not accept uploads").into_response();
    };
    if let Err(response) = authorize(&index.route, hosted, &identity, None, Action::Write, &headers) {
        return response;
    }
    let response = accept_upload(
        UploadContext {
            state: &state,
            index,
            hosted,
            identity: &identity,
            headers: &headers,
            actor: actor.as_deref(),
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

fn browser_upload(headers: &HeaderMap) -> Result<bool, Response> {
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
        Err((StatusCode::FORBIDDEN, "browser upload rejected").into_response())
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
    headers: &'a HeaderMap,
    actor: Option<&'a str>,
    browser: bool,
}

async fn accept_upload(context: UploadContext<'_>, multipart: Multipart) -> Response {
    let UploadContext {
        state,
        index,
        hosted,
        identity,
        headers,
        actor,
        browser,
    } = context;
    let max_file_size = [index.policy.max_file_size(), hosted.policy.max_file_size()]
        .into_iter()
        .flatten()
        .min();
    let (form, staged) = match collect_form(multipart, &state.blobs, max_file_size, browser).await {
        Ok(form) => form,
        Err(response) => {
            security_upload_event(headers, actor, &index.route, Some(&hosted.name), "failure")
                .reason(Some("multipart body rejected"))
                .emit();
            return response;
        }
    };
    let Some(staged) = staged else {
        let err = UploadError::Missing("content");
        let (_, reason) = upload_error_message(&err);
        security_upload_event(headers, actor, &index.route, Some(&hosted.name), "denied")
            .project(form.name.as_deref().map(normalize_name).as_deref())
            .version(form.version.as_deref())
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
            security_upload_event(headers, actor, &index.route, Some(&hosted.name), "denied")
                .project(form_project.as_deref())
                .version(form_version.as_deref())
                .filename(form_filename.as_deref())
                .reason(Some(&reason))
                .emit();
            return upload_error_response(&err);
        }
    };
    let project = prepared.normalized.clone();
    if let Err(response) = authorize(&index.route, hosted, identity, Some(&project), Action::Write, headers) {
        return response;
    }
    let version = prepared.record.version.clone();
    let filename = prepared.filename.clone();
    let digest = prepared.digest.as_str().to_owned();
    let audit = UploadAudit {
        headers,
        actor: actor.map(str::to_owned),
        request_id: request_id(headers),
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
    let quota = match project_quota_reservation(state, index, hosted, &prepared, &project, &filename) {
        Ok(quota) => quota,
        Err(block) => {
            emit_upload_status_event(&audit, &block);
            return block.response;
        }
    };
    if let Some(block) = upload_status_response(
        cache::project_status(state, index, &project).await,
        &index.route,
        &project,
    ) {
        emit_upload_status_event(&audit, &block);
        return block.response;
    }
    admit_and_store(state, &hosted.name, &index.route, &project, prepared, quota, &audit).await
}

/// Durably admit the upload into the ingress datacenter, store it, and acknowledge it against the
/// configured datacenter durability quorum. Admission stages a durable write intent and proves the
/// backend can commit same-DC-durable writes; claiming the operation id makes a retry replay the original
/// result instead of mutating twice. Once stored, the intent advances to admitted and the write's
/// datacenter acknowledgement decides the response: a proven-durable write is `200` and finalizes, while
/// one still short of quorum is reported retry-safe rather than failed. A refused admission returns its
/// response unchanged and nothing is stored.
async fn admit_and_store(
    state: &Arc<ServingState>,
    hosted_name: &str,
    route: &str,
    project: &str,
    prepared: upload::PreparedUpload,
    quota: Option<PendingQuota>,
    audit: &UploadAudit<'_>,
) -> Response {
    let digest = prepared.digest.clone();
    let request = admission::AdmissionRequest {
        tenant: route,
        authority: project,
        filename: &prepared.filename,
        digest: prepared.digest.as_str(),
        size: prepared
            .record
            .file
            .size
            .expect("a prepared upload carries its byte size"),
        ingress_dc: &admission::ingress_dc(state.availability_topology()),
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
            return response;
        }
    };
    // Claim the operation before mutating: a retry of a write that already published replays its stored
    // result rather than running the mutation a second time.
    if let Some(response) = claim_short_circuit(state.meta.claim_operation(
        &intent.operation,
        Some(now + OPERATION_RETENTION_SECS),
        now,
    )) {
        return response;
    }
    let stored = match cache::store_upload(state, hosted_name, prepared, quota).await {
        Ok(stored) => stored,
        // A store fault stays a 5xx and leaves the operation pending, so a retry re-drives it.
        Err(err) => return upload_store_error_response(audit, err),
    };
    // The first stored file publishes the project, so it assigns the project's home datacenter through
    // the ownership group; a later file finds a home already set and only reads it. The project routes
    // through its canonical authority key — its PEP 503 normalized name — so every name variant homes
    // under one authority.
    let authority = crate::name::authority_key(project);
    if stored {
        state.claim_first_publish_home(&authority).await;
    }
    // The bytes are durable whether this stored fresh content or deduplicated an identical resend, so the
    // intent advances to admitted either way, which lets the reaper reclaim it.
    let _ = state
        .meta
        .advance_intent(&intent.intent_key, IntentPhase::Admitted, (state.clock)());
    emit_store_side_effects(state, audit, stored);
    let ack = acknowledge::local_ack(
        state.write_ack_policy(),
        acknowledge::same_dc_members(state.availability_topology()),
        acknowledge::local_node_id(state.availability_topology()),
        digest.clone(),
    );
    let remote_sources = state.remote_frontier_sources();
    let metadata = if remote_sources.is_empty() {
        // No remote datacenter is configured to wait on: a `none`/`dc` write, or an `ha` group that spans
        // a single datacenter. The local journal commit is then the whole metadata dimension and the write
        // acknowledges locally. This is distinct from a remote that is configured but unreachable, which
        // the gather in the other arm waits out and reports retry-safe rather than acknowledging.
        acknowledge::MetadataDimension::Local(AckDecision::Acknowledged)
    } else {
        // An `ha` write waits for an eligible remote datacenter to commit the exact operation: the
        // authority epoch it committed under and the metadata serial that covers it. A serial that cannot
        // be read fails closed to a frontier no remote covers, so the write reports retry-safe rather than
        // falsely durable.
        acknowledge::MetadataDimension::Remote {
            sources: remote_sources,
            authority: &authority,
            operation: peryx_ha_distributed::MetadataOperation {
                epoch: state.committed_authority_epoch(&authority).await,
                frontier: state.meta.current_serial().unwrap_or(u64::MAX),
            },
        }
    };
    let response = if let Some(metrics) = state.dc_durability() {
        acknowledge::ack_response(
            acknowledge::resolve_dc_ack(
                ack,
                metadata,
                &digest,
                state.receipt_sources(),
                state.write_ack_deadline(),
                metrics,
            )
            .await,
            &intent.operation,
        )
    } else {
        acknowledge::AckResponse {
            status: StatusCode::OK,
            body: b"upload accepted".to_vec(),
            finalize: true,
        }
    };
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
    (response.status, response.body).into_response()
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
        audit.headers,
        audit.actor.as_deref(),
        audit.route,
        Some(audit.hosted),
        "denied",
    )
    .project(Some(audit.project))
    .version(Some(audit.version))
    .filename(Some(audit.filename))
    .digest(Some(audit.digest))
    .reason(Some("ingress admission rejected"))
    .emit();
}

fn project_quota_reservation(
    state: &Arc<ServingState>,
    index: &Index,
    hosted: &Index,
    prepared: &upload::PreparedUpload,
    project: &str,
    filename: &str,
) -> Result<Option<PendingQuota>, UploadStatusBlock> {
    let Some((limit, audit)) = effective_project_quota(index, hosted) else {
        return Ok(None);
    };
    let exists = cache::upload_exists(state, &hosted.name, project, filename);
    if upload_quota_result(exists, &index.route, project)? {
        return Ok(None);
    }
    let incoming = prepared
        .record
        .file
        .size
        .expect("a prepared upload carries its byte size");
    let package = PackageName::new(project);
    let request = quota::quota_reservation(
        &hosted.name,
        &package,
        Some(prepared.record.version.as_str()),
        prepared.digest.as_str(),
        incoming,
        peryx_storage::meta::AccountingClass::Hosted,
        (state.clock)(),
    );
    let admission = quota::admit_upload(&state.meta, request, limit, audit);
    let admission = upload_quota_result(admission, &index.route, project)?;
    match admission {
        Admission::Reserved(reservation) => {
            quota::record_decision(state, hosted, project, false);
            Ok(Some(reservation))
        }
        Admission::Rejected { total } => {
            quota::record_decision(state, hosted, project, true);
            Err(upload_quota_denial(limit, project, filename, total))
        }
    }
}

fn effective_project_quota(index: &Index, hosted: &Index) -> Option<(u64, bool)> {
    match (
        index.policy.max_project_size(),
        (hosted.name != index.name)
            .then(|| hosted.policy.max_project_size())
            .flatten(),
    ) {
        (Some(index_limit), Some(hosted_limit)) => Some((
            index_limit.min(hosted_limit),
            index.policy.quota_audit() && hosted.policy.quota_audit(),
        )),
        (Some(limit), None) => Some((limit, index.policy.quota_audit())),
        (None, Some(limit)) => Some((limit, hosted.policy.quota_audit())),
        (None, None) => None,
    }
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
        audit.headers,
        audit.actor.as_deref(),
        audit.route,
        Some(audit.hosted),
        if audit_only { "audit" } else { "denied" },
    )
    .project(Some(audit.project))
    .version(Some(audit.version))
    .filename(Some(audit.filename))
    .digest(Some(audit.digest))
    .reason(Some(&denial.reason))
    .emit();
    (!audit_only).then(|| policy_denial_response(&denial))
}

struct UploadAudit<'a> {
    headers: &'a HeaderMap,
    actor: Option<String>,
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
        state.metrics.record(Event::Upload {
            route: audit.route.to_owned(),
            project: audit.project.to_owned(),
        });
        peryx_events::webhook::emit(
            state.clone(),
            &WebhookEvent {
                kind: WebhookEventKind::Upload,
                created_at_unix: audit.created_at_unix,
                index: audit.index.to_owned(),
                route: audit.route.to_owned(),
                hosted_index: audit.hosted.to_owned(),
                project: audit.project.to_owned(),
                version: Some(audit.version.to_owned()),
                filename: Some(audit.filename.to_owned()),
                digest: Some(audit.digest.to_owned()),
                count: 1,
                actor: audit.actor.clone(),
                request_id: audit.request_id.clone(),
            },
        );
    }
    security_upload_event(
        audit.headers,
        audit.actor.as_deref(),
        audit.route,
        Some(audit.hosted),
        if stored { "success" } else { "noop" },
    )
    .project(Some(audit.project))
    .version(Some(audit.version))
    .filename(Some(audit.filename))
    .digest(Some(audit.digest))
    .count(usize::from(stored))
    .reason((!stored).then_some("same content already stored"))
    .emit();
}

/// Map a store failure to its client response and security observation. A different-content collision is
/// a `400`; any other fault is a `500`, and the operation is left pending so a retry re-drives it.
fn upload_store_error_response(audit: &UploadAudit<'_>, err: CacheError) -> Response {
    match err {
        CacheError::FileExists(filename) => {
            security_upload_event(
                audit.headers,
                audit.actor.as_deref(),
                audit.route,
                Some(audit.hosted),
                "denied",
            )
            .project(Some(audit.project))
            .version(Some(audit.version))
            .filename(Some(&filename))
            .digest(Some(audit.digest))
            .reason(Some("file exists with different content"))
            .emit();
            (
                StatusCode::BAD_REQUEST,
                format!("File already exists: {filename:?} has different content; use a different filename"),
            )
                .into_response()
        }
        err => {
            let reason = err.user_message();
            security_upload_event(
                audit.headers,
                audit.actor.as_deref(),
                audit.route,
                Some(audit.hosted),
                "failure",
            )
            .project(Some(audit.project))
            .version(Some(audit.version))
            .filename(Some(audit.filename))
            .digest(Some(audit.digest))
            .reason(Some(&reason))
            .emit();
            tracing::error!(error = ?err, "upload store failed");
            cache_error_response(&err, CacheContext::upload(audit.route, audit.project))
        }
    }
}

fn emit_upload_status_event(audit: &UploadAudit<'_>, block: &UploadStatusBlock) {
    security_upload_event(
        audit.headers,
        audit.actor.as_deref(),
        audit.route,
        Some(audit.hosted),
        block.result,
    )
    .project(Some(audit.project))
    .version(Some(audit.version))
    .filename(Some(audit.filename))
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
) -> Result<T, UploadStatusBlock> {
    result.map_err(|err| upload_quota_failure(&err.into(), route, project))
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
    headers: &'a HeaderMap,
    actor: Option<&'a str>,
    route: &'a str,
    hosted_index: Option<&'a str>,
    result: &'static str,
) -> peryx_events::security::Event<'a> {
    let event = peryx_events::security::Event::new("upload", result)
        .actor(actor)
        .index(route)
        .request(headers);
    if let Some(hosted_index) = hosted_index {
        event.hosted_index(hosted_index)
    } else {
        event
    }
}

#[cfg(test)]
mod tests {
    use peryx_storage::meta::MetaError;

    use super::*;

    #[test]
    fn test_upload_status_response_maps_policy_and_store_errors() {
        assert!(upload_status_response(Ok(ProjectStatus::Active), "root/pypi", "flask").is_none());
        let archived = upload_status_response(Ok(ProjectStatus::Archived), "root/pypi", "flask").unwrap();
        assert_eq!(archived.response.status(), StatusCode::FORBIDDEN);
        assert_eq!(archived.result, "denied");
        assert_eq!(archived.reason, "project \"flask\" is archived; uploads are disabled");

        let failure = upload_status_response(Err(CacheError::Meta(meta_error())), "root/pypi", "flask").unwrap();
        assert_eq!(failure.response.status(), StatusCode::INTERNAL_SERVER_ERROR);
        assert_eq!(failure.result, "failure");
        assert!(failure.reason.contains("metadata store error"));
    }

    #[test]
    fn test_upload_quota_failure_preserves_the_storage_fault() {
        let failure =
            upload_quota_result::<(), _>(Err(CacheError::Meta(meta_error())), "root/pypi", "flask").unwrap_err();

        assert_eq!(failure.response.status(), StatusCode::INTERNAL_SERVER_ERROR);
        assert_eq!(failure.result, "failure");
        assert!(failure.reason.contains("metadata store error"));
    }

    #[test]
    fn test_upload_quota_failure_describes_the_accounting_fault() {
        let failure = upload_quota_result::<(), _>(
            Err(peryx_storage::meta::QuotaError::Empty { field: "project" }),
            "root/pypi",
            "flask",
        )
        .unwrap_err();

        assert_eq!(failure.response.status(), StatusCode::INTERNAL_SERVER_ERROR);
        assert_eq!(failure.result, "failure");
        assert_eq!(failure.reason, "quota accounting error: project must not be empty");
    }

    fn meta_error() -> MetaError {
        MetaError::Decode(serde_json::from_str::<serde_json::Value>("not json").unwrap_err())
    }

    fn record(state: OperationState) -> peryx_storage::meta::OperationOutcomeRecord {
        peryx_storage::meta::OperationOutcomeRecord {
            state,
            response: b"upload accepted".to_vec(),
            expiry_unix: None,
            updated_at_unix: 0,
        }
    }

    #[test]
    fn test_claim_short_circuit_replays_a_published_operation() {
        let response = claim_short_circuit(Ok(OperationClaim::Existing(record(OperationState::Published))))
            .expect("a published operation replays");
        assert_eq!(response.status(), StatusCode::OK);
    }

    #[test]
    fn test_claim_short_circuit_proceeds_for_a_fresh_or_still_running_claim() {
        assert!(claim_short_circuit(Ok(OperationClaim::Admitted)).is_none());
        assert!(claim_short_circuit(Ok(OperationClaim::Existing(record(OperationState::Pending)))).is_none());
    }

    #[test]
    fn test_claim_short_circuit_fails_closed_when_the_claim_cannot_be_read() {
        let response = claim_short_circuit(Err(meta_error())).expect("an unreadable claim fails closed");
        assert_eq!(response.status(), StatusCode::INTERNAL_SERVER_ERROR);
    }

    fn audit(headers: &HeaderMap) -> UploadAudit<'_> {
        UploadAudit {
            headers,
            actor: None,
            request_id: None,
            created_at_unix: 0,
            index: "root/pypi",
            route: "root/pypi",
            hosted: "hosted",
            project: "flask",
            version: "1.0",
            filename: "flask-1.0-py3-none-any.whl",
            digest: "aa",
        }
    }

    #[test]
    fn test_upload_store_error_response_reports_a_content_collision_as_bad_request() {
        let headers = HeaderMap::new();
        let response =
            upload_store_error_response(&audit(&headers), CacheError::FileExists("flask-1.0.whl".to_owned()));
        assert_eq!(response.status(), StatusCode::BAD_REQUEST);
    }

    #[test]
    fn test_upload_store_error_response_reports_a_store_fault_as_internal_error() {
        let headers = HeaderMap::new();
        let response = upload_store_error_response(&audit(&headers), CacheError::Meta(meta_error()));
        assert_eq!(response.status(), StatusCode::INTERNAL_SERVER_ERROR);
    }
}
