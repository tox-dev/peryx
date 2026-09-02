//! Durable ingress admission for OCI blob uploads.
//!
//! A push lands at whichever datacenter the client reached. Blob bytes are content-addressed, so they
//! are committed and digest-verified first; only then does a durable write intent bind them to the
//! index, repository, canonical authority, byte length, and operation id a home datacenter needs to
//! publish the repository membership. That ordering is what makes the two crash windows recoverable in
//! opposite directions: a crash before the intent lands leaves nothing but an unreferenced blob the
//! content sweep reclaims, and a crash after it leaves a record
//! [`finalize`](super::finalize) replays, so a published membership is never the only trace of a write.
//!
//! The intent is not released once staged. Its content is already durable here, so an upload whose
//! publication is turned away - a fence, a store fault, a lost response - stays finalizable rather than
//! being lost with the request that carried it.

use peryx_storage::meta::{
    BackpressureState, IntentAdmission, IntentLimits, IntentStageOutcome, MetaStore, QuotaReservationRecord,
};
use serde::{Deserialize, Serialize};

use axum::http::header;
use axum::response::Response;

use super::ServeError;
use crate::error::{ErrorCode, error_response};

/// The per-authority admission ceilings and the fraction of them at which backpressure trips. Bounds
/// the retained backlog per repository authority so a home that stops finalizing cannot let staged
/// uploads grow without limit, and no single busy repository starves the ledger of every other.
/// Backpressure trips at 80% of either ceiling, one shed signal ahead of the hard bound.
pub(in crate::registry) const STAGING_LIMITS: IntentLimits = IntentLimits {
    max_records: 65_536,
    max_bytes: 64 * 1024 * 1024 * 1024,
    backpressure_percent: 80,
};

/// The prefix OCI blob admission mints its intent keys under, which is what tells the finalizer an
/// intent is one of its own rather than another ecosystem's.
pub(in crate::registry) const BLOB_INTENT_PREFIX: &str = "oci:blob:";

/// The payload version this build writes and [`super::finalize`] accepts. A retained payload from a
/// future version is left pending rather than guessed at.
pub(in crate::registry) const PAYLOAD_VERSION: u32 = 1;

/// The datacenter id recorded for a deployment that configures no roster.
const STANDALONE_DC: &str = "local";

/// How many seconds a shed client is told to wait before retrying.
const SHED_RETRY_AFTER: header::HeaderValue = header::HeaderValue::from_static("30");

/// The finalization envelope a staged OCI blob intent carries, versioned so a payload written by a
/// later build is recognised as unreadable rather than misread. It names everything the home needs
/// after the request and its upload session are gone: which index and repository the digest joins, the
/// operation whose one terminal result a retry replays, and the quota reservation the admission took.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(in crate::registry) struct BlobIntent {
    pub(in crate::registry) version: u32,
    pub(in crate::registry) index: String,
    pub(in crate::registry) repo: String,
    /// The canonical OCI authority key the repository homes under.
    pub(in crate::registry) authority: String,
    pub(in crate::registry) digest: String,
    /// The committed byte length: the whole body for a monolithic push, the session's committed offset
    /// for a resumable one, so a resumable completion is bound to the offset it finished at.
    pub(in crate::registry) size: u64,
    pub(in crate::registry) ingress_dc: String,
    pub(in crate::registry) operation: String,
    /// The resumable session whose durable record the publication closes; `None` for a monolithic push.
    pub(in crate::registry) session: Option<String>,
    /// The quota reservation the push took, committed by whichever publication wins.
    pub(in crate::registry) reservation: Option<QuotaReservationRecord>,
}

/// The identity of a durable blob offered for ingress admission. Its bytes are already committed;
/// these fields bind the intent that keeps the upload finalizable near the client.
pub(in crate::registry) struct AdmissionRequest<'a> {
    pub(in crate::registry) index: &'a str,
    pub(in crate::registry) repo: &'a str,
    pub(in crate::registry) digest: &'a str,
    pub(in crate::registry) size: u64,
    pub(in crate::registry) operation: &'a str,
    pub(in crate::registry) session: Option<&'a str>,
    pub(in crate::registry) reservation: Option<&'a QuotaReservationRecord>,
    pub(in crate::registry) ingress_dc: &'a str,
}

pub(in crate::registry) enum Admission {
    /// The upload is retained under this intent key, which settles once its membership commits.
    Staged(String),
    /// The authority's retained backlog is full. Carries the response to return unchanged.
    Shed(Box<Response>),
}

/// Retain `request` for home finalization, at most `limits` un-finalized intents per authority.
///
/// # Errors
/// Returns a store error when the intent ledger cannot be read or committed.
pub(in crate::registry) fn admit(
    meta: &MetaStore,
    limits: IntentLimits,
    request: &AdmissionRequest<'_>,
    now: i64,
) -> Result<Admission, ServeError> {
    let authority = crate::name::authority_key(request.repo);
    let key = intent_key(request.index, request.repo, request.digest);
    let payload = serde_json::to_vec(&BlobIntent {
        version: PAYLOAD_VERSION,
        index: request.index.to_owned(),
        repo: request.repo.to_owned(),
        authority: authority.clone(),
        digest: request.digest.to_owned(),
        size: request.size,
        ingress_dc: request.ingress_dc.to_owned(),
        operation: request.operation.to_owned(),
        session: request.session.map(str::to_owned),
        reservation: request.reservation.cloned(),
    })
    .expect("a blob ingress intent serializes");
    let result = meta.stage_intent(
        IntentAdmission {
            authority: &authority,
            key: &key,
            digest: request.digest,
            size: request.size,
            payload: &payload,
        },
        limits,
        now,
    )?;
    match result.outcome {
        // A blob key already binds its content by digest, so a resend of the same digest is the same
        // upload and resolves onto the intent already staged for it.
        IntentStageOutcome::Admitted | IntentStageOutcome::Duplicate => {
            if result.pressure == BackpressureState::Backpressured {
                let usage = meta.staged_intent_usage(&authority)?;
                tracing::warn!(
                    authority,
                    records = usage.records,
                    bytes = usage.bytes,
                    "oci ingress admission backpressured: authority retention crossed the soft threshold ahead of its hard bound"
                );
            }
            Ok(Admission::Staged(key))
        }
        IntentStageOutcome::Conflict
        | IntentStageOutcome::RejectedOverRecordLimit
        | IntentStageOutcome::RejectedOverByteLimit => Ok(Admission::Shed(Box::new(shed()))),
    }
}

/// The intent key a blob is retained under. The digest is part of it, so the key binds the content and
/// a resend of the same layer to the same repository deduplicates onto one retained record.
pub(in crate::registry) fn intent_key(index: &str, repo: &str, digest: &str) -> String {
    format!("{BLOB_INTENT_PREFIX}{index}:{repo}:{digest}")
}

pub(in crate::registry) fn ingress_dc(topology: &peryx_core::TopologyConfig) -> String {
    topology
        .local_datacenter()
        .map_or_else(|| STANDALONE_DC.to_owned(), ToOwned::to_owned)
}

/// Shed a push whose authority has no retention left: a `503` carrying a [`Retry-After`] so the client
/// backs off until the home drains the backlog, rather than the registry publishing a membership no
/// home could recover.
///
/// [`Retry-After`]: https://www.rfc-editor.org/rfc/rfc9110.html#field.retry-after
fn shed() -> Response {
    let mut response = error_response(
        ErrorCode::Unavailable,
        "ingress admission retention is full for this repository; retry the push",
    );
    response.headers_mut().insert(header::RETRY_AFTER, SHED_RETRY_AFTER);
    response
}

#[cfg(test)]
#[path = "../../tests/unit/registry/admission/tests.rs"]
mod tests;
