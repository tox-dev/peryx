//! Durable ingress admission for legacy `PyPI` uploads.
//!
//! A client uploads to whichever datacenter it reaches. Before an upload is stored, a durable write
//! intent binds it to the tenant, the ecosystem authority key, the digest, the size, the ingress DC, and
//! an operation id, and the configured backend must be able to prove same-datacenter durability. The
//! intent gives a retried upload one identity: an identical resend resolves the same intent instead of
//! staging its bytes twice, and a different-content resend of the same filename is refused as it is on
//! publication.
//!
//! Publication, home assignment, and cross-DC replication stay out of admission; they run downstream once
//! the ingress DC holds the upload durably.

use axum::http::{StatusCode, header};
use axum::response::{IntoResponse, Response};
use peryx_core::TopologyConfig;
use peryx_storage::blob::{DurabilityCapabilities, DurabilityRequirement, DurabilityShortfall};
use peryx_storage::meta::{
    BackpressureState, IntentAdmission, IntentLimits, IntentStageOutcome, IntentStageResult, MetaError, MetaStore,
};
use serde::{Deserialize, Serialize};

/// The per-authority admission ceilings and the fraction of them at which backpressure trips. Bounds the
/// retention buffer per authority so a stalled home DC cannot let staged uploads grow without limit and no
/// single busy project starves the ledger of every other. Backpressure trips at 80% of either ceiling, one
/// shed signal ahead of the hard bound.
pub(super) const STAGING_LIMITS: IntentLimits = IntentLimits {
    max_records: 65_536,
    max_bytes: 64 * 1024 * 1024 * 1024,
    backpressure_percent: 80,
};

/// How long a shed client is told to wait before retrying, the [`Retry-After`] the shed response carries so
/// the client backs off instead of hammering a home DC that is still unavailable.
///
/// [`Retry-After`]: https://www.rfc-editor.org/rfc/rfc9110.html#field.retry-after
const SHED_RETRY_AFTER_SECS: u32 = 30;

/// The datacenter id recorded for a single-node deployment that configures no roster.
const STANDALONE_DC: &str = "local";

/// The identity of an upload offered for ingress admission. The bytes are already staged; these fields
/// bind the durable intent that holds the upload near the client.
pub(super) struct AdmissionRequest<'a> {
    pub tenant: &'a str,
    pub authority: &'a str,
    pub filename: &'a str,
    pub digest: &'a str,
    pub size: u64,
    pub ingress_dc: &'a str,
}

/// The identity a staged intent binds, serialized as the intent payload so a recovered intent names the
/// tenant, authority, artifact, ingress DC, and operation id it was admitted for.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
struct IngressIntent {
    tenant: String,
    authority: String,
    digest: String,
    size: u64,
    ingress_dc: String,
    operation: String,
}

/// The result of admitting an upload for durable ingress staging.
pub(super) enum Admission {
    /// The upload is durably staged and may proceed to storage, carrying the intent identity the handler
    /// advances on publish and keys the operation ledger on.
    Admitted(AdmittedIntent),
    /// The upload is refused: a conflicting resend, a full backlog, an unsupported backend, or a staging
    /// failure. Carries the response to return unchanged.
    Reject(Response),
}

/// The durable identity a fresh or deduplicated admission established.
pub(super) struct AdmittedIntent {
    /// The intent-ledger key, advanced [`Pending`](peryx_storage::meta::IntentPhase::Pending) to
    /// [`Admitted`](peryx_storage::meta::IntentPhase::Admitted) once the write is durable.
    pub intent_key: String,
    /// The operation id a retry replays under, so the mutation runs once.
    pub operation: String,
}

/// Admit `request` for durable ingress staging into `meta`, retaining at most `limits` un-finalized
/// intents per authority and requiring `durability` to prove same-datacenter durability. The intent binds
/// the upload's identity so an identical resend deduplicates and a different-content resend of the same
/// filename is refused.
pub(super) fn admit(
    meta: &MetaStore,
    durability: DurabilityCapabilities,
    limits: IntentLimits,
    request: &AdmissionRequest<'_>,
    now: i64,
) -> Admission {
    admit_staged(meta, durability, limits, request, now).unwrap_or_else(|err| Admission::Reject(staging_failed(&err)))
}

fn admit_staged(
    meta: &MetaStore,
    durability: DurabilityCapabilities,
    limits: IntentLimits,
    request: &AdmissionRequest<'_>,
    now: i64,
) -> Result<Admission, MetaError> {
    if let Err(reject) = durability_gate(durability) {
        return Ok(reject);
    }
    let key = intent_key(request.tenant, request.authority, request.filename);
    let operation = format!("{key}:{}", request.digest);
    let payload = serde_json::to_vec(&IngressIntent {
        tenant: request.tenant.to_owned(),
        authority: request.authority.to_owned(),
        digest: request.digest.to_owned(),
        size: request.size,
        ingress_dc: request.ingress_dc.to_owned(),
        operation: operation.clone(),
    })
    .expect("an ingress intent serializes");
    let result = meta.stage_intent(
        IntentAdmission {
            authority: request.authority,
            key: &key,
            digest: request.digest,
            size: request.size,
            payload: &payload,
        },
        limits,
        now,
    )?;
    Ok(match stage_gate(result, request.filename) {
        Ok(()) => {
            if result.pressure == BackpressureState::Backpressured {
                let usage = meta.staged_intent_usage(request.authority)?;
                tracing::warn!(
                    authority = request.authority,
                    records = usage.records,
                    bytes = usage.bytes,
                    "ingress admission backpressured: authority retention crossed the soft threshold ahead of its hard bound"
                );
            }
            Admission::Admitted(AdmittedIntent {
                intent_key: key,
                operation,
            })
        }
        Err(reject) => reject,
    })
}

/// Same-datacenter durability must be provable before an upload is admitted: the ingress backend has to
/// commit race-safe, integrity-checked writes so a staged artifact cannot be silently clobbered or
/// corrupted before the home DC finalizes it.
fn durability_gate(durability: DurabilityCapabilities) -> Result<(), Admission> {
    durability
        .check(DurabilityRequirement::REPLICATED)
        .map_err(|shortfall| Admission::Reject(unsupported_durability(shortfall)))
}

/// Map the intent-ledger outcome to an admission decision: a fresh or identical intent proceeds, a
/// different-content resend of the same filename is refused with the file-conflict error publication
/// already returns, and an authority at either hard ceiling sheds load with a retry-after backoff.
fn stage_gate(result: IntentStageResult, filename: &str) -> Result<(), Admission> {
    match result.outcome {
        IntentStageOutcome::Admitted | IntentStageOutcome::Duplicate => Ok(()),
        IntentStageOutcome::Conflict => Err(Admission::Reject(conflicting_content(filename))),
        IntentStageOutcome::RejectedOverRecordLimit => Err(Admission::Reject(shed("record"))),
        IntentStageOutcome::RejectedOverByteLimit => Err(Admission::Reject(shed("byte"))),
    }
}

/// The intent key binds an upload to its file identity within a tenant and authority, so an identical
/// resend deduplicates and a different-content resend of the same filename conflicts.
fn intent_key(tenant: &str, authority: &str, filename: &str) -> String {
    format!("pypi:{tenant}:{authority}:{filename}")
}

/// The datacenter this node stages into, read from the configured roster; a rosterless single node
/// stages under [`STANDALONE_DC`].
pub(super) fn ingress_dc(topology: &TopologyConfig) -> String {
    topology
        .local_datacenter()
        .map_or_else(|| STANDALONE_DC.to_owned(), ToOwned::to_owned)
}

/// A different-content resend of a taken filename is refused with the response publication already
/// returns, so admission preserves the client-visible upload error.
fn conflicting_content(filename: &str) -> Response {
    (
        StatusCode::BAD_REQUEST,
        format!("File already exists: {filename:?} has different content; use a different filename"),
    )
        .into_response()
}

/// Shed an upload whose authority sits at its `bound` ceiling: a `503` carrying a [`Retry-After`] so the
/// client backs off and retries once the home DC drains the retained backlog, rather than losing the write.
///
/// [`Retry-After`]: https://www.rfc-editor.org/rfc/rfc9110.html#field.retry-after
fn shed(bound: &str) -> Response {
    (
        StatusCode::SERVICE_UNAVAILABLE,
        [(header::RETRY_AFTER, SHED_RETRY_AFTER_SECS.to_string())],
        format!("ingress admission retention is full: authority is at its {bound} ceiling"),
    )
        .into_response()
}

fn unsupported_durability(shortfall: DurabilityShortfall) -> Response {
    (
        StatusCode::SERVICE_UNAVAILABLE,
        format!("same-datacenter durability unavailable: {shortfall}"),
    )
        .into_response()
}

fn staging_failed(err: &MetaError) -> Response {
    tracing::error!(error = ?err, "ingress admission staging failed");
    (StatusCode::INTERNAL_SERVER_ERROR, "ingress admission failed").into_response()
}

#[cfg(test)]
#[path = "../../tests/unit/serving/admission/tests.rs"]
mod tests;
