use std::collections::BTreeSet;
use std::num::NonZeroUsize;
use std::sync::Arc;
use std::time::Duration;

use axum::body::Body;
use axum::http::{Request, StatusCode, header};
use axum::{Router, routing::get as route_get};
use http_body_util::BodyExt as _;
use peryx_driver::state::AppState;
use peryx_ha::AvailabilityRuntime as _;
use peryx_ha::{ControlAuthorizer as _, ReferenceInventory as _};
use peryx_storage::blob::BlobStore;
use peryx_storage::meta::{MetaStore, ObservedFrontier};
use serde_json::Value;
use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};
use tower::ServiceExt as _;

use super::*;
use crate::read_through::DEFAULT_READ_THROUGH_LIMITS;
use crate::support::TestServer;
use crate::{
    ChangePage, DcDurabilityMetrics, DistributedAnalyticsCompleteness, DistributedBlobDurability,
    DistributedPrepareContext, DistributedServiceConfig, PROTOCOL_VERSION, install_services,
};

const TOKEN: &str = "replication-secret";

struct StaticAuthorizer(AvailabilityAudience);

struct UnavailableAuthorizer;

struct StaticFrontier(Option<crate::FrontierReply>);

struct EmptyReferences;

struct DenyControl;

struct TestListener(std::net::TcpListener);

struct FailingListener;

struct PanickingAddressListener;

struct ConsensusOrderListener {
    listener: std::net::TcpListener,
    log_path: std::path::PathBuf,
    stopped: std::sync::mpsc::Sender<bool>,
}

struct ConsensusOrderSignal {
    listener: Option<tokio::net::TcpListener>,
    log_path: std::path::PathBuf,
    stopped: Option<std::sync::mpsc::Sender<bool>>,
}

impl Drop for ConsensusOrderSignal {
    fn drop(&mut self) {
        drop(self.listener.take());
        let consensus_running = crate::raft::persistence::RaftLogStore::open(&self.log_path).is_err();
        let _ = self.stopped.take().unwrap().send(consensus_running);
    }
}

impl crate::PreparedAvailabilityListener for ConsensusOrderListener {
    fn address(&self) -> std::net::SocketAddr {
        self.listener.local_addr().unwrap()
    }

    fn serve(
        self: Box<Self>,
        _router: Router,
        shutdown: tokio_util::sync::CancellationToken,
    ) -> Result<crate::AvailabilityListenerFuture, crate::AvailabilityListenerError> {
        self.listener.set_nonblocking(true).unwrap();
        let listener = tokio::net::TcpListener::from_std(self.listener).unwrap();
        let stopped = ConsensusOrderSignal {
            listener: Some(listener),
            log_path: self.log_path,
            stopped: Some(self.stopped),
        };
        Ok(Box::pin(async move {
            let _stopped = stopped;
            shutdown.cancelled_owned().await;
            Ok(())
        }))
    }
}

impl crate::PreparedAvailabilityListener for FailingListener {
    fn address(&self) -> std::net::SocketAddr {
        "127.0.0.1:0".parse().unwrap()
    }

    fn serve(
        self: Box<Self>,
        _router: Router,
        _shutdown: tokio_util::sync::CancellationToken,
    ) -> Result<crate::AvailabilityListenerFuture, crate::AvailabilityListenerError> {
        Err(crate::AvailabilityListenerError::setup(std::io::Error::other(
            "injected failure",
        )))
    }
}

impl crate::PreparedAvailabilityListener for PanickingAddressListener {
    fn address(&self) -> std::net::SocketAddr {
        panic!("injected address panic")
    }

    fn serve(
        self: Box<Self>,
        _router: Router,
        _shutdown: tokio_util::sync::CancellationToken,
    ) -> Result<crate::AvailabilityListenerFuture, crate::AvailabilityListenerError> {
        Ok(Box::pin(async { Ok(()) }))
    }
}

struct DropSignal {
    listener: Option<tokio::net::TcpListener>,
    stopped: Option<tokio::sync::oneshot::Sender<()>>,
}

impl Drop for DropSignal {
    fn drop(&mut self) {
        drop(self.listener.take());
        if let Some(stopped) = self.stopped.take() {
            let _ = stopped.send(());
        }
    }
}

struct ControlledListener {
    listener: std::net::TcpListener,
    trigger: tokio::sync::oneshot::Receiver<()>,
    entered: Option<tokio::sync::oneshot::Sender<()>>,
    stopped: tokio::sync::oneshot::Sender<()>,
    panic: bool,
}

impl crate::PreparedAvailabilityListener for ControlledListener {
    fn address(&self) -> std::net::SocketAddr {
        self.listener.local_addr().unwrap()
    }

    fn serve(
        self: Box<Self>,
        _router: Router,
        shutdown: tokio_util::sync::CancellationToken,
    ) -> Result<crate::AvailabilityListenerFuture, crate::AvailabilityListenerError> {
        let Self {
            listener,
            trigger,
            entered,
            stopped,
            panic,
        } = *self;
        listener.set_nonblocking(true).unwrap();
        let listener = tokio::net::TcpListener::from_std(listener).unwrap();
        let stopped = DropSignal {
            listener: Some(listener),
            stopped: Some(stopped),
        };
        Ok(Box::pin(async move {
            let _stopped = stopped;
            if let Some(entered) = entered {
                let _ = entered.send(());
            }
            tokio::select! {
                result = trigger => {
                    let _ = result;
                    assert!(!panic, "injected listener panic");
                }
                () = shutdown.cancelled_owned() => {}
            }
            Ok(())
        }))
    }
}

impl crate::PreparedAvailabilityListener for TestListener {
    fn address(&self) -> std::net::SocketAddr {
        self.0.local_addr().unwrap()
    }

    fn serve(
        self: Box<Self>,
        router: Router,
        shutdown: tokio_util::sync::CancellationToken,
    ) -> Result<crate::AvailabilityListenerFuture, crate::AvailabilityListenerError> {
        self.0.set_nonblocking(true).unwrap();
        let listener = tokio::net::TcpListener::from_std(self.0).unwrap();
        Ok(Box::pin(async move {
            axum::serve(
                listener,
                router.into_make_service_with_connect_info::<std::net::SocketAddr>(),
            )
            .with_graceful_shutdown(shutdown.cancelled_owned())
            .await
            .unwrap();
            Ok(())
        }))
    }
}

fn listener() -> Box<dyn crate::PreparedAvailabilityListener> {
    Box::new(TestListener(std::net::TcpListener::bind("127.0.0.1:0").unwrap()))
}

fn ha_config(dir: &tempfile::TempDir) -> RuntimeConfig {
    let mut config = primary_config(dir, DistributedMode::Ha);
    config.node_identity = Some("writer".to_owned());
    config.writer_identity = Some("writer".to_owned());
    config.membership = Some(membership(vec![member(
        "writer",
        "east",
        "http://127.0.0.1:0",
        RuntimeMemberRole::Writer,
    )]));
    config
}

fn install_runtime_services(state: &mut Arc<AppState>, config: &RuntimeConfig) {
    install_services(
        &DistributedServiceConfig {
            runtime: config.clone(),
            read_only: false,
            write_ack_deadline: Duration::ZERO,
        },
        Arc::get_mut(state).unwrap(),
    )
    .unwrap();
}

async fn prepare_with_listener(
    config: RuntimeConfig,
    state: Arc<AppState>,
    listener: Box<dyn crate::PreparedAvailabilityListener>,
) -> anyhow::Result<peryx_ha::PreparedAvailability<Router, crate::DistributedHandle>> {
    runtime(&config, &state)
        .unwrap()
        .prepare(DistributedPrepareContext {
            config,
            state,
            control_authorizer: Arc::new(DenyControl),
            references: Arc::new(EmptyReferences),
            listener: Some(listener),
        })
        .await
}

async fn assert_ownership_unavailable(state: &AppState) {
    let authority = state.serving.ownership_authority().unwrap();
    assert!(matches!(
        authority.claim_home("resource").await,
        Err(peryx_ha::OwnershipError::Unavailable(reason)) if reason == "ownership is not active"
    ));
}

async fn assert_distributed_work_unavailable(state: &AppState) {
    let copier = state.serving.cross_dc_copier().unwrap();
    let placement = state.serving.placement_reconciler().unwrap();
    let reclaimer = state.serving.blob_reclaimer().unwrap();
    for error in [
        copier.copy_pass(&|| false, NonZeroUsize::MIN).await.unwrap_err(),
        placement
            .reconcile_pass(&|| false, NonZeroUsize::MIN)
            .await
            .unwrap_err(),
        reclaimer
            .reclaim_pass(&|| false, 1, NonZeroUsize::MIN)
            .await
            .unwrap_err(),
    ] {
        assert_eq!(
            (error.code(), error.message()),
            ("availability_inactive", "distributed availability is not active")
        );
    }
}

impl peryx_ha::ReferenceInventory for EmptyReferences {
    fn referenced(&self) -> Result<BTreeSet<String>, String> {
        Ok(BTreeSet::new())
    }
}

#[async_trait::async_trait]
impl peryx_ha::ControlAuthorizer for DenyControl {
    async fn authenticate(
        &self,
        _authorization: Option<&str>,
    ) -> Result<Option<peryx_ha::ControlActor>, peryx_ha::ControlAuthenticationError> {
        Ok(None)
    }

    fn allows(&self, _actor: &peryx_ha::ControlActor, _permission: peryx_ha::ControlPermission) -> bool {
        false
    }
}

#[async_trait::async_trait]
impl crate::MetadataFrontierProvider for StaticFrontier {
    async fn frontier(&self, _authority: &str) -> Option<crate::FrontierReply> {
        self.0.as_ref().map(|frontier| crate::FrontierReply {
            epoch: frontier.epoch,
            applied_frontier: frontier.applied_frontier,
        })
    }
}

#[async_trait::async_trait]
impl AvailabilityAuthorizer for StaticAuthorizer {
    async fn authorize(
        &self,
        _authorization: Option<&str>,
    ) -> Result<AvailabilityAudience, peryx_ha::AvailabilityAuthenticationError> {
        Ok(self.0)
    }
}

#[async_trait::async_trait]
impl AvailabilityAuthorizer for UnavailableAuthorizer {
    async fn authorize(
        &self,
        _authorization: Option<&str>,
    ) -> Result<AvailabilityAudience, peryx_ha::AvailabilityAuthenticationError> {
        Err(peryx_ha::AvailabilityAuthenticationError)
    }
}

fn runtime(config: &RuntimeConfig, state: &Arc<AppState>) -> anyhow::Result<DistributedRuntime> {
    runtime_with_audience(config, state, AvailabilityAudience::Public)
}

fn runtime_with_audience(
    config: &RuntimeConfig,
    state: &Arc<AppState>,
    audience: AvailabilityAudience,
) -> anyhow::Result<DistributedRuntime> {
    runtime_with_frontier(config, state, audience, Arc::new(StaticFrontier(None)))
}

fn runtime_with_frontier(
    config: &RuntimeConfig,
    state: &Arc<AppState>,
    audience: AvailabilityAudience,
    frontier: Arc<dyn crate::MetadataFrontierProvider>,
) -> anyhow::Result<DistributedRuntime> {
    runtime_with_authorizer(config, state, frontier, Arc::new(StaticAuthorizer(audience)))
}

fn runtime_with_authorizer(
    config: &RuntimeConfig,
    state: &Arc<AppState>,
    frontier: Arc<dyn crate::MetadataFrontierProvider>,
    authorizer: Arc<dyn AvailabilityAuthorizer>,
) -> anyhow::Result<DistributedRuntime> {
    DistributedRuntime::new(
        config,
        &DistributedRuntimeContext {
            meta: state.serving.meta.clone(),
            blobs: state.serving.blobs.clone(),
            clock: state.serving.clock.clone(),
            replica_views: state.clone(),
            analytics: Arc::new(state.serving.metrics.clone()),
            frontier,
        },
        authorizer,
    )
}

fn state() -> (tempfile::TempDir, Arc<AppState>) {
    let dir = tempfile::tempdir().unwrap();
    let meta = crate::support::distributed_meta(dir.path().join("peryx.redb"));
    let blobs = BlobStore::new(dir.path().join("blobs"));
    (dir, Arc::new(AppState::new(meta, blobs, 60, Vec::new())))
}

fn install_distributed_services(state: &mut Arc<AppState>, mode: peryx_core::TopologyMode, role: peryx_core::NodeRole) {
    let topology = peryx_core::TopologyConfig {
        mode,
        group: Some("ownership".to_owned()),
        members: Vec::new(),
        local_node: None,
    };
    let metrics = Arc::new(DcDurabilityMetrics::default());
    let durability = Arc::new(DistributedBlobDurability::new(
        topology.clone(),
        peryx_ha::DurabilityPolicy::Local,
        Vec::new(),
        Vec::new(),
        Duration::ZERO,
        metrics,
    ));
    Arc::get_mut(state)
        .unwrap()
        .install_distributed_availability(peryx_ha::AvailabilityStateInstall {
            role,
            topology,
            blobs: peryx_ha::BlobServices::new(None, durability),
            analytics: Arc::new(DistributedAnalyticsCompleteness),
            capabilities: peryx_ha::AvailabilityCapabilities::default(),
            authority_drainer: None,
            operations: None,
        })
        .unwrap();
}

fn primary_config(dir: &tempfile::TempDir, mode: DistributedMode) -> RuntimeConfig {
    RuntimeConfig {
        mode,
        role: RuntimeRole::Primary {
            source: "primary-a".to_owned(),
            token: TOKEN.to_owned(),
        },
        write_ack_policy: peryx_ha::DurabilityPolicy::Local,
        membership: None,
        node_identity: None,
        writer_identity: None,
        data_dir: dir.path().to_path_buf(),
        read_through: None,
    }
}

fn replica_config(dir: &tempfile::TempDir, upstream: &str) -> RuntimeConfig {
    RuntimeConfig {
        mode: DistributedMode::Dc,
        role: RuntimeRole::Replica {
            upstream: upstream.to_owned(),
            token: TOKEN.to_owned(),
            poll_interval: Duration::from_millis(1),
            page_size: NonZeroUsize::new(2).unwrap(),
        },
        write_ack_policy: peryx_ha::DurabilityPolicy::Local,
        membership: None,
        node_identity: None,
        writer_identity: Some("writer-a".to_owned()),
        data_dir: dir.path().to_path_buf(),
        read_through: None,
    }
}

fn member(node: &str, datacenter: &str, address: &str, role: RuntimeMemberRole) -> RuntimeMember {
    RuntimeMember {
        node: node.to_owned(),
        datacenter: datacenter.to_owned(),
        address: address.to_owned(),
        role,
    }
}

fn membership(members: Vec<RuntimeMember>) -> RuntimeMembership {
    RuntimeMembership {
        group: "ownership".to_owned(),
        members,
    }
}

/// Addresses the voter `ha_config` gives this node; the router refuses an RPC aimed at another.
async fn post_vote(router: &Router) -> StatusCode {
    let request = openraft::raft::VoteRequest::new(openraft::Vote::new(1, 0), None);
    router
        .clone()
        .oneshot(
            Request::post("/+replication/v1/raft/vote")
                .header(header::AUTHORIZATION, format!("Bearer {TOKEN}"))
                .header(
                    "x-peryx-raft-target",
                    crate::consensus_runtime::voter_id("east").to_string(),
                )
                .header(header::CONTENT_TYPE, "application/json")
                .body(Body::from(serde_json::to_vec(&request).unwrap()))
                .unwrap(),
        )
        .await
        .unwrap()
        .status()
}

async fn status_line(address: std::net::SocketAddr, request: &str) -> String {
    let mut connection = tokio::net::TcpStream::connect(address).await.unwrap();
    connection
        .write_all(format!("{request} HTTP/1.1\r\nHost: localhost\r\nAuthorization: Bearer {TOKEN}\r\nContent-Length: 0\r\nConnection: close\r\n\r\n").as_bytes())
        .await
        .unwrap();
    let mut response = Vec::new();
    connection.read_to_end(&mut response).await.unwrap();
    String::from_utf8_lossy(&response)
        .lines()
        .next()
        .expect("the listener answers with a status line")
        .to_owned()
}

async fn get(router: &Router, path: &str) -> (StatusCode, Value) {
    let response = router
        .clone()
        .oneshot(
            Request::get(path)
                .header(header::AUTHORIZATION, format!("Bearer {TOKEN}"))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    let status = response.status();
    let body = response.into_body().collect().await.unwrap().to_bytes();
    (status, serde_json::from_slice(&body).unwrap())
}

#[test]
fn runtime_configuration_helpers_resolve_roles_and_locality() {
    let dir = tempfile::tempdir().unwrap();
    let mut primary = primary_config(&dir, DistributedMode::Dc);
    let replica = replica_config(&dir, "http://primary.internal:4460");
    assert_eq!(DistributedMode::Dc.as_str(), "dc");
    assert_eq!(DistributedMode::Ha.as_str(), "ha");
    assert_eq!(primary.role.token(), TOKEN);
    assert_eq!(replica.role.token(), TOKEN);

    primary.membership = Some(membership(vec![
        member("writer", "east", "http://east.internal:4460", RuntimeMemberRole::Writer),
        member(
            "replica",
            "west",
            "http://west.internal:4460",
            RuntimeMemberRole::Replica,
        ),
    ]));
    let roster = primary.membership.as_ref().unwrap();
    assert_eq!(local_datacenter(&primary, roster), None);
    primary.writer_identity = Some("writer".to_owned());
    assert_eq!(local_datacenter(&primary, roster).as_deref(), Some("east"));
    primary.node_identity = Some("replica".to_owned());
    assert_eq!(local_datacenter(&primary, roster).as_deref(), Some("west"));
    primary.node_identity = Some("unknown".to_owned());
    assert_eq!(local_datacenter(&primary, roster), None);

    assert_eq!(peer_blob_base("host.internal:4460"), "http://host.internal:4460");
    assert_eq!(peer_blob_base("http://host.internal:4460"), "http://host.internal:4460");
    assert_eq!(
        peer_blob_base("https://host.internal:4460"),
        "https://host.internal:4460"
    );
}

#[test]
fn readiness_sources_follow_membership_roles() {
    let dir = tempfile::tempdir().unwrap();
    let mut config = primary_config(&dir, DistributedMode::Dc);
    assert!(group_readiness_source(&config).is_none());
    assert!(primary_liveness(&config).is_none());

    config.membership = Some(membership(vec![member(
        "writer",
        "east",
        "http://east.internal:4460",
        RuntimeMemberRole::Writer,
    )]));
    assert_eq!(
        group_readiness_source(&config).unwrap().members,
        vec![("writer".to_owned(), MemberRole::Writer)]
    );
    assert!(primary_liveness(&config).is_none());

    config.membership.as_mut().unwrap().members.push(member(
        "replica",
        "west",
        "http://west.internal:4460",
        RuntimeMemberRole::Replica,
    ));
    assert_eq!(
        group_readiness_source(&config).unwrap().members,
        vec![
            ("writer".to_owned(), MemberRole::Writer),
            ("replica".to_owned(), MemberRole::Replica),
        ]
    );
    assert!(primary_liveness(&config).is_some());
}

#[tokio::test]
async fn remote_blob_availability_requires_a_valid_remote_peer() {
    let (dir, mut state) = state();
    let mut config = primary_config(&dir, DistributedMode::Dc);
    assert!(
        remote_blob_availability(
            &config,
            state.serving.meta.clone(),
            state.serving.blobs.clone(),
            state.serving.clock.clone(),
        )
        .unwrap()
        .is_none()
    );

    config.membership = Some(membership(vec![member(
        "writer",
        "east",
        "http://east.internal:4460",
        RuntimeMemberRole::Writer,
    )]));
    assert!(
        remote_blob_availability(
            &config,
            state.serving.meta.clone(),
            state.serving.blobs.clone(),
            state.serving.clock.clone(),
        )
        .unwrap()
        .is_none()
    );
    config.writer_identity = Some("writer".to_owned());
    assert!(
        remote_blob_availability(
            &config,
            state.serving.meta.clone(),
            state.serving.blobs.clone(),
            state.serving.clock.clone(),
        )
        .unwrap()
        .is_none()
    );

    config.membership.as_mut().unwrap().members.push(member(
        "replica",
        "west",
        "not a url",
        RuntimeMemberRole::Replica,
    ));
    assert!(
        remote_blob_availability(
            &config,
            state.serving.meta.clone(),
            state.serving.blobs.clone(),
            state.serving.clock.clone(),
        )
        .is_err()
    );
    config.membership.as_mut().unwrap().members[1].address = "http://west.internal:4460".to_owned();
    config.read_through = Some(DEFAULT_READ_THROUGH_LIMITS);
    let availability = remote_blob_availability(
        &config,
        state.serving.meta.clone(),
        state.serving.blobs.clone(),
        state.serving.clock.clone(),
    )
    .unwrap();
    assert!(availability.is_none());
    let topology = peryx_core::TopologyConfig {
        mode: peryx_core::TopologyMode::Dc,
        group: Some("ownership".to_owned()),
        members: Vec::new(),
        local_node: None,
    };
    let metrics = Arc::new(DcDurabilityMetrics::default());
    let durability = Arc::new(DistributedBlobDurability::new(
        topology.clone(),
        peryx_ha::DurabilityPolicy::Local,
        Vec::new(),
        Vec::new(),
        Duration::ZERO,
        metrics.clone(),
    ));
    Arc::get_mut(&mut state)
        .unwrap()
        .install_distributed_availability(peryx_ha::AvailabilityStateInstall {
            role: peryx_core::NodeRole::Writer,
            topology,
            blobs: peryx_ha::BlobServices::new(availability, durability),
            analytics: Arc::new(DistributedAnalyticsCompleteness),
            capabilities: peryx_ha::AvailabilityCapabilities::default(),
            authority_drainer: None,
            operations: None,
        })
        .unwrap();

    assert!(
        state
            .serving
            .ensure_blob_local(&peryx_storage::blob::Digest::of(b"missing"))
            .await
            .unwrap()
            .is_none()
    );
}

#[rstest::rstest]
#[case::writer_first(false)]
#[case::writer_last(true)]
fn remote_blob_availability_prefers_the_writer_in_roster_order(#[case] writer_last: bool) {
    let (dir, state) = state();
    let writer = member(
        "remote-writer",
        "west",
        "http://writer.internal:4460",
        RuntimeMemberRole::Writer,
    );
    let replica = member("remote-replica", "west", "not a url", RuntimeMemberRole::Replica);
    let mut remote = if writer_last {
        vec![replica, writer]
    } else {
        vec![writer, replica]
    };
    remote.insert(
        0,
        member(
            "local",
            "east",
            "http://local.internal:4460",
            RuntimeMemberRole::Replica,
        ),
    );
    let mut config = replica_config(&dir, "http://primary.internal:4460");
    config.node_identity = Some("local".to_owned());
    config.membership = Some(membership(remote));

    assert!(
        remote_blob_availability(
            &config,
            state.serving.meta.clone(),
            state.serving.blobs.clone(),
            state.serving.clock.clone(),
        )
        .unwrap()
        .is_some()
    );
}

#[test]
fn remote_blob_availability_rejects_an_invalid_local_datacenter() {
    let (dir, state) = state();
    let mut config = primary_config(&dir, DistributedMode::Dc);
    config.writer_identity = Some("writer".to_owned());
    config.membership = Some(membership(vec![
        member("writer", "", "http://east.internal:4460", RuntimeMemberRole::Writer),
        member(
            "replica",
            "west",
            "http://west.internal:4460",
            RuntimeMemberRole::Replica,
        ),
    ]));

    assert!(
        remote_blob_availability(
            &config,
            state.serving.meta.clone(),
            state.serving.blobs.clone(),
            state.serving.clock.clone(),
        )
        .is_err()
    );
}

#[test]
fn replica_transport_assembly_handles_roster_duplicates_and_errors() {
    let roster = membership(vec![
        member(
            "local",
            "east",
            "http://local.internal:4460",
            RuntimeMemberRole::Replica,
        ),
        member("peer-a", "west", "http://peer.internal:4460", RuntimeMemberRole::Writer),
        member(
            "peer-b",
            "west",
            "http://peer.internal:4460",
            RuntimeMemberRole::Replica,
        ),
    ]);
    assert!(
        metadata_peers(
            Some(&roster),
            Some("local"),
            "http://peer.internal:4460",
            TOKEN,
            3,
            NonZeroUsize::MIN,
        )
        .is_ok()
    );
    assert!(metadata_peers(None, None, "not a url", TOKEN, 0, NonZeroUsize::MIN).is_err());

    let invalid = membership(vec![member("broken", "west", "not a url", RuntimeMemberRole::Replica)]);
    assert_eq!(
        metadata_peers(
            Some(&invalid),
            None,
            "http://upstream.internal:4460",
            TOKEN,
            0,
            NonZeroUsize::MIN,
        )
        .err()
        .unwrap()
        .to_string(),
        "build the metadata peer transport for broken"
    );
}

#[test]
fn replica_blob_deferral_resolves_local_and_remote_datacenters() {
    let dir = tempfile::tempdir().unwrap();
    let config = replica_config(&dir, "http://primary.internal:4460");
    let (local, delegates) = replica_blob_deferral(&config, TOKEN).unwrap();
    assert!(local.is_empty());
    assert!(delegates.is_empty());

    let mut config = config;
    config.node_identity = Some("local".to_owned());
    config.membership = Some(membership(vec![
        member(
            "local",
            "east",
            "http://local.internal:4460",
            RuntimeMemberRole::Replica,
        ),
        member(
            "remote",
            "west",
            "http://remote.internal:4460",
            RuntimeMemberRole::Writer,
        ),
    ]));
    let (local, delegates) = replica_blob_deferral(&config, TOKEN).unwrap();
    assert_eq!(local, "east");
    assert_eq!(delegates.len(), 1);
}

#[tokio::test]
async fn primary_runtime_mounts_routes_and_starts_no_workers() {
    let (dir, state) = state();
    let mut runtime = runtime(&primary_config(&dir, DistributedMode::Dc), &state).unwrap();
    assert!(!runtime.is_replica());
    assert_eq!(sync_cycle(&mut runtime).await, None);
    let (lifecycle, _) = crate::lifecycle::Lifecycle::new();
    assert!(
        runtime
            .ignite_consensus_with_lifecycle(lifecycle)
            .await
            .unwrap()
            .is_none()
    );
    assert_eq!(
        runtime.reclamation_frontiers().observe(),
        Some(ObservedFrontier {
            replica: None,
            backup: None,
        })
    );
    let router = runtime.routes();
    let digest = state.serving.blobs.put_bytes(b"receipt").await.unwrap();

    let changes = router
        .clone()
        .oneshot(
            Request::get("/+replication/v1/changes?after=0&limit=1")
                .header(header::AUTHORIZATION, format!("Bearer {TOKEN}"))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(changes.status(), StatusCode::OK);
    let receipt = router
        .clone()
        .oneshot(
            Request::get(format!("/+replication/v1/receipts/sha256/{}", digest.as_str()))
                .header(header::AUTHORIZATION, format!("Bearer {TOKEN}"))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(
        receipt.status(),
        StatusCode::NOT_FOUND,
        "a node with no configured identity cannot attest a receipt"
    );
    assert_eq!(get(&router, "/+replication/v1/health").await.0, StatusCode::OK);
    assert_eq!(get(&router, "/+replication/v1/ready").await.0, StatusCode::OK);
    let (lifecycle, _) = crate::lifecycle::Lifecycle::new();
    lifecycle.activate();
    let runtime = runtime.prepare_worker_runtime().unwrap();
    assert!(runtime.start_with_lifecycle(lifecycle).unwrap().is_none());
}

#[tokio::test]
async fn primary_runtime_receipt_names_the_configured_node() {
    let (dir, state) = state();
    let config = RuntimeConfig {
        writer_identity: Some("east-1".to_owned()),
        ..primary_config(&dir, DistributedMode::Dc)
    };
    let runtime = runtime(&config, &state).unwrap();
    let digest = state.serving.blobs.put_bytes(b"receipt").await.unwrap();

    let (status, body) = get(
        &runtime.routes(),
        &format!("/+replication/v1/receipts/sha256/{}", digest.as_str()),
    )
    .await;

    assert_eq!(status, StatusCode::OK);
    assert_eq!(
        body,
        serde_json::json!({ "node": "east-1", "digest": digest.as_str(), "size": b"receipt".len() })
    );
}

#[tokio::test]
async fn prepared_runtime_owns_distributed_startup() {
    let (dir, mut state) = state();
    let config = primary_config(&dir, DistributedMode::Dc);
    install_services(
        &DistributedServiceConfig {
            runtime: config.clone(),
            read_only: false,
            write_ack_deadline: Duration::ZERO,
        },
        Arc::get_mut(&mut state).unwrap(),
    )
    .unwrap();
    let runtime = runtime(&config, &state).unwrap();

    let prepared = runtime
        .prepare(DistributedPrepareContext {
            config,
            state: state.clone(),
            control_authorizer: Arc::new(DenyControl),
            references: Arc::new(EmptyReferences),
            listener: None,
        })
        .await
        .unwrap();

    assert!(!prepared.is_replica);
    assert!(prepared.metrics.is_empty());
    assert_eq!(
        get(&prepared.public_routes, "/+replication/v1/health").await.0,
        StatusCode::OK
    );
}

#[tokio::test]
async fn prepared_ha_runtime_binds_consensus_workers_and_shutdown() {
    let (dir, mut state) = state();
    let mut config = primary_config(&dir, DistributedMode::Ha);
    config.node_identity = Some("writer".to_owned());
    config.writer_identity = Some("writer".to_owned());
    config.membership = Some(membership(vec![
        member("writer", "east", "http://127.0.0.1:0", RuntimeMemberRole::Writer),
        member("replica", "west", "http://127.0.0.1:1", RuntimeMemberRole::Replica),
    ]));
    install_services(
        &DistributedServiceConfig {
            runtime: config.clone(),
            read_only: false,
            write_ack_deadline: Duration::ZERO,
        },
        Arc::get_mut(&mut state).unwrap(),
    )
    .unwrap();
    assert_eq!(EmptyReferences.referenced().unwrap(), BTreeSet::new());
    assert!(DenyControl.authenticate(None).await.unwrap().is_none());
    assert!(!DenyControl.allows(
        &peryx_ha::ControlActor::new("operator"),
        peryx_ha::ControlPermission::Read
    ));

    let prepared = runtime(&config, &state)
        .unwrap()
        .prepare(DistributedPrepareContext {
            config,
            state: state.clone(),
            control_authorizer: Arc::new(DenyControl),
            references: Arc::new(EmptyReferences),
            listener: Some(listener()),
        })
        .await
        .unwrap();

    assert!(!prepared.is_replica);
    assert_ownership_unavailable(&state).await;
    assert_distributed_work_unavailable(&state).await;
    let address = prepared.handle.listener_address().unwrap();
    let mut active = prepared.activate().unwrap();
    assert!(state.serving.ownership_authority().is_some());
    let mut connection = tokio::net::TcpStream::connect(address).await.unwrap();
    connection
        .write_all(b"GET /+replication/v1/health HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\r\n")
        .await
        .unwrap();
    let mut response = Vec::new();
    connection.read_to_end(&mut response).await.unwrap();
    assert!(response.starts_with(b"HTTP/1.1 "));
    peryx_ha::ActiveAvailabilityHandle::shutdown(&mut active.handle)
        .await
        .unwrap();
    drop(tokio::net::TcpListener::bind(address).await.unwrap());
}

#[tokio::test]
async fn ha_public_routes_refuse_peer_votes_until_consensus_ignites() {
    let (dir, mut state) = state();
    let config = ha_config(&dir);
    install_runtime_services(&mut state, &config);
    let prepared = prepare_with_listener(config, state, listener()).await.unwrap();

    assert_eq!(
        post_vote(&prepared.public_routes).await,
        StatusCode::SERVICE_UNAVAILABLE
    );

    prepared.shutdown().await.unwrap();
}

#[tokio::test]
async fn ha_public_routes_serve_peer_votes_once_consensus_ignites() {
    let (dir, mut state) = state();
    let config = ha_config(&dir);
    install_runtime_services(&mut state, &config);
    let prepared = prepare_with_listener(config, state, listener()).await.unwrap();
    let public = prepared.public_routes.clone();
    let mut active = prepared.activate().unwrap();

    assert_eq!(post_vote(&public).await, StatusCode::OK);

    peryx_ha::ActiveAvailabilityHandle::shutdown(&mut active.handle)
        .await
        .unwrap();
}

#[tokio::test]
async fn ha_public_routes_refuse_peer_votes_after_shutdown() {
    let (dir, mut state) = state();
    let config = ha_config(&dir);
    install_runtime_services(&mut state, &config);
    let prepared = prepare_with_listener(config, state, listener()).await.unwrap();
    let public = prepared.public_routes.clone();
    let mut active = prepared.activate().unwrap();
    peryx_ha::ActiveAvailabilityHandle::shutdown(&mut active.handle)
        .await
        .unwrap();

    assert_eq!(post_vote(&public).await, StatusCode::SERVICE_UNAVAILABLE);
}

/// The roster names one address per member, so the socket that answers a peer RPC has to be the one
/// every other peer transport already dials: the public server, never the control listener.
#[tokio::test]
async fn the_control_listener_does_not_answer_peer_votes() {
    let (dir, mut state) = state();
    let config = ha_config(&dir);
    install_runtime_services(&mut state, &config);
    let prepared = prepare_with_listener(config, state, listener()).await.unwrap();
    let address = prepared.handle.listener_address().unwrap();
    let mut active = prepared.activate().unwrap();

    assert_eq!(
        status_line(address, "POST /+replication/v1/raft/vote").await,
        "HTTP/1.1 404 Not Found"
    );

    peryx_ha::ActiveAvailabilityHandle::shutdown(&mut active.handle)
        .await
        .unwrap();
}

#[tokio::test]
async fn prepared_replica_shutdown_terminates_worker_runtime() {
    let (dir, mut state) = state();
    let config = replica_config(&dir, "http://127.0.0.1:1");
    install_runtime_services(&mut state, &config);
    state.serving.meta.claim_writer_identity("writer-a").unwrap();
    let prepared = runtime(&config, &state)
        .unwrap()
        .prepare(DistributedPrepareContext {
            config,
            state,
            control_authorizer: Arc::new(DenyControl),
            references: Arc::new(EmptyReferences),
            listener: None,
        })
        .await
        .unwrap();

    prepared.shutdown().await.unwrap();
}

#[tokio::test]
async fn dropping_an_active_replica_cancels_worker_runtime() {
    let (dir, mut state) = state();
    let config = replica_config(&dir, "http://127.0.0.1:1");
    install_runtime_services(&mut state, &config);
    state.serving.meta.claim_writer_identity("writer-a").unwrap();
    let prepared = runtime(&config, &state)
        .unwrap()
        .prepare(DistributedPrepareContext {
            config,
            state,
            control_authorizer: Arc::new(DenyControl),
            references: Arc::new(EmptyReferences),
            listener: None,
        })
        .await
        .unwrap();

    drop(prepared.activate().unwrap().handle);
}

#[tokio::test]
async fn listener_address_panic_is_observed_by_the_caller() {
    let (dir, mut state) = state();
    let config = ha_config(&dir);
    install_runtime_services(&mut state, &config);
    let prepared = prepare_with_listener(config, state, Box::new(PanickingAddressListener))
        .await
        .unwrap();

    let panic = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| prepared.handle.listener_address()))
        .expect_err("listener address should panic");

    assert_eq!(panic.downcast_ref::<&str>(), Some(&"injected address panic"));
    let listener = crate::PreparedAvailabilityListener::serve(
        Box::new(PanickingAddressListener),
        axum::Router::new(),
        tokio_util::sync::CancellationToken::new(),
    )
    .unwrap();
    listener.await.unwrap();
    prepared.shutdown().await.unwrap();
}

#[tokio::test]
async fn missing_listener_starts_no_consensus_log_or_authority() {
    let (dir, mut state) = state();
    let mut config = primary_config(&dir, DistributedMode::Ha);
    config.node_identity = Some("writer".to_owned());
    config.writer_identity = Some("writer".to_owned());
    config.membership = Some(membership(vec![member(
        "writer",
        "east",
        "http://127.0.0.1:0",
        RuntimeMemberRole::Writer,
    )]));
    install_services(
        &DistributedServiceConfig {
            runtime: config.clone(),
            read_only: false,
            write_ack_deadline: Duration::ZERO,
        },
        Arc::get_mut(&mut state).unwrap(),
    )
    .unwrap();

    let error = runtime(&config, &state)
        .unwrap()
        .prepare(DistributedPrepareContext {
            config,
            state: state.clone(),
            control_authorizer: Arc::new(DenyControl),
            references: Arc::new(EmptyReferences),
            listener: None,
        })
        .await
        .err()
        .unwrap();

    assert!(
        error.to_string().contains("requires a bound availability listener"),
        "{error}"
    );
    assert!(!dir.path().join(LOG_STORE_SUBPATH).exists());
    assert_ownership_unavailable(&state).await;
}

#[tokio::test]
async fn assembly_failure_publishes_no_authority() {
    let (dir, mut state) = state();
    let mut config = primary_config(&dir, DistributedMode::Ha);
    config.node_identity = Some("writer".to_owned());
    config.writer_identity = Some("writer".to_owned());
    config.membership = Some(membership(vec![member(
        "writer",
        "",
        "http://127.0.0.1:0",
        RuntimeMemberRole::Writer,
    )]));
    let error = install_services(
        &DistributedServiceConfig {
            runtime: config,
            read_only: false,
            write_ack_deadline: Duration::ZERO,
        },
        Arc::get_mut(&mut state).unwrap(),
    )
    .unwrap_err();

    assert!(error.to_string().contains("local datacenter"), "{error}");
    assert!(state.serving.ownership_authority().is_none());
    assert!(state.serving.cross_dc_copier().is_none());
    assert!(state.serving.placement_reconciler().is_none());
    assert!(state.serving.blob_reclaimer().is_none());
}

#[tokio::test]
async fn listener_setup_failure_cleans_started_resources_before_returning() {
    let (dir, mut state) = state();
    let config = ha_config(&dir);
    install_runtime_services(&mut state, &config);

    let prepared = prepare_with_listener(config, state.clone(), Box::new(FailingListener))
        .await
        .unwrap();
    assert_eq!(prepared.handle.listener_address(), Some("127.0.0.1:0".parse().unwrap()));
    assert_ownership_unavailable(&state).await;
    assert!(!dir.path().join(LOG_STORE_SUBPATH).exists());

    let error = prepared.activate().err().unwrap();

    assert!(error.to_string().contains("injected failure"), "{error}");
    assert_ownership_unavailable(&state).await;
    drop(crate::raft::persistence::RaftLogStore::open(dir.path().join(LOG_STORE_SUBPATH)).unwrap());
}

#[tokio::test]
async fn listener_panic_is_reported_after_all_owned_resources_stop() {
    let (dir, mut state) = state();
    let config = ha_config(&dir);
    install_runtime_services(&mut state, &config);
    let socket = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let address = socket.local_addr().unwrap();
    let (trigger, panic) = tokio::sync::oneshot::channel();
    let (entered, listener_entered) = tokio::sync::oneshot::channel();
    let (stopped, listener_stopped) = tokio::sync::oneshot::channel();
    let listener_address = socket.local_addr().unwrap();
    let prepared = prepare_with_listener(
        config,
        state,
        Box::new(ControlledListener {
            listener: socket,
            trigger: panic,
            entered: Some(entered),
            stopped,
            panic: true,
        }),
    )
    .await
    .unwrap();
    assert_eq!(prepared.handle.listener_address(), Some(listener_address));

    let mut handle = prepared.activate().unwrap().handle;
    listener_entered.await.unwrap();
    trigger.send(()).unwrap();
    let failure = peryx_ha::ActiveAvailabilityHandle::wait_for_failure(&mut handle).await;
    assert!(failure.to_string().contains("injected listener panic"), "{failure}");
    let error = peryx_ha::ActiveAvailabilityHandle::shutdown(&mut handle)
        .await
        .unwrap_err();
    listener_stopped.await.unwrap();

    assert_eq!(error.failures()[0].stage, peryx_ha::AvailabilityShutdownStage::Listener);
    drop(tokio::net::TcpListener::bind(address).await.unwrap());
    drop(crate::raft::persistence::RaftLogStore::open(dir.path().join(LOG_STORE_SUBPATH)).unwrap());
}

#[tokio::test]
async fn listener_failure_is_observed_before_shutdown_cleanup() {
    let (dir, mut state) = state();
    let config = ha_config(&dir);
    install_runtime_services(&mut state, &config);
    let socket = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let address = socket.local_addr().unwrap();
    let (trigger, panic) = tokio::sync::oneshot::channel();
    let (entered, listener_entered) = tokio::sync::oneshot::channel();
    let (stopped, listener_stopped) = tokio::sync::oneshot::channel();
    let prepared = prepare_with_listener(
        config,
        state,
        Box::new(ControlledListener {
            listener: socket,
            trigger: panic,
            entered: Some(entered),
            stopped,
            panic: true,
        }),
    )
    .await
    .unwrap();
    let mut handle = prepared.activate().unwrap().handle;

    listener_entered.await.unwrap();
    trigger.send(()).unwrap();
    let failure = peryx_ha::ActiveAvailabilityHandle::wait_for_failure(&mut handle).await;
    assert!(failure.to_string().contains("injected listener panic"), "{failure}");
    assert!(peryx_ha::ActiveAvailabilityHandle::shutdown(&mut handle).await.is_err());
    listener_stopped.await.unwrap();
    drop(tokio::net::TcpListener::bind(address).await.unwrap());
    drop(crate::raft::persistence::RaftLogStore::open(dir.path().join(LOG_STORE_SUBPATH)).unwrap());
}

#[tokio::test]
async fn prepared_listener_stays_inactive_until_activation_and_drop_cancels_it() {
    let (dir, mut state) = state();
    let config = ha_config(&dir);
    install_runtime_services(&mut state, &config);
    let socket = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let address = socket.local_addr().unwrap();
    let (_trigger, release) = tokio::sync::oneshot::channel();
    let (entered, mut listener_entered) = tokio::sync::oneshot::channel();
    let (stopped, listener_stopped) = tokio::sync::oneshot::channel();
    let prepared = prepare_with_listener(
        config,
        state.clone(),
        Box::new(ControlledListener {
            listener: socket,
            trigger: release,
            entered: Some(entered),
            stopped,
            panic: false,
        }),
    )
    .await
    .unwrap();

    assert_ownership_unavailable(&state).await;
    assert!(!dir.path().join(LOG_STORE_SUBPATH).exists());
    assert_eq!(
        listener_entered.try_recv(),
        Err(tokio::sync::oneshot::error::TryRecvError::Empty)
    );
    let active = prepared.activate().unwrap();
    listener_entered.await.unwrap();
    assert!(state.serving.ownership_authority().is_some());

    drop(active.handle);
    listener_stopped.await.unwrap();

    drop(tokio::net::TcpListener::bind(address).await.unwrap());
}

#[tokio::test]
async fn listener_destructor_precedes_consensus_teardown_after_handle_drop() {
    let (dir, mut state) = state();
    let config = ha_config(&dir);
    install_runtime_services(&mut state, &config);
    let (stopped, listener_stopped) = std::sync::mpsc::channel();
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let listener_address = listener.local_addr().unwrap();
    let prepared = prepare_with_listener(
        config,
        state,
        Box::new(ConsensusOrderListener {
            listener,
            log_path: dir.path().join(LOG_STORE_SUBPATH),
            stopped,
        }),
    )
    .await
    .unwrap();
    assert_eq!(prepared.handle.listener_address(), Some(listener_address));
    drop(prepared.activate().unwrap().handle);

    assert!(listener_stopped.recv().unwrap());
}

#[tokio::test]
async fn ha_runtime_mounts_the_frontier_endpoint() {
    let (dir, state) = state();
    let runtime = runtime_with_frontier(
        &primary_config(&dir, DistributedMode::Ha),
        &state,
        AvailabilityAudience::Public,
        Arc::new(StaticFrontier(Some(crate::FrontierReply {
            epoch: 7,
            applied_frontier: 11,
        }))),
    )
    .unwrap();

    let response = runtime
        .routes()
        .oneshot(
            Request::get("/+replication/v1/frontier/resource")
                .header(header::AUTHORIZATION, format!("Bearer {TOKEN}"))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
}

#[tokio::test]
async fn primary_runtime_reports_group_readiness_and_ignites_consensus() {
    let (dir, mut state) = state();
    install_distributed_services(&mut state, peryx_core::TopologyMode::Ha, peryx_core::NodeRole::Writer);
    let mut config = primary_config(&dir, DistributedMode::Ha);
    config.write_ack_policy = peryx_ha::DurabilityPolicy::Majority;
    config.node_identity = Some("writer".to_owned());
    config.writer_identity = Some("writer".to_owned());
    config.membership = Some(membership(vec![
        member("writer", "east", "http://127.0.0.1:4460", RuntimeMemberRole::Writer),
        member("replica", "west", "http://127.0.0.1:4461", RuntimeMemberRole::Replica),
    ]));
    let runtime = runtime_with_audience(&config, &state, AvailabilityAudience::Operator).unwrap();
    assert!(runtime.reclamation_frontiers().observe().is_none());
    let (_, document) = get(&runtime.routes(), "/+replication/v1/health").await;
    assert_eq!(document["mode"], "ha");
    assert_eq!(document["ready"], true);
    assert_eq!(document["serial"], 0);
    assert!(document.get("peers").is_some());
    assert_eq!(
        document["group_readiness"],
        serde_json::json!({
            "ready": false,
            "durable_frontier": 0,
            "policy": "majority",
            "blocked": {"insufficient_members": {"reporting": 1, "required": 2}},
        }),
    );

    let (lifecycle, _) = crate::lifecycle::Lifecycle::new();
    let consensus = runtime
        .ignite_consensus_with_lifecycle(lifecycle)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(consensus.authority.cluster_status().voters, vec!["east", "west"]);
    consensus.shutdown().unwrap();
}

#[tokio::test]
async fn replication_health_routes_report_password_overload() {
    let (dir, state) = state();
    let runtime = runtime_with_authorizer(
        &primary_config(&dir, DistributedMode::Ha),
        &state,
        Arc::new(StaticFrontier(None)),
        Arc::new(UnavailableAuthorizer),
    )
    .unwrap();

    for path in ["/+replication/v1/health", "/+replication/v1/ready"] {
        let (status, document) = get(&runtime.routes(), path).await;
        assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE, "{path}");
        assert_eq!(document, serde_json::json!({"error": "identity service unavailable"}));
    }
}

#[tokio::test]
async fn primary_runtime_reports_blob_store_failure() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("blobs"), b"not a directory").unwrap();
    let meta = MetaStore::open(dir.path().join("peryx.redb")).unwrap();
    let state = Arc::new(AppState::new(
        meta,
        BlobStore::new(dir.path().join("blobs")),
        60,
        Vec::new(),
    ));
    let runtime = runtime(&primary_config(&dir, DistributedMode::Ha), &state).unwrap();

    let (status, document) = get(&runtime.routes(), "/+replication/v1/ready").await;
    assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE);
    assert_eq!(document["reasons"], serde_json::json!(["blob_store"]));
}

#[tokio::test]
async fn replica_runtime_pulls_metadata_and_reports_worker_health() {
    let primary_dir = tempfile::tempdir().unwrap();
    let meta = MetaStore::open(primary_dir.path().join("peryx.redb")).unwrap();
    meta.commit_driver_txn(|_| Ok::<_, peryx_storage::meta::MetaError>(((), vec![b"one".to_vec()])))
        .unwrap();
    let server = TestServer::start(
        primary_router(
            "writer-a",
            TOKEN,
            meta,
            BlobStore::new(primary_dir.path().join("blobs")),
        )
        .unwrap(),
    )
    .await;
    let (dir, state) = state();
    state.serving.meta.claim_writer_identity("writer-a").unwrap();
    let mut runtime = runtime(&replica_config(&dir, &server.url), &state).unwrap();
    assert!(runtime.is_replica());
    assert_eq!(sync_cycle(&mut runtime).await, Some(true));

    runtime.availability.workers.as_ref().unwrap().record_panic();
    let (status, document) = get(&runtime.routes(), "/+replication/v1/ready").await;
    assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE);
    assert_eq!(document["reasons"], serde_json::json!(["worker_unhealthy"]));
    let (lifecycle, _) = crate::lifecycle::Lifecycle::new();
    lifecycle.activate();
    let runtime = runtime.prepare_worker_runtime().unwrap();
    assert!(runtime.start_with_lifecycle(lifecycle).unwrap().is_some());
}

#[tokio::test]
async fn replica_runtime_builds_rostered_services_and_beacon() {
    let (dir, mut state) = state();
    install_distributed_services(&mut state, peryx_core::TopologyMode::Ha, peryx_core::NodeRole::Replica);
    state.serving.meta.claim_writer_identity("writer").unwrap();
    let mut config = replica_config(&dir, "http://127.0.0.1:1");
    config.mode = DistributedMode::Ha;
    config.node_identity = Some("replica".to_owned());
    config.writer_identity = Some("writer".to_owned());
    config.membership = Some(membership(vec![
        member("writer", "east", "http://127.0.0.1:4460", RuntimeMemberRole::Writer),
        member("replica", "west", "http://127.0.0.1:4461", RuntimeMemberRole::Replica),
    ]));
    let runtime = runtime(&config, &state).unwrap();

    let (lifecycle, _) = crate::lifecycle::Lifecycle::new();
    lifecycle.activate();
    let runtime = runtime.prepare_worker_runtime().unwrap();
    assert!(runtime.start_with_lifecycle(lifecycle).unwrap().is_some());
}

#[test]
fn replica_runtime_rejects_invalid_urls() {
    let (dir, state) = state();
    state.serving.meta.claim_writer_identity("writer-a").unwrap();

    assert!(runtime(&replica_config(&dir, "not a url"), &state).is_err());
}

#[test]
fn consensus_plan_validates_ha_rosters() {
    let dir = tempfile::tempdir().unwrap();
    assert!(
        consensus_plan(&primary_config(&dir, DistributedMode::Dc))
            .unwrap()
            .is_none()
    );
    assert!(
        consensus_plan(&primary_config(&dir, DistributedMode::Ha))
            .unwrap()
            .is_none()
    );

    let mut config = primary_config(&dir, DistributedMode::Ha);
    config.membership = Some(membership(vec![member(
        "writer",
        "east",
        "http://127.0.0.1:4460",
        RuntimeMemberRole::Writer,
    )]));
    assert_eq!(
        consensus_plan(&config).err().unwrap().to_string(),
        "an `ha` consensus roster needs a `node-identity` naming this node's own member entry"
    );
    config.node_identity = Some("unknown".to_owned());
    assert!(consensus_plan(&config).is_err());
    config.node_identity = Some("writer".to_owned());
    config.membership.as_mut().unwrap().members[0].address = "not a url".to_owned();
    assert!(consensus_plan(&config).is_err());
    config.membership.as_mut().unwrap().members[0].address = "http://127.0.0.1:4460".to_owned();
    assert!(consensus_plan(&config).unwrap().is_some());
}

#[tokio::test]
async fn replica_readiness_names_frontier_lag_before_the_first_cycle() {
    let (dir, state) = state();
    state.serving.meta.claim_writer_identity("writer-a").unwrap();
    let runtime = runtime(&replica_config(&dir, "http://127.0.0.1:1"), &state).unwrap();

    let (status, document) = get(&runtime.routes(), "/+replication/v1/ready").await;
    assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE);
    assert_eq!(document["reasons"], serde_json::json!(["frontier_lag"]));
}

#[tokio::test]
async fn replica_status_filters_fields_by_audience() {
    let (dir, state) = state();
    state.serving.meta.claim_writer_identity("writer-a").unwrap();
    let config = replica_config(&dir, "http://127.0.0.1:1");
    let operator = runtime_with_audience(&config, &state, AvailabilityAudience::Operator).unwrap();
    let administrator = runtime_with_audience(&config, &state, AvailabilityAudience::Administrator).unwrap();

    assert_eq!(operator.metrics().len(), 3);

    let (_, operator_document) = get(&operator.routes(), "/+replication/v1/health").await;
    let (_, administrator_document) = get(&administrator.routes(), "/+replication/v1/health").await;

    assert!(operator_document.get("serial").is_some());
    assert_eq!(operator_document["readable_serial"], 0);
    assert!(operator_document.get("upstream").is_none());
    assert_eq!(administrator_document["upstream"], "http://127.0.0.1:1/");
}

#[tokio::test]
async fn replica_cycle_records_an_incompatible_schema() {
    let page = ChangePage {
        version: PROTOCOL_VERSION + 1,
        source: "writer-a".to_owned(),
        after: 0,
        current_serial: 1,
        changes: Vec::new(),
    };
    let server = TestServer::start(Router::new().route(
        "/+replication/v1/changes",
        route_get(move || {
            let page = page.clone();
            async move { axum::Json(page) }
        }),
    ))
    .await;
    let (dir, state) = state();
    state.serving.meta.claim_writer_identity("writer-a").unwrap();
    let mut runtime = runtime(&replica_config(&dir, &server.url), &state).unwrap();

    assert_eq!(sync_cycle(&mut runtime).await, Some(true));
    let (_, document) = get(&runtime.routes(), "/+replication/v1/ready").await;
    assert_eq!(
        document["reasons"],
        serde_json::json!(["incompatible_schema", "retired_peers", "frontier_lag"])
    );
}

async fn sync_cycle(runtime: &mut DistributedRuntime) -> Option<bool> {
    let (replica, _) = runtime.replica.as_mut()?;
    Some(replica.cycle().await.unwrap_or(true))
}
