//! Finalizing an admitted upload at its authority's home datacenter.

use peryx_driver::state::{ClusterStatus, HomeClaim, OwnershipAuthority, OwnershipError, TransferOutcome};
use peryx_identity::Principal;
use peryx_storage::meta::{
    ArtifactPlacement, ArtifactSource, IntentAdmission, IntentLimits, IntentPhase, OperationState,
};

use super::support::*;
use crate::serving::finalize::{
    Finalization, FinalizeDescriptor, FinalizeError, FinalizeFailure, finalize_admitted_upload,
};

/// The artifact whose admitted bytes a finalize publishes.
const ARTIFACT: &[u8] = b"finalized-artifact-bytes";
/// The staging key the admission minted, `pypi:{route}:{project}:{filename}`.
const INTENT_KEY: &str = "pypi:hosted:flask:flask-1.0-py3-none-any.whl";
/// The project the upload publishes under, which is also the fenced authority.
const AUTHORITY: &str = "flask";
/// The serialized file record served on the project page; opaque to the finalize path.
const RECORD: &[u8] = br#"{"filename":"flask-1.0-py3-none-any.whl"}"#;

/// The artifact's sha256, as both admission and finalize see it.
fn digest() -> String {
    Digest::of(ARTIFACT).as_str().to_owned()
}

/// The operation id whose terminal outcome a retry replays, `{intent_key}:{digest}`.
fn operation() -> String {
    format!("{INTENT_KEY}:{}", digest())
}

/// Generous per-authority ceilings; the finalize tests exercise a single admission, not the bounds.
const LIMITS: IntentLimits = IntentLimits {
    max_records: 1_000,
    max_bytes: 1 << 30,
    backpressure_percent: 80,
};

/// Stage the admitted intent for `digest`, the durable record a home finalize reads.
fn stage(meta: &MetaStore, digest: &str) {
    meta.stage_intent(
        IntentAdmission {
            authority: AUTHORITY,
            key: INTENT_KEY,
            digest,
            size: ARTIFACT.len() as u64,
            payload: b"payload",
        },
        LIMITS,
        1000,
    )
    .unwrap();
}

/// Stage the admitted intent and place the artifact's bytes, the state a home finalize reads.
fn admit(meta: &MetaStore) {
    stage(meta, &digest());
    meta.put_artifact_placement(&digest(), &ArtifactPlacement::record(ArtifactSource::Hosted, true))
        .unwrap();
}

/// A descriptor whose digest and size agree with the admitted intent and whose principal writes here.
fn descriptor<'a>(operation: &'a str, digest: &'a str, principal: &'a Principal) -> FinalizeDescriptor<'a> {
    FinalizeDescriptor {
        operation,
        authority: AUTHORITY,
        principal,
        index_name: "hosted",
        normalized: "flask",
        display: "Flask",
        filename: "flask-1.0-py3-none-any.whl",
        artifact_sha256: digest,
        artifact_size: ARTIFACT.len() as u64,
        record: RECORD,
        version: "1.0",
        submitted_at_unix: 1000,
        expiry_unix: Some(5000),
    }
}

/// An ownership group whose committed epoch is `epoch`. Epoch `0` is the unassigned sentinel, which
/// admits nothing, so a finalize presenting it is fenced.
struct Group {
    epoch: u64,
}

#[async_trait::async_trait]
impl OwnershipAuthority for Group {
    async fn has_home(&self, _authority: &str) -> bool {
        self.epoch != 0
    }

    async fn claim_home(&self, _authority: &str) -> Result<HomeClaim, OwnershipError> {
        Ok(HomeClaim::AlreadyHomed)
    }

    fn cluster_status(&self) -> ClusterStatus {
        ClusterStatus {
            leader: Some("east".to_owned()),
            term: 1,
            voters: vec!["east".to_owned()],
        }
    }

    async fn committed_epoch(&self, _authority: &str) -> u64 {
        self.epoch
    }

    async fn admit_epoch(&self, _authority: &str, presented: u64) -> bool {
        self.epoch != 0 && presented == self.epoch
    }

    async fn transfer_home(
        &self,
        _authority: &str,
        _new_home: &str,
    ) -> Result<Option<TransferOutcome>, OwnershipError> {
        Ok(None)
    }
}

#[tokio::test]
async fn test_finalize_publishes_rows_outcome_and_intent_advance() {
    let harness = harness_with(true, true).await;
    admit(&harness.state.meta);
    let principal = Principal::Named {
        subject: "uploader".to_owned(),
    };
    let operation = operation();
    let digest = digest();

    let outcome = finalize_admitted_upload(&harness.state, INTENT_KEY, &descriptor(&operation, &digest, &principal))
        .await
        .unwrap();

    assert_eq!(
        outcome,
        Finalization::Published {
            response: b"upload accepted".to_vec()
        }
    );
    assert_eq!(
        harness.state.meta.current_serial().unwrap(),
        1,
        "the publish appends exactly one outbox journal entry"
    );
    let record = harness.state.meta.operation_outcome(&operation).unwrap().unwrap();
    assert_eq!(record.state, OperationState::Published);
    assert_eq!(record.response, b"upload accepted");
    assert_eq!(
        harness.state.meta.staged_intent(INTENT_KEY).unwrap().unwrap().phase,
        IntentPhase::Admitted,
        "the finalize advances the intent out of pending"
    );
}

#[tokio::test]
async fn test_finalize_replays_the_first_result_without_a_second_write() {
    let harness = harness_with(true, true).await;
    admit(&harness.state.meta);
    let principal = Principal::Named {
        subject: "uploader".to_owned(),
    };
    let operation = operation();
    let digest = digest();
    finalize_admitted_upload(&harness.state, INTENT_KEY, &descriptor(&operation, &digest, &principal))
        .await
        .unwrap();

    let replay = finalize_admitted_upload(&harness.state, INTENT_KEY, &descriptor(&operation, &digest, &principal))
        .await
        .unwrap();

    assert_eq!(
        replay,
        Finalization::Replayed {
            response: b"upload accepted".to_vec()
        }
    );
    assert_eq!(
        harness.state.meta.current_serial().unwrap(),
        1,
        "the replay appends no second journal entry"
    );
}

#[tokio::test]
async fn test_finalize_returns_not_staged_for_an_absent_intent() {
    let harness = harness_with(true, true).await;
    let principal = Principal::Named {
        subject: "uploader".to_owned(),
    };
    let operation = operation();
    let digest = digest();

    let result =
        finalize_admitted_upload(&harness.state, INTENT_KEY, &descriptor(&operation, &digest, &principal)).await;

    assert_eq!(result, Err(FinalizeError::NotStaged));
}

#[rstest::rstest]
#[case::fenced(FinalizeFailure::Fenced)]
#[case::unauthorized(FinalizeFailure::Unauthorized)]
#[case::missing_placement(FinalizeFailure::MissingPlacement)]
#[case::checksum(FinalizeFailure::ChecksumMismatch)]
#[tokio::test]
async fn test_a_validation_failure_rejects_before_publication(#[case] failure: FinalizeFailure) {
    let harness = harness_with(true, true).await;
    let digest = digest();
    stage(&harness.state.meta, &digest);
    // Place the bytes for every case but the one whose refusal is their absence.
    if failure != FinalizeFailure::MissingPlacement {
        harness
            .state
            .meta
            .put_artifact_placement(&digest, &ArtifactPlacement::record(ArtifactSource::Hosted, true))
            .unwrap();
    }
    if failure == FinalizeFailure::Fenced {
        harness.state.set_ownership_authority(Arc::new(Group { epoch: 0 }));
    }
    let principal = match failure {
        FinalizeFailure::Unauthorized => Principal::Named {
            subject: "stranger".to_owned(),
        },
        _ => Principal::Named {
            subject: "uploader".to_owned(),
        },
    };
    let operation = operation();
    let mut request = descriptor(&operation, &digest, &principal);
    if failure == FinalizeFailure::ChecksumMismatch {
        request.artifact_size += 1;
    }

    let result = finalize_admitted_upload(&harness.state, INTENT_KEY, &request).await;

    assert_eq!(result, Err(FinalizeError::Rejected(failure)));
    assert_eq!(
        harness.state.meta.current_serial().unwrap(),
        0,
        "a rejected finalize publishes nothing"
    );
    assert!(
        harness.state.meta.operation_outcome(&operation).unwrap().is_none(),
        "a refusal records no terminal outcome, so a retry re-evaluates it"
    );
    assert_eq!(
        harness.state.meta.staged_intent(INTENT_KEY).unwrap().unwrap().phase,
        IntentPhase::Pending,
        "a rejected finalize leaves the intent pending",
    );
}

#[tokio::test]
async fn test_a_transient_refusal_finalizes_on_retry_once_the_condition_clears() {
    let harness = harness_with(true, true).await;
    stage(&harness.state.meta, &digest());
    let principal = Principal::Named {
        subject: "uploader".to_owned(),
    };
    let operation = operation();
    let digest = digest();

    // The bytes are not placed yet, so the first finalize refuses without recording a durable outcome.
    let first =
        finalize_admitted_upload(&harness.state, INTENT_KEY, &descriptor(&operation, &digest, &principal)).await;
    assert_eq!(first, Err(FinalizeError::Rejected(FinalizeFailure::MissingPlacement)));

    // Once the bytes arrive, the retry re-evaluates and publishes rather than replaying the refusal.
    harness
        .state
        .meta
        .put_artifact_placement(&digest, &ArtifactPlacement::record(ArtifactSource::Hosted, true))
        .unwrap();
    let retry =
        finalize_admitted_upload(&harness.state, INTENT_KEY, &descriptor(&operation, &digest, &principal)).await;

    assert_eq!(
        retry,
        Ok(Finalization::Published {
            response: b"upload accepted".to_vec()
        })
    );
    assert_eq!(harness.state.meta.current_serial().unwrap(), 1);
}

#[tokio::test]
async fn test_finalize_fails_closed_for_an_unknown_index() {
    let harness = harness_with(true, true).await;
    admit(&harness.state.meta);
    let principal = Principal::Named {
        subject: "uploader".to_owned(),
    };
    let operation = operation();
    let digest = digest();
    let mut request = descriptor(&operation, &digest, &principal);
    request.index_name = "no-such-index";

    let result = finalize_admitted_upload(&harness.state, INTENT_KEY, &request).await;

    assert_eq!(
        result,
        Err(FinalizeError::Rejected(FinalizeFailure::Unauthorized)),
        "an index no ACL covers grants no write",
    );
}
