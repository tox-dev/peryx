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
mod tests {
    use peryx_core::{NodeRole, TopologyConfig, TopologyMember, TopologyMode};
    use peryx_storage::blob::DurabilityCapabilities;
    use peryx_storage::meta::{IntentPhase, MetaStore};
    use redb::TableDefinition;

    use super::*;

    fn meta(dir: &tempfile::TempDir) -> MetaStore {
        MetaStore::open(dir.path().join("meta.redb")).unwrap()
    }

    fn request<'a>(filename: &'a str, digest: &'a str) -> AdmissionRequest<'a> {
        AdmissionRequest {
            tenant: "root/hosted",
            authority: "flask",
            filename,
            digest,
            size: 11,
            ingress_dc: "dc-a",
        }
    }

    /// Per-authority ceilings for the small-buffer tests: `records` and `bytes` hard bounds with the
    /// production 80% backpressure fraction.
    fn limits(records: u64, bytes: u64) -> IntentLimits {
        IntentLimits {
            max_records: records,
            max_bytes: bytes,
            backpressure_percent: 80,
        }
    }

    /// The response a rejected admission carried, or `None` when it admitted, so a test reads the shed
    /// status and headers or asserts nothing was refused.
    fn rejected(admission: Admission) -> Option<Response> {
        match admission {
            Admission::Reject(response) => Some(response),
            Admission::Admitted(_) => None,
        }
    }

    #[test]
    fn test_admit_stages_a_fresh_intent_bound_to_its_identity() {
        let dir = tempfile::tempdir().unwrap();
        let meta = meta(&dir);
        let request = request("flask-1.0.whl", "aa");

        assert!(matches!(
            admit(&meta, DurabilityCapabilities::FILESYSTEM, STAGING_LIMITS, &request, 10),
            Admission::Admitted(_)
        ));

        let staged = meta
            .staged_intent("pypi:root/hosted:flask:flask-1.0.whl")
            .unwrap()
            .unwrap();
        assert_eq!((staged.digest.as_str(), staged.size), ("aa", 11));
        let intent: IngressIntent = serde_json::from_slice(&staged.payload).unwrap();
        assert_eq!(intent.ingress_dc, "dc-a");
        assert_eq!(intent.operation, "pypi:root/hosted:flask:flask-1.0.whl:aa");
    }

    #[test]
    fn test_admit_accepts_an_object_store_that_proves_durability() {
        let dir = tempfile::tempdir().unwrap();
        let meta = meta(&dir);

        let admission = admit(
            &meta,
            DurabilityCapabilities::object_store(true, true),
            STAGING_LIMITS,
            &request("flask-1.0.whl", "aa"),
            10,
        );

        assert!(rejected(admission).is_none());
        assert_eq!(meta.count_staged_intents().unwrap(), 1);
    }

    #[test]
    fn test_admit_deduplicates_an_identical_resend() {
        let dir = tempfile::tempdir().unwrap();
        let meta = meta(&dir);
        let request = request("flask-1.0.whl", "aa");

        assert!(matches!(
            admit(&meta, DurabilityCapabilities::FILESYSTEM, STAGING_LIMITS, &request, 10),
            Admission::Admitted(_)
        ));
        assert!(matches!(
            admit(&meta, DurabilityCapabilities::FILESYSTEM, STAGING_LIMITS, &request, 20),
            Admission::Admitted(_)
        ));

        assert_eq!(meta.count_staged_intents().unwrap(), 1);
    }

    #[test]
    fn test_admit_refuses_different_content_for_a_taken_filename() {
        let dir = tempfile::tempdir().unwrap();
        let meta = meta(&dir);
        admit(
            &meta,
            DurabilityCapabilities::FILESYSTEM,
            STAGING_LIMITS,
            &request("flask-1.0.whl", "aa"),
            10,
        );

        let admission = admit(
            &meta,
            DurabilityCapabilities::FILESYSTEM,
            STAGING_LIMITS,
            &request("flask-1.0.whl", "bb"),
            20,
        );

        assert_eq!(rejected(admission).unwrap().status(), StatusCode::BAD_REQUEST);
    }

    #[test]
    fn test_admit_sheds_load_when_the_record_ceiling_is_reached() {
        let dir = tempfile::tempdir().unwrap();
        let meta = meta(&dir);
        admit(
            &meta,
            DurabilityCapabilities::FILESYSTEM,
            limits(1, 1 << 20),
            &request("flask-1.0.whl", "aa"),
            10,
        );

        let admission = admit(
            &meta,
            DurabilityCapabilities::FILESYSTEM,
            limits(1, 1 << 20),
            &request("click-8.0.whl", "bb"),
            20,
        );

        let response = rejected(admission).unwrap();
        assert_eq!(response.status(), StatusCode::SERVICE_UNAVAILABLE);
        assert_eq!(
            response.headers()[header::RETRY_AFTER],
            SHED_RETRY_AFTER_SECS.to_string().as_str()
        );
    }

    #[test]
    fn test_admit_sheds_load_when_the_next_intent_would_cross_the_byte_ceiling() {
        let dir = tempfile::tempdir().unwrap();
        let meta = meta(&dir);
        admit(
            &meta,
            DurabilityCapabilities::FILESYSTEM,
            limits(8, 15),
            &request("flask-1.0.whl", "aa"),
            10,
        );

        let admission = admit(
            &meta,
            DurabilityCapabilities::FILESYSTEM,
            limits(8, 15),
            &request("click-8.0.whl", "bb"),
            20,
        );

        let response = rejected(admission).unwrap();
        assert_eq!(response.status(), StatusCode::SERVICE_UNAVAILABLE);
        assert_eq!(
            response.headers()[header::RETRY_AFTER],
            SHED_RETRY_AFTER_SECS.to_string().as_str()
        );
    }

    #[test]
    fn test_admit_still_stages_while_backpressured_below_the_hard_bound() {
        let dir = tempfile::tempdir().unwrap();
        let meta = meta(&dir);

        // Two records, backpressure at 80%: the first admitted intent already sits at the soft threshold,
        // so admission stays open under backpressure rather than shedding before the hard bound.
        let admission = admit(
            &meta,
            DurabilityCapabilities::FILESYSTEM,
            limits(2, 1 << 20),
            &request("flask-1.0.whl", "aa"),
            10,
        );

        assert!(matches!(admission, Admission::Admitted(_)));
        assert_eq!(meta.count_staged_intents().unwrap(), 1);
    }

    #[test]
    fn test_admit_bounds_each_authority_independently() {
        let dir = tempfile::tempdir().unwrap();
        let meta = meta(&dir);
        let full = admit(
            &meta,
            DurabilityCapabilities::FILESYSTEM,
            limits(1, 1 << 20),
            &request("flask-1.0.whl", "aa"),
            10,
        );
        assert!(matches!(full, Admission::Admitted(_)));

        // A second authority at its own ceiling admits though the first authority is full: the buffer is
        // bounded per authority, so one busy project cannot starve another.
        let other = AdmissionRequest {
            authority: "click",
            ..request("click-8.0.whl", "bb")
        };
        let admission = admit(
            &meta,
            DurabilityCapabilities::FILESYSTEM,
            limits(1, 1 << 20),
            &other,
            20,
        );

        assert!(matches!(admission, Admission::Admitted(_)));
        assert_eq!(meta.count_staged_intents().unwrap(), 2);
    }

    #[test]
    fn test_admit_refuses_a_backend_that_cannot_prove_durability() {
        let dir = tempfile::tempdir().unwrap();
        let meta = meta(&dir);

        let admission = admit(
            &meta,
            DurabilityCapabilities::object_store(false, false),
            STAGING_LIMITS,
            &request("flask-1.0.whl", "aa"),
            10,
        );

        assert_eq!(rejected(admission).unwrap().status(), StatusCode::SERVICE_UNAVAILABLE);
        assert_eq!(meta.count_staged_intents().unwrap(), 0);
    }

    #[test]
    fn test_admit_surfaces_a_store_fault_as_internal_error() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("meta.redb");
        {
            let db = redb::Database::create(&path).unwrap();
            let txn = db.begin_write().unwrap();
            {
                let mut table = txn
                    .open_table(TableDefinition::<&str, &[u8]>::new("ingress_intent"))
                    .unwrap();
                table
                    .insert("pypi:root/hosted:flask:flask-1.0.whl", b"not json".as_slice())
                    .unwrap();
            }
            txn.commit().unwrap();
        }
        let meta = MetaStore::open(&path).unwrap();

        let admission = admit(
            &meta,
            DurabilityCapabilities::FILESYSTEM,
            STAGING_LIMITS,
            &request("flask-1.0.whl", "aa"),
            10,
        );

        assert_eq!(rejected(admission).unwrap().status(), StatusCode::INTERNAL_SERVER_ERROR);
    }

    #[test]
    fn test_ingress_dc_reads_the_local_roster_member() {
        let topology = TopologyConfig {
            mode: TopologyMode::Dc,
            group: Some("group".to_owned()),
            members: vec![TopologyMember {
                node: "node-1".to_owned(),
                dc: "dc-west".to_owned(),
                address: "node-1.internal".to_owned(),
                role: NodeRole::Writer,
            }],
            local_node: Some("node-1".to_owned()),
        };

        assert_eq!(ingress_dc(&topology), "dc-west");
    }

    #[test]
    fn test_ingress_dc_falls_back_for_a_rosterless_node() {
        assert_eq!(ingress_dc(&TopologyConfig::default()), STANDALONE_DC);
    }

    /// The staged key `request("flask-1.0.whl", ..)` admits under, used to read its retained phase back.
    const STAGED_KEY: &str = "pypi:root/hosted:flask:flask-1.0.whl";

    /// Home loss before admission: the home DC is unreachable when the client uploads. Admission does not
    /// contact the home - publication and home assignment run downstream - so it still retains the write
    /// durably as a pending intent rather than refusing it, and nothing is published under the outage.
    #[test]
    fn test_fault_home_loss_before_admission_retains_the_write_unpublished() {
        let dir = tempfile::tempdir().unwrap();
        let meta = meta(&dir);

        let admission = admit(
            &meta,
            DurabilityCapabilities::FILESYSTEM,
            STAGING_LIMITS,
            &request("flask-1.0.whl", "aa"),
            10,
        );

        assert!(matches!(admission, Admission::Admitted(_)));
        assert_eq!(
            meta.staged_intent(STAGED_KEY).unwrap().unwrap().phase,
            IntentPhase::Pending
        );
    }

    /// Home loss after local durability: the bytes and intent are durable near the client, but the home
    /// never finalizes them. A reaper sweep with retention far in the future spares the pending intent, so
    /// an indefinite outage never drops a retained write in flight.
    #[test]
    fn test_fault_home_loss_after_local_durability_never_drops_the_retained_write() {
        let dir = tempfile::tempdir().unwrap();
        let meta = meta(&dir);
        admit(
            &meta,
            DurabilityCapabilities::FILESYSTEM,
            STAGING_LIMITS,
            &request("flask-1.0.whl", "aa"),
            10,
        );

        assert_eq!(meta.prune_ingress_intents(1_000_000, 60, 100).unwrap(), 0);
        assert_eq!(
            meta.staged_intent(STAGED_KEY).unwrap().unwrap().phase,
            IntentPhase::Pending
        );
        assert_eq!(meta.count_staged_intents().unwrap(), 1);
    }

    /// Home loss during retry: while the home is still down the client resends the same upload. The intent
    /// deduplicates to one retained write rather than staging its bytes twice, and it stays pending, so a
    /// retry under the outage is safe and still unpublished.
    #[test]
    fn test_fault_home_loss_during_retry_deduplicates_and_stays_pending() {
        let dir = tempfile::tempdir().unwrap();
        let meta = meta(&dir);
        let request = request("flask-1.0.whl", "aa");
        admit(&meta, DurabilityCapabilities::FILESYSTEM, STAGING_LIMITS, &request, 10);

        let resend = admit(&meta, DurabilityCapabilities::FILESYSTEM, STAGING_LIMITS, &request, 20);

        assert!(matches!(resend, Admission::Admitted(_)));
        assert_eq!(meta.count_staged_intents().unwrap(), 1);
        assert_eq!(
            meta.staged_intent(STAGED_KEY).unwrap().unwrap().phase,
            IntentPhase::Pending
        );
    }

    /// Home loss after expiry: a retained write the home never finalizes within its window expires, and the
    /// reaper reclaims it once the retention window passes, releasing the authority's slot so the buffer
    /// does not fill with dead writes during a long outage.
    #[test]
    fn test_fault_home_loss_after_expiry_reclaims_the_slot() {
        let dir = tempfile::tempdir().unwrap();
        let meta = meta(&dir);
        admit(
            &meta,
            DurabilityCapabilities::FILESYSTEM,
            STAGING_LIMITS,
            &request("flask-1.0.whl", "aa"),
            10,
        );
        meta.advance_intent(STAGED_KEY, IntentPhase::Expired, 20).unwrap();

        assert_eq!(meta.prune_ingress_intents(100, 60, 100).unwrap(), 1);
        assert_eq!(meta.staged_intent(STAGED_KEY).unwrap(), None);
        assert_eq!(meta.staged_intent_usage("flask").unwrap().records, 0);
    }
}
