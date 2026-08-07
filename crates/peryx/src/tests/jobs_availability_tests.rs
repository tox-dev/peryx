//! Background jobs and storage policy exercised across the `none`, `dc`, and `ha` availability modes.
//!
//! Four surfaces carry a durable contract a hosted service must keep whatever its availability posture:
//! a node-local background job runs and cancels, a repository quota admits or refuses a write, a
//! retention plan enumerates removal candidates from authoritative state, and the catalog lists the
//! projects an index serves. These probes hold that each reaches the same documented outcome whether the
//! node runs as a single-node `none` writer or a `dc`/`ha` writer that fronts a consensus group:
//! replication changes how a mutation becomes durable, never what a client observes. A read-only replica
//! originates none of it and refuses every mutation at its gate.
//!
//! The fault arms follow the availability harness. A replica serves a job, quota, or catalog read only
//! within the serial it has applied, so a partition withholds authoritative state rather than exposing a
//! frontier it has not reached ([OWASP authorization guidance]: a follower fails closed). Reassigning a
//! repository's authority home - the ownership every one of these surfaces resolves against - commits
//! under a leader, is refused when the node is not the leader, and reaches consensus once under a retried
//! idempotency key ([Kubernetes CronJob controller]: one run under one valid authority).
//!
//! Every arm is deterministic: requests run through the in-process router with [`ServiceExt::oneshot`], a
//! replica advances one [`sync_cycle`](crate::replication::ReplicationRuntime::sync_cycle) at a time, and
//! a job parks on its cancellation signal rather than a timer. No test sleeps, binds a fixed port, or
//! reads a real clock. Following the [PyPA specifications] for the `PyPI` surface.
//!
//! [PyPA specifications]: https://packaging.python.org/en/latest/specifications/
//! [OWASP authorization guidance]: https://cheatsheetseries.owasp.org/cheatsheets/Authorization_Cheat_Sheet.html
//! [Kubernetes CronJob controller]: https://kubernetes.io/docs/concepts/workloads/controllers/cron-jobs/

use std::num::NonZeroUsize;
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use axum::Router;
use axum::body::Body;
use axum::http::{Request, StatusCode, header};
use base64::Engine as _;
use base64::engine::general_purpose::STANDARD;
use http_body_util::BodyExt as _;
use peryx_core::Ecosystem;
use peryx_driver::jobs::{JobContext, JobFailure, JobLimits, JobReport, JobScheduler, NodeJob, Submit};
use peryx_driver::state::{
    AppState, CommandOutcome, CommandReceipt, ControlCommand, ControlError, ControlPlane, MembershipControl,
};
use peryx_ha_distributed::primary_router;
use peryx_identity::{GrantScope, Role};
use peryx_policy::PolicyConfig;
use peryx_storage::blob::{BlobStore, Digest};
use peryx_storage::meta::{JobRunQuery, JobState, MetaError, MetaStore};
use rstest::rstest;
use serde_json::{Value, json};
use tokio::sync::Notify;
use tower::ServiceExt as _;

use crate::availability::{
    AvailabilityPosture, RosterFrontierSource, TransferCoordinator, router as availability_router,
};
use crate::config::{
    AvailabilityConfig, Config, DcMember, DcMembership, DcRole, IndexConfig, IndexKind, ReplicationConfig, SecretSource,
};
use crate::replication::ReplicationRuntime;
use crate::server::{build_state, router_for};

/// The shared secret a replica presents to its primary; irrelevant to the assertions but required to
/// build the runtime.
const TOKEN: &str = "replica-secret";
/// The secret every hosted index accepts for writes, presented as the upload credential.
const UPLOAD: &str = "s3cret";
/// The password a seeded administrator authenticates with.
const PASSWORD: &str = "jobs availability password";
/// The writer identity a `dc`/`ha` store is claimed under, so a replica agrees with what it follows.
const WRITER_IDENTITY: &str = "writer-a";
/// A real wheel with parseable metadata, so the publish path admits it. Its name and version are fixed
/// by the archive, so the upload form fields must agree.
const WHEEL: &[u8] = include_bytes!("../../../../tests/frontend/fixtures/veloxdemo-1.0.0-py3-none-any.whl");
/// The project and version the fixture wheel carries.
const PROJECT: &str = "veloxdemo";
const VERSION: &str = "1.0.0";
/// The per-project and per-repository byte caps the `tight` index configures. The fixture wheel is
/// larger than either, so a publish there always crosses the limit.
const TIGHT_LIMIT: u64 = 100;
/// A journaled management record the primary holds and a partitioned replica must not surface until it
/// has applied the page that carries it.
const MANAGED_KEY: &str = "pypi\u{0}p\u{0}hosted/secret";
const MANAGED_VALUE: &[u8] = b"managed-record";

/// A hosted index at `route == name` that accepts `UPLOAD` for writes, with `policy` applied.
fn hosted(name: &str, ecosystem: Ecosystem, policy: PolicyConfig) -> IndexConfig {
    IndexConfig {
        name: name.to_owned(),
        route: name.to_owned(),
        ecosystem,
        kind: IndexKind::Hosted { volatile: true },
        anonymous_read: None,
        tokens: vec![crate::tests::writer_token(SecretSource::Literal(UPLOAD.to_owned()))],
        policy,
        ecosystem_policy: toml::Table::new(),
        ecosystem_settings: toml::Table::new(),
        webhooks: Vec::new(),
    }
}

/// A `store` index without limits (for jobs, retention, and catalog probes) and a `tight` index whose
/// per-project and per-repository byte caps a fixture publish always crosses.
fn feature_indexes() -> Vec<IndexConfig> {
    let tight = PolicyConfig {
        max_project_size_bytes: Some(TIGHT_LIMIT),
        max_accounted_bytes: Some(TIGHT_LIMIT),
        ..PolicyConfig::default()
    };
    vec![
        hosted("store", peryx_ecosystem_pypi::ECOSYSTEM, PolicyConfig::default()),
        hosted("tight", peryx_ecosystem_pypi::ECOSYSTEM, tight),
    ]
}

/// A single-node `none` writer: the default posture, with no availability resource.
fn none_config(dir: &tempfile::TempDir) -> Config {
    Config {
        data_dir: dir.path().to_path_buf(),
        indexes: feature_indexes(),
        ..Config::default()
    }
}

/// A `dc` writer that fronts a two-member group and journals its mutations for replication.
fn dc_writer_config(dir: &tempfile::TempDir) -> Config {
    Config {
        data_dir: dir.path().to_path_buf(),
        writer_identity: Some(WRITER_IDENTITY.to_owned()),
        availability: AvailabilityConfig::Dc(primary_replication()),
        dc_membership: Some(group()),
        indexes: feature_indexes(),
        ..Config::default()
    }
}

/// An `ha` writer: the same primary posture reached through an `ha` roster, so the mode label differs and
/// the durability requirement is replicated.
/// A single-datacenter `ha` roster: one writer, no remote datacenter to wait on. A single-process `ha`
/// behavior test proves its writes locally, since cross-datacenter write completion needs a reachable
/// remote and is exercised by the multi-process availability harness rather than faked here.
fn solo_group() -> DcMembership {
    DcMembership {
        group: "east".to_owned(),
        members: vec![DcMember {
            node: WRITER_IDENTITY.to_owned(),
            dc: "east-1".to_owned(),
            address: "10.0.0.1:8080".to_owned(),
            role: DcRole::Writer,
        }],
    }
}

fn ha_writer_config(dir: &tempfile::TempDir) -> Config {
    Config {
        data_dir: dir.path().to_path_buf(),
        writer_identity: Some(WRITER_IDENTITY.to_owned()),
        availability: AvailabilityConfig::Ha(primary_replication()),
        dc_membership: Some(solo_group()),
        indexes: feature_indexes(),
        ..Config::default()
    }
}

/// A `dc` replica that follows `upstream`. A configured replica is read-only, so its router refuses
/// mutations.
fn dc_replica_config(dir: &tempfile::TempDir, upstream: &str) -> Config {
    claim_writer(dir);
    Config {
        data_dir: dir.path().to_path_buf(),
        writer_identity: Some(WRITER_IDENTITY.to_owned()),
        availability: AvailabilityConfig::Dc(replica_replication(upstream)),
        indexes: feature_indexes(),
        ..Config::default()
    }
}

/// An `ha` replica: the same read-only follower reached through an `ha` roster.
fn ha_replica_config(dir: &tempfile::TempDir, upstream: &str) -> Config {
    claim_writer(dir);
    Config {
        data_dir: dir.path().to_path_buf(),
        writer_identity: Some(WRITER_IDENTITY.to_owned()),
        availability: AvailabilityConfig::Ha(replica_replication(upstream)),
        indexes: feature_indexes(),
        ..Config::default()
    }
}

fn primary_replication() -> ReplicationConfig {
    ReplicationConfig::Primary {
        source: WRITER_IDENTITY.to_owned(),
        token: SecretSource::Literal(TOKEN.to_owned()),
    }
}

fn replica_replication(upstream: &str) -> ReplicationConfig {
    ReplicationConfig::Replica {
        upstream: upstream.to_owned(),
        token: SecretSource::Literal(TOKEN.to_owned()),
        poll_interval: Duration::from_millis(1),
        page_size: NonZeroUsize::MIN,
    }
}

fn group() -> DcMembership {
    DcMembership {
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
    }
}

/// Seed the replica store's writer identity before it is opened read-only, the way a provisioned replica
/// is handed the writer it will follow.
fn claim_writer(dir: &tempfile::TempDir) {
    MetaStore::open(dir.path().join("peryx.redb"))
        .unwrap()
        .claim_writer_identity(WRITER_IDENTITY)
        .unwrap();
}

/// Build a writer node's state and neutral router. A writer accepts mutations, so its router carries no
/// read-only gate.
fn writer_node(config: &Config) -> (Arc<AppState>, Router) {
    let state = build_state(config).unwrap();
    let router = router_for(state.clone());
    (state, router)
}

/// Build a read-only replica's state and full router (with its mutation gate) plus the runtime that
/// drives its sync.
fn replica_node(config: &Config) -> (Arc<AppState>, Router, ReplicationRuntime) {
    let state = build_state(config).unwrap();
    let runtime = ReplicationRuntime::new(config, &state).unwrap();
    let router = runtime.mount(router_for(state.clone()));
    (state, router, runtime)
}

/// Create a server-wide administrator and return its Basic `user:password` credential.
async fn admin(state: &AppState) -> String {
    let id = state.users.create("root").unwrap().id;
    state.users.set_password(&id, PASSWORD).await.unwrap();
    state
        .authorization
        .grant(&id, Role::Administrator, GrantScope::Server)
        .unwrap();
    format!("root:{PASSWORD}")
}

/// Create a repository reader and return its Basic `user:password` credential. A reader passes
/// authentication but holds no administration scope.
async fn reader(state: &AppState) -> String {
    let id = state.users.create("rita").unwrap().id;
    state.users.set_password(&id, PASSWORD).await.unwrap();
    state
        .authorization
        .grant(&id, Role::RepositoryReader, GrantScope::Server)
        .unwrap();
    format!("rita:{PASSWORD}")
}

/// The Basic header value carrying `secret` under `user`.
fn basic(user: &str, secret: &str) -> String {
    format!("Basic {}", STANDARD.encode(format!("{user}:{secret}")))
}

/// One request through the router. `auth` is a Basic `user:password` pair; `json` sets a JSON body and
/// content type, which the retention mutation requires.
async fn send(
    router: &Router,
    method: &str,
    path: &str,
    auth: Option<&str>,
    body: Option<Value>,
) -> (StatusCode, Value) {
    let mut request = Request::builder().method(method).uri(path);
    if let Some(auth) = auth {
        request = request.header(header::AUTHORIZATION, format!("Basic {}", STANDARD.encode(auth)));
    }
    if body.is_some() {
        request = request.header(header::CONTENT_TYPE, "application/json");
    }
    let payload = body.map_or_else(Body::empty, |value| Body::from(value.to_string()));
    let response = router.clone().oneshot(request.body(payload).unwrap()).await.unwrap();
    let status = response.status();
    let bytes = response.into_body().collect().await.unwrap().to_bytes();
    (status, serde_json::from_slice(&bytes).unwrap_or(Value::Null))
}

/// Publish the fixture wheel to `route`, authenticated by `secret`, and return the response status.
/// `PyPI` uploads use the `__token__` Basic convention over a legacy multipart form.
async fn upload(router: &Router, route: &str, secret: &str) -> StatusCode {
    let boundary = "peryxjobsboundary";
    let digest = Digest::of(WHEEL);
    let mut body = Vec::new();
    for (name, value) in [
        (":action", "file_upload"),
        ("name", PROJECT),
        ("version", VERSION),
        ("filetype", "bdist_wheel"),
        ("sha256_digest", digest.as_str()),
    ] {
        body.extend_from_slice(
            format!("--{boundary}\r\nContent-Disposition: form-data; name=\"{name}\"\r\n\r\n{value}\r\n").as_bytes(),
        );
    }
    let filename = format!("{PROJECT}-{VERSION}-py3-none-any.whl");
    body.extend_from_slice(
        format!("--{boundary}\r\nContent-Disposition: form-data; name=\"content\"; filename=\"{filename}\"\r\n\r\n")
            .as_bytes(),
    );
    body.extend_from_slice(WHEEL);
    body.extend_from_slice(format!("\r\n--{boundary}--\r\n").as_bytes());
    let request = Request::builder()
        .method("POST")
        .uri(format!("/{route}/"))
        .header(
            header::CONTENT_TYPE,
            format!("multipart/form-data; boundary={boundary}"),
        )
        .header(header::AUTHORIZATION, basic("__token__", secret))
        .body(Body::from(body))
        .unwrap();
    router.clone().oneshot(request).await.unwrap().status()
}

/// The availability mode a node reports on its public topology surface.
async fn topology_mode(router: &Router) -> Value {
    let request = Request::builder()
        .method("GET")
        .uri("/+availability/topology")
        .body(Body::empty())
        .unwrap();
    let response = router.clone().oneshot(request).await.unwrap();
    let bytes = response.into_body().collect().await.unwrap().to_bytes();
    serde_json::from_slice::<Value>(&bytes).unwrap()["mode"].clone()
}

/// A durable job that parks on its cancellation signal, so a test can observe an attempt in flight and
/// cancel it deterministically. `started` fires once the run begins, after the scheduler has registered
/// the attempt, so the test knows the token is live before it cancels.
struct ParkedJob {
    started: Arc<Notify>,
}

#[async_trait]
impl NodeJob for ParkedJob {
    fn kind(&self) -> &'static str {
        "test_parked"
    }

    fn scope(&self) -> &'static str {
        ""
    }

    fn persist_as(&self) -> Option<peryx_storage::meta::JobKind> {
        Some(peryx_storage::meta::JobKind::CacheRefresh)
    }

    async fn run(&self, ctx: &JobContext) -> Result<JobReport, JobFailure> {
        self.started.notify_one();
        ctx.cancelled().await;
        Ok(JobReport::default())
    }
}

/// A retention plan targeting every artifact: an `expire` prefix rule with no `keep` pins every version
/// as a removal candidate.
fn expire_all(repository: &str) -> Value {
    json!({ "repository": repository, "expire": [{ "selector": "project-prefix", "prefix": "" }] })
}

/// A node-local job runs to completion and cancels the same way in every availability mode. The scheduler
/// registers the attempt on the node running it, so an administrator stops it through the neutral router;
/// the durable run lands `cancelled` whatever the mode, and the node reports the mode it ran under.
#[rstest]
#[case::none(none_config as fn(&tempfile::TempDir) -> Config, "none")]
#[case::dc(dc_writer_config as fn(&tempfile::TempDir) -> Config, "dc")]
#[case::ha(ha_writer_config as fn(&tempfile::TempDir) -> Config, "ha")]
#[tokio::test]
async fn test_a_node_local_job_runs_and_cancels_in_every_mode(
    #[case] build: fn(&tempfile::TempDir) -> Config,
    #[case] mode: &str,
) {
    let dir = tempfile::tempdir().unwrap();
    let (state, router) = writer_node(&build(&dir));
    let root = admin(&state).await;
    assert_eq!(
        topology_mode(&router).await,
        mode,
        "the node runs under the {mode} mode"
    );

    let scheduler = JobScheduler::new(state.serving.clone(), JobLimits::node_local());
    let started = Arc::new(Notify::new());
    assert!(matches!(
        scheduler.submit(Arc::new(ParkedJob {
            started: started.clone()
        })),
        Submit::Queued
    ));
    started.notified().await;
    let running = state
        .meta
        .query_job_runs(&JobRunQuery {
            cursor: None,
            limit: 25,
        })
        .unwrap()
        .runs
        .into_iter()
        .find(|run| run.state == JobState::Running)
        .expect("the parked attempt is running");

    // An unknown run is a 404 and an anonymous caller is refused, so cancellation is administrator-gated
    // ahead of any signal, identically to every other mode.
    assert_eq!(
        send(&router, "POST", "/+jobs/jr_00000000000000ff/cancel", Some(&root), None)
            .await
            .0,
        StatusCode::NOT_FOUND,
    );
    assert_eq!(
        send(&router, "POST", &format!("/+jobs/{}/cancel", running.id), None, None)
            .await
            .0,
        StatusCode::UNAUTHORIZED,
    );

    // The running attempt takes the cancellation signal and unwinds; the durable run lands cancelled.
    assert_eq!(
        send(
            &router,
            "POST",
            &format!("/+jobs/{}/cancel", running.id),
            Some(&root),
            None
        )
        .await
        .0,
        StatusCode::ACCEPTED,
    );
    scheduler.shutdown().await;
    assert_eq!(
        state.meta.get_job_run(&running.id).unwrap().unwrap().state,
        JobState::Cancelled,
    );
}

/// A repository quota admits or refuses a write the same way in every mode. A publish that would cross a
/// configured cap is denied at ingress with `403`, before any bytes are stored; a publish under an
/// unlimited index is admitted; and the read surface reports the authoritative limit whatever the mode.
#[rstest]
#[case::none(none_config as fn(&tempfile::TempDir) -> Config)]
#[case::dc(dc_writer_config as fn(&tempfile::TempDir) -> Config)]
#[case::ha(ha_writer_config as fn(&tempfile::TempDir) -> Config)]
#[tokio::test]
async fn test_a_quota_admits_or_refuses_a_write_in_every_mode(#[case] build: fn(&tempfile::TempDir) -> Config) {
    let dir = tempfile::tempdir().unwrap();
    let (state, router) = writer_node(&build(&dir));
    let root = admin(&state).await;

    // The fixture wheel is larger than the tight cap, so its publish is refused at the boundary.
    assert_eq!(upload(&router, "tight", UPLOAD).await, StatusCode::FORBIDDEN);
    // An unlimited index admits the same publish.
    assert_eq!(upload(&router, "store", UPLOAD).await, StatusCode::OK);

    // The read surface reports the configured cap, drawn from the same authoritative policy the write was
    // admitted against.
    let (status, quota) = send(&router, "GET", "/+quota/repository?repository=tight", Some(&root), None).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(quota["accounted_bytes"]["limit"], TIGHT_LIMIT);
    assert_eq!(quota["limits"]["max_accounted_bytes"], TIGHT_LIMIT);
}

/// A retention plan enumerates removal candidates from authoritative state the same way in every mode.
/// The plan is a preview that never mutates, so it reads the published versions and marks each an
/// `expire` rule reaches; it stays administrator-gated whatever the mode.
#[rstest]
#[case::none(none_config as fn(&tempfile::TempDir) -> Config)]
#[case::dc(dc_writer_config as fn(&tempfile::TempDir) -> Config)]
#[case::ha(ha_writer_config as fn(&tempfile::TempDir) -> Config)]
#[tokio::test]
async fn test_retention_plans_authoritative_candidates_in_every_mode(#[case] build: fn(&tempfile::TempDir) -> Config) {
    let dir = tempfile::tempdir().unwrap();
    let (state, router) = writer_node(&build(&dir));
    let root = admin(&state).await;
    let rita = reader(&state).await;
    assert_eq!(upload(&router, "store", UPLOAD).await, StatusCode::OK);

    // An anonymous caller is refused and a reader without administration is answered as if the endpoint
    // did not exist, so a probe learns nothing regardless of the mode.
    assert_eq!(
        send(&router, "POST", "/+retention/plan", None, Some(expire_all("store")))
            .await
            .0,
        StatusCode::UNAUTHORIZED,
    );
    assert_eq!(
        send(
            &router,
            "POST",
            "/+retention/plan",
            Some(&rita),
            Some(expire_all("store"))
        )
        .await
        .0,
        StatusCode::NOT_FOUND,
    );

    // The administrator's plan marks the published release for removal under the prefix rule.
    let (status, plan) = send(
        &router,
        "POST",
        "/+retention/plan",
        Some(&root),
        Some(expire_all("store")),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert!(plan["summary"]["policy_version"].is_number(), "{plan}");
    let candidate = plan["candidates"]
        .as_array()
        .unwrap()
        .iter()
        .find(|decision| decision["project"] == PROJECT)
        .expect("the published project is a candidate");
    assert_eq!(candidate["version"], VERSION);
    assert_eq!(candidate["outcome"], "remove");
    assert_eq!(candidate["rule"], "project-prefix");
}

/// The catalog lists the projects an index serves the same way in every mode: a published release becomes
/// visible in the root Simple index whatever the availability posture.
#[rstest]
#[case::none(none_config as fn(&tempfile::TempDir) -> Config)]
#[case::dc(dc_writer_config as fn(&tempfile::TempDir) -> Config)]
#[case::ha(ha_writer_config as fn(&tempfile::TempDir) -> Config)]
#[tokio::test]
async fn test_the_catalog_lists_published_projects_in_every_mode(#[case] build: fn(&tempfile::TempDir) -> Config) {
    let dir = tempfile::tempdir().unwrap();
    let (state, router) = writer_node(&build(&dir));
    let _root = admin(&state).await;

    // Before any publish the catalog is empty; after one the project is listed.
    let request = Request::builder()
        .method("GET")
        .uri("/store/simple/")
        .header(header::ACCEPT, "application/json")
        .body(Body::empty())
        .unwrap();
    let empty = router.clone().oneshot(request).await.unwrap();
    assert_eq!(empty.status(), StatusCode::OK);
    let empty = empty.into_body().collect().await.unwrap().to_bytes();
    assert!(
        !String::from_utf8_lossy(&empty).contains(PROJECT),
        "the catalog starts empty"
    );

    assert_eq!(upload(&router, "store", UPLOAD).await, StatusCode::OK);

    let request = Request::builder()
        .method("GET")
        .uri("/store/simple/")
        .header(header::ACCEPT, "application/json")
        .body(Body::empty())
        .unwrap();
    let listed = router.clone().oneshot(request).await.unwrap();
    assert_eq!(listed.status(), StatusCode::OK);
    let listed = listed.into_body().collect().await.unwrap().to_bytes();
    let catalog: Value = serde_json::from_slice(&listed).unwrap();
    assert!(
        catalog["projects"]
            .as_array()
            .unwrap()
            .iter()
            .any(|entry| entry["name"] == PROJECT),
        "the published project appears in the catalog: {catalog}",
    );
}

/// A read-only replica originates none of these surfaces: it refuses a job cancel, a publish, and a
/// retention plan at its gate with `503 read_only_replica`, ahead of any handler. Holds for a `dc` and an
/// `ha` follower.
#[rstest]
#[case::dc(dc_replica_config as fn(&tempfile::TempDir, &str) -> Config)]
#[case::ha(ha_replica_config as fn(&tempfile::TempDir, &str) -> Config)]
#[tokio::test]
async fn test_a_read_only_replica_refuses_every_feature_mutation(
    #[case] build: fn(&tempfile::TempDir, &str) -> Config,
) {
    let dir = tempfile::tempdir().unwrap();
    let (state, router, _runtime) = replica_node(&build(&dir, "http://writer.invalid/"));
    let root = admin(&state).await;
    assert!(state.read_only, "a configured replica is read-only");

    for (method, path, body) in [
        ("POST", "/+jobs/jr_00000000000000ff/cancel".to_owned(), None),
        ("POST", "/+retention/plan".to_owned(), Some(expire_all("store"))),
    ] {
        let (status, document) = send(&router, method, &path, Some(&root), body).await;
        assert_eq!(
            status,
            StatusCode::SERVICE_UNAVAILABLE,
            "{path} is refused on a replica"
        );
        assert_eq!(document["error"], "read_only_replica");
    }
    assert_eq!(
        upload(&router, "store", UPLOAD).await,
        StatusCode::SERVICE_UNAVAILABLE,
        "a replica refuses a publish",
    );
}

/// Stand up a primary whose journal already carries [`MANAGED_KEY`], served over an in-process listener a
/// replica can follow. The returned directory keeps the primary's stores alive.
async fn start_primary() -> (tempfile::TempDir, String, tokio::task::JoinHandle<()>) {
    let dir = tempfile::tempdir().unwrap();
    let meta = MetaStore::open(dir.path().join("peryx.redb")).unwrap();
    let blobs = BlobStore::new(dir.path().join("blobs"));
    meta.commit_driver_txn(|txn| {
        txn.put(MANAGED_KEY, MANAGED_VALUE)?;
        Ok::<_, MetaError>(((), vec![b"managed".to_vec()]))
    })
    .unwrap();
    let router = primary_router(WRITER_IDENTITY, TOKEN, meta, blobs).unwrap();
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let task = tokio::spawn(async move { axum::serve(listener, router).await.unwrap() });
    (dir, format!("http://{address}/"), task)
}

/// A partitioned replica surfaces authoritative feature state only within the serial it has applied: the
/// storage read model is gated by the applied frontier, so the primary's later record is invisible and
/// every mutation is refused until one sync cycle brings the state within the frontier.
#[tokio::test]
async fn test_a_replica_surfaces_feature_state_only_within_its_frontier() {
    let (_primary_dir, upstream, serve) = start_primary().await;
    let dir = tempfile::tempdir().unwrap();
    let (state, router, mut runtime) = replica_node(&dc_replica_config(&dir, &upstream));
    let root = admin(&state).await;

    // Partitioned: no cycle has run, so the replica holds nothing past serial zero and cannot see the
    // primary's record. It refuses to originate the state a partition withholds.
    assert_eq!(state.meta.current_serial().unwrap(), 0);
    assert!(state.meta.get_driver_value(MANAGED_KEY).unwrap().is_none());
    let refused = send(
        &router,
        "POST",
        "/+retention/plan",
        Some(&root),
        Some(expire_all("store")),
    )
    .await;
    assert_eq!(refused.0, StatusCode::SERVICE_UNAVAILABLE);
    assert_eq!(refused.1["error"], "read_only_replica");

    // Heal the partition: one cycle applies the page, moving the primary's state within the frontier.
    assert_eq!(runtime.sync_cycle().await, Some(true));
    assert_eq!(state.meta.current_serial().unwrap(), 1);
    assert_eq!(
        state.meta.get_driver_value(MANAGED_KEY).unwrap().as_deref(),
        Some(MANAGED_VALUE),
    );

    serve.abort();
}

/// A control double returning a fixed result and counting its submissions, so a test drives the authority
/// command surface without a live Raft node and proves a retried key never resubmits.
struct FakeControl {
    result: Result<CommandReceipt, ControlError>,
    calls: std::sync::Mutex<usize>,
}

#[async_trait]
impl MembershipControl for FakeControl {
    async fn submit(&self, _command: ControlCommand) -> Result<CommandReceipt, ControlError> {
        *self.calls.lock().unwrap() += 1;
        self.result.clone()
    }
}

/// A `none` writer state carrying an administrator, plus a `dc` writer posture. The command surface reads
/// the control plane a test installs, so the underlying store never needs a live group.
async fn command_node(
    result: Result<CommandReceipt, ControlError>,
) -> (tempfile::TempDir, Arc<AppState>, String, Arc<FakeControl>) {
    let dir = tempfile::tempdir().unwrap();
    let (state, _router) = writer_node(&none_config(&dir));
    let root = admin(&state).await;
    let control = Arc::new(FakeControl {
        result,
        calls: std::sync::Mutex::new(0),
    });
    state.set_control_plane(Arc::new(ControlPlane::new(control.clone(), Arc::new(|| 0))));
    (dir, state, root, control)
}

/// A `transfer_authority` command reassigning a repository's home, optionally under an idempotency `key`.
async fn transfer(state: &Arc<AppState>, auth: &str, key: Option<&str>) -> (StatusCode, Value) {
    let mut request = Request::builder()
        .method("POST")
        .uri("/availability/v1/commands")
        .header(header::CONTENT_TYPE, "application/json")
        .header(header::AUTHORIZATION, basic_header(auth));
    if let Some(value) = key {
        request = request.header("idempotency-key", value);
    }
    let body = json!({ "type": "transfer_authority", "authority": "store", "new_home": "west" });
    let posture = AvailabilityPosture::from_config(&AvailabilityConfig::Dc(primary_replication())).expect("dc posture");
    let coordinator = Arc::new(TransferCoordinator::new(Arc::new(RosterFrontierSource::new(
        Vec::new(),
        "token",
    ))));
    let response = availability_router(state.clone(), posture, coordinator)
        .oneshot(request.body(Body::from(body.to_string())).unwrap())
        .await
        .unwrap();
    let status = response.status();
    let bytes = axum::body::to_bytes(response.into_body(), usize::MAX).await.unwrap();
    (status, serde_json::from_slice(&bytes).unwrap_or(Value::Null))
}

fn basic_header(auth: &str) -> String {
    format!("Basic {}", STANDARD.encode(auth))
}

fn committed(index: u64) -> CommandReceipt {
    CommandReceipt {
        term: 5,
        index,
        outcome: CommandOutcome::Committed,
        old_voters: Vec::new(),
        new_voters: Vec::new(),
    }
}

/// Reassigning a repository's authority home is refused when the node is not the leader: the ownership
/// every storage surface resolves against cannot move from a node that lost leadership, so the transfer
/// answers `503` rather than committing to a minority.
#[tokio::test]
async fn test_authority_transfer_is_refused_when_the_node_is_not_the_leader() {
    let (_dir, state, root, control) = command_node(Err(ControlError::NotLeader {
        leader: Some("east.internal:4460".to_owned()),
    }))
    .await;

    let (status, _) = transfer(&state, &root, None).await;

    assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE);
    assert_eq!(*control.calls.lock().unwrap(), 1, "the command reached the group once");
}

/// A committed authority transfer returns its receipt, and a retried submission under the same
/// idempotency key returns the one committed result without reaching the group again: an authority moves
/// under exactly one valid decision, so a client retry never double-applies it.
#[tokio::test]
async fn test_authority_transfer_commits_and_a_retry_reaches_consensus_once() {
    let (_dir, state, root, control) = command_node(Ok(committed(9))).await;

    let (first_status, first) = transfer(&state, &root, Some("k1")).await;
    let (second_status, second) = transfer(&state, &root, Some("k1")).await;

    assert_eq!((first_status, second_status), (StatusCode::OK, StatusCode::OK));
    assert_eq!(first["index"], 9);
    assert_eq!(first["outcome"], "committed");
    assert_eq!(first, second, "the replay returns the one committed result");
    assert_eq!(
        *control.calls.lock().unwrap(),
        1,
        "the retry never resubmitted to consensus"
    );
}
