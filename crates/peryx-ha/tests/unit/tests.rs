use std::collections::BTreeSet;
use std::num::NonZeroUsize;

use async_trait::async_trait;
use mockall::mock;
use peryx_storage::blob::{BlobDurability, BlobError, BlobStorage, Digest, DurabilityRequirement};
use peryx_storage::meta::MetaStore;
use rstest::rstest;

use super::*;

mock! {
    Reader {}

    #[async_trait]
    impl RemoteBlobReader for Reader {
        async fn read_through(
            &self,
            meta: &MetaStore,
            blobs: &BlobStorage,
            digest: &Digest,
        ) -> Result<ReadThroughOutcome, ReadThroughError>;
    }
}

fn stores() -> (tempfile::TempDir, MetaStore, BlobStorage) {
    let directory = tempfile::tempdir().unwrap();
    let meta = MetaStore::open(directory.path().join("peryx.redb")).unwrap();
    let blobs = BlobStorage::filesystem(directory.path().join("blobs"));
    (directory, meta, blobs)
}

#[rstest]
#[case(AvailabilityMode::None, "none", DurabilityRequirement::LOCAL, false)]
#[case(AvailabilityMode::Dc, "dc", DurabilityRequirement::REPLICATED, true)]
#[case(AvailabilityMode::Ha, "ha", DurabilityRequirement::REPLICATED, true)]
fn availability_mode_contract(
    #[case] mode: AvailabilityMode,
    #[case] name: &str,
    #[case] requirement: DurabilityRequirement,
    #[case] distributed: bool,
) {
    assert_eq!(mode.as_str(), name);
    assert_eq!(mode.durability_requirement(), requirement);
    assert_eq!(mode.is_distributed(), distributed);
    assert_eq!(
        serde_json::from_str::<AvailabilityMode>(&format!("\"{name}\"")).unwrap(),
        mode
    );
}

#[test]
fn aggregate_delta_saturates_each_total() {
    assert_eq!(
        AggregateDelta {
            downloads: u64::MAX,
            bytes: 3,
        }
        .saturating_add(AggregateDelta {
            downloads: 1,
            bytes: u64::MAX,
        }),
        AggregateDelta {
            downloads: u64::MAX,
            bytes: u64::MAX,
        }
    );
}

#[rstest]
#[case(ControlCommand::AddLearner { datacenter: "east".into(), address: "node".into() }, "add_learner", "east")]
#[case(ControlCommand::PromoteVoter { datacenter: "east".into() }, "promote_voter", "east")]
#[case(ControlCommand::RemoveVoter { datacenter: "east".into() }, "remove_voter", "east")]
#[case(ControlCommand::ReplaceVoter { remove: "west".into(), datacenter: "east".into(), address: "node".into() }, "replace_voter", "east")]
#[case(ControlCommand::TransferAuthority { authority: "repo".into(), new_home: "east".into() }, "transfer_authority", "repo")]
#[case(ControlCommand::AdvanceEpoch { authority: "repo".into() }, "advance_epoch", "repo")]
fn control_command_contract(#[case] command: ControlCommand, #[case] kind: &str, #[case] target: &str) {
    assert_eq!(command.kind(), kind);
    assert_eq!(command.target(), target);
}

#[rstest]
#[case(CommandOutcome::Committed, "committed")]
#[case(CommandOutcome::NoChange, "no_change")]
fn command_outcome_contract(#[case] outcome: CommandOutcome, #[case] name: &str) {
    assert_eq!(outcome.as_str(), name);
    assert_eq!(serde_json::to_string(&outcome).unwrap(), format!("\"{name}\""));
}

#[rstest]
#[case(ControlError::NotLeader { leader: None }, "not_leader", "not the consensus leader")]
#[case(ControlError::NotLeader { leader: Some("node".into()) }, "not_leader", "not the consensus leader; leader at node")]
#[case(ControlError::Unavailable("down".into()), "unavailable", "consensus command did not commit: down")]
#[case(ControlError::Invalid("bad".into()), "invalid", "invalid command: bad")]
#[case(
    ControlError::Overloaded,
    "overloaded",
    "too many concurrent availability commands in flight"
)]
#[case(
    ControlError::KeyReuse,
    "key_reuse",
    "idempotency key already used for a different command"
)]
fn control_error_contract(#[case] error: ControlError, #[case] kind: &str, #[case] message: &str) {
    assert_eq!(error.kind(), kind);
    assert_eq!(error.to_string(), message);
}

#[test]
fn voter_roster_applies_addition_before_removal() {
    let current = BTreeSet::from([1, 2]);
    assert_eq!(plan_voter_roster(&current, Some(3), Some(1)), BTreeSet::from([2, 3]));
    assert_eq!(plan_voter_roster(&current, None, None), current);
    assert_eq!(plan_voter_roster(&current, Some(2), Some(3)), current);
}

#[test]
fn availability_task_error_exposes_stable_fields() {
    let error = AvailabilityTaskError::new("copy_failed", "peer unavailable");
    assert_eq!(error.code(), "copy_failed");
    assert_eq!(error.message(), "peer unavailable");
    assert_eq!(error.to_string(), "copy_failed: peer unavailable");
    assert!(std::error::Error::source(&error).is_none());
}

#[tokio::test]
async fn remote_fill_without_reader_is_disabled() {
    let (_directory, meta, blobs) = stores();
    assert_eq!(
        fill_from_remote_placement(None, &meta, &blobs, &Digest::of(b"missing")).await,
        None
    );
}

#[tokio::test]
async fn remote_fill_returns_stored_metadata_after_service() {
    let (_directory, meta, blobs) = stores();
    let content = b"content";
    let digest = blobs.blocking().put_bytes(content).unwrap();
    let mut reader = MockReader::new();
    reader
        .expect_read_through()
        .once()
        .return_once(|_, _, _| Ok(ReadThroughOutcome::Served));

    assert_eq!(
        fill_from_remote_placement(Some(&reader), &meta, &blobs, &digest)
            .await
            .unwrap()
            .bytes,
        content.len() as u64
    );
}

#[rstest]
#[case(Ok(ReadThroughOutcome::Served))]
#[case(Ok(ReadThroughOutcome::Unavailable))]
#[case(Err(ReadThroughError::Blob(BlobError::from(std::io::Error::other("failed")))))]
#[tokio::test]
async fn remote_fill_returns_none_without_local_content(#[case] outcome: Result<ReadThroughOutcome, ReadThroughError>) {
    let (_directory, meta, blobs) = stores();
    let mut reader = MockReader::new();
    reader.expect_read_through().once().return_once(|_, _, _| outcome);

    assert_eq!(
        fill_from_remote_placement(Some(&reader), &meta, &blobs, &Digest::of(b"missing")).await,
        None
    );
}

#[rstest]
#[case(OperationKind::Publish, "publish")]
#[case(OperationKind::Withdraw, "withdraw")]
#[case(OperationKind::Delete, "delete")]
#[case(OperationKind::CacheFill, "cache-fill")]
#[case(OperationKind::Visibility, "visibility")]
fn operation_kind_contract(#[case] kind: OperationKind, #[case] name: &str) {
    assert_eq!(kind.as_str(), name);
    assert_eq!(kind.to_string(), name);
    assert_eq!(serde_json::to_string(&kind).unwrap(), format!("\"{name}\""));
}

#[rstest]
#[case(DurabilityPolicy::Local, 8, 1)]
#[case(DurabilityPolicy::Majority, 8, 5)]
#[case(DurabilityPolicy::Everywhere, 8, 8)]
#[case(DurabilityPolicy::AtLeast(NonZeroUsize::new(3).unwrap()), 8, 3)]
fn durability_policy_contract(#[case] policy: DurabilityPolicy, #[case] configured: usize, #[case] expected: usize) {
    assert_eq!(policy.required_acks(configured), expected);
}

#[rstest]
#[case(TransportError::Disconnected, true, None)]
#[case(TransportError::Timeout, true, None)]
#[case(TransportError::ServerError { status: 503 }, true, None)]
#[case(TransportError::AtCapacity, true, None)]
#[case(TransportError::Unauthenticated, false, Some("unauthenticated"))]
#[case(TransportError::BadStatus { status: 404 }, false, Some("bad_status"))]
#[case(TransportError::Malformed, false, Some("malformed"))]
#[case(TransportError::FrameTooLarge { limit: 1, actual: 2 }, false, Some("frame_too_large"))]
#[case(TransportError::TooManyOperations { limit: 1, actual: 2 }, false, Some("too_many_operations"))]
#[case(TransportError::SourceChanged { expected: "a".into(), actual: "b".into() }, false, Some("source_changed"))]
#[case(TransportError::FrontierGap { expected: 1, actual: 2 }, false, Some("frontier_gap"))]
#[case(TransportError::EmptyBatch { frontier: 2, after: 1 }, false, Some("empty_batch"))]
#[case(TransportError::DigestMismatch { expected: "a".into(), actual: "b".into() }, false, Some("digest_mismatch"))]
#[case(TransportError::BlobNotFound { digest: "a".into() }, false, Some("blob_not_found"))]
fn transport_error_contract(
    #[case] error: TransportError,
    #[case] retryable: bool,
    #[case] terminal_reason: Option<&str>,
) {
    assert_eq!(error.is_retryable(), retryable);
    assert_eq!(error.terminal_reason(), terminal_reason);
    assert!(!error.to_string().is_empty());
}

#[rstest]
#[case(ByteAckDecision::Acknowledged { nodes: vec!["one".into()] }, true)]
#[case(ByteAckDecision::Pending { nodes: Vec::new(), remaining: 1 }, false)]
fn byte_ack_decision_contract(#[case] decision: ByteAckDecision, #[case] acknowledged: bool) {
    assert_eq!(decision.is_acknowledged(), acknowledged);
}

#[rstest]
#[case(ByteEvidence::Filesystem(ByteAckDecision::Acknowledged { nodes: vec!["one".into()] }), true, BlobDurability::Filesystem)]
#[case(ByteEvidence::Filesystem(ByteAckDecision::Pending { nodes: Vec::new(), remaining: 1 }), false, BlobDurability::Filesystem)]
#[case(ByteEvidence::ObjectStore { acknowledged: true }, true, BlobDurability::ObjectStore)]
#[case(ByteEvidence::ObjectStore { acknowledged: false }, false, BlobDurability::ObjectStore)]
fn byte_evidence_contract(#[case] evidence: ByteEvidence, #[case] durable: bool, #[case] scope: BlobDurability) {
    assert_eq!(evidence.is_durable(), durable);
    assert_eq!(evidence.scope(), scope);
}

#[rstest]
#[case(None, false)]
#[case(Some(""), false)]
#[case(Some("00"), false)]
#[case(Some("01"), true)]
#[case(Some("ff"), true)]
#[case(Some("00-00-00-01"), true)]
#[case(Some("00-00-00-gg"), false)]
#[case(Some("00-00-00-001"), false)]
fn sampling_contract(#[case] traceparent: Option<&str>, #[case] expected: bool) {
    assert_eq!(sampled(traceparent), expected);
}

#[rstest]
#[case(None, false)]
#[case(Some("00-00-00-01"), false)]
#[case(Some("00-00-00-01"), true)]
fn operation_telemetry_emits_only_sampled_context(#[case] traceparent: Option<&str>, #[case] sampled: bool) {
    let telemetry = OperationTelemetry {
        source: "east".into(),
        epoch: 1,
        serial: 2,
        kind: "publish",
        traceparent: traceparent.map(str::to_owned),
        sampled,
    };
    telemetry.emit();
    assert_eq!(serde_json::to_value(&telemetry).unwrap()["sampled"], sampled);
}

#[test]
fn public_error_messages_preserve_context() {
    assert_eq!(CompletenessError.to_string(), "distributed analytics are unavailable");
    assert_eq!(
        OwnershipError::NotLeader { leader: None }.to_string(),
        "not the ownership leader"
    );
    assert_eq!(
        OwnershipError::NotLeader {
            leader: Some("node".into()),
        }
        .to_string(),
        "not the ownership leader; leader at node"
    );
    assert_eq!(
        OwnershipError::Unavailable("down".into()).to_string(),
        "ownership claim did not commit: down"
    );
}
