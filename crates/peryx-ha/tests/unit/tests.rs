use std::convert::Infallible;
use std::num::NonZeroUsize;
use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use rstest::rstest;
use tokio::sync::oneshot;

use super::*;

#[path = "endpoint_tests.rs"]
mod endpoint_tests;
#[path = "placement_tests.rs"]
mod placement_tests;

#[rstest]
#[case::active(10, 9, false)]
#[case::boundary(10, 10, true)]
#[case::expired(10, 11, true)]
fn reclaim_guard_expiry_has_an_inclusive_boundary(
    #[case] expires_at_unix: i64,
    #[case] now: i64,
    #[case] expected: bool,
) {
    assert_eq!(ReclaimGuard { expires_at_unix }.is_expired_at(now), expected);
}

#[test]
fn static_backend_id_preserves_its_value() {
    assert_eq!(BackendId::from_static("filesystem").as_str(), "filesystem");
}

#[test]
fn artifact_placement_query_defaults_to_the_first_bounded_page() {
    assert_eq!(
        ArtifactPlacementQuery::default(),
        ArtifactPlacementQuery {
            cursor: None,
            limit: 25,
        }
    );
}

#[test]
fn artifact_placement_health_totals_each_availability_class() {
    assert_eq!(
        ArtifactPlacementHealth {
            local: 2,
            remote_only: 3,
            unavailable: 5,
        }
        .total(),
        10
    );
}

#[rstest]
#[case::hosted(ArtifactSource::Hosted, "hosted", false, ByteAvailability::Unavailable)]
#[case::proxy(ArtifactSource::Proxy, "proxy", true, ByteAvailability::RemoteOnly)]
#[case::generated(ArtifactSource::Generated, "generated", false, ByteAvailability::Unavailable)]
fn artifact_source_projects_absent_bytes(
    #[case] source: ArtifactSource,
    #[case] label: &str,
    #[case] has_upstream: bool,
    #[case] availability: ByteAvailability,
) {
    assert_eq!(source.as_str(), label);
    assert_eq!(source.has_upstream(), has_upstream);
    assert_eq!(
        ArtifactPlacement::record(source, false),
        ArtifactPlacement { source, availability }
    );
}

#[rstest]
#[case::local(ByteAvailability::Local, "local", true)]
#[case::remote(ByteAvailability::RemoteOnly, "remote_only", false)]
#[case::unavailable(ByteAvailability::Unavailable, "unavailable", false)]
fn byte_availability_projects_its_wire_value(
    #[case] availability: ByteAvailability,
    #[case] label: &str,
    #[case] local: bool,
) {
    assert_eq!((availability.as_str(), availability.is_local()), (label, local));
}

#[rstest]
#[case::verified(PlacementEvent::BytesVerified, ByteAvailability::Local)]
#[case::removed(PlacementEvent::BytesRemoved, ByteAvailability::RemoteOnly)]
#[case::failed(PlacementEvent::WriteFailed, ByteAvailability::RemoteOnly)]
#[case::repaired(PlacementEvent::Repaired { present: true }, ByteAvailability::Local)]
fn artifact_placement_applies_events(#[case] event: PlacementEvent, #[case] availability: ByteAvailability) {
    assert_eq!(
        ArtifactPlacement::record(ArtifactSource::Proxy, false).after(event),
        ArtifactPlacement {
            source: ArtifactSource::Proxy,
            availability,
        }
    );
}

#[test]
fn reconcile_input_builds_its_stable_key_and_pending_record() {
    let input = NewReconcileEntry {
        source: "east",
        epoch: 4,
        serial: 12,
        durably_committed: true,
        already_applied: false,
        superseded: true,
        traceparent: Some("parent"),
    };

    assert_eq!(input.key(), "east:4:12");
    assert_eq!(
        input.record(1_800_000_000),
        ReconcileEntry {
            source: "east".to_owned(),
            epoch: 4,
            serial: 12,
            durably_committed: true,
            already_applied: false,
            superseded: true,
            traceparent: Some("parent".to_owned()),
            outcome: None,
            updated_at_unix: 1_800_000_000,
        }
    );
}

#[rstest]
#[case::pending(None, true)]
#[case::settled(Some("replayed".to_owned()), false)]
fn reconcile_entry_reports_pending_status(#[case] outcome: Option<String>, #[case] expected: bool) {
    assert_eq!(
        ReconcileEntry {
            source: "east".to_owned(),
            epoch: 4,
            serial: 12,
            durably_committed: true,
            already_applied: false,
            superseded: false,
            traceparent: None,
            outcome,
            updated_at_unix: 1_800_000_000,
        }
        .is_pending(),
        expected
    );
}

struct Availability(Option<BlobMetadata>);

#[async_trait]
impl BlobAvailability for Availability {
    async fn ensure_local(&self, _digest: &Digest) -> Result<Option<BlobMetadata>, BlobAvailabilityError> {
        Ok(self.0)
    }
}

struct FailedAvailability(BlobAvailabilityFailure);

#[async_trait]
impl BlobAvailability for FailedAvailability {
    async fn ensure_local(&self, _digest: &Digest) -> Result<Option<BlobMetadata>, BlobAvailabilityError> {
        Err(BlobAvailabilityError::new(self.0, std::io::Error::other("unavailable")))
    }
}

struct Durability(WriteDurability);

#[async_trait]
impl BlobWriteDurability for Durability {
    async fn confirm(&self, _write: CommittedBlob<'_>) -> WriteDurability {
        self.0
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ShutdownStage {
    Activation,
    ListenerAcceptance,
    Workers,
    Consensus,
    DedicatedRuntimes,
    FallbackDrop,
}

struct LifecycleHandle {
    stages: Arc<Mutex<Vec<ShutdownStage>>>,
    started: Option<oneshot::Sender<()>>,
    release: Option<oneshot::Receiver<()>>,
    completed: Option<oneshot::Sender<()>>,
    failure: Option<oneshot::Receiver<AvailabilityFailure>>,
    shutdown_complete: bool,
}

impl LifecycleHandle {
    fn record(&self, stage: ShutdownStage) {
        self.stages.lock().unwrap().push(stage);
    }
}

#[async_trait]
impl AvailabilityHandle for LifecycleHandle {
    type Active = Self;
    type Error = Infallible;

    fn activate(self) -> Result<Self::Active, Self::Error> {
        self.record(ShutdownStage::Activation);
        Ok(self)
    }

    async fn shutdown(mut self) -> Result<(), AvailabilityShutdownError> {
        self.record(ShutdownStage::ListenerAcceptance);
        self.started.take().unwrap().send(()).unwrap();
        self.release.take().unwrap().await.unwrap();
        self.record(ShutdownStage::Workers);
        self.record(ShutdownStage::Consensus);
        self.record(ShutdownStage::DedicatedRuntimes);
        self.shutdown_complete = true;
        self.completed.take().unwrap().send(()).unwrap();
        Ok(())
    }
}

#[async_trait]
impl ActiveAvailabilityHandle for LifecycleHandle {
    async fn wait_for_failure(&mut self) -> AvailabilityFailure {
        self.failure.take().unwrap().await.unwrap()
    }

    async fn shutdown(&mut self) -> Result<(), AvailabilityShutdownError> {
        self.record(ShutdownStage::ListenerAcceptance);
        self.started.take().unwrap().send(()).unwrap();
        self.release.take().unwrap().await.unwrap();
        self.record(ShutdownStage::Workers);
        self.record(ShutdownStage::Consensus);
        self.record(ShutdownStage::DedicatedRuntimes);
        self.shutdown_complete = true;
        self.completed.take().unwrap().send(()).unwrap();
        Ok(())
    }
}

impl Drop for LifecycleHandle {
    fn drop(&mut self) {
        if !self.shutdown_complete {
            self.record(ShutdownStage::FallbackDrop);
        }
    }
}

#[tokio::test]
async fn active_availability_shutdown_runs_each_stage() {
    let stages = Arc::new(Mutex::new(Vec::new()));
    let (started, started_rx) = oneshot::channel();
    let (release_tx, release) = oneshot::channel();
    let (completed, completed_rx) = oneshot::channel();
    release_tx.send(()).unwrap();
    let mut handle = LifecycleHandle {
        stages: Arc::clone(&stages),
        started: Some(started),
        release: Some(release),
        completed: Some(completed),
        failure: None,
        shutdown_complete: false,
    };

    <LifecycleHandle as ActiveAvailabilityHandle>::shutdown(&mut handle)
        .await
        .unwrap();
    started_rx.await.unwrap();
    completed_rx.await.unwrap();
    assert_eq!(
        *stages.lock().unwrap(),
        [
            ShutdownStage::ListenerAcceptance,
            ShutdownStage::Workers,
            ShutdownStage::Consensus,
            ShutdownStage::DedicatedRuntimes,
        ]
    );
}

fn prepared_availability(handle: LifecycleHandle) -> PreparedAvailability<(), LifecycleHandle> {
    PreparedAvailability {
        public_routes: (),
        private_routes: None,
        metrics: Vec::new(),
        is_replica: false,
        handle,
    }
}

#[test]
fn prepared_availability_activates_the_handle() {
    let stages = Arc::new(Mutex::new(Vec::new()));
    let (started, _) = oneshot::channel();
    let (_, release) = oneshot::channel();
    let prepared = prepared_availability(LifecycleHandle {
        stages: Arc::clone(&stages),
        started: Some(started),
        release: Some(release),
        completed: None,
        failure: None,
        shutdown_complete: false,
    });

    let _active = prepared.activate().unwrap();

    assert_eq!(*stages.lock().unwrap(), [ShutdownStage::Activation]);
}

#[tokio::test]
async fn availability_handle_reports_signaled_failure() {
    let (failure_sender, failure_receiver) = oneshot::channel();
    let mut handle = LifecycleHandle {
        stages: Arc::new(Mutex::new(Vec::new())),
        started: None,
        release: None,
        completed: None,
        failure: Some(failure_receiver),
        shutdown_complete: false,
    };

    failure_sender
        .send(AvailabilityFailure::new("runtime stopped"))
        .unwrap();

    assert_eq!(handle.wait_for_failure().await.to_string(), "runtime stopped");
}

#[tokio::test]
async fn prepared_availability_awaits_backend_shutdown() {
    let stages = Arc::new(Mutex::new(Vec::new()));
    let (started_sender, started_receiver) = oneshot::channel();
    let (release_sender, release_receiver) = oneshot::channel();
    let (completed_sender, mut completed_receiver) = oneshot::channel();
    let shutdown = tokio::spawn(
        prepared_availability(LifecycleHandle {
            stages: Arc::clone(&stages),
            started: Some(started_sender),
            release: Some(release_receiver),
            completed: Some(completed_sender),
            failure: None,
            shutdown_complete: false,
        })
        .shutdown(),
    );

    started_receiver.await.unwrap();
    assert!(matches!(
        completed_receiver.try_recv(),
        Err(oneshot::error::TryRecvError::Empty)
    ));
    release_sender.send(()).unwrap();
    shutdown.await.unwrap().unwrap();
    completed_receiver.await.unwrap();

    assert_eq!(
        *stages.lock().unwrap(),
        [
            ShutdownStage::ListenerAcceptance,
            ShutdownStage::Workers,
            ShutdownStage::Consensus,
            ShutdownStage::DedicatedRuntimes,
        ]
    );
}

#[test]
fn applied_frontier_notifies_subscribers_and_retains_the_latest_value() {
    let frontier = AppliedFrontier::default();
    let clone = frontier.clone();
    let subscriber = frontier.subscribe();

    frontier.publish(7);

    assert_eq!(*subscriber.borrow(), 7);
    assert_eq!(*clone.subscribe().borrow(), 7);
}

#[test]
fn prepared_availability_drop_uses_handle_fallback() {
    let stages = Arc::new(Mutex::new(Vec::new()));
    let (started, _) = oneshot::channel();
    let (_, release) = oneshot::channel();
    drop(prepared_availability(LifecycleHandle {
        stages: Arc::clone(&stages),
        started: Some(started),
        release: Some(release),
        completed: None,
        failure: None,
        shutdown_complete: false,
    }));

    assert_eq!(*stages.lock().unwrap(), [ShutdownStage::FallbackDrop]);
}

#[test]
fn availability_shutdown_error_preserves_first_source() {
    let error = AvailabilityShutdownError::new(
        AvailabilityShutdownStage::Listener,
        std::io::Error::other("listener stopped"),
    );

    assert_eq!(error.failures()[0].stage, AvailabilityShutdownStage::Listener);
    assert_eq!(
        std::error::Error::source(&error).unwrap().to_string(),
        "listener stopped"
    );
}

#[test]
fn availability_shutdown_error_aggregates_and_formats_failures() {
    let mut error = AvailabilityShutdownError::new(
        AvailabilityShutdownStage::Listener,
        std::io::Error::other("listener stopped"),
    );
    error.push(
        AvailabilityShutdownStage::Runtime,
        std::io::Error::other("runtime stopped"),
    );

    assert_eq!(
        error
            .failures()
            .iter()
            .map(|failure| (failure.stage, failure.source.to_string()))
            .collect::<Vec<_>>(),
        [
            (AvailabilityShutdownStage::Listener, "listener stopped".into()),
            (AvailabilityShutdownStage::Runtime, "runtime stopped".into()),
        ]
    );
    assert_eq!(
        error.to_string(),
        "availability shutdown failed; Listener: listener stopped; Runtime: runtime stopped"
    );
}

#[rstest]
#[case(
    AvailabilityMode::None,
    "none",
    DurabilityRequirement::LOCAL,
    AvailabilityResources::None
)]
#[case(
    AvailabilityMode::Dc,
    "dc",
    DurabilityRequirement::REPLICATED,
    AvailabilityResources::Distributed
)]
#[case(
    AvailabilityMode::Ha,
    "ha",
    DurabilityRequirement::REPLICATED,
    AvailabilityResources::Distributed
)]
fn availability_mode_contract(
    #[case] mode: AvailabilityMode,
    #[case] name: &str,
    #[case] requirement: DurabilityRequirement,
    #[case] resources: AvailabilityResources,
) {
    assert_eq!(mode.as_str(), name);
    assert_eq!(mode.durability_requirement(), requirement);
    assert_eq!(mode.is_distributed(), resources.has_distributed_state());
    assert_eq!(mode.availability_resources(), resources);
    assert_eq!(
        serde_json::from_str::<AvailabilityMode>(&format!("\"{name}\"")).unwrap(),
        mode
    );
    if mode == AvailabilityMode::None {
        assert_eq!(AvailabilityMode::default(), mode);
    }
}

#[rstest]
#[case(AvailabilityResources::None, false)]
#[case(AvailabilityResources::Distributed, true)]
fn availability_resources_are_atomic(#[case] resources: AvailabilityResources, #[case] enabled: bool) {
    assert_eq!(resources.has_distributed_state(), enabled);
    assert_eq!(resources.has_routes(), enabled);
    assert_eq!(resources.has_metrics(), enabled);
    assert_eq!(resources.has_background_tasks(), enabled);
}

#[rstest]
#[case::none_writer(AvailabilityResources::None, false, None)]
#[case::none_replica(AvailabilityResources::None, true, None)]
#[case::distributed_writer(AvailabilityResources::Distributed, false, None)]
#[case::distributed_replica(AvailabilityResources::Distributed, true, Some(AVAILABILITY_BLOB_VIEW))]
fn availability_resources_select_replica_views(
    #[case] resources: AvailabilityResources,
    #[case] is_replica: bool,
    #[case] expected: Option<&str>,
) {
    assert_eq!(resources.replica_derived_view(is_replica), expected);
}

#[test]
fn aggregate_delta_saturates_each_total() {
    assert_eq!(AggregateDelta::default(), AggregateDelta { downloads: 0, bytes: 0 });
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

#[test]
fn generated_value_contracts_distinguish_fields_and_variants() {
    let key = AggregateKey {
        day: 1,
        repository: "repository".into(),
        resource: "resource".into(),
        group: "group".into(),
        source: "source".into(),
    };
    for different in [
        AggregateKey { day: 2, ..key.clone() },
        AggregateKey {
            repository: "other".into(),
            ..key.clone()
        },
        AggregateKey {
            resource: "other".into(),
            ..key.clone()
        },
        AggregateKey {
            group: "other".into(),
            ..key.clone()
        },
        AggregateKey {
            source: "other".into(),
            ..key.clone()
        },
    ] {
        assert_derived_contract(&key, &different);
        assert_ne!(key.cmp(&different), std::cmp::Ordering::Equal);
        assert_ne!(key.partial_cmp(&different), Some(std::cmp::Ordering::Equal));
    }

    assert_derived_contract(
        &AggregateDelta { downloads: 1, bytes: 2 },
        &AggregateDelta { downloads: 2, bytes: 2 },
    );
    assert_derived_contract(
        &AggregateDelta { downloads: 1, bytes: 2 },
        &AggregateDelta { downloads: 1, bytes: 3 },
    );

    assert_derived_contract(
        &CommandReceipt {
            term: 1,
            index: 2,
            outcome: CommandOutcome::Committed,
            old_voters: vec!["old".into()],
            new_voters: vec!["new".into()],
        },
        &CommandReceipt {
            term: 2,
            index: 2,
            outcome: CommandOutcome::Committed,
            old_voters: vec!["old".into()],
            new_voters: vec!["new".into()],
        },
    );
    assert_derived_contract(&CommandOutcome::Committed, &CommandOutcome::NoChange);
    assert_derived_contract(&ControlActor::new("one"), &ControlActor::new("two"));
    assert_derived_contract(
        &AvailabilityTaskReport {
            processed: 1,
            changed: 2,
        },
        &AvailabilityTaskReport {
            processed: 2,
            changed: 2,
        },
    );
    assert_derived_contract(
        &AvailabilityTaskError::new("one", "message"),
        &AvailabilityTaskError::new("two", "message"),
    );
    assert_derived_contract(
        &ReplicaPage {
            changes: 1,
            serial: 2,
            primary_serial: 3,
        },
        &ReplicaPage {
            changes: 2,
            serial: 2,
            primary_serial: 3,
        },
    );
    assert_derived_contract(&DurabilityPolicy::Local, &DurabilityPolicy::Majority);
    assert_derived_contract(
        &DurabilityPolicy::AtLeast(NonZeroUsize::new(1).unwrap()),
        &DurabilityPolicy::AtLeast(NonZeroUsize::new(2).unwrap()),
    );
    assert_derived_contract(
        &ByteEvidence::Filesystem(ByteAckDecision::Acknowledged {
            nodes: vec!["one".into()],
            required: 1,
        }),
        &ByteEvidence::ObjectStore { acknowledged: true },
    );
}

#[rstest]
#[case(
    ControlCommand::AddLearner { datacenter: "east".into(), address: "one".into() },
    ControlCommand::AddLearner { datacenter: "east".into(), address: "two".into() }
)]
#[case(
    ControlCommand::PromoteVoter { datacenter: "east".into() },
    ControlCommand::RemoveVoter { datacenter: "east".into() }
)]
#[case(
    ControlCommand::ReplaceVoter { remove: "west".into(), datacenter: "east".into(), address: "one".into() },
    ControlCommand::ReplaceVoter { remove: "north".into(), datacenter: "east".into(), address: "one".into() }
)]
#[case(
    ControlCommand::TransferAuthority { authority: "repo".into(), new_home: "east".into() },
    ControlCommand::TransferAuthority { authority: "repo".into(), new_home: "west".into() }
)]
#[case(
    ControlCommand::AdvanceEpoch { authority: "one".into() },
    ControlCommand::AdvanceEpoch { authority: "two".into() }
)]
fn control_command_derives_distinguish_payloads(#[case] value: ControlCommand, #[case] different: ControlCommand) {
    assert_derived_contract(&value, &different);
}

#[rstest]
#[case(ControlError::NotLeader { leader: None }, ControlError::NotLeader { leader: Some("node".into()) })]
#[case(ControlError::Unavailable("one".into()), ControlError::Unavailable("two".into()))]
#[case(ControlError::Invalid("one".into()), ControlError::Invalid("two".into()))]
#[case(ControlError::Overloaded, ControlError::KeyReuse)]
fn control_error_derives_distinguish_payloads(#[case] value: ControlError, #[case] different: ControlError) {
    assert_derived_contract(&value, &different);
}

#[rstest]
#[case(TransportError::Disconnected, TransportError::Timeout)]
#[case(TransportError::ServerError { status: 500 }, TransportError::ServerError { status: 503 })]
#[case(TransportError::BadStatus { status: 400 }, TransportError::BadStatus { status: 404 })]
#[case(
    TransportError::FrameTooLarge { limit: 1, actual: 2 },
    TransportError::FrameTooLarge { limit: 2, actual: 2 }
)]
#[case(
    TransportError::TooManyOperations { limit: 1, actual: 2 },
    TransportError::TooManyOperations { limit: 1, actual: 3 }
)]
#[case(
    TransportError::SourceChanged { expected: "one".into(), actual: "two".into() },
    TransportError::SourceChanged { expected: "one".into(), actual: "three".into() }
)]
#[case(
    TransportError::FrontierGap { expected: 1, actual: 2 },
    TransportError::FrontierGap { expected: 2, actual: 2 }
)]
#[case(
    TransportError::EmptyBatch { frontier: 2, after: 1 },
    TransportError::EmptyBatch { frontier: 3, after: 1 }
)]
#[case(
    TransportError::DigestMismatch { expected: "one".into(), actual: "two".into() },
    TransportError::DigestMismatch { expected: "two".into(), actual: "two".into() }
)]
#[case(
    TransportError::BlobNotFound { digest: "one".into() },
    TransportError::BlobNotFound { digest: "two".into() }
)]
fn transport_error_derives_distinguish_payloads(#[case] value: TransportError, #[case] different: TransportError) {
    assert_derived_contract(&value, &different);
    assert!(std::error::Error::source(&value).is_none());
}

#[rstest]
#[case(
    ControlCommand::AddLearner { datacenter: "east".into(), address: "node".into() },
    r#"{"type":"add_learner","datacenter":"east","address":"node"}"#,
    "add_learner",
    "east"
)]
#[case(
    ControlCommand::PromoteVoter { datacenter: "east".into() },
    r#"{"type":"promote_voter","datacenter":"east"}"#,
    "promote_voter",
    "east"
)]
#[case(
    ControlCommand::RemoveVoter { datacenter: "east".into() },
    r#"{"type":"remove_voter","datacenter":"east"}"#,
    "remove_voter",
    "east"
)]
#[case(
    ControlCommand::ReplaceVoter {
        remove: "west".into(),
        datacenter: "east".into(),
        address: "node".into(),
    },
    r#"{"type":"replace_voter","remove":"west","datacenter":"east","address":"node"}"#,
    "replace_voter",
    "east"
)]
#[case(
    ControlCommand::TransferAuthority { authority: "repo".into(), new_home: "east".into() },
    r#"{"type":"transfer_authority","authority":"repo","new_home":"east"}"#,
    "transfer_authority",
    "repo"
)]
#[case(
    ControlCommand::AdvanceEpoch { authority: "repo".into() },
    r#"{"type":"advance_epoch","authority":"repo"}"#,
    "advance_epoch",
    "repo"
)]
fn control_command_contract(
    #[case] command: ControlCommand,
    #[case] json: &str,
    #[case] kind: &str,
    #[case] target: &str,
) {
    assert_eq!(command.kind(), kind);
    assert_eq!(command.target(), target);
    assert_eq!(serde_json::from_str::<ControlCommand>(json).unwrap(), command);
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
    assert!(std::error::Error::source(&error).is_none());
}

#[test]
fn control_actor_preserves_identity() {
    let actor = ControlActor::new("operator");
    assert_eq!((actor.as_str(), actor.to_string()), ("operator", "operator".into()));
}

#[test]
fn availability_task_error_exposes_stable_fields() {
    let error = AvailabilityTaskError::new("copy_failed", "peer unavailable");
    assert_eq!(error.code(), "copy_failed");
    assert_eq!(error.message(), "peer unavailable");
    assert_eq!(error.to_string(), "copy_failed: peer unavailable");
    assert!(std::error::Error::source(&error).is_none());
}

struct SuccessfulDrainer;

#[async_trait]
impl AuthorityDrainer for SuccessfulDrainer {
    async fn drain(
        &self,
        now: i64,
        cancelled: &(dyn Fn() -> bool + Send + Sync),
    ) -> Result<AvailabilityTaskReport, AvailabilityTaskError> {
        Ok(AvailabilityTaskReport {
            processed: u64::try_from(now).unwrap(),
            changed: u64::from(cancelled()),
        })
    }
}

#[tokio::test]
async fn test_authority_drainer_is_object_safe() {
    let drainer: &dyn AuthorityDrainer = &SuccessfulDrainer;
    assert_eq!(
        drainer.drain(7, &|| false).await.unwrap(),
        AvailabilityTaskReport {
            processed: 7,
            changed: 0
        }
    );
}

#[rstest]
#[case(BlobAvailabilityFailure::Placement, "Placement: unavailable")]
#[case(BlobAvailabilityFailure::Transfer, "Transfer: unavailable")]
#[case(BlobAvailabilityFailure::Storage, "Storage: unavailable")]
#[tokio::test]
async fn blob_availability_error_preserves_failure(#[case] failure: BlobAvailabilityFailure, #[case] message: &str) {
    let error = FailedAvailability(failure)
        .ensure_local(&Digest::of(b"missing"))
        .await
        .unwrap_err();

    assert_eq!(error.kind(), failure);
    assert_eq!(error.to_string(), message);
    assert_eq!(std::error::Error::source(&error).unwrap().to_string(), "unavailable");
}

#[tokio::test]
async fn blob_services_exposes_availability() {
    let metadata = BlobMetadata {
        bytes: 7,
        modified: None,
    };
    let services = BlobServices::new(
        Some(Arc::new(Availability(Some(metadata)))),
        Arc::new(Durability(WriteDurability::Unavailable)),
    );
    assert_eq!(
        services
            .availability()
            .unwrap()
            .ensure_local(&Digest::of(b"content"))
            .await
            .unwrap(),
        Some(metadata)
    );
}

#[tokio::test]
async fn blob_services_can_disable_availability() {
    let services = BlobServices::new(None, Arc::new(Durability(WriteDurability::Pending)));
    assert!(services.availability().is_none());
}

#[rstest]
#[case(WriteDurability::Confirmed { scope: BlobDurability::Filesystem })]
#[case(WriteDurability::Confirmed { scope: BlobDurability::ObjectStore })]
#[case(WriteDurability::Pending)]
#[case(WriteDurability::Unavailable)]
#[tokio::test]
async fn blob_services_dispatches_durability(#[case] outcome: WriteDurability) {
    let digest = Digest::of(b"content");
    let write = CommittedBlob::new(
        &digest,
        "repository",
        AuthorityEpoch(7),
        None,
        BlobDurability::Filesystem,
    );
    assert_eq!(
        BlobServices::new(None, Arc::new(Durability(outcome)))
            .durability()
            .confirm(write)
            .await,
        outcome
    );
}

#[test]
fn committed_blob_exposes_write_context() {
    let digest = Digest::of(b"content");
    let write = CommittedBlob::new(
        &digest,
        "repository",
        AuthorityEpoch(7),
        None,
        BlobDurability::ObjectStore,
    );

    assert_eq!(write.digest(), &digest);
    assert_eq!(write.authority(), "repository");
    assert_eq!(write.epoch(), AuthorityEpoch(7));
    assert_eq!(write.commit(), None);
    assert_eq!(write.local_durability(), BlobDurability::ObjectStore);
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
#[case(DurabilityPolicy::Local, "local")]
#[case(DurabilityPolicy::Majority, "majority")]
#[case(DurabilityPolicy::Everywhere, "everywhere")]
#[case(DurabilityPolicy::AtLeast(NonZeroUsize::new(3).unwrap()), "at_least")]
fn durability_policy_name_contract(#[case] policy: DurabilityPolicy, #[case] expected: &str) {
    assert_eq!(policy.as_str(), expected);
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
#[case::source_changed(TransportError::SourceChanged { expected: "a".into(), actual: "b".into() }, true)]
#[case::frontier_gap(TransportError::FrontierGap { expected: 1, actual: 2 }, true)]
#[case::empty_batch(TransportError::EmptyBatch { frontier: 2, after: 1 }, true)]
#[case::bad_status(TransportError::BadStatus { status: 429 }, false)]
fn protocol_error_rearm_contract(#[case] error: TransportError, #[case] expected: bool) {
    assert_eq!(error.requires_explicit_rearm(), expected);
}

#[rstest]
#[case(ByteAckDecision::Acknowledged { nodes: vec!["one".into()], required: 1 }, true)]
#[case(ByteAckDecision::Pending { nodes: Vec::new(), required: 1, remaining: 1 }, false)]
fn byte_ack_decision_contract(#[case] decision: ByteAckDecision, #[case] acknowledged: bool) {
    assert_eq!(decision.is_acknowledged(), acknowledged);
}

#[test]
fn byte_ack_decision_derives_preserve_variants() {
    assert_derived_contract(
        &ByteAckDecision::Acknowledged {
            nodes: vec!["one".into()],
            required: 1,
        },
        &ByteAckDecision::Pending {
            nodes: vec!["one".into()],
            required: 2,
            remaining: 1,
        },
    );
}

#[rstest]
#[case(ByteEvidence::Filesystem(ByteAckDecision::Acknowledged { nodes: vec!["one".into()], required: 1 }), true, BlobDurability::Filesystem)]
#[case(ByteEvidence::Filesystem(ByteAckDecision::Pending { nodes: Vec::new(), required: 1, remaining: 1 }), false, BlobDurability::Filesystem)]
#[case(ByteEvidence::ObjectStore { acknowledged: true }, true, BlobDurability::ObjectStore)]
#[case(ByteEvidence::ObjectStore { acknowledged: false }, false, BlobDurability::ObjectStore)]
fn byte_evidence_contract(#[case] evidence: ByteEvidence, #[case] durable: bool, #[case] scope: BlobDurability) {
    assert_eq!(evidence.is_durable(), durable);
    assert_eq!(evidence.scope(), scope);
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

#[rstest]
#[case::active(94, true)]
#[case::guard_boundary(95, false)]
#[case::past_expiry(101, false)]
fn authority_write_lease_reserves_the_clock_skew_window(#[case] now: i64, #[case] admitted: bool) {
    let lease = AuthorityWriteLease {
        authority: "proj".to_owned(),
        epoch: 1,
        id: "write-1".to_owned(),
        expires_at_unix: 100,
    };

    assert_eq!(lease.admits(now), admitted);
}

struct AuthorityWithoutWriteLeases;

#[async_trait]
impl OwnershipAuthority for AuthorityWithoutWriteLeases {
    async fn claim_home(&self, _authority: &str) -> Result<HomeClaim, OwnershipError> {
        Err(OwnershipError::Unavailable("claim disabled".to_owned()))
    }

    fn cluster_status(&self) -> ClusterStatus {
        ClusterStatus {
            leader: None,
            term: 0,
            voters: Vec::new(),
        }
    }

    async fn committed_epoch(&self, _authority: &str) -> u64 {
        0
    }

    async fn admit_epoch(&self, _authority: &str, _presented: u64) -> bool {
        false
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
async fn ownership_authority_defaults_fail_closed_without_write_lease_support() {
    let authority = AuthorityWithoutWriteLeases;
    let lease = AuthorityWriteLease {
        authority: "proj".to_owned(),
        epoch: 1,
        id: "write-1".to_owned(),
        expires_at_unix: 100,
    };

    assert!(matches!(
        authority.begin_epoch_write("proj", 1).await,
        Err(OwnershipError::Unavailable(message)) if message == "ownership write leasing is unavailable"
    ));
    assert!(matches!(
        authority.finish_epoch_write(&lease).await,
        Err(OwnershipError::Unavailable(message)) if message == "ownership write leasing is unavailable"
    ));
    assert!(matches!(
        authority.claim_home("proj").await,
        Err(OwnershipError::Unavailable(_))
    ));
    assert_eq!(
        authority.cluster_status(),
        ClusterStatus {
            leader: None,
            term: 0,
            voters: Vec::new(),
        }
    );
    assert_eq!(authority.committed_epoch("proj").await, 0);
    assert!(!authority.admit_epoch("proj", 1).await);
    assert_eq!(authority.transfer_home("proj", "west").await.unwrap(), None);
}

#[tokio::test]
async fn ownership_authority_defaults_fail_closed_without_singleton_support() {
    let authority = AuthorityWithoutWriteLeases;
    let lease = SingletonLease {
        job: "reclamation".to_owned(),
        holder: "node-1".to_owned(),
        term: 4,
        generation: 2,
        expires_at_unix: 100,
    };

    assert!(matches!(
        authority.acquire_singleton_lease("reclamation", "node-1").await,
        Err(OwnershipError::Unavailable(message)) if message == "singleton leasing is unavailable"
    ));
    assert!(matches!(
        authority.renew_singleton_lease(&lease).await,
        Err(OwnershipError::Unavailable(message)) if message == "singleton leasing is unavailable"
    ));
    assert!(matches!(
        authority.release_singleton_lease(&lease).await,
        Err(OwnershipError::Unavailable(message)) if message == "singleton leasing is unavailable"
    ));
}

#[test]
fn frontier_reply_serialization_preserves_the_contract() {
    let reply = FrontierReply {
        epoch: 7,
        applied_frontier: 41,
    };

    assert_eq!(
        serde_json::from_str::<FrontierReply>(&serde_json::to_string(&reply).unwrap()).unwrap(),
        reply
    );
}

#[test]
fn availability_failures_preserve_debug_and_clone_contracts() {
    let failure = AvailabilityFailure::new("runtime stopped");
    assert_eq!(failure.to_string(), "runtime stopped");

    let mut shutdown = AvailabilityShutdownError::new(
        AvailabilityShutdownStage::Consensus,
        std::io::Error::other("consensus stopped"),
    );
    shutdown.push(
        AvailabilityShutdownStage::Runtime,
        std::io::Error::other("runtime stopped"),
    );
    assert_eq!(shutdown.failures().len(), 2);
    assert_eq!(shutdown.failures()[1].source.to_string(), "runtime stopped");
}

#[tokio::test]
async fn availability_runtime_prepares_its_declared_resources() {
    let stages = Arc::new(Mutex::new(Vec::new()));
    let (started, _) = oneshot::channel();
    let (_, release) = oneshot::channel();
    let runtime = TestRuntime;

    assert_eq!(<TestRuntime as AvailabilityRuntime>::routes(&runtime), "availability");
    assert!(<TestRuntime as AvailabilityRuntime>::metrics(&runtime).is_empty());

    let prepared = <TestRuntime as AvailabilityRuntime>::prepare(
        runtime,
        LifecycleHandle {
            stages: Arc::clone(&stages),
            started: Some(started),
            release: Some(release),
            completed: None,
            failure: None,
            shutdown_complete: false,
        },
    )
    .await
    .unwrap();
    assert_eq!(prepared.public_routes, "availability");
    assert_eq!(prepared.private_routes, Some("operator"));
    assert!(prepared.metrics.is_empty());
    assert!(prepared.is_replica);
    drop(prepared);
    assert_eq!(*stages.lock().unwrap(), [ShutdownStage::FallbackDrop]);
}

#[test]
fn analytics_batch_round_trip_preserves_identity_and_totals() {
    let batch = AnalyticsBatch {
        interval: IntervalId {
            producer: ProducerId("writer-east".into()),
            epoch: AuthorityEpoch(7),
            sequence: 11,
        },
        rows: vec![AggregateRow {
            key: AggregateKey {
                day: 20_000,
                repository: "packages".into(),
                resource: "project".into(),
                group: "team".into(),
                source: "mirror".into(),
            },
            delta: AggregateDelta {
                downloads: 3,
                bytes: 42,
            },
        }],
    };

    assert_eq!(
        serde_json::from_str::<AnalyticsBatch>(&serde_json::to_string(&batch).unwrap()).unwrap(),
        batch
    );
    assert_eq!(batch.rows[0].key.source, "mirror");
}

#[tokio::test]
async fn control_executor_reports_receipt_and_metrics() {
    let executor: &dyn ControlExecutor = &TestControlExecutor;
    let receipt = executor
        .execute(
            "operator",
            Some("request-7"),
            ControlCommand::TransferAuthority {
                authority: "packages".into(),
                new_home: "west".into(),
            },
        )
        .await
        .unwrap();
    assert_eq!(
        receipt,
        CommandReceipt {
            term: 7,
            index: 11,
            outcome: CommandOutcome::Committed,
            old_voters: vec!["operator".into()],
            new_voters: vec!["packages:request-7".into()],
        }
    );
    assert_eq!(serde_json::to_value(&receipt).unwrap()["term"], 7);

    let metrics = executor.metrics();
    assert_eq!(
        metrics,
        ControlMetrics {
            completed: 1,
            p50_ms: 2,
            p99_ms: 3
        }
    );
    assert_eq!(serde_json::to_value(metrics).unwrap()["completed"], 1);
}

#[test]
fn availability_task_contract_preserves_counts_and_errors() {
    let report = AvailabilityTaskReport {
        processed: 8,
        changed: 3,
    };
    assert_eq!((report.processed, report.changed), (8, 3));
    assert_eq!(
        AvailabilityTaskReport::default(),
        AvailabilityTaskReport {
            processed: 0,
            changed: 0
        }
    );

    let error = AvailabilityTaskError::new("copy_failed", "peer unavailable");
    assert_eq!(error.code(), "copy_failed");
    assert_eq!(error.message(), "peer unavailable");
    assert_eq!(error.to_string(), "copy_failed: peer unavailable");
}

#[test]
fn replica_page_and_operation_observer_preserve_replication_context() {
    let page = ReplicaPage {
        changes: 3,
        serial: 11,
        primary_serial: 13,
    };
    assert_eq!((page.changes, page.serial, page.primary_serial), (3, 11, 13));

    let observer = TestOperationObserver::default();
    let operation = OperationObservation {
        source: "writer-east".into(),
        epoch: AuthorityEpoch(7),
        serial: 11,
        kind: OperationKind::Publish,
    };
    observer.record(operation);
    assert_eq!(
        observer.observations.lock().unwrap().as_slice(),
        [OperationObservation {
            source: "writer-east".into(),
            epoch: AuthorityEpoch(7),
            serial: 11,
            kind: OperationKind::Publish,
        }]
    );
}

#[test]
fn transport_dtos_preserve_peer_and_write_context() {
    let digest = Digest::of(b"content");
    let receipt = PeerReceipt {
        node: "node-east".into(),
        digest: digest.clone(),
        size: 7,
    };
    assert_eq!(
        (&receipt.node, &receipt.digest, receipt.size),
        (&"node-east".to_owned(), &digest, 7)
    );

    let remote = RemoteAck {
        datacenter: "west".into(),
        epoch: 7,
        applied_frontier: 11,
    };
    assert_eq!(
        (remote.datacenter.as_str(), remote.epoch, remote.applied_frontier),
        ("west", 7, 11)
    );

    let request = WriteAckRequest {
        digest: &digest,
        authority: "packages",
        operation: MetadataOperation { epoch: 7, frontier: 11 },
    };
    assert_eq!(request.digest, &digest);
    assert_eq!(request.authority, "packages");
    assert_eq!(request.operation, MetadataOperation { epoch: 7, frontier: 11 });
}

#[rstest]
#[case::acknowledged(
    ByteAckDecision::Acknowledged { nodes: vec!["east".into(), "west".into()], required: 2 },
    &["east", "west"],
    2,
    None
)]
#[case::pending(
    ByteAckDecision::Pending { nodes: vec!["east".into()], required: 2, remaining: 1 },
    &["east"],
    2,
    Some(1)
)]
fn byte_ack_decision_preserves_evidence(
    #[case] decision: ByteAckDecision,
    #[case] expected_nodes: &[&str],
    #[case] expected_required: usize,
    #[case] expected_remaining: Option<usize>,
) {
    let (nodes, required, remaining) = match &decision {
        ByteAckDecision::Acknowledged { nodes, required } => (nodes, *required, None),
        ByteAckDecision::Pending {
            nodes,
            required,
            remaining,
        } => (nodes, *required, Some(*remaining)),
    };
    assert_eq!(nodes, expected_nodes);
    assert_eq!(required, expected_required);
    assert_eq!(remaining, expected_remaining);
}

#[derive(Clone, Copy)]
struct TestRuntime;

#[async_trait]
impl AvailabilityRuntime for TestRuntime {
    type Context = LifecycleHandle;
    type Routes = &'static str;
    type PreparedHandle = LifecycleHandle;
    type Error = Infallible;

    fn routes(&self) -> Self::Routes {
        "availability"
    }

    fn metrics(&self) -> Vec<Arc<dyn PrometheusSource>> {
        Vec::new()
    }

    async fn prepare(
        self,
        handle: Self::Context,
    ) -> Result<PreparedAvailability<Self::Routes, Self::PreparedHandle>, Self::Error> {
        Ok(PreparedAvailability {
            public_routes: "availability",
            private_routes: Some("operator"),
            metrics: self.metrics(),
            is_replica: true,
            handle,
        })
    }
}

struct TestControlExecutor;

#[async_trait]
impl ControlExecutor for TestControlExecutor {
    async fn execute(
        &self,
        actor: &str,
        key: Option<&str>,
        command: ControlCommand,
    ) -> Result<CommandReceipt, ControlError> {
        Ok(CommandReceipt {
            term: 7,
            index: 11,
            outcome: CommandOutcome::Committed,
            old_voters: vec![actor.into()],
            new_voters: vec![format!("{}:{}", command.target(), key.unwrap())],
        })
    }

    fn metrics(&self) -> ControlMetrics {
        ControlMetrics {
            completed: 1,
            p50_ms: 2,
            p99_ms: 3,
        }
    }
}

#[derive(Default)]
struct TestOperationObserver {
    observations: Mutex<Vec<OperationObservation>>,
}

impl OperationObserver for TestOperationObserver {
    fn record(&self, operation: OperationObservation) {
        self.observations.lock().unwrap().push(operation);
    }
}

fn assert_derived_contract<Value: Eq>(value: &Value, different: &Value) {
    assert!(value != different);
}

fn reclamation_digest(suffix: u8) -> peryx_identity::ArtifactDigest {
    peryx_identity::ArtifactDigest::from_sha256(format!("{suffix:064x}")).unwrap()
}

fn reclamation_tombstone(suffix: u8, state: ReclamationState, fence: u64) -> ReclamationTombstone {
    ReclamationTombstone {
        digest: reclamation_digest(suffix),
        state,
        required_frontier: 5,
        fence,
        attempts: 1,
        selected_at_unix: 10,
        updated_at_unix: 10,
    }
}

fn reclamation_placement(suffix: u8, state: BlobPlacementState) -> BlobPlacementRecord {
    BlobPlacementRecord {
        key: BlobPlacementKey {
            digest: reclamation_digest(suffix),
            backend: BackendId::new("filesystem").unwrap(),
            data_center: DataCenterId::new("home").unwrap(),
            location: BackendLocation::new("blobs/aa").unwrap(),
        },
        state,
        fence: 1,
        transfer_attempt: 1,
        generation: 1,
        updated_at_unix: 10,
    }
}

fn reclamation_snapshot(
    tombstone: Option<ReclamationTombstone>,
    placements: Vec<BlobPlacementRecord>,
) -> ReclamationSnapshot {
    ReclamationSnapshot { tombstone, placements }
}

#[rstest]
#[case(None, None, true)]
#[case(None, Some(4), false)]
#[case(Some(4), None, false)]
#[case(Some(5), Some(5), true)]
#[case(Some(4), Some(9), false)]
#[case(Some(9), Some(4), false)]
fn reclamation_frontier_requires_every_observed_plane(
    #[case] replica: Option<u64>,
    #[case] backup: Option<u64>,
    #[case] covers: bool,
) {
    assert_eq!(ObservedFrontier { replica, backup }.covers(5), covers);
}

#[test]
fn reclamation_selection_creates_a_pending_candidate() {
    let digest = reclamation_digest(1);
    let outcome =
        decide_reclamation_selection(&digest, &reclamation_snapshot(None, Vec::new()), false, 7, 3, 100).unwrap();

    let expected = ReclamationTombstone {
        digest,
        state: ReclamationState::Pending,
        required_frontier: 7,
        fence: 3,
        attempts: 1,
        selected_at_unix: 100,
        updated_at_unix: 100,
    };
    assert_eq!(outcome.replacement(), Some(&expected));
    assert_eq!(outcome, SelectOutcome::Selected(expected));
}

#[rstest]
#[case(true, Vec::new(), SkipReason::Referenced)]
#[case(
    false,
    vec![reclamation_placement(1, BlobPlacementState::Verified { size: 10 })],
    SkipReason::Serveable
)]
fn reclamation_selection_rejects_ineligible_digests(
    #[case] referenced: bool,
    #[case] placements: Vec<BlobPlacementRecord>,
    #[case] reason: SkipReason,
) {
    let outcome = decide_reclamation_selection(
        &reclamation_digest(1),
        &reclamation_snapshot(None, placements),
        referenced,
        5,
        1,
        10,
    )
    .unwrap();
    assert_eq!(outcome.replacement(), None);
    assert_eq!(outcome, SelectOutcome::Ineligible(reason));
}

#[rstest]
#[case(BlobPlacementState::Pending)]
#[case(BlobPlacementState::Failed { class: BlobPlacementFailure::SourceUnavailable })]
#[case(BlobPlacementState::Revoked)]
fn reclamation_selection_ignores_nonserveable_placements(#[case] state: BlobPlacementState) {
    assert!(matches!(
        decide_reclamation_selection(
            &reclamation_digest(1),
            &reclamation_snapshot(None, vec![reclamation_placement(1, state)]),
            false,
            5,
            1,
            10,
        )
        .unwrap(),
        SelectOutcome::Selected(_)
    ));
}

#[rstest]
#[case(true, Vec::new(), SkipReason::Referenced)]
#[case(
    false,
    vec![reclamation_placement(1, BlobPlacementState::Verified { size: 10 })],
    SkipReason::Serveable
)]
fn reclamation_selection_skips_an_existing_ineligible_candidate(
    #[case] referenced: bool,
    #[case] placements: Vec<BlobPlacementRecord>,
    #[case] reason: SkipReason,
) {
    let outcome = decide_reclamation_selection(
        &reclamation_digest(1),
        &reclamation_snapshot(Some(reclamation_tombstone(1, ReclamationState::Pending, 3)), placements),
        referenced,
        7,
        4,
        20,
    )
    .unwrap();
    let SelectOutcome::Skipped(record) = outcome else {
        panic!("candidate was not skipped");
    };
    assert_eq!(record.state, ReclamationState::Skipped { reason });
    assert_eq!((record.fence, record.attempts, record.updated_at_unix), (4, 2, 20));
}

#[test]
fn reclamation_selection_advances_existing_candidate_monotonically() {
    let outcome = decide_reclamation_selection(
        &reclamation_digest(1),
        &reclamation_snapshot(Some(reclamation_tombstone(1, ReclamationState::Ready, 3)), Vec::new()),
        false,
        4,
        5,
        20,
    )
    .unwrap();
    assert_eq!(
        outcome,
        SelectOutcome::Selected(ReclamationTombstone {
            digest: reclamation_digest(1),
            state: ReclamationState::Pending,
            required_frontier: 5,
            fence: 5,
            attempts: 2,
            selected_at_unix: 10,
            updated_at_unix: 20,
        })
    );
}

#[test]
fn reclamation_selection_rejects_a_stale_fence() {
    let error = decide_reclamation_selection(
        &reclamation_digest(1),
        &reclamation_snapshot(Some(reclamation_tombstone(1, ReclamationState::Pending, 5)), Vec::new()),
        false,
        5,
        3,
        20,
    )
    .unwrap_err();
    assert_eq!(error, ReclamationDecisionError::StaleFence { current: 5, applied: 3 });
}

#[test]
fn reclamation_readiness_requires_a_candidate() {
    assert_eq!(
        decide_reclamation_readiness(
            &reclamation_snapshot(None, Vec::new()),
            false,
            ObservedFrontier {
                replica: Some(5),
                backup: Some(5),
            },
            1,
            20,
        )
        .unwrap_err(),
        ReclamationDecisionError::MissingCandidate
    );
}

#[test]
fn reclamation_readiness_promotes_a_covered_candidate() {
    let outcome = decide_reclamation_readiness(
        &reclamation_snapshot(Some(reclamation_tombstone(1, ReclamationState::Pending, 3)), Vec::new()),
        false,
        ObservedFrontier {
            replica: Some(5),
            backup: Some(6),
        },
        4,
        20,
    )
    .unwrap();
    assert_eq!(
        outcome,
        ReadyOutcome::Ready(ReclamationTombstone {
            digest: reclamation_digest(1),
            state: ReclamationState::Ready,
            required_frontier: 5,
            fence: 4,
            attempts: 2,
            selected_at_unix: 10,
            updated_at_unix: 20,
        })
    );
}

#[test]
fn reclamation_readiness_preserves_a_lagging_candidate() {
    let observed = ObservedFrontier {
        replica: Some(4),
        backup: Some(9),
    };
    let outcome = decide_reclamation_readiness(
        &reclamation_snapshot(Some(reclamation_tombstone(1, ReclamationState::Pending, 3)), Vec::new()),
        false,
        observed,
        4,
        20,
    )
    .unwrap();
    assert_eq!(
        outcome,
        ReadyOutcome::NotReady {
            tombstone: ReclamationTombstone {
                digest: reclamation_digest(1),
                state: ReclamationState::Pending,
                required_frontier: 5,
                fence: 4,
                attempts: 2,
                selected_at_unix: 10,
                updated_at_unix: 20,
            },
            observed,
        }
    );
}

#[rstest]
#[case(true, Vec::new(), SkipReason::Referenced)]
#[case(
    false,
    vec![reclamation_placement(1, BlobPlacementState::Verified { size: 10 })],
    SkipReason::Serveable
)]
fn reclamation_readiness_skips_a_newly_ineligible_candidate(
    #[case] referenced: bool,
    #[case] placements: Vec<BlobPlacementRecord>,
    #[case] reason: SkipReason,
) {
    let outcome = decide_reclamation_readiness(
        &reclamation_snapshot(Some(reclamation_tombstone(1, ReclamationState::Pending, 3)), placements),
        referenced,
        ObservedFrontier {
            replica: Some(5),
            backup: Some(5),
        },
        4,
        20,
    )
    .unwrap();
    let ReadyOutcome::Skipped(record) = outcome else {
        panic!("ineligible candidate was not skipped");
    };
    assert_eq!(record.state, ReclamationState::Skipped { reason });
}

#[rstest]
#[case(ReclamationState::Ready)]
#[case(ReclamationState::Skipped { reason: SkipReason::Referenced })]
fn reclamation_readiness_preserves_terminal_state(#[case] state: ReclamationState) {
    let record = reclamation_tombstone(1, state, 3);
    let outcome = decide_reclamation_readiness(
        &reclamation_snapshot(Some(record.clone()), Vec::new()),
        false,
        ObservedFrontier {
            replica: Some(9),
            backup: Some(9),
        },
        3,
        20,
    )
    .unwrap();
    assert_eq!(outcome.replacement(), &record);
}

#[test]
fn reclamation_state_helpers_enforce_fences_and_summarize_progress() {
    let pending = reclamation_tombstone(1, ReclamationState::Pending, 5);
    let ready = reclamation_tombstone(2, ReclamationState::Ready, 5);
    let skipped = reclamation_tombstone(
        3,
        ReclamationState::Skipped {
            reason: SkipReason::Referenced,
        },
        5,
    );
    assert_eq!(
        pending.validate_fence(3),
        Err(ReclamationDecisionError::StaleFence { current: 5, applied: 3 })
    );
    assert_eq!(pending.validate_fence(5), Ok(()));
    assert!(!pending.is_skipped());
    assert!(skipped.is_skipped());
    assert_eq!(
        ReclamationProgress::from_tombstones([&pending, &ready, &skipped]),
        ReclamationProgress {
            pending: 1,
            ready: 1,
            skipped: 1,
        }
    );
}

#[test]
fn blob_placement_routing_reports_contents_and_serveability() {
    let empty = BlobPlacementRouting::default();
    assert_eq!((empty.is_empty(), empty.is_serveable()), (true, false));

    let pending = BlobPlacementRouting {
        pending: vec![reclamation_placement(1, BlobPlacementState::Pending)],
        ..BlobPlacementRouting::default()
    };
    assert_eq!((pending.is_empty(), pending.is_serveable()), (false, false));

    let remote = BlobPlacementRouting {
        verified_remote: vec![reclamation_placement(1, BlobPlacementState::Verified { size: 10 })],
        ..BlobPlacementRouting::default()
    };
    assert_eq!((remote.is_empty(), remote.is_serveable()), (false, true));
}

#[derive(Default)]
struct SnapshotStore(Mutex<Option<Vec<u8>>>);

impl VisibilitySnapshotStore for SnapshotStore {
    type Error = Infallible;

    fn load_snapshot(&self) -> Result<Option<Vec<u8>>, Self::Error> {
        Ok(self.0.lock().unwrap().clone())
    }

    fn save_snapshot(&self, bytes: &[u8]) -> Result<(), Self::Error> {
        *self.0.lock().unwrap() = Some(bytes.to_vec());
        Ok(())
    }
}

#[test]
fn visibility_snapshot_store_reference_delegates_operations() {
    let store = SnapshotStore::default();
    <&SnapshotStore as VisibilitySnapshotStore>::save_snapshot(&&store, b"snapshot").unwrap();

    assert_eq!(
        <&SnapshotStore as VisibilitySnapshotStore>::load_snapshot(&&store).unwrap(),
        Some(b"snapshot".to_vec())
    );
}
