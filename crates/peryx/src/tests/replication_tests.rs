use std::collections::{BTreeMap, BTreeSet};
use std::num::NonZeroUsize;
use std::time::Duration;

use axum::Router;
use axum::body::Body;
use axum::http::{Request, StatusCode, header};
use http_body_util::BodyExt as _;
use peryx_driver::IndexKind as RuntimeIndexKind;
use peryx_identity::Action;
use peryx_replication::{ChangePage, primary_router};
use peryx_storage::blob::BlobStore;
use peryx_storage::meta::MetaStore;
use rstest::rstest;
use tower::ServiceExt as _;

use crate::config::{
    Config, IndexKind, ReplicationConfig, SecretSource, TokenConfig, UpstreamConfig, UpstreamRoutingConfig,
    WebhookConfig, WebhookSecret,
};
use crate::replication::ReplicationRuntime;
use crate::server::{build_router, build_state, router_for};

const TOKEN: &str = "replica-secret";

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
    Config {
        data_dir: dir.path().to_path_buf(),
        replication,
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

fn primary_stores() -> (tempfile::TempDir, MetaStore, BlobStore) {
    let dir = tempfile::tempdir().unwrap();
    let meta = MetaStore::open(dir.path().join("peryx.redb")).unwrap();
    let blobs = BlobStore::new(dir.path().join("blobs"));
    (dir, meta, blobs)
}

async fn get(router: &Router, path: &str) -> (StatusCode, Vec<u8>) {
    let response = router
        .clone()
        .oneshot(Request::get(path).body(Body::empty()).unwrap())
        .await
        .unwrap();
    let status = response.status();
    let body = response.into_body().collect().await.unwrap().to_bytes().to_vec();
    (status, body)
}

#[tokio::test]
async fn test_primary_runtime_mounts_authenticated_routes() {
    let dir = tempfile::tempdir().unwrap();
    let config = config(
        &dir,
        Some(ReplicationConfig::Primary {
            source: "primary-a".to_owned(),
            token: SecretSource::Literal(TOKEN.to_owned()),
        }),
    );
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
    let runtime = ReplicationRuntime::new(&config, &state).unwrap();

    assert!(runtime.is_replica());
    let subscriber = tracing_subscriber::fmt().with_writer(std::io::sink).finish();
    let guard = tracing::subscriber::set_default(subscriber);
    assert_eq!(runtime.sync_cycle().await, Some(false));
    drop(guard);

    let task = runtime.start().unwrap();
    tokio::time::timeout(Duration::from_secs(2), async {
        while state.meta.current_serial().unwrap() != 3 {
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    })
    .await
    .unwrap();
    tokio::time::sleep(Duration::from_millis(20)).await;
    task.abort();

    assert_eq!(state.meta.journal_after(0, 10).unwrap().len(), 3);
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
    let runtime = ReplicationRuntime::new(&config, &state).unwrap();

    assert_eq!(runtime.sync_cycle().await, Some(true));
    assert_eq!(
        state.meta.get_driver_value("pypi\0upload").unwrap().as_deref(),
        Some(b"record".as_slice())
    );
}

#[tokio::test]
async fn test_replica_runtime_waits_after_a_sync_error() {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}/", listener.local_addr().unwrap());
    drop(listener);
    let dir = tempfile::tempdir().unwrap();
    let config = config(&dir, Some(replica_config(&url, 10)));
    let state = build_state(&config).unwrap();
    let runtime = ReplicationRuntime::new(&config, &state).unwrap();

    assert_eq!(runtime.sync_cycle().await, Some(true));
    let router = runtime.mount(router_for(state));
    let (status, body) = get(&router, "/+replication/v1/health").await;
    let health: serde_json::Value = serde_json::from_slice(&body).unwrap();
    assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE);
    assert_eq!(health["status"], "error");
    let (_, body) = get(&router, "/metrics").await;
    assert!(
        String::from_utf8(body)
            .unwrap()
            .contains("peryx_replication_sync_errors_total 1\n")
    );
}

#[tokio::test]
async fn test_replica_health_starts_unready() {
    let dir = tempfile::tempdir().unwrap();
    let config = config(&dir, Some(replica_config("https://primary.example/", 10)));
    let state = build_state(&config).unwrap();
    let runtime = ReplicationRuntime::new(&config, &state).unwrap();
    let router = runtime.mount(router_for(state));

    let (status, body) = get(&router, "/+replication/v1/health").await;
    let health: serde_json::Value = serde_json::from_slice(&body).unwrap();
    assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE);
    assert_eq!(
        health,
        serde_json::json!({"status": "starting", "serial": 0, "primary_serial": null, "lag": null})
    );
    let (_, body) = get(&router, "/metrics").await;
    let metrics = String::from_utf8(body).unwrap();
    assert!(metrics.contains("peryx_replication_caught_up 0\n"));
    assert!(metrics.contains("peryx_replication_serial 0\n"));
    assert!(!metrics.contains("peryx_replication_primary_serial "));
}

#[tokio::test]
async fn test_replica_health_and_metrics_track_catch_up() {
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
    let runtime = ReplicationRuntime::new(&config, &state).unwrap();
    let router = runtime.mount(router_for(state));

    assert_eq!(runtime.sync_cycle().await, Some(false));
    let (status, body) = get(&router, "/+replication/v1/health").await;
    let health: serde_json::Value = serde_json::from_slice(&body).unwrap();
    assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE);
    assert_eq!(
        health,
        serde_json::json!({"status": "catching_up", "serial": 2, "primary_serial": 3, "lag": 1})
    );
    let (_, body) = get(&router, "/metrics").await;
    let metrics = String::from_utf8(body).unwrap();
    assert!(metrics.contains("peryx_replication_changes_total 2\n"));
    assert!(metrics.contains("peryx_replication_primary_serial 3\n"));
    assert!(metrics.contains("peryx_replication_lag 1\n"));

    assert_eq!(runtime.sync_cycle().await, Some(true));
    let (status, body) = get(&router, "/+replication/v1/health").await;
    let health: serde_json::Value = serde_json::from_slice(&body).unwrap();
    assert_eq!(status, StatusCode::OK);
    assert_eq!(
        health,
        serde_json::json!({"status": "caught_up", "serial": 3, "primary_serial": 3, "lag": 0})
    );
    let (_, body) = get(&router, "/metrics").await;
    let metrics = String::from_utf8(body).unwrap();
    assert!(metrics.contains("peryx_replication_caught_up 1\n"));
    assert!(metrics.contains("peryx_replication_changes_total 3\n"));
    assert!(metrics.contains("peryx_replication_lag 0\n"));
}

#[tokio::test]
async fn test_disabled_runtime_mounts_no_routes_or_task() {
    let dir = tempfile::tempdir().unwrap();
    let config = config(&dir, None);
    let state = build_state(&config).unwrap();
    let runtime = ReplicationRuntime::new(&config, &state).unwrap();

    assert!(!runtime.is_replica());
    assert_eq!(runtime.sync_cycle().await, None);
    let router = runtime.mount(router_for(state));
    assert!(runtime.start().is_none());
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
    let (_, body) = get(&router, "/metrics").await;
    assert!(!String::from_utf8(body).unwrap().contains("peryx_replication_"));
}

#[test]
fn test_replica_runtime_disables_local_writers() {
    let dir = tempfile::tempdir().unwrap();
    let mut config = config(&dir, Some(replica_config("https://primary.example/", 10)));
    let IndexKind::Cached { password, routing, .. } = &mut config.indexes[0].kind else {
        panic!("expected the default cached index");
    };
    *password = Some(SecretSource::File("missing-upstream-password".into()));
    *routing = Some(Box::new(UpstreamRoutingConfig {
        upstreams: vec![UpstreamConfig {
            name: "primary".to_owned(),
            url: "https://packages.example/simple/".to_owned(),
            username: Some("replica".to_owned()),
            password: Some(SecretSource::File("missing-routed-upstream-password".into())),
            token: None,
        }],
        fallback: true,
        protected: Vec::new(),
        pins: BTreeMap::default(),
    }));
    let IndexKind::Hosted { upload_token, .. } = &mut config.indexes[1].kind else {
        panic!("expected the default hosted index");
    };
    *upload_token = Some(SecretSource::File("missing-upload-token".into()));
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

    assert!(error.to_string().contains("build replica HTTP client"), "{error}");
}
