use std::collections::BTreeSet;
use std::sync::Arc;
use std::time::Duration;

use axum::body::Body;
use axum::http::{Request, StatusCode};
use peryx_driver::AppState;
use peryx_ha::{BackendId, ObservedFrontier};
use peryx_ha::{
    ClusterStatus, ControlActor, ControlAuthenticationError, ControlAuthorizer, ControlPermission, HomeClaim,
    OwnershipAuthority, OwnershipError, ReclamationFrontiers, ReferenceInventory, TransferOutcome,
};
use peryx_storage::blob::{BlobStorage, BlobStore};
use peryx_storage::meta::MetaStore;
use tower::ServiceExt as _;

use super::*;

const DEADLOCK_GUARD: Duration = Duration::from_secs(90);

struct EmptyReferences;

impl ReferenceInventory for EmptyReferences {
    fn referenced(&self) -> Result<BTreeSet<String>, String> {
        Ok(BTreeSet::new())
    }
}

struct EmptyFrontiers;

struct DenyControl;

struct StaticAuthority;

#[async_trait::async_trait]
impl ControlAuthorizer for DenyControl {
    async fn authenticate(
        &self,
        _authorization: Option<&str>,
    ) -> Result<Option<ControlActor>, ControlAuthenticationError> {
        Ok(None)
    }

    fn allows(&self, _actor: &ControlActor, _permission: ControlPermission) -> bool {
        false
    }
}

impl ReclamationFrontiers for EmptyFrontiers {
    fn observe(&self) -> Option<ObservedFrontier> {
        Some(ObservedFrontier {
            replica: None,
            backup: None,
        })
    }
}

#[async_trait::async_trait]
impl OwnershipAuthority for StaticAuthority {
    async fn has_home(&self, _authority: &str) -> bool {
        true
    }

    async fn claim_home(&self, _authority: &str) -> Result<HomeClaim, OwnershipError> {
        Ok(HomeClaim::AssignedHere)
    }

    fn cluster_status(&self) -> ClusterStatus {
        ClusterStatus {
            leader: Some("local".to_owned()),
            term: 7,
            voters: vec!["local".to_owned()],
        }
    }

    async fn committed_epoch(&self, _authority: &str) -> u64 {
        7
    }

    async fn admit_epoch(&self, _authority: &str, presented: u64) -> bool {
        presented == 7
    }

    async fn transfer_home(&self, authority: &str, new_home: &str) -> Result<Option<TransferOutcome>, OwnershipError> {
        Ok(Some(TransferOutcome {
            from: authority.to_owned(),
            to: new_home.to_owned(),
            epoch: 8,
        }))
    }
}

fn member(node: &str, datacenter: &str, address: &str, role: RuntimeMemberRole) -> crate::RuntimeMember {
    crate::RuntimeMember {
        node: node.to_owned(),
        datacenter: datacenter.to_owned(),
        address: address.to_owned(),
        role,
    }
}

fn topology(members: Vec<crate::RuntimeMember>) -> ServiceTopology {
    ServiceTopology {
        membership: Some(crate::RuntimeMembership {
            group: "availability".to_owned(),
            members,
        }),
        node_identity: Some("local".to_owned()),
    }
}

fn storage() -> (tempfile::TempDir, BlobStore, BackendId) {
    let dir = tempfile::tempdir().unwrap();
    let store = BlobStore::new(dir.path().join("blobs"));
    (dir, store, BackendId::new("filesystem").unwrap())
}

fn runtime_config(dir: &tempfile::TempDir, mode: DistributedMode, role: RuntimeRole) -> RuntimeConfig {
    RuntimeConfig {
        mode,
        role,
        membership: Some(RuntimeMembership {
            group: "availability".to_owned(),
            members: vec![member(
                "local",
                "east",
                "http://east.internal:4460",
                RuntimeMemberRole::Writer,
            )],
        }),
        node_identity: Some("local".to_owned()),
        writer_identity: Some("local".to_owned()),
        data_dir: dir.path().to_path_buf(),
        read_through: None,
    }
}

fn service_context(dir: &tempfile::TempDir) -> DistributedServiceContext {
    DistributedServiceContext {
        meta: crate::support::distributed_meta(dir.path().join("peryx.redb")),
        blobs: BlobStorage::filesystem(dir.path().join("blobs")),
        clock: Arc::new(|| 1_800_000_000),
    }
}

#[test]
fn distributed_services_own_topology_durability_and_metrics() {
    let dir = tempfile::tempdir().unwrap();
    let services = <DistributedServiceAssembly as peryx_ha::AvailabilityAssembler>::assemble(
        &DistributedServiceConfig {
            runtime: runtime_config(
                &dir,
                DistributedMode::Dc,
                RuntimeRole::Primary {
                    source: "local".to_owned(),
                    token: "token".to_owned(),
                },
            ),
            read_only: false,
            write_ack_policy: peryx_ha::DurabilityPolicy::Majority,
            write_ack_deadline: Duration::from_secs(5),
        },
        &service_context(&dir),
    )
    .unwrap();

    assert_eq!(services.role, peryx_core::NodeRole::Writer);
    assert_eq!(services.topology.mode, peryx_core::TopologyMode::Dc);
    assert_eq!(services.topology.local_node.as_deref(), Some("local"));
    assert_eq!(services.metrics.len(), 1);
}

#[test]
fn service_installation_applies_the_distributed_contract() {
    let dir = tempfile::tempdir().unwrap();
    let mut state = AppState::new(
        MetaStore::open(dir.path().join("peryx.redb")).unwrap(),
        BlobStorage::filesystem(dir.path().join("blobs")),
        60,
        Vec::new(),
    );

    install_services(
        &DistributedServiceConfig {
            runtime: runtime_config(
                &dir,
                DistributedMode::Dc,
                RuntimeRole::Primary {
                    source: "local".to_owned(),
                    token: "token".to_owned(),
                },
            ),
            read_only: false,
            write_ack_policy: peryx_ha::DurabilityPolicy::Majority,
            write_ack_deadline: Duration::from_secs(5),
        },
        &mut state,
    )
    .unwrap();

    assert_eq!(state.serving.availability_role(), peryx_core::NodeRole::Writer);
    assert_eq!(state.serving.availability_topology().mode, peryx_core::TopologyMode::Dc);
    assert_eq!(state.http_routes().count(), 1);
    assert!(state.serving.authority_drainer().is_some());
}

#[test]
fn service_installation_propagates_remote_transport_errors() {
    let dir = tempfile::tempdir().unwrap();
    let mut runtime = runtime_config(
        &dir,
        DistributedMode::Dc,
        RuntimeRole::Primary {
            source: "local".to_owned(),
            token: "token".to_owned(),
        },
    );
    runtime.read_through = Some(crate::read_through::DEFAULT_READ_THROUGH_LIMITS);
    runtime
        .membership
        .as_mut()
        .unwrap()
        .members
        .push(member("remote", "west", "not a url", RuntimeMemberRole::Writer));
    let mut state = AppState::new(
        MetaStore::open(dir.path().join("peryx.redb")).unwrap(),
        BlobStorage::filesystem(dir.path().join("blobs")),
        60,
        Vec::new(),
    );

    assert!(
        install_services(
            &DistributedServiceConfig {
                runtime,
                read_only: false,
                write_ack_policy: peryx_ha::DurabilityPolicy::Local,
                write_ack_deadline: Duration::ZERO,
            },
            &mut state,
        )
        .is_err()
    );
}

#[tokio::test]
async fn control_assembly_mounts_authenticated_routes() {
    let dir = tempfile::tempdir().unwrap();
    let context = service_context(&dir);
    let router = availability_control_router(
        &runtime_config(
            &dir,
            DistributedMode::Ha,
            RuntimeRole::Replica {
                upstream: "http://east.internal:4460".to_owned(),
                token: "token".to_owned(),
                poll_interval: Duration::from_secs(1),
                page_size: std::num::NonZeroUsize::MIN,
            },
        ),
        AvailabilityControlContext {
            authorizer: Arc::new(DenyControl),
            read_only: true,
            meta: context.meta,
            control: None,
            ownership: None,
        },
    );

    let response = router
        .oneshot(Request::get("/availability/v1/status").body(Body::empty()).unwrap())
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
    assert!(!DenyControl.allows(&ControlActor::new("operator"), ControlPermission::Read));
}

#[tokio::test]
async fn worker_assembly_binds_distributed_jobs() {
    let dir = tempfile::tempdir().unwrap();
    let context = service_context(&dir);
    let mut config = runtime_config(
        &dir,
        DistributedMode::Ha,
        RuntimeRole::Primary {
            source: "local".to_owned(),
            token: "token".to_owned(),
        },
    );
    config.membership.as_mut().unwrap().members.push(member(
        "remote",
        "west",
        "http://west.internal:4460",
        RuntimeMemberRole::Replica,
    ));
    let authority = Arc::new(StaticAuthority);
    assert!(authority.has_home("resource").await);
    assert_eq!(authority.claim_home("resource").await.unwrap(), HomeClaim::AssignedHere);
    assert_eq!(authority.cluster_status().term, 7);
    assert_eq!(authority.committed_epoch("resource").await, 7);
    assert!(authority.admit_epoch("resource", 7).await);
    assert_eq!(authority.transfer_home("east", "west").await.unwrap().unwrap().epoch, 8);
    let workers = assemble_workers(
        &config,
        DistributedWorkerContext {
            filesystem: context.blobs.filesystem_store().cloned(),
            backend: context.blobs.backend_id(),
            meta: context.meta.clone(),
            blobs: context.blobs,
            clock: context.clock,
            authority: Some(authority),
            references: Arc::new(EmptyReferences),
            frontiers: Arc::new(EmptyFrontiers),
        },
    )
    .unwrap();

    assert_eq!(
        workers
            .copier
            .unwrap()
            .copy_pass(&|| false, std::num::NonZeroUsize::MIN)
            .await
            .unwrap(),
        peryx_ha::AvailabilityTaskReport::default()
    );
    assert_eq!(
        workers
            .placement
            .unwrap()
            .reconcile_pass(&|| false, std::num::NonZeroUsize::MIN)
            .await
            .unwrap(),
        peryx_ha::AvailabilityTaskReport::default()
    );
    assert_eq!(
        workers
            .reclaimer
            .unwrap()
            .reclaim_pass(&|| false, 0, std::num::NonZeroUsize::MIN)
            .await
            .unwrap(),
        peryx_ha::AvailabilityTaskReport::default()
    );
}

#[tokio::test]
async fn deferred_worker_inputs_fail_closed_until_bound_and_active() {
    let active = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let ownership = DeferredOwnership::new(active.clone());
    ownership.bind(Some(Arc::new(StaticAuthority)));

    assert!(!ownership.has_home("resource").await);
    assert!(matches!(
        ownership.claim_home("resource").await,
        Err(OwnershipError::Unavailable(_))
    ));
    assert_eq!(
        ownership.cluster_status(),
        ClusterStatus {
            leader: None,
            term: 0,
            voters: Vec::new(),
        }
    );
    assert_eq!(ownership.committed_epoch("resource").await, 0);
    assert!(!ownership.admit_epoch("resource", 7).await);
    assert!(matches!(
        ownership.transfer_home("resource", "west").await,
        Err(OwnershipError::Unavailable(_))
    ));

    active.store(true, std::sync::atomic::Ordering::Release);
    assert!(ownership.has_home("resource").await);
    assert_eq!(ownership.claim_home("resource").await.unwrap(), HomeClaim::AssignedHere);
    assert_eq!(ownership.cluster_status().term, 7);
    assert_eq!(ownership.committed_epoch("resource").await, 7);
    assert!(ownership.admit_epoch("resource", 7).await);
    assert_eq!(ownership.transfer_home("east", "west").await.unwrap().unwrap().epoch, 8);

    let references = DeferredReferences::default();
    assert_eq!(
        references.referenced().unwrap_err(),
        "reference inventory is not active"
    );
    references.bind(Arc::new(EmptyReferences));
    assert_eq!(references.referenced().unwrap(), BTreeSet::new());

    let frontiers = DeferredFrontiers::default();
    assert_eq!(frontiers.observe(), None);
    frontiers.bind(Arc::new(EmptyFrontiers));
    assert_eq!(
        frontiers.observe(),
        Some(ObservedFrontier {
            replica: None,
            backup: None,
        })
    );
}

#[test]
fn worker_assembly_skips_replica_and_non_filesystem_workers() {
    let dir = tempfile::tempdir().unwrap();
    let context = service_context(&dir);
    let replica = runtime_config(
        &dir,
        DistributedMode::Dc,
        RuntimeRole::Replica {
            upstream: "http://east.internal:4460".to_owned(),
            token: "token".to_owned(),
            poll_interval: Duration::from_secs(1),
            page_size: std::num::NonZeroUsize::MIN,
        },
    );
    let workers = assemble_workers(
        &replica,
        DistributedWorkerContext {
            filesystem: None,
            backend: context.blobs.backend_id(),
            meta: context.meta.clone(),
            blobs: context.blobs.clone(),
            clock: context.clock.clone(),
            authority: None,
            references: Arc::new(EmptyReferences),
            frontiers: Arc::new(EmptyFrontiers),
        },
    )
    .unwrap();
    assert!(workers.copier.is_none());
    assert!(workers.placement.is_none());
    assert!(workers.reclaimer.is_none());

    let primary = runtime_config(
        &dir,
        DistributedMode::Dc,
        RuntimeRole::Primary {
            source: "local".to_owned(),
            token: "token".to_owned(),
        },
    );
    let workers = assemble_workers(
        &primary,
        DistributedWorkerContext {
            filesystem: None,
            backend: context.blobs.backend_id(),
            meta: context.meta,
            blobs: context.blobs,
            clock: context.clock,
            authority: None,
            references: Arc::new(EmptyReferences),
            frontiers: Arc::new(EmptyFrontiers),
        },
    )
    .unwrap();
    assert!(workers.copier.is_none());
    assert!(workers.placement.is_none());
    assert!(workers.reclaimer.is_some());
}

#[test]
fn transport_sources_follow_locality_and_validate_addresses() {
    let dir = tempfile::tempdir().unwrap();
    let mut config = runtime_config(
        &dir,
        DistributedMode::Ha,
        RuntimeRole::Primary {
            source: "local".to_owned(),
            token: "token".to_owned(),
        },
    );
    config.membership.as_mut().unwrap().members.extend([
        member(
            "local-peer",
            "east",
            "http://east-peer.internal:4460",
            RuntimeMemberRole::Replica,
        ),
        member("remote", "west", "west.internal:4460", RuntimeMemberRole::Replica),
    ]);

    assert_eq!(receipt_sources(&config).unwrap().len(), 1);
    assert_eq!(remote_frontier_sources(&config).unwrap().len(), 1);
    assert_eq!(member_base_url("https://peer.internal"), "https://peer.internal");
    config.membership.as_mut().unwrap().members[1].address = "not a url".to_owned();
    assert!(receipt_sources(&config).is_err());
    config.membership.as_mut().unwrap().members[1].address = "http://east-peer.internal:4460".to_owned();
    config.membership.as_mut().unwrap().members[2].address = "://invalid".to_owned();
    assert!(remote_frontier_sources(&config).is_err());

    config.mode = DistributedMode::Dc;
    assert!(remote_frontier_sources(&config).unwrap().is_empty());
    config.mode = DistributedMode::Ha;
    config.node_identity = Some("unknown".to_owned());
    assert!(receipt_sources(&config).unwrap().is_empty());
    assert!(remote_frontier_sources(&config).unwrap().is_empty());
    config.membership = None;
    assert!(receipt_sources(&config).unwrap().is_empty());
    assert!(remote_frontier_sources(&config).unwrap().is_empty());
}

#[test]
fn topology_maps_ha_replica_members_and_read_only_state() {
    let dir = tempfile::tempdir().unwrap();
    let mut config = runtime_config(
        &dir,
        DistributedMode::Ha,
        RuntimeRole::Replica {
            upstream: "http://east.internal:4460".to_owned(),
            token: "token".to_owned(),
            poll_interval: Duration::from_secs(1),
            page_size: std::num::NonZeroUsize::MIN,
        },
    );
    config.membership.as_mut().unwrap().members[0].role = RuntimeMemberRole::Replica;

    let topology = super::topology(&config, true);

    assert_eq!(runtime_role(&config.role), peryx_core::NodeRole::Replica);
    assert_eq!(topology.mode, peryx_core::TopologyMode::Ha);
    assert_eq!(topology.members[0].role, peryx_core::NodeRole::Replica);
    assert_eq!(topology.local_node, None);
}

#[test]
fn assembly_skips_topologies_without_a_local_member() {
    let (_dir, store, backend) = storage();
    let missing_identity = ServiceTopology {
        membership: None,
        node_identity: None,
    };
    let missing_membership = ServiceTopology {
        membership: None,
        node_identity: Some("local".to_owned()),
    };
    let unknown_identity = topology(vec![member(
        "remote",
        "west",
        "http://west.internal:4460",
        RuntimeMemberRole::Replica,
    )]);

    for unresolved in [&missing_identity, &missing_membership, &unknown_identity] {
        assert!(
            cross_dc_blob_copier(unresolved, "token".to_owned(), store.clone(), backend.clone())
                .unwrap()
                .is_none()
        );
        assert!(
            filesystem_placement_reconciler(unresolved, store.clone())
                .unwrap()
                .is_none()
        );
    }
}

#[test]
fn assembly_rejects_invalid_datacenters() {
    let (_dir, store, backend) = storage();
    let invalid_local = topology(vec![member(
        "local",
        "",
        "http://local.internal:4460",
        RuntimeMemberRole::Writer,
    )]);
    assert!(cross_dc_blob_copier(&invalid_local, "token".to_owned(), store.clone(), backend).is_err());
    assert!(filesystem_placement_reconciler(&invalid_local, store.clone()).is_err());

    let invalid_remote = topology(vec![
        member("local", "east", "http://east.internal:4460", RuntimeMemberRole::Writer),
        member("remote", "", "http://west.internal:4460", RuntimeMemberRole::Replica),
    ]);
    assert!(filesystem_placement_reconciler(&invalid_remote, store).is_err());
}

#[test]
fn assembly_builds_multi_datacenter_services() {
    let (_dir, store, backend) = storage();
    let topology = topology(vec![
        member("local", "east", "http://east.internal:4460", RuntimeMemberRole::Writer),
        member(
            "west-replica-a",
            "west",
            "http://west-a.internal:4460",
            RuntimeMemberRole::Replica,
        ),
        member(
            "west-writer",
            "west",
            "http://west-writer.internal:4460",
            RuntimeMemberRole::Writer,
        ),
        member(
            "west-replica-b",
            "west",
            "http://west-b.internal:4460",
            RuntimeMemberRole::Replica,
        ),
        member(
            "north-replica",
            "north",
            "north.internal:4460",
            RuntimeMemberRole::Replica,
        ),
    ]);

    assert!(
        cross_dc_blob_copier(&topology, "token".to_owned(), store.clone(), backend)
            .unwrap()
            .is_some()
    );
    assert!(filesystem_placement_reconciler(&topology, store).unwrap().is_some());
}

#[test]
fn assembly_skips_single_datacenter_workers() {
    let (_dir, store, backend) = storage();
    let topology = topology(vec![member(
        "local",
        "east",
        "http://east.internal:4460",
        RuntimeMemberRole::Writer,
    )]);

    assert!(
        cross_dc_blob_copier(&topology, "token".to_owned(), store.clone(), backend)
            .unwrap()
            .is_none()
    );
    assert!(filesystem_placement_reconciler(&topology, store).unwrap().is_none());
}

#[test]
fn reclamation_selector_requires_the_writer_in_membership() {
    let references = Arc::new(EmptyReferences);
    let frontiers = Arc::new(EmptyFrontiers);
    assert_eq!(references.referenced().unwrap(), BTreeSet::new());
    assert_eq!(
        frontiers.observe(),
        Some(ObservedFrontier {
            replica: None,
            backup: None,
        })
    );

    let missing_identity = ServiceTopology {
        membership: None,
        node_identity: None,
    };
    let missing_membership = ServiceTopology {
        membership: None,
        node_identity: None,
    };
    let unknown_writer = topology(vec![member(
        "remote",
        "west",
        "http://west.internal:4460",
        RuntimeMemberRole::Replica,
    )]);
    for unresolved in [&missing_identity, &missing_membership, &unknown_writer] {
        assert!(blob_reclamation_selector(unresolved, references.clone(), frontiers.clone()).is_none());
    }

    assert!(
        blob_reclamation_selector(
            &topology(vec![member(
                "local",
                "east",
                "http://east.internal:4460",
                RuntimeMemberRole::Writer,
            )]),
            references,
            frontiers,
        )
        .is_some()
    );
}

#[test]
fn local_member_requires_an_explicit_identity() {
    let mut topology = topology(vec![member(
        "local",
        "east",
        "http://east.internal:4460",
        RuntimeMemberRole::Writer,
    )]);
    topology.node_identity = None;

    assert!(local_member(&topology).is_none());
}

enum ListenerBehavior {
    Complete,
    Fail,
    Panic,
    PanicString,
    PanicOther,
    SetupPanic,
    SetupWait {
        entered: tokio::sync::oneshot::Sender<()>,
        release: std::sync::mpsc::Receiver<()>,
        exited: tokio::sync::oneshot::Sender<()>,
    },
    Wait {
        started: tokio::sync::oneshot::Sender<()>,
    },
}

struct SignalListener(ListenerBehavior);

struct ExitFuture(Option<tokio::sync::oneshot::Sender<()>>);

impl std::future::Future for ExitFuture {
    type Output = Result<(), AvailabilityListenerError>;

    fn poll(
        mut self: std::pin::Pin<&mut Self>,
        _context: &mut std::task::Context<'_>,
    ) -> std::task::Poll<Self::Output> {
        let _ = self.0.take().unwrap().send(());
        std::task::Poll::Ready(Ok(()))
    }
}

impl Drop for ExitFuture {
    fn drop(&mut self) {
        if let Some(exited) = self.0.take() {
            let _ = exited.send(());
        }
    }
}

impl PreparedAvailabilityListener for SignalListener {
    fn address(&self) -> std::net::SocketAddr {
        "127.0.0.1:0".parse().unwrap()
    }

    fn serve(
        self: Box<Self>,
        _router: axum::Router,
        shutdown: tokio_util::sync::CancellationToken,
    ) -> Result<AvailabilityListenerFuture, AvailabilityListenerError> {
        Ok(match self.0 {
            ListenerBehavior::Complete => Box::pin(async { Ok(()) }),
            ListenerBehavior::Fail => {
                Box::pin(async { Err(AvailabilityListenerError::Serve("listener failure".to_owned())) })
            }
            ListenerBehavior::Panic => Box::pin(async { panic!("listener panic") }),
            ListenerBehavior::PanicString => {
                Box::pin(async { std::panic::panic_any("listener string panic".to_owned()) })
            }
            ListenerBehavior::PanicOther => Box::pin(async { std::panic::panic_any(7_u8) }),
            ListenerBehavior::SetupPanic => panic!("listener setup panic"),
            ListenerBehavior::SetupWait {
                entered,
                release,
                exited,
            } => {
                entered.send(()).unwrap();
                release.recv().unwrap();
                Box::pin(ExitFuture(Some(exited)))
            }
            ListenerBehavior::Wait { started } => Box::pin(async move {
                started.send(()).unwrap();
                shutdown.cancelled().await;
                Ok(())
            }),
        })
    }
}

#[test]
fn signal_listener_reports_its_bound_address() {
    assert_eq!(
        PreparedAvailabilityListener::address(&SignalListener(ListenerBehavior::Complete)),
        "127.0.0.1:0".parse().unwrap()
    );
}

#[test]
fn home_placement_recording_requires_active_availability() {
    let directory = tempfile::tempdir().unwrap();
    let meta = MetaStore::open(directory.path().join("meta.redb")).unwrap();
    let active = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
    let recorder = GatedHomePlacementRecorder {
        recorder: std::sync::Arc::new(crate::placement_policy::DistributedHomePlacementRecorder::new(
            meta.clone(),
            peryx_ha::BackendId::new("filesystem").unwrap(),
            peryx_ha::DataCenterId::new("home").unwrap(),
            std::sync::Arc::new(|| 42),
        )),
        active: active.clone(),
    };
    let digest = "7".repeat(64);

    assert_eq!(
        peryx_ha::HomePlacementRecorder::record(&recorder, &digest, 12, 3).unwrap_err(),
        "distributed availability is not active"
    );
    active.store(true, std::sync::atomic::Ordering::Release);
    peryx_ha::HomePlacementRecorder::record(&recorder, &digest, 12, 3).unwrap();

    assert_eq!(
        meta.blob_placements(&peryx_identity::ArtifactDigest::from_sha256(digest).unwrap())
            .unwrap()[0]
            .state,
        peryx_ha::BlobPlacementState::Verified { size: 12 }
    );
}

#[tokio::test]
async fn listener_waits_for_the_shared_activation_gate() {
    let (lifecycle, _) = Lifecycle::new();
    let (started, mut running) = tokio::sync::oneshot::channel();
    let mut listener = RunningListener::start(
        Box::new(SignalListener(ListenerBehavior::Wait { started })),
        axum::Router::new(),
        lifecycle.clone(),
    )
    .await
    .unwrap();
    assert!(matches!(
        running.try_recv(),
        Err(tokio::sync::oneshot::error::TryRecvError::Empty)
    ));

    lifecycle.activate();
    running.await.unwrap();
    listener.cancel();
    listener.join().unwrap();
}

#[tokio::test]
async fn listener_cancellation_before_activation_stops_without_serving() {
    let (lifecycle, _) = Lifecycle::new();
    let mut listener = RunningListener::start(
        Box::new(SignalListener(ListenerBehavior::Complete)),
        axum::Router::new(),
        lifecycle,
    )
    .await
    .unwrap();

    listener.cancel();
    listener.join().unwrap();
    listener.join().unwrap();
}

#[tokio::test]
async fn dropping_listener_startup_cancels_its_thread() {
    let (lifecycle, _) = Lifecycle::new();
    let cancellation = lifecycle.cancellation();
    let (entered, entry) = tokio::sync::oneshot::channel();
    let (release, released) = std::sync::mpsc::channel();
    let (exited, exit) = tokio::sync::oneshot::channel();
    let startup = tokio::spawn(RunningListener::start(
        Box::new(SignalListener(ListenerBehavior::SetupWait {
            entered,
            release: released,
            exited,
        })),
        axum::Router::new(),
        lifecycle,
    ));
    entry.await.unwrap();
    startup.abort();
    assert!(matches!(startup.await, Err(error) if error.is_cancelled()));
    release.send(()).unwrap();

    cancellation.cancelled().await;
    exit.await.unwrap();
}

#[tokio::test]
async fn listener_exit_future_signals_when_polled() {
    let (exited, exit) = tokio::sync::oneshot::channel();

    ExitFuture(Some(exited)).await.unwrap();

    exit.await.unwrap();
}

enum ListenerFailureKind {
    Stopped,
    Task,
    Serve,
}

#[rstest::rstest]
#[case::complete(
    ListenerBehavior::Complete,
    "availability listener stopped unexpectedly",
    ListenerFailureKind::Stopped
)]
#[case::panic(
    ListenerBehavior::Panic,
    "availability listener task failed: listener panic",
    ListenerFailureKind::Task
)]
#[case::error(
    ListenerBehavior::Fail,
    "availability listener failed: listener failure",
    ListenerFailureKind::Serve
)]
#[tokio::test]
async fn listener_exit_fails_shared_supervision(
    #[case] behavior: ListenerBehavior,
    #[case] expected: &str,
    #[case] kind: ListenerFailureKind,
) {
    let (lifecycle, mut failures) = Lifecycle::new();
    let mut listener = RunningListener::start(
        Box::new(SignalListener(behavior)),
        axum::Router::new(),
        lifecycle.clone(),
    )
    .await
    .unwrap();
    lifecycle.activate();

    assert_eq!(failures.wait().await, expected);
    let error = listener.join().unwrap_err();
    assert!(matches!(
        (kind, error),
        (ListenerFailureKind::Stopped, AvailabilityListenerError::Stopped)
            | (ListenerFailureKind::Task, AvailabilityListenerError::Task(_))
            | (ListenerFailureKind::Serve, AvailabilityListenerError::Serve(_))
    ));
}

#[tokio::test]
async fn listener_panic_payloads_are_reported() {
    for (behavior, expected) in [
        (ListenerBehavior::PanicString, "listener string panic"),
        (ListenerBehavior::PanicOther, "panic"),
    ] {
        let (lifecycle, mut failures) = Lifecycle::new();
        let mut listener = RunningListener::start(
            Box::new(SignalListener(behavior)),
            axum::Router::new(),
            lifecycle.clone(),
        )
        .await
        .unwrap();
        lifecycle.activate();

        assert_eq!(
            failures.wait().await,
            format!("availability listener task failed: {expected}")
        );
        assert!(matches!(listener.join(), Err(AvailabilityListenerError::Task(_))));
    }
}

#[tokio::test]
async fn listener_setup_panic_is_reported() {
    let (lifecycle, _) = Lifecycle::new();
    let error = RunningListener::start(
        Box::new(SignalListener(ListenerBehavior::SetupPanic)),
        axum::Router::new(),
        lifecycle,
    )
    .await
    .err()
    .unwrap();

    assert!(matches!(error, AvailabilityListenerError::Task(_)));
}

#[test]
fn listener_io_errors_are_setup_errors() {
    assert!(matches!(
        AvailabilityListenerError::from(std::io::Error::other("runtime failure")),
        AvailabilityListenerError::Setup(message) if message == "runtime failure"
    ));
    assert!(matches!(
        listener_thread_error(std::io::Error::other("thread failure")),
        AvailabilityListenerError::Task(message) if message == "thread failure"
    ));
}

#[test]
fn shutdown_failure_aggregation_keeps_each_stage() {
    let mut failure = None;
    record_shutdown_failure(
        &mut failure,
        AvailabilityShutdownStage::Listener,
        std::io::Error::other("listener"),
    );
    record_shutdown_failure(
        &mut failure,
        AvailabilityShutdownStage::Runtime,
        std::io::Error::other("runtime"),
    );

    assert_eq!(
        failure
            .unwrap()
            .failures()
            .iter()
            .map(|failure| failure.stage)
            .collect::<Vec<_>>(),
        [AvailabilityShutdownStage::Listener, AvailabilityShutdownStage::Runtime]
    );
}

#[tokio::test]
async fn startup_failure_preserves_cleanup_failure() {
    let (lifecycle, _) = Lifecycle::new();
    lifecycle.activate();
    let listener = RunningListener::start(
        Box::new(SignalListener(ListenerBehavior::Fail)),
        axum::Router::new(),
        lifecycle.clone(),
    )
    .await
    .unwrap();
    let active = ActiveDistributed {
        lifecycle,
        listener: OwnedResource::Owned(listener),
        consensus: OwnedResource::Absent,
        runtime: OwnedResource::Absent,
        bindings: RuntimeBindings::new(false),
    };

    let error = fail_startup::<()>(anyhow::anyhow!("startup failure"), active)
        .await
        .unwrap_err()
        .downcast::<StartupCleanupError>()
        .unwrap();

    assert_eq!(error.startup.to_string(), "startup failure");
    assert_eq!(error.cleanup.failures()[0].stage, AvailabilityShutdownStage::Listener);
}

#[tokio::test]
async fn bounded_shutdown_reports_a_stalled_owner() {
    let (release, released) = std::sync::mpsc::channel();
    let mut owner = OwnedResource::Owned(move || -> Result<(), std::io::Error> {
        released.recv().unwrap();
        Ok(())
    });
    assert!(owner.wait_shutdown(Duration::ZERO).await.is_none());
    let first_failure = owner
        .shutdown(AvailabilityShutdownStage::Listener, Duration::ZERO, |shutdown| {
            shutdown()
        })
        .await
        .unwrap();
    let second_failure = owner.wait_shutdown(Duration::ZERO).await.unwrap();
    let mut first_completion = first_failure.completion.unwrap();
    let mut second_completion = second_failure.completion.unwrap();
    release.send(()).unwrap();

    assert_eq!(first_failure.stage, AvailabilityShutdownStage::Listener);
    assert_eq!(first_failure.error.to_string(), "shutdown deadline exceeded");
    assert_eq!(second_failure.stage, AvailabilityShutdownStage::Listener);
    assert_eq!(second_failure.error.to_string(), "shutdown deadline exceeded");
    assert!(owner.wait_shutdown(DEADLOCK_GUARD).await.is_none());
    first_completion.wait_for(|completed| *completed).await.unwrap();
    second_completion.wait_for(|completed| *completed).await.unwrap();
    assert!(owner.wait_shutdown(DEADLOCK_GUARD).await.is_none());
}

#[tokio::test]
async fn active_shutdown_cancels_its_runtime() {
    let (lifecycle, _) = Lifecycle::new();
    let mut active = ActiveDistributed {
        lifecycle,
        listener: OwnedResource::Absent,
        consensus: OwnedResource::Absent,
        runtime: OwnedResource::Owned(
            crate::runtime_worker::AvailabilityRuntime::start(Arc::new(
                crate::runtime_worker::WorkerShared::for_replica(),
            ))
            .unwrap(),
        ),
        bindings: RuntimeBindings::new(false),
    };

    active.shutdown_owned().await.unwrap();
}

#[tokio::test]
async fn stalled_shutdown_moves_to_the_process_reaper() {
    let (release, released) = std::sync::mpsc::channel();
    let mut owner = OwnedResource::Owned(move || -> Result<(), std::io::Error> {
        released.recv().unwrap();
        Ok(())
    });

    let failure = owner
        .shutdown(AvailabilityShutdownStage::Runtime, Duration::ZERO, |shutdown| {
            shutdown()
        })
        .await
        .unwrap();
    let mut completion = failure.completion.unwrap();
    drop(owner);
    release.send(()).unwrap();

    assert_eq!(failure.stage, AvailabilityShutdownStage::Runtime);
    assert_eq!(failure.error.to_string(), "shutdown deadline exceeded");
    completion.wait_for(|completed| *completed).await.unwrap();
}

#[tokio::test]
async fn bounded_shutdown_reports_an_owner_panic() {
    let mut owner =
        OwnedResource::Owned(|| -> Result<(), std::io::Error> { std::panic::panic_any("shutdown panic".to_owned()) });
    let failure = owner
        .shutdown(AvailabilityShutdownStage::Runtime, DEADLOCK_GUARD, |shutdown| {
            shutdown()
        })
        .await
        .unwrap();

    assert_eq!(failure.stage, AvailabilityShutdownStage::Runtime);
    assert_eq!(failure.error.to_string(), "shutdown panicked: shutdown panic");
    assert!(failure.completion.is_none());
}

#[tokio::test]
async fn process_reaper_survives_panics_and_errors() {
    let panic = reap_process_resource("panic", || -> Result<(), std::io::Error> { panic!("reaper panic") });
    let error = reap_process_resource("error", || Err(std::io::Error::other("reaper error")));
    let (completed, completion) = tokio::sync::oneshot::channel();
    let signal = reap_process_resource("signal", move || {
        completed.send(()).unwrap();
        Ok::<_, std::io::Error>(())
    });

    completion.await.unwrap();
    panic.join().unwrap();
    error.join().unwrap();
    signal.join().unwrap();
}
