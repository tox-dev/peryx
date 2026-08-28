use peryx_ha::{ArtifactPlacement, ArtifactSource};
use peryx_identity::Principal;
use peryx_storage::meta::{IntentAdmission, IntentLimits, IntentPhase, OperationState};

use super::support::*;
use crate::serving::finalize::{
    Finalization, FinalizeDescriptor, FinalizeError, FinalizeFailure, finalize_admitted_upload,
};

const ARTIFACT: &[u8] = b"finalized-artifact-bytes";
const INTENT_KEY: &str = "pypi:hosted:flask:flask-1.0-py3-none-any.whl";
const AUTHORITY: &str = "flask";
const RECORD: &[u8] = br#"{"filename":"flask-1.0-py3-none-any.whl"}"#;

fn digest() -> String {
    Digest::of(ARTIFACT).as_str().to_owned()
}

fn operation() -> String {
    format!("{INTENT_KEY}:{}", digest())
}

const LIMITS: IntentLimits = IntentLimits {
    max_records: 1_000,
    max_bytes: 1 << 30,
    backpressure_percent: 80,
};

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

fn admit(meta: &MetaStore) {
    stage(meta, &digest());
    meta.put_artifact_placement(&digest(), &ArtifactPlacement::record(ArtifactSource::Hosted, true))
        .unwrap();
}

const fn descriptor<'a>(operation: &'a str, digest: &'a str, principal: &'a Principal) -> FinalizeDescriptor<'a> {
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

#[tokio::test]
async fn test_finalize_publishes_rows_outcome_and_intent_advance() {
    let harness = authority_harness().await;
    initialize_distributed_schema(&harness.state);
    admit(&harness.state.serving.meta);
    let principal = Principal::Named {
        subject: "uploader".to_owned(),
    };
    let operation = operation();
    let digest = digest();

    let outcome = finalize_admitted_upload(
        &harness.state.serving,
        INTENT_KEY,
        &descriptor(&operation, &digest, &principal),
    )
    .await
    .unwrap();

    assert_eq!(
        outcome,
        Finalization::Published {
            response: b"upload accepted".to_vec()
        }
    );
    assert_eq!(
        harness.state.serving.meta.current_serial().unwrap(),
        1,
        "the publish appends exactly one outbox journal entry"
    );
    let record = harness
        .state
        .serving
        .meta
        .operation_outcome(&operation)
        .unwrap()
        .unwrap();
    assert_eq!(record.state, OperationState::Published);
    assert_eq!(record.response, b"upload accepted");
    assert_eq!(
        harness
            .state
            .serving
            .meta
            .staged_intent(INTENT_KEY)
            .unwrap()
            .unwrap()
            .phase,
        IntentPhase::Admitted,
        "the finalize advances the intent out of pending"
    );
}

#[tokio::test]
async fn test_finalize_replays_the_first_result_without_a_second_write() {
    let harness = authority_harness().await;
    initialize_distributed_schema(&harness.state);
    admit(&harness.state.serving.meta);
    let principal = Principal::Named {
        subject: "uploader".to_owned(),
    };
    let operation = operation();
    let digest = digest();
    finalize_admitted_upload(
        &harness.state.serving,
        INTENT_KEY,
        &descriptor(&operation, &digest, &principal),
    )
    .await
    .unwrap();

    let replay = finalize_admitted_upload(
        &harness.state.serving,
        INTENT_KEY,
        &descriptor(&operation, &digest, &principal),
    )
    .await
    .unwrap();

    assert_eq!(
        replay,
        Finalization::Replayed {
            response: b"upload accepted".to_vec()
        }
    );
    assert_eq!(
        harness.state.serving.meta.current_serial().unwrap(),
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

    let result = finalize_admitted_upload(
        &harness.state.serving,
        INTENT_KEY,
        &descriptor(&operation, &digest, &principal),
    )
    .await;

    assert_eq!(result, Err(FinalizeError::NotStaged));
}

#[rstest::rstest]
#[case::fenced(FinalizeFailure::Fenced)]
#[case::unauthorized(FinalizeFailure::Unauthorized)]
#[case::missing_placement(FinalizeFailure::MissingPlacement)]
#[case::checksum(FinalizeFailure::ChecksumMismatch)]
#[tokio::test]
async fn test_a_validation_failure_rejects_before_publication(#[case] failure: FinalizeFailure) {
    let harness = authority_harness().await;
    initialize_distributed_schema(&harness.state);
    let digest = digest();
    stage(&harness.state.serving.meta, &digest);

    if failure != FinalizeFailure::MissingPlacement {
        harness
            .state
            .serving
            .meta
            .put_artifact_placement(&digest, &ArtifactPlacement::record(ArtifactSource::Hosted, true))
            .unwrap();
    }
    if failure == FinalizeFailure::Fenced {
        install_authority(
            &harness.state,
            AuthorityDouble {
                committed: 0,
                current: 0,
            },
        );
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

    let result = finalize_admitted_upload(&harness.state.serving, INTENT_KEY, &request).await;

    assert_eq!(result, Err(FinalizeError::Rejected(failure)));
    assert_eq!(
        harness.state.serving.meta.current_serial().unwrap(),
        0,
        "a rejected finalize publishes nothing"
    );
    assert!(
        harness
            .state
            .serving
            .meta
            .operation_outcome(&operation)
            .unwrap()
            .is_none(),
        "a refusal records no terminal outcome, so a retry re-evaluates it"
    );
    assert_eq!(
        harness
            .state
            .serving
            .meta
            .staged_intent(INTENT_KEY)
            .unwrap()
            .unwrap()
            .phase,
        IntentPhase::Pending,
        "a rejected finalize leaves the intent pending",
    );
}

#[tokio::test]
async fn test_a_transient_refusal_finalizes_on_retry_once_the_condition_clears() {
    let harness = authority_harness().await;
    initialize_distributed_schema(&harness.state);
    stage(&harness.state.serving.meta, &digest());
    let principal = Principal::Named {
        subject: "uploader".to_owned(),
    };
    let operation = operation();
    let digest = digest();

    let first = finalize_admitted_upload(
        &harness.state.serving,
        INTENT_KEY,
        &descriptor(&operation, &digest, &principal),
    )
    .await;
    assert_eq!(first, Err(FinalizeError::Rejected(FinalizeFailure::MissingPlacement)));

    harness
        .state
        .serving
        .meta
        .put_artifact_placement(&digest, &ArtifactPlacement::record(ArtifactSource::Hosted, true))
        .unwrap();
    let retry = finalize_admitted_upload(
        &harness.state.serving,
        INTENT_KEY,
        &descriptor(&operation, &digest, &principal),
    )
    .await;

    assert_eq!(
        retry,
        Ok(Finalization::Published {
            response: b"upload accepted".to_vec()
        })
    );
    assert_eq!(harness.state.serving.meta.current_serial().unwrap(), 1);
}

#[tokio::test]
async fn test_finalize_fails_closed_for_an_unknown_index() {
    let harness = harness_with(true, true).await;
    initialize_distributed_schema(&harness.state);
    admit(&harness.state.serving.meta);
    let principal = Principal::Named {
        subject: "uploader".to_owned(),
    };
    let operation = operation();
    let digest = digest();
    let mut request = descriptor(&operation, &digest, &principal);
    request.index_name = "no-such-index";

    let result = finalize_admitted_upload(&harness.state.serving, INTENT_KEY, &request).await;

    assert_eq!(
        result,
        Err(FinalizeError::Rejected(FinalizeFailure::Unauthorized)),
        "an index no ACL covers grants no write",
    );
}
