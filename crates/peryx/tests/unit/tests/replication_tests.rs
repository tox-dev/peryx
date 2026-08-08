use std::collections::{BTreeMap, BTreeSet};
use std::num::{NonZeroU32, NonZeroUsize};
use std::time::Duration;

use axum::body::Body;
use axum::http::{Request, StatusCode, header};
use axum::routing::get as route_get;
use axum::{Json, Router};
use base64::Engine as _;
use base64::engine::general_purpose::STANDARD;
use http_body_util::BodyExt as _;
use peryx_driver::IndexKind as RuntimeIndexKind;
use peryx_driver::state::AppState;
use peryx_ha_distributed::{
    BLOB_VIEW, ChangePage, DEFAULT_RECONNECT_POLICY, ReconnectPolicy, SyncOutcome, TransportError, primary_router,
};
use peryx_identity::{Action, GrantScope, Role};
use peryx_storage::blob::{BlobStore, Digest};
use peryx_storage::meta::MetaStore;
use rstest::rstest;
use tower::ServiceExt as _;

use crate::config::{
    AvailabilityConfig, Config, DcMember, DcMembership, DcRole, IndexKind, ReplicationConfig, SecretSource,
    TokenConfig, UpstreamConfig, UpstreamRoutingConfig, WebhookConfig, WebhookSecret,
};
use crate::replication::{ReplicationRuntime, schedule_delay};
use crate::server::{build_router, build_state, router_for};

const TOKEN: &str = "replica-secret";
const WRITER_IDENTITY: &str = "writer-a";

struct TestServer {
    url: String,
    task: tokio::task::JoinHandle<()>,
}

impl TestServer {
    async fn start(router: Router) -> Self {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let task = tokio::spawn(async move {
            axum::serve(listener, router).await.unwrap();
        });
        Self {
            url: format!("http://{address}/"),
            task,
        }
    }
}

impl Drop for TestServer {
    fn drop(&mut self) {
        self.task.abort();
    }
}

fn config(dir: &tempfile::TempDir, replication: Option<ReplicationConfig>) -> Config {
    let replica = matches!(replication, Some(ReplicationConfig::Replica { .. }));
    if replica {
        MetaStore::open(dir.path().join("peryx.redb"))
            .unwrap()
            .claim_writer_identity(WRITER_IDENTITY)
            .unwrap();
    }
    Config {
        data_dir: dir.path().to_path_buf(),
        writer_identity: replica.then(|| WRITER_IDENTITY.to_owned()),
        availability: replication.map_or(AvailabilityConfig::None, AvailabilityConfig::Dc),
        ..Config::default()
    }
}

fn replica_config(upstream: &str, page_size: usize) -> ReplicationConfig {
    ReplicationConfig::Replica {
        upstream: upstream.to_owned(),
        token: SecretSource::Literal(TOKEN.to_owned()),
        poll_interval: Duration::from_millis(1),
        page_size: NonZeroUsize::new(page_size).unwrap(),
    }
}

fn primary_config() -> ReplicationConfig {
    ReplicationConfig::Primary {
        source: "primary-a".to_owned(),
        token: SecretSource::Literal(TOKEN.to_owned()),
    }
}

#[tokio::test]
async fn test_ignite_consensus_forms_no_group_outside_an_ha_roster() {
    let dir = tempfile::tempdir().unwrap();
    let config = config(&dir, Some(primary_config()));
    let state = build_state(&config).unwrap();
    let replication = ReplicationRuntime::new(&config, &state).unwrap();

    assert!(replication.ignite_consensus().await.unwrap().is_none());
}

#[test]
fn test_a_dc_node_registers_the_durability_metric() {
    let dir = tempfile::tempdir().unwrap();
    let config = config(&dir, Some(primary_config()));
    let state = build_state(&config).unwrap();
    let _replication = ReplicationRuntime::new(&config, &state).unwrap();

    let mut body = String::new();
    state.write_process_metrics(&mut body);
    assert!(
        body.contains("peryx_dc_ack_durable_total"),
        "a dc node exposes the datacenter durability outcome metric: {body}"
    );
}

#[test]
fn test_a_none_node_registers_no_durability_metric() {
    let dir = tempfile::tempdir().unwrap();
    let config = config(&dir, None);
    let state = build_state(&config).unwrap();
    assert!(ReplicationRuntime::from_config(&config, &state).unwrap().is_none());

    let mut body = String::new();
    state.write_process_metrics(&mut body);
    assert!(
        !body.contains("peryx_dc_ack"),
        "a single-node none process runs no datacenter durability decision: {body}"
    );
}

fn ha_group_config(dir: &tempfile::TempDir) -> Config {
    Config {
        data_dir: dir.path().to_path_buf(),
        writer_identity: Some(WRITER_IDENTITY.to_owned()),
        node_identity: Some(WRITER_IDENTITY.to_owned()),
        availability: AvailabilityConfig::Ha(primary_config()),
        dc_membership: Some(DcMembership {
            group: "ownership".to_owned(),
            members: vec![DcMember {
                node: WRITER_IDENTITY.to_owned(),
                dc: "east".to_owned(),
                address: "http://127.0.0.1:4461".to_owned(),
                role: DcRole::Writer,
            }],
        }),
        ..Config::default()
    }
}

#[tokio::test]
async fn test_ignite_consensus_registers_the_group_for_the_mutation_path() {
    use peryx_driver::state::HomeClaim;

    let dir = tempfile::tempdir().unwrap();
    let config = ha_group_config(&dir);
    let state = build_state(&config).unwrap();
    let replication = ReplicationRuntime::new(&config, &state).unwrap();

    let consensus = replication
        .ignite_consensus()
        .await
        .unwrap()
        .expect("an ha roster ignites a consensus group");
    state.set_ownership_authority(consensus.authority);

    // The mutation path reaches the same group through the state. The lone voter elects itself, so a
    // first claim eventually assigns the home here rather than forwarding.
    let authority = state.ownership_authority().expect("the group is registered").clone();
    let mut claim = None;
    for _ in 0..50 {
        if let Ok(outcome) = authority.claim_home("proj").await {
            claim = Some(outcome);
            break;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    assert_eq!(claim, Some(HomeClaim::AssignedHere));
}

#[tokio::test]
async fn test_ignite_consensus_surfaces_a_store_failure() {
    let dir = tempfile::tempdir().unwrap();
    let config = ha_group_config(&dir);
    let state = build_state(&config).unwrap();
    let replication = ReplicationRuntime::new(&config, &state).unwrap();
    // A file where the consensus log directory belongs makes ignition fail, and the error propagates
    // rather than being swallowed.
    std::fs::write(dir.path().join("raft"), b"not a directory").unwrap();

    assert!(replication.ignite_consensus().await.is_err());
}

#[tokio::test]
async fn test_build_state_projects_the_configured_dc_topology() {
    let dir = tempfile::tempdir().unwrap();
    let config = Config {
        data_dir: dir.path().to_path_buf(),
        writer_identity: Some(WRITER_IDENTITY.to_owned()),
        availability: AvailabilityConfig::Dc(primary_config()),
        dc_membership: Some(DcMembership {
            group: "east".to_owned(),
            members: vec![
                DcMember {
                    node: WRITER_IDENTITY.to_owned(),
                    dc: "east-1".to_owned(),
                    address: "10.0.0.1:8080".to_owned(),
                    role: DcRole::Writer,
                },
                DcMember {
                    node: "replica-b".to_owned(),
                    dc: "east-2".to_owned(),
                    address: "10.0.0.2:8080".to_owned(),
                    role: DcRole::Replica,
                },
            ],
        }),
        ..Config::default()
    };

    let topology = build_state(&config).unwrap().availability_topology().clone();

    assert_eq!(topology.mode, peryx_core::TopologyMode::Dc);
    assert_eq!(topology.group.as_deref(), Some("east"));
    assert_eq!(topology.local_node.as_deref(), Some(WRITER_IDENTITY));
    let roles = topology
        .members
        .iter()
        .map(|member| (member.node.as_str(), member.role))
        .collect::<Vec<_>>();
    assert_eq!(
        roles,
        vec![
            (WRITER_IDENTITY, peryx_core::NodeRole::Writer),
            ("replica-b", peryx_core::NodeRole::Replica),
        ],
    );
    assert_eq!(topology.members[0].address, "10.0.0.1:8080");
}

#[tokio::test]
async fn test_build_state_derives_the_writer_role_for_a_read_only_primary() {
    let dir = tempfile::tempdir().unwrap();
    MetaStore::open(dir.path().join("peryx.redb"))
        .unwrap()
        .claim_writer_identity(WRITER_IDENTITY)
        .unwrap();
    let config = Config {
        data_dir: dir.path().to_path_buf(),
        writer_identity: Some(WRITER_IDENTITY.to_owned()),
        availability: AvailabilityConfig::Dc(primary_config()),
        read_only: true,
        ..Config::default()
    };

    let state = build_state(&config).unwrap();

    assert!(state.read_only, "the primary is configured read-only");
    assert_eq!(
        state.availability_role(),
        peryx_core::NodeRole::Writer,
        "a read-only primary holds write authority, so the topology self-role agrees with the \
         listener and replication surfaces that read it as the writer",
    );
}

#[tokio::test]
async fn test_build_state_derives_the_replica_role_for_a_configured_replica() {
    let dir = tempfile::tempdir().unwrap();
    let config = config(&dir, Some(replica_config("http://primary-a/", 16)));

    let state = build_state(&config).unwrap();

    assert_eq!(state.availability_role(), peryx_core::NodeRole::Replica);
}

fn primary_stores() -> (tempfile::TempDir, MetaStore, BlobStore) {
    let dir = tempfile::tempdir().unwrap();
    let meta = MetaStore::open(dir.path().join("peryx.redb")).unwrap();
    let blobs = BlobStore::new(dir.path().join("blobs"));
    (dir, meta, blobs)
}

async fn get(router: &Router, path: &str) -> (StatusCode, Vec<u8>) {
    get_as(router, path, None).await
}

async fn get_as(router: &Router, path: &str, credentials: Option<&str>) -> (StatusCode, Vec<u8>) {
    let mut request = Request::get(path);
    if let Some(credentials) = credentials {
        request = request.header(header::AUTHORIZATION, format!("Basic {}", STANDARD.encode(credentials)));
    }
    let response = router
        .clone()
        .oneshot(request.body(Body::empty()).unwrap())
        .await
        .unwrap();
    let status = response.status();
    let body = response.into_body().collect().await.unwrap().to_bytes().to_vec();
    (status, body)
}

async fn document(router: &Router, path: &str, credentials: Option<&str>) -> (StatusCode, serde_json::Value) {
    let (status, body) = get_as(router, path, credentials).await;
    (status, serde_json::from_slice(&body).unwrap())
}

const PASSWORD: &str = "local availability password";

async fn credential(state: &AppState, name: &str, role: Role) -> String {
    let user = state.users.create(name).unwrap();
    state.users.set_password(&user.id, PASSWORD).await.unwrap();
    state.authorization.grant(&user.id, role, GrantScope::Server).unwrap();
    format!("{name}:{PASSWORD}")
}

/// A stand-in primary that always answers `changes` with a page tagged an unsupported protocol
/// version, so a replica polling it records a schema fault rather than a transport failure.
async fn incompatible_primary() -> TestServer {
    let page = ChangePage {
        version: u16::MAX,
        source: "primary-a".to_owned(),
        after: 0,
        current_serial: 1,
        changes: Vec::new(),
    };
    let handler = route_get(move || {
        let page = page.clone();
        async move { Json(page) }
    });
    TestServer::start(Router::new().route("/+replication/v1/changes", handler)).await
}

#[tokio::test]
async fn test_primary_runtime_mounts_authenticated_routes() {
    let dir = tempfile::tempdir().unwrap();
    let config = config(&dir, Some(primary_config()));
    let router = build_router(&config).unwrap();

    let response = router
        .oneshot(
            Request::get("/+replication/v1/changes?after=0&limit=10")
                .header(header::AUTHORIZATION, format!("Bearer {TOKEN}"))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    let status = response.status();
    let page = serde_json::from_slice::<ChangePage>(&response.into_body().collect().await.unwrap().to_bytes()).unwrap();

    assert_eq!(status, StatusCode::OK);
    assert_eq!(page.source, "primary-a");
}

#[tokio::test]
async fn test_replica_runtime_drains_available_pages() {
    let (_primary_dir, primary_meta, primary_blobs) = primary_stores();
    primary_meta
        .commit_driver_txn(|_| {
            Ok::<_, peryx_storage::meta::MetaError>(((), vec![b"one".to_vec(), b"two".to_vec(), b"three".to_vec()]))
        })
        .unwrap();
    let server = TestServer::start(primary_router("primary-a", TOKEN, primary_meta, primary_blobs).unwrap()).await;
    let replica_dir = tempfile::tempdir().unwrap();
    let config = config(&replica_dir, Some(replica_config(&server.url, 1)));
    let state = build_state(&config).unwrap();
    let mut runtime = ReplicationRuntime::new(&config, &state).unwrap();

    assert!(runtime.is_replica());
    let subscriber = tracing_subscriber::fmt().with_writer(std::io::sink).finish();
    let guard = tracing::subscriber::set_default(subscriber);
    assert_eq!(runtime.sync_cycle().await, Some(false));
    drop(guard);

    let router = runtime.mount(router_for(state.clone()));
    let availability = runtime.start().unwrap().unwrap();
    let deadline = tokio::time::sleep(Duration::from_secs(10));
    tokio::pin!(deadline);
    loop {
        if get(&router, "/+replication/v1/ready").await.0 == StatusCode::OK {
            break;
        }
        tokio::select! {
            () = &mut deadline => panic!(
                "replica runtime did not drain pages; current serial is {}",
                state.meta.current_serial().unwrap()
            ),
            () = tokio::time::sleep(Duration::from_millis(5)) => {}
        }
    }
    drop(availability);

    assert_eq!(state.meta.journal_after(0, 10).unwrap().len(), 3);
}

#[tokio::test]
async fn test_a_replica_serves_the_change_feed_it_has_applied() {
    let (_primary_dir, primary_meta, primary_blobs) = primary_stores();
    primary_meta
        .commit_driver_txn(|_| Ok::<_, peryx_storage::meta::MetaError>(((), vec![b"one".to_vec(), b"two".to_vec()])))
        .unwrap();
    let server = TestServer::start(primary_router("writer-a", TOKEN, primary_meta, primary_blobs).unwrap()).await;
    let replica_dir = tempfile::tempdir().unwrap();
    let config = config(&replica_dir, Some(replica_config(&server.url, 10)));
    let state = build_state(&config).unwrap();
    let mut runtime = ReplicationRuntime::new(&config, &state).unwrap();
    assert_eq!(runtime.sync_cycle().await, Some(true));

    // The replica now mounts a follower change-feed. Pull it and confirm it relays the writer's stream.
    let router = runtime.mount(Router::new());
    let request = Request::get("/+replication/v1/changes?after=0&limit=10")
        .header(header::AUTHORIZATION, format!("Bearer {TOKEN}"))
        .body(Body::empty())
        .unwrap();
    let response = router.clone().oneshot(request).await.unwrap();
    let status = response.status();
    let page = serde_json::from_slice::<ChangePage>(&response.into_body().collect().await.unwrap().to_bytes()).unwrap();

    assert_eq!(status, StatusCode::OK);
    // The replica stamps the writer's authoritative source, not its own identity, and serves only up to
    // what it durably applied.
    assert_eq!(page.source, "writer-a");
    assert_eq!(page.current_serial, 2);
    assert_eq!(
        page.changes.iter().map(|change| change.serial).collect::<Vec<_>>(),
        vec![1, 2]
    );
}

#[tokio::test]
async fn test_a_replica_reports_the_pass_done_when_no_metadata_peer_answers() {
    // No server is listening, so the sole configured peer refuses and no peer answers the round. The
    // cycle reports the loss for a retry rather than advancing.
    let replica_dir = tempfile::tempdir().unwrap();
    let config = config(&replica_dir, Some(replica_config("http://127.0.0.1:1/", 10)));
    let state = build_state(&config).unwrap();
    let mut runtime = ReplicationRuntime::new(&config, &state).unwrap();

    let subscriber = tracing_subscriber::fmt().with_writer(std::io::sink).finish();
    let guard = tracing::subscriber::set_default(subscriber);
    assert_eq!(runtime.sync_cycle().await, Some(true));
    drop(guard);

    assert_eq!(state.meta.current_serial().unwrap(), 0);
}

#[tokio::test]
async fn test_a_replica_cycle_records_an_apply_failure_when_the_store_refuses_the_write() {
    let (_primary_dir, primary_meta, primary_blobs) = primary_stores();
    primary_meta
        .commit_driver_txn(|_| Ok::<_, peryx_storage::meta::MetaError>(((), vec![b"one".to_vec()])))
        .unwrap();
    let server = TestServer::start(primary_router("primary-a", TOKEN, primary_meta, primary_blobs).unwrap()).await;
    // A read-only replica store answers every read the cycle makes yet refuses the apply write, the one
    // path where a page arrives and validates but committing it fails, so pull_round surfaces the apply
    // error rather than a transport loss.
    let replica_dir = tempfile::tempdir().unwrap();
    let config = config(&replica_dir, Some(replica_config(&server.url, 10)));
    let meta = MetaStore::open_existing_read_only(replica_dir.path().join("peryx.redb")).unwrap();
    let state = std::sync::Arc::new(AppState::new(
        meta,
        BlobStore::new(replica_dir.path().join("blobs")),
        60,
        Vec::new(),
    ));
    let mut runtime = ReplicationRuntime::new(&config, &state).unwrap();

    let subscriber = tracing_subscriber::fmt().with_writer(std::io::sink).finish();
    let guard = tracing::subscriber::set_default(subscriber);
    // The cycle records the apply failure and reports the pass done: re-fetching the same cursor at the
    // poll cadence is the recovery, not a backoff.
    assert_eq!(runtime.sync_cycle().await, Some(true));
    drop(guard);

    // The refused write left the journal at zero, and the metadata error is on the metrics.
    assert_eq!(state.meta.current_serial().unwrap(), 0);
    let router = runtime.mount(router_for(state.clone()));
    let (_, body) = get(&router, "/metrics").await;
    let body = String::from_utf8(body).unwrap();
    assert!(body.contains("peryx_ha_distributed_sync_errors_total 1\n"), "{body}");
}

#[tokio::test]
async fn test_replica_runtime_copies_primary_metadata() {
    let (_primary_dir, primary_meta, primary_blobs) = primary_stores();
    primary_meta
        .commit_driver_txn(|txn| {
            txn.put("pypi\0upload", b"record")?;
            Ok::<_, peryx_storage::meta::MetaError>(((), vec![b"upload".to_vec()]))
        })
        .unwrap();
    let server = TestServer::start(primary_router("primary-a", TOKEN, primary_meta, primary_blobs).unwrap()).await;
    let replica_dir = tempfile::tempdir().unwrap();
    let config = config(&replica_dir, Some(replica_config(&server.url, 10)));
    let state = build_state(&config).unwrap();
    let mut runtime = ReplicationRuntime::new(&config, &state).unwrap();

    assert_eq!(runtime.sync_cycle().await, Some(true));
    assert_eq!(
        state.meta.get_driver_value("pypi\0upload").unwrap().as_deref(),
        Some(b"record".as_slice())
    );
}

#[tokio::test]
async fn test_replica_dispatches_applied_keys_to_ecosystem_drivers() {
    let (_primary_dir, primary_meta, primary_blobs) = primary_stores();
    primary_meta
        .commit_driver_txn(|txn| {
            txn.put("pypi\0p\0hosted/flask", b"Flask")?;
            Ok::<_, peryx_storage::meta::MetaError>(((), vec![b"upload".to_vec()]))
        })
        .unwrap();
    let server = TestServer::start(primary_router("primary-a", TOKEN, primary_meta, primary_blobs).unwrap()).await;
    let replica_dir = tempfile::tempdir().unwrap();
    let config = config(&replica_dir, Some(replica_config(&server.url, 10)));
    let state = build_state(&config).unwrap();
    // A cached hosted page whose project the applied marker names, so the driver's invalidation shows.
    let hot = state.hot_key("hosted", "flask", "simple.html");
    state
        .cache
        .store_hot(hot.clone(), axum::body::Bytes::from_static(b"x"), i64::MAX);
    let mut runtime = ReplicationRuntime::new(&config, &state).unwrap();

    assert_eq!(runtime.sync_cycle().await, Some(true));

    // The replica handed the applied project key to the PyPI driver, which retired flask's pages by
    // advancing their epoch; the OCI driver's default hook ignored the key.
    assert_ne!(
        state.hot_key("hosted", "flask", "simple.html"),
        hot,
        "the replica dispatched the change to the ecosystem driver"
    );
}

#[test]
fn test_apply_replicated_page_dispatches_a_changed_page_to_drivers() {
    let dir = tempfile::tempdir().unwrap();
    let config = config(&dir, None);
    let state = build_state(&config).unwrap();
    let hot = state.hot_key("hosted", "flask", "simple.html");
    state
        .cache
        .store_hot(hot.clone(), axum::body::Bytes::from_static(b"x"), i64::MAX);

    // Synchronous and independent of the async sync loop, so this covers the dispatch every run.
    crate::replication::apply_replicated_page(
        &state,
        SyncOutcome {
            changes: 1,
            serial: 1,
            primary_serial: 1,
        },
        &["pypi\u{0}p\u{0}hosted/flask".to_owned()],
    );

    assert_ne!(
        state.hot_key("hosted", "flask", "simple.html"),
        hot,
        "a page with changes reached the PyPI driver, which retired flask's pages",
    );
    assert_eq!(
        state.meta.view_frontier(peryx_driver::state::SEARCH_VIEW).unwrap(),
        Some(1),
        "every affected view rebuilt, so the search view frontier advanced over the applied serial",
    );
}

#[test]
fn test_apply_replicated_page_holds_the_frontier_when_a_view_rebuild_fails() {
    let dir = tempfile::tempdir().unwrap();
    let config = config(&dir, None);
    let state = build_state(&config).unwrap();
    // A corrupt upload record for flask makes deriving its search document fail, so the driver reports a
    // blocked view rather than rebuilding it.
    peryx_ecosystem_pypi::store::put_upload(
        &state.meta,
        "hosted",
        "flask",
        "flask-1.0-py3-none-any.whl",
        b"not json",
    )
    .unwrap();

    crate::replication::apply_replicated_page(
        &state,
        SyncOutcome {
            changes: 1,
            serial: 1,
            primary_serial: 1,
        },
        &["pypi\u{0}u\u{0}hosted/flask/flask-1.0-py3-none-any.whl".to_owned()],
    );

    assert_eq!(
        state.meta.view_frontier(peryx_driver::state::SEARCH_VIEW).unwrap(),
        None,
        "a required view that could not rebuild holds the frontier at its prior value",
    );
}

#[test]
fn test_apply_replicated_page_holds_the_frontier_when_recording_it_fails() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("meta.redb");
    MetaStore::open(&path).unwrap();
    // A read-only store lets every derived view rebuild yet refuses the frontier write, the one path
    // where no driver blocked but recording the applied serial still fails.
    let meta = MetaStore::open_existing_read_only(&path).unwrap();
    let state = AppState::new(meta, BlobStore::new(dir.path().join("blobs")), 60, Vec::new());

    crate::replication::apply_replicated_page(
        &state,
        SyncOutcome {
            changes: 1,
            serial: 1,
            primary_serial: 1,
        },
        &[],
    );

    assert_eq!(
        state.meta.view_frontier(peryx_driver::state::SEARCH_VIEW).unwrap(),
        None,
        "a failed frontier write leaves the readable frontier where it was rather than crashing",
    );
}

#[test]
fn test_apply_replicated_page_ignores_a_page_with_no_changes() {
    let dir = tempfile::tempdir().unwrap();
    let config = config(&dir, None);
    let state = build_state(&config).unwrap();
    let hot = state.hot_key("hosted", "flask", "simple.html");
    state
        .cache
        .store_hot(hot.clone(), axum::body::Bytes::from_static(b"x"), i64::MAX);

    crate::replication::apply_replicated_page(
        &state,
        SyncOutcome {
            changes: 0,
            serial: 0,
            primary_serial: 0,
        },
        &["pypi\u{0}p\u{0}hosted/flask".to_owned()],
    );

    assert_eq!(
        state.hot_key("hosted", "flask", "simple.html"),
        hot,
        "a page with no changes dispatched nothing",
    );
}

#[tokio::test]
async fn test_replica_runtime_copies_primary_blobs() {
    let (_primary_dir, primary_meta, primary_blobs) = primary_stores();
    let digest = primary_blobs.write(b"artifact").unwrap();
    primary_meta
        .commit_driver_txn(|txn| {
            txn.reference_blob(digest.as_str(), 8);
            Ok::<_, peryx_storage::meta::MetaError>(((), vec![b"upload".to_vec()]))
        })
        .unwrap();
    let server = TestServer::start(primary_router("primary-a", TOKEN, primary_meta, primary_blobs).unwrap()).await;
    let replica_dir = tempfile::tempdir().unwrap();
    let config = config(&replica_dir, Some(replica_config(&server.url, 10)));
    let state = build_state(&config).unwrap();
    let mut runtime = ReplicationRuntime::new(&config, &state).unwrap();

    assert_eq!(runtime.sync_cycle().await, Some(true));
    assert_eq!(state.blobs.read_bytes(&digest, 8).await.unwrap(), b"artifact");
}

#[tokio::test]
async fn test_dual_replica_advances_both_planes_when_the_blob_is_available() {
    let (_primary_dir, primary_meta, primary_blobs) = primary_stores();
    let digest = primary_blobs.write(b"artifact").unwrap();
    primary_meta
        .commit_driver_txn(|txn| {
            txn.reference_blob(digest.as_str(), 8);
            Ok::<_, peryx_storage::meta::MetaError>(((), vec![b"upload".to_vec()]))
        })
        .unwrap();
    let server = TestServer::start(primary_router("primary-a", TOKEN, primary_meta, primary_blobs).unwrap()).await;
    let replica_dir = tempfile::tempdir().unwrap();
    let config = config(&replica_dir, Some(replica_config(&server.url, 10)));
    let state = build_state(&config).unwrap();
    let mut runtime = ReplicationRuntime::new(&config, &state).unwrap();

    assert_eq!(runtime.sync_cycle().await, Some(true));

    // The metadata plane committed the serial and the blob plane pulled its bytes and advanced its
    // frontier to match, so a reader gated on both views sees the record fully byte-backed.
    assert_eq!(state.meta.current_serial().unwrap(), 1);
    assert_eq!(state.blobs.read_bytes(&digest, 8).await.unwrap(), b"artifact");
    assert_eq!(state.meta.view_frontier(BLOB_VIEW).unwrap(), Some(1));
}

#[tokio::test]
async fn test_dual_replica_reports_blob_fetch_counts_to_operators() {
    let (_primary_dir, primary_meta, primary_blobs) = primary_stores();
    let digest = primary_blobs.write(b"artifact").unwrap();
    primary_meta
        .commit_driver_txn(|txn| {
            txn.reference_blob(digest.as_str(), 8);
            Ok::<_, peryx_storage::meta::MetaError>(((), vec![b"upload".to_vec()]))
        })
        .unwrap();
    let server = TestServer::start(primary_router("primary-a", TOKEN, primary_meta, primary_blobs).unwrap()).await;
    let replica_dir = tempfile::tempdir().unwrap();
    let config = config(&replica_dir, Some(replica_config(&server.url, 10)));
    let state = build_state(&config).unwrap();
    let mut runtime = ReplicationRuntime::new(&config, &state).unwrap();
    let router = runtime.mount(router_for(state));

    assert_eq!(runtime.sync_cycle().await, Some(true));

    // The blob plane pulled the one referenced blob and left nothing outstanding, so a scrape shows the
    // pull without waiting for the readable frontier to lag.
    let (_, body) = get(&router, "/metrics").await;
    let body = String::from_utf8(body).unwrap();
    assert!(body.contains("peryx_ha_distributed_blobs_fetched_total 1\n"), "{body}");
    assert!(body.contains("peryx_ha_distributed_blobs_pending 0\n"), "{body}");
}

#[tokio::test]
async fn test_dual_replica_advances_metadata_while_a_missing_blob_holds_the_blob_frontier() {
    let (_primary_dir, primary_meta, primary_blobs) = primary_stores();
    // Reference a blob the primary never stored, so serving it 404s and the blob plane cannot advance.
    let digest = Digest::of(b"artifact");
    primary_meta
        .commit_driver_txn(|txn| {
            txn.put("pypi\0upload", b"record")?;
            txn.reference_blob(digest.as_str(), 8);
            Ok::<_, peryx_storage::meta::MetaError>(((), vec![b"upload".to_vec()]))
        })
        .unwrap();
    let server = TestServer::start(primary_router("primary-a", TOKEN, primary_meta, primary_blobs).unwrap()).await;
    let replica_dir = tempfile::tempdir().unwrap();
    let config = config(&replica_dir, Some(replica_config(&server.url, 10)));
    let state = build_state(&config).unwrap();
    let mut runtime = ReplicationRuntime::new(&config, &state).unwrap();

    let subscriber = tracing_subscriber::fmt().with_writer(std::io::sink).finish();
    let guard = tracing::subscriber::set_default(subscriber);
    assert_eq!(runtime.sync_cycle().await, Some(true));
    drop(guard);

    // The metadata plane committed the record even though its blob never arrived...
    assert_eq!(
        state.meta.get_driver_value("pypi\0upload").unwrap().as_deref(),
        Some(b"record".as_slice())
    );
    assert_eq!(state.meta.current_serial().unwrap(), 1);
    // ...while the blob frontier stays put, so a reader gated on the blob view never sees the serial.
    assert!(state.blobs.head(&digest).await.unwrap().is_none());
    assert_eq!(state.meta.view_frontier(BLOB_VIEW).unwrap(), None);
}

#[tokio::test]
async fn test_dual_replica_heals_the_blob_frontier_after_the_blob_arrives() {
    let (_primary_dir, primary_meta, primary_blobs) = primary_stores();
    let digest = Digest::of(b"artifact");
    primary_meta
        .commit_driver_txn(|txn| {
            txn.reference_blob(digest.as_str(), 8);
            Ok::<_, peryx_storage::meta::MetaError>(((), vec![b"upload".to_vec()]))
        })
        .unwrap();
    let server =
        TestServer::start(primary_router("primary-a", TOKEN, primary_meta, primary_blobs.clone()).unwrap()).await;
    let replica_dir = tempfile::tempdir().unwrap();
    let config = config(&replica_dir, Some(replica_config(&server.url, 10)));
    let state = build_state(&config).unwrap();
    let mut runtime = ReplicationRuntime::new(&config, &state).unwrap();

    let subscriber = tracing_subscriber::fmt().with_writer(std::io::sink).finish();
    let guard = tracing::subscriber::set_default(subscriber);
    // The first pass commits metadata, but the blob is not on the primary yet, so its frontier holds.
    assert_eq!(runtime.sync_cycle().await, Some(true));
    assert_eq!(state.meta.view_frontier(BLOB_VIEW).unwrap(), None);

    // The blob lands on the primary; the next pass re-derives the outstanding set from the tail, pulls
    // it, and advances the blob frontier with no new metadata to apply.
    assert_eq!(primary_blobs.write(b"artifact").unwrap(), digest);
    assert_eq!(runtime.sync_cycle().await, Some(true));
    drop(guard);

    assert_eq!(state.blobs.read_bytes(&digest, 8).await.unwrap(), b"artifact");
    assert_eq!(state.meta.view_frontier(BLOB_VIEW).unwrap(), Some(1));
}

/// Advertise a verified placement of `digest` in datacenter `dc`, the descriptor a replica reads to decide
/// a blob lives on a peer.
fn seed_remote_placement(meta: &MetaStore, digest: &Digest, dc: &str, size: u64) {
    use peryx_identity::ArtifactDigest;
    use peryx_storage::meta::{BackendId, BackendLocation, BlobPlacementKey, BlobPlacementTransition, DataCenterId};

    let artifact = ArtifactDigest::from_sha256(digest.as_str()).unwrap();
    let key = BlobPlacementKey {
        digest: artifact.clone(),
        backend: BackendId::new("filesystem").unwrap(),
        data_center: DataCenterId::new(dc).unwrap(),
        location: BackendLocation::new(format!("filesystem/{}", digest.as_str())).unwrap(),
    };
    meta.apply_blob_placement(&key, &BlobPlacementTransition::Stage, 1, 10)
        .unwrap();
    meta.apply_blob_placement(
        &key,
        &BlobPlacementTransition::Verify {
            observed: artifact,
            size,
        },
        1,
        20,
    )
    .unwrap();
}

/// A replica config that resolves its own `dc-a` from `node_identity` and reaches the writer's `dc-b` as a
/// remote peer, so a blob placed only in `dc-b` is deferred to read-through.
fn cross_dc_replica_config(dir: &tempfile::TempDir, upstream: &str) -> Config {
    MetaStore::open(dir.path().join("peryx.redb"))
        .unwrap()
        .claim_writer_identity(WRITER_IDENTITY)
        .unwrap();
    Config {
        data_dir: dir.path().to_path_buf(),
        writer_identity: Some(WRITER_IDENTITY.to_owned()),
        node_identity: Some("replica-a".to_owned()),
        availability: AvailabilityConfig::Dc(replica_config(upstream, 10)),
        dc_membership: Some(DcMembership {
            group: "g".to_owned(),
            members: vec![
                DcMember {
                    node: "replica-a".to_owned(),
                    dc: "dc-a".to_owned(),
                    address: "http://replica-a.invalid:8080".to_owned(),
                    role: DcRole::Replica,
                },
                DcMember {
                    node: WRITER_IDENTITY.to_owned(),
                    dc: "dc-b".to_owned(),
                    address: upstream.to_owned(),
                    role: DcRole::Writer,
                },
            ],
        }),
        ..Config::default()
    }
}

#[tokio::test]
async fn test_dual_replica_defers_a_peer_held_blob_to_read_through() {
    let (_primary_dir, primary_meta, primary_blobs) = primary_stores();
    // The primary holds the blob, so a whole-pull would succeed; deferral must skip it regardless.
    let digest = primary_blobs.write(b"artifact").unwrap();
    primary_meta
        .commit_driver_txn(|txn| {
            txn.reference_blob(digest.as_str(), 8);
            Ok::<_, peryx_storage::meta::MetaError>(((), vec![b"upload".to_vec()]))
        })
        .unwrap();
    let server = TestServer::start(primary_router("primary-a", TOKEN, primary_meta, primary_blobs).unwrap()).await;
    let replica_dir = tempfile::tempdir().unwrap();
    let config = cross_dc_replica_config(&replica_dir, &server.url);
    let state = build_state(&config).unwrap();
    // The placement descriptor names peer dc-b, none in the local dc-a, as if it rode in the replicated page.
    seed_remote_placement(&state.meta, &digest, "dc-b", 8);
    let mut runtime = ReplicationRuntime::new(&config, &state).unwrap();

    assert_eq!(runtime.sync_cycle().await, Some(true));

    // The metadata committed, but the peer-held blob was left absent for read-through rather than pulled...
    assert_eq!(state.meta.current_serial().unwrap(), 1);
    assert!(state.blobs.head(&digest).await.unwrap().is_none());
    // ...and its serial is still readable, since the blob frontier advances on the peer placement alone.
    assert_eq!(state.meta.view_frontier(BLOB_VIEW).unwrap(), Some(1));
}

#[tokio::test]
async fn test_dual_replica_retries_after_a_metadata_sync_error() {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}/", listener.local_addr().unwrap());
    // Hold the port and reset every connection so the replica always sees a transport failure. Dropping
    // the listener would free the port for a parallel test's mock primary to rebind and answer, flaking
    // the retry into a different sync outcome.
    let _reset = tokio::spawn(async move {
        while let Ok((stream, _)) = listener.accept().await {
            drop(stream);
        }
    });
    let dir = tempfile::tempdir().unwrap();
    let config = config(&dir, Some(replica_config(&url, 10)));
    let state = build_state(&config).unwrap();
    let mut runtime = ReplicationRuntime::new(&config, &state).unwrap();

    let subscriber = tracing_subscriber::fmt().with_writer(std::io::sink).finish();
    let guard = tracing::subscriber::set_default(subscriber);
    // The metadata plane cannot reach the primary, so the cycle records the error and asks to retry
    // without advancing either frontier.
    assert_eq!(runtime.sync_cycle().await, Some(true));
    drop(guard);

    assert_eq!(state.meta.current_serial().unwrap(), 0);
    assert_eq!(state.meta.view_frontier(BLOB_VIEW).unwrap(), None);
}

#[tokio::test]
async fn test_replica_requires_the_blob_view() {
    let replica_dir = tempfile::tempdir().unwrap();
    let config = config(&replica_dir, Some(replica_config("https://primary.example/", 10)));
    let state = build_state(&config).unwrap();

    assert_eq!(
        &*state.serving.required_views,
        [peryx_driver::state::SEARCH_VIEW, BLOB_VIEW].as_slice()
    );
}

#[tokio::test]
async fn test_replica_runtime_forwards_blobs_to_a_follower() {
    let (_primary_dir, primary_meta, primary_blobs) = primary_stores();
    let digest = primary_blobs.write(b"artifact").unwrap();
    primary_meta
        .commit_driver_txn(|txn| {
            txn.reference_blob(digest.as_str(), 8);
            Ok::<_, peryx_storage::meta::MetaError>(((), vec![b"upload".to_vec()]))
        })
        .unwrap();
    let primary = TestServer::start(primary_router("primary-a", TOKEN, primary_meta, primary_blobs).unwrap()).await;
    let replica_dir = tempfile::tempdir().unwrap();
    let intermediate_config = config(&replica_dir, Some(replica_config(&primary.url, 10)));
    let replica_state = build_state(&intermediate_config).unwrap();
    assert_eq!(
        ReplicationRuntime::new(&intermediate_config, &replica_state)
            .unwrap()
            .sync_cycle()
            .await,
        Some(true)
    );
    let replica = TestServer::start(
        primary_router(
            "replica-b",
            TOKEN,
            replica_state.meta.clone(),
            replica_state.blobs.clone(),
        )
        .unwrap(),
    )
    .await;
    let follower_dir = tempfile::tempdir().unwrap();
    let follower_config = config(&follower_dir, Some(replica_config(&replica.url, 10)));
    let follower_state = build_state(&follower_config).unwrap();

    assert_eq!(
        ReplicationRuntime::new(&follower_config, &follower_state)
            .unwrap()
            .sync_cycle()
            .await,
        Some(true)
    );
    assert_eq!(follower_state.blobs.read_bytes(&digest, 8).await.unwrap(), b"artifact");
}

#[tokio::test]
async fn test_replica_stays_live_but_unready_while_starting() {
    let dir = tempfile::tempdir().unwrap();
    let config = config(&dir, Some(replica_config("https://primary.example/", 10)));
    let state = build_state(&config).unwrap();
    let runtime = ReplicationRuntime::new(&config, &state).unwrap();
    let router = runtime.mount(router_for(state));

    let (health_status, health) = document(&router, "/+replication/v1/health", None).await;
    assert_eq!(health_status, StatusCode::OK);
    assert_eq!(
        health,
        serde_json::json!({"mode": "dc", "role": "replica", "ready": false, "reasons": ["frontier_lag"]})
    );

    let (ready_status, ready) = document(&router, "/+replication/v1/ready", None).await;
    assert_eq!(ready_status, StatusCode::SERVICE_UNAVAILABLE);
    assert_eq!(ready, health);
}

#[tokio::test]
async fn test_replica_readiness_reports_a_sync_error() {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}/", listener.local_addr().unwrap());
    // Hold the port and reset every connection so the replica always sees a transport failure. Dropping
    // the listener would free the port for a parallel test's mock primary to reuse; answering with its
    // own protocol version, that would flake this into an incompatible-schema reason.
    let _reset = tokio::spawn(async move {
        while let Ok((stream, _)) = listener.accept().await {
            drop(stream);
        }
    });
    let dir = tempfile::tempdir().unwrap();
    let config = config(&dir, Some(replica_config(&url, 10)));
    let state = build_state(&config).unwrap();
    let mut runtime = ReplicationRuntime::new(&config, &state).unwrap();

    assert_eq!(runtime.sync_cycle().await, Some(true));
    let router = runtime.mount(router_for(state));

    assert_eq!(get(&router, "/+replication/v1/health").await.0, StatusCode::OK);
    let (status, ready) = document(&router, "/+replication/v1/ready", None).await;
    assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE);
    assert_eq!(
        ready,
        serde_json::json!({"mode": "dc", "role": "replica", "ready": false, "reasons": ["sync_error"]})
    );
    let (_, body) = get(&router, "/metrics").await;
    let body = String::from_utf8(body).unwrap();
    assert!(body.contains("peryx_ha_distributed_sync_errors_total 1\n"), "{body}");
    assert!(
        body.contains("peryx_availability_sync_errors_total{class=\"transport\"} 1\n"),
        "{body}"
    );
    assert!(body.contains("peryx_availability_sync_cycles_total 1\n"), "{body}");
}

#[tokio::test]
async fn test_replica_readiness_reports_an_incompatible_schema() {
    let primary = incompatible_primary().await;
    let dir = tempfile::tempdir().unwrap();
    let config = config(&dir, Some(replica_config(&primary.url, 10)));
    let state = build_state(&config).unwrap();
    let mut runtime = ReplicationRuntime::new(&config, &state).unwrap();

    assert_eq!(runtime.sync_cycle().await, Some(true));
    let router = runtime.mount(router_for(state));

    let (status, ready) = document(&router, "/+replication/v1/ready", None).await;
    assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE);
    assert_eq!(
        ready,
        serde_json::json!({"mode": "dc", "role": "replica", "ready": false, "reasons": ["incompatible_schema"]})
    );
    let (_, body) = get(&router, "/metrics").await;
    let body = String::from_utf8(body).unwrap();
    assert!(
        body.contains("peryx_availability_sync_errors_total{class=\"schema\"} 1\n"),
        "{body}"
    );
}

#[tokio::test]
async fn test_replica_readiness_recovers_and_reports_serials_to_operators() {
    let (_primary_dir, primary_meta, primary_blobs) = primary_stores();
    primary_meta
        .commit_driver_txn(|_| {
            Ok::<_, peryx_storage::meta::MetaError>(((), vec![b"one".to_vec(), b"two".to_vec(), b"three".to_vec()]))
        })
        .unwrap();
    let server = TestServer::start(primary_router("primary-a", TOKEN, primary_meta, primary_blobs).unwrap()).await;
    let dir = tempfile::tempdir().unwrap();
    let config = config(&dir, Some(replica_config(&server.url, 2)));
    let state = build_state(&config).unwrap();
    let operator = credential(&state, "Olivia", Role::Operator).await;
    let mut runtime = ReplicationRuntime::new(&config, &state).unwrap();
    let router = runtime.mount(router_for(state));

    assert_eq!(runtime.sync_cycle().await, Some(false));
    let (status, ready) = document(&router, "/+replication/v1/ready", Some(&operator)).await;
    assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE);
    assert_eq!(
        ready,
        serde_json::json!({
            "mode": "dc", "role": "replica", "ready": false, "reasons": ["frontier_lag"],
            "serial": 2, "primary_serial": 3, "lag": 1,
            "synced_changes": 2, "sync_errors": 0,
        })
    );
    let (_, body) = get(&router, "/metrics").await;
    let body = String::from_utf8(body).unwrap();
    assert!(body.contains("peryx_ha_distributed_lag 1\n"), "{body}");
    assert!(body.contains("peryx_availability_pending_serials 1\n"), "{body}");
    assert!(body.contains("peryx_availability_sync_cycles_total 1\n"), "{body}");
    assert!(body.contains("peryx_availability_apply_seconds_count 1\n"), "{body}");

    assert_eq!(runtime.sync_cycle().await, Some(true));
    let (status, ready) = document(&router, "/+replication/v1/ready", Some(&operator)).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(
        ready,
        serde_json::json!({
            "mode": "dc", "role": "replica", "ready": true, "reasons": [],
            "serial": 3, "primary_serial": 3, "lag": 0,
            "synced_changes": 3, "sync_errors": 0,
        })
    );
    let (_, body) = get(&router, "/metrics").await;
    let body = String::from_utf8(body).unwrap();
    assert!(body.contains("peryx_ha_distributed_lag 0\n"), "{body}");
    // Caught up to the primary at serial 3, and readability reaches it: applying each page rebuilt the
    // affected derived views and advanced the search view frontier before the serial became visible, so
    // no read waits on a later search to refresh the index.
    assert!(body.contains("peryx_ha_distributed_readable_serial 3\n"), "{body}");
    assert!(body.contains("peryx_availability_pending_serials 0\n"), "{body}");
    assert!(body.contains("peryx_availability_sync_cycles_total 2\n"), "{body}");
}

#[tokio::test]
async fn test_availability_health_filters_topology_by_caller_class() {
    let dir = tempfile::tempdir().unwrap();
    let config = config(
        &dir,
        Some(replica_config("http://replica:s3cr3t@primary.example:8443/", 10)),
    );
    let state = build_state(&config).unwrap();
    let operator = credential(&state, "Olivia", Role::Operator).await;
    let administrator = credential(&state, "Alice", Role::Administrator).await;
    let runtime = ReplicationRuntime::new(&config, &state).unwrap();
    let router = runtime.mount(router_for(state));

    let (_, public) = document(&router, "/+replication/v1/health", None).await;
    assert_eq!(
        public,
        serde_json::json!({"mode": "dc", "role": "replica", "ready": false, "reasons": ["frontier_lag"]})
    );

    let (_, operator) = document(&router, "/+replication/v1/health", Some(&operator)).await;
    assert!(operator.get("serial").is_some());
    assert!(operator.get("lag").is_some());
    assert!(operator.get("upstream").is_none());

    let (_, administrator) = document(&router, "/+replication/v1/health", Some(&administrator)).await;
    let upstream = administrator["upstream"].as_str().unwrap();
    assert_eq!(upstream, "http://primary.example:8443/");
    assert!(!upstream.contains("replica"));
    assert!(!upstream.contains("s3cr3t"));
}

#[tokio::test]
async fn test_primary_exposes_ready_availability() {
    let dir = tempfile::tempdir().unwrap();
    let config = config(&dir, Some(primary_config()));
    let state = build_state(&config).unwrap();
    let administrator = credential(&state, "Alice", Role::Administrator).await;
    let runtime = ReplicationRuntime::new(&config, &state).unwrap();
    assert_eq!(
        runtime.reclamation_frontiers().observe(),
        Some(peryx_storage::meta::ObservedFrontier {
            replica: None,
            backup: None,
        })
    );
    let router = runtime.mount(router_for(state));

    let (status, ready) = document(&router, "/+replication/v1/ready", Some(&administrator)).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(
        ready,
        serde_json::json!({"mode": "dc", "role": "primary", "ready": true, "reasons": [], "serial": 0})
    );
    assert_eq!(get(&router, "/+replication/v1/health").await.0, StatusCode::OK);
}

#[tokio::test]
async fn test_readiness_reports_a_failed_blob_store_in_ha_mode() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("blobs"), b"not a directory").unwrap();
    let mut config = config(&dir, Some(primary_config()));
    let AvailabilityConfig::Dc(replication) = config.availability else {
        panic!("config helper builds a dc primary");
    };
    config.availability = AvailabilityConfig::Ha(replication);
    let router = build_router(&config).unwrap();

    let (status, ready) = document(&router, "/+replication/v1/ready", None).await;
    assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE);
    assert_eq!(
        ready,
        serde_json::json!({"mode": "ha", "role": "primary", "ready": false, "reasons": ["blob_store"]})
    );
}

#[tokio::test]
async fn test_disabled_runtime_mounts_no_routes_or_task() {
    let dir = tempfile::tempdir().unwrap();
    let config = config(&dir, None);
    let state = build_state(&config).unwrap();
    assert!(ReplicationRuntime::from_config(&config, &state).unwrap().is_none());

    let router = router_for(state);
    let response = router
        .clone()
        .oneshot(
            Request::get("/+replication/v1/changes?after=0&limit=10")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::NOT_FOUND);
    assert_eq!(get(&router, "/+replication/v1/health").await.0, StatusCode::NOT_FOUND);
    assert_eq!(get(&router, "/+replication/v1/ready").await.0, StatusCode::NOT_FOUND);
    let (_, body) = get(&router, "/metrics").await;
    let metrics = String::from_utf8(body).unwrap();
    assert!(!metrics.contains("peryx_ha_distributed_"), "{metrics}");
    assert!(!metrics.contains("peryx_availability_worker_"), "{metrics}");
}

#[tokio::test]
async fn test_primary_runtime_starts_no_replica_services() {
    let dir = tempfile::tempdir().unwrap();
    let config = config(&dir, Some(primary_config()));
    let state = build_state(&config).unwrap();

    assert!(
        ReplicationRuntime::new(&config, &state)
            .unwrap()
            .start()
            .unwrap()
            .is_none()
    );
}

#[test]
fn test_reclamation_frontiers_include_configured_replicas() {
    let dir = tempfile::tempdir().unwrap();
    let mut config = config(&dir, Some(primary_config()));
    config.dc_membership = Some(DcMembership {
        group: "group".to_owned(),
        members: vec![DcMember {
            node: "replica-a".to_owned(),
            dc: "east".to_owned(),
            address: "http://replica-a/".to_owned(),
            role: DcRole::Replica,
        }],
    });
    let state = build_state(&config).unwrap();

    assert!(
        ReplicationRuntime::new(&config, &state)
            .unwrap()
            .reclamation_frontiers()
            .observe()
            .is_none()
    );
}

#[test]
fn test_replica_runtime_disables_local_writers() {
    let dir = tempfile::tempdir().unwrap();
    let mut config = config(&dir, Some(replica_config("https://primary.example/", 10)));
    let IndexKind::Cached { routing, .. } = &mut config.indexes[0].kind else {
        panic!("expected the default cached index");
    };
    *routing = UpstreamRoutingConfig {
        upstreams: vec![UpstreamConfig {
            name: "primary".to_owned(),
            url: "https://packages.example/simple/".to_owned(),
            artifact_url: None,
            username: Some("replica".to_owned()),
            password: Some(SecretSource::File("missing-routed-upstream-password".into())),
            token: None,
            credential_exec: None,
            credential_refresh: None,
            tls: crate::config::UpstreamTlsConfig::default(),
        }],
        fallback: true,
        protected: Vec::new(),
        pins: BTreeMap::default(),
    };
    config.indexes[1].tokens.extend([
        TokenConfig {
            name: "reader".to_owned(),
            secret: SecretSource::Literal("reader-secret".to_owned()),
            projects: vec!["*".to_owned()],
            actions: BTreeSet::from([Action::Read, Action::Write]),
            expires_at: None,
        },
        TokenConfig {
            name: "writer".to_owned(),
            secret: SecretSource::File("missing-writer-token".into()),
            projects: vec!["*".to_owned()],
            actions: BTreeSet::from([Action::Write]),
            expires_at: None,
        },
    ]);
    config.indexes[1].webhooks.push(WebhookConfig {
        name: "audit".to_owned(),
        url: "https://hooks.example/audit".to_owned(),
        secret: WebhookSecret::Env("PERYX_TEST_MISSING_REPLICA_WEBHOOK_SECRET".to_owned()),
        events: Vec::new(),
    });

    let state = build_state(&config).unwrap();

    assert!(state.read_only);
    assert!(matches!(
        state.indexes[0].kind,
        RuntimeIndexKind::Cached { offline: true, .. }
    ));
    assert!(state.upstream_routes.is_empty());
    assert!(state.indexes[1].acl.grants_to_anyone(Action::Read));
    assert!(!state.indexes[1].acl.grants_to_anyone(Action::Write));
    assert!(!state.indexes[1].acl.grants_to_anyone(Action::Delete));
    assert!(matches!(
        state.indexes[2].kind,
        RuntimeIndexKind::Virtual { upload: None, .. }
    ));
    assert!(state.webhooks.is_empty());
}

#[rstest]
#[case::primary(ReplicationConfig::Primary {
    source: "primary-a".to_owned(),
    token: SecretSource::File("missing-primary-token".into()),
}, "read the primary replication token")]
#[case::replica(ReplicationConfig::Replica {
    upstream: "https://primary.example/".to_owned(),
    token: SecretSource::File("missing-replica-token".into()),
    poll_interval: Duration::from_secs(1),
    page_size: NonZeroUsize::new(10).unwrap(),
}, "read the replica replication token")]
fn test_replication_runtime_reports_secret_errors(#[case] replication: ReplicationConfig, #[case] expected: &str) {
    let dir = tempfile::tempdir().unwrap();
    let config = config(&dir, Some(replication));
    let state = build_state(&config).unwrap();

    let Err(error) = ReplicationRuntime::new(&config, &state) else {
        panic!("expected the missing replication token to fail");
    };

    assert!(error.to_string().contains(expected), "{error}");
}

#[test]
fn test_replication_runtime_rejects_an_invalid_upstream_url() {
    let dir = tempfile::tempdir().unwrap();
    let config = config(&dir, Some(replica_config("not a URL", 10)));
    let state = build_state(&config).unwrap();

    let Err(error) = ReplicationRuntime::new(&config, &state) else {
        panic!("expected the invalid upstream URL to fail");
    };

    assert!(
        error.to_string().contains("build the replica metadata peer set"),
        "{error}"
    );
}

fn bounded_policy(max_attempts: u32) -> ReconnectPolicy {
    ReconnectPolicy::new(
        Duration::from_millis(100),
        NonZeroU32::new(2).unwrap(),
        Duration::from_secs(30),
        NonZeroU32::new(max_attempts).unwrap(),
    )
}

#[test]
fn test_schedule_delay_waits_the_poll_interval_when_caught_up() {
    let mut attempt = 4;
    let delay = schedule_delay(
        &Ok(true),
        &mut attempt,
        &DEFAULT_RECONNECT_POLICY,
        Duration::from_secs(5),
    );
    assert_eq!(delay, Duration::from_secs(5));
    assert_eq!(attempt, 0, "an applied pass resets the failure count");
}

#[test]
fn test_schedule_delay_pulls_the_next_page_at_once_when_more_remains() {
    let mut attempt = 2;
    let delay = schedule_delay(
        &Ok(false),
        &mut attempt,
        &DEFAULT_RECONNECT_POLICY,
        Duration::from_secs(5),
    );
    assert_eq!(delay, Duration::ZERO);
    assert_eq!(attempt, 0);
}

#[test]
fn test_schedule_delay_backs_off_a_retryable_transport_loss() {
    let mut attempt = 0;
    let delay = schedule_delay(
        &Err(TransportError::Disconnected),
        &mut attempt,
        &bounded_policy(10),
        Duration::from_secs(5),
    );
    assert_eq!(attempt, 1);
    assert_eq!(
        delay,
        Duration::from_millis(100),
        "the first backoff is the policy base"
    );
}

#[test]
fn test_schedule_delay_falls_back_to_the_poll_interval_once_the_policy_gives_up() {
    let mut attempt = 0;
    let delay = schedule_delay(
        &Err(TransportError::Disconnected),
        &mut attempt,
        &bounded_policy(1),
        Duration::from_secs(5),
    );
    assert_eq!(attempt, 1);
    assert_eq!(
        delay,
        Duration::from_secs(5),
        "an exhausted budget keeps trying at the base cadence"
    );
}

#[tokio::test]
async fn test_replica_cycle_records_a_transport_loss_and_reports_the_pass_done() {
    // The upstream URL is valid but nothing listens, so a metadata fetch fails to connect. The cycle
    // records the loss and reports the pass as done, and the run loop backs it off rather than spinning.
    let replica_dir = tempfile::tempdir().unwrap();
    let config = config(&replica_dir, Some(replica_config("http://127.0.0.1:1/", 1)));
    let state = build_state(&config).unwrap();
    let mut runtime = ReplicationRuntime::new(&config, &state).unwrap();

    let subscriber = tracing_subscriber::fmt().with_writer(std::io::sink).finish();
    let guard = tracing::subscriber::set_default(subscriber);
    let caught_up = runtime.sync_cycle().await;
    drop(guard);

    assert_eq!(
        caught_up,
        Some(true),
        "a transport loss ends the pass so the loop can back off"
    );
}

#[tokio::test]
async fn test_replica_cycle_reports_a_local_cursor_mismatch() {
    // Advance the local journal past the replica cursor so reading the durable resume state fails its
    // consistency check before any fetch; the cycle records it and reports the pass done.
    let replica_dir = tempfile::tempdir().unwrap();
    let config = config(&replica_dir, Some(replica_config("http://127.0.0.1:1/", 1)));
    let state = build_state(&config).unwrap();
    state
        .serving
        .meta
        .commit_driver_txn(|_| Ok::<_, peryx_storage::meta::MetaError>(((), vec![b"stray".to_vec()])))
        .unwrap();
    let mut runtime = ReplicationRuntime::new(&config, &state).unwrap();

    let subscriber = tracing_subscriber::fmt().with_writer(std::io::sink).finish();
    let guard = tracing::subscriber::set_default(subscriber);
    let caught_up = runtime.sync_cycle().await;
    drop(guard);

    assert_eq!(caught_up, Some(true));
}

#[tokio::test]
async fn test_a_replica_that_knows_its_identity_spawns_a_frontier_beacon() {
    let dir = tempfile::tempdir().unwrap();
    let mut config = config(&dir, Some(replica_config("http://writer.invalid/", 1)));
    config.node_identity = Some("replica-a".to_owned());
    let state = build_state(&config).unwrap();
    let runtime = ReplicationRuntime::new(&config, &state).unwrap();

    let availability = runtime.start().unwrap();
    assert!(availability.is_some());
}
