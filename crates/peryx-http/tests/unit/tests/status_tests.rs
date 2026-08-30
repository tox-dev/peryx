use std::collections::BTreeSet;
use std::sync::Arc;

use axum::body::Body;
use axum::http::{Request, StatusCode, header};
use base64::Engine as _;
use base64::engine::general_purpose::STANDARD;
use peryx_core::Ecosystem;
use peryx_driver::authz::AuthorizationService;
use peryx_driver::state::{AppState, Index, IndexKind};
use peryx_driver::users::UserService;
use peryx_identity::{Action, Glob, Grant, GrantScope, IndexAcl, NamedToken, PasswordPolicy, Role};
use peryx_policy::Policy;
use peryx_storage::meta::MetaStore;
use peryx_upstream::{NamedUpstream, UpstreamRouter};
use rstest::rstest;
use tower::ServiceExt as _;

fn writer_acl(secret: impl Into<String>) -> IndexAcl {
    IndexAcl {
        anonymous_read: true,
        tokens: vec![NamedToken {
            name: "uploader".to_owned(),
            secret: secret.into(),
            grants: vec![Grant {
                resources: vec![Glob::new("*")],
                actions: BTreeSet::from([Action::Write, Action::Delete]),
            }],
            expires_at: None,
        }],
    }
}

const UPLOAD_SECRET: &str = "upload-secret";
const USER_PASSWORD: &str = "local password";

const PUBLIC_KEYS: &[&str] = &["version", "role", "health", "indexes"];
const OPERATOR_KEYS: &[&str] = &[
    "serial",
    "requests",
    "blob_storage",
    "by_ecosystem",
    "metric_families",
    "metrics_durability_failure",
];

#[derive(Clone, Copy)]
enum StoreFault {
    None,
    Analytics,
    Authentication,
    Authorization,
}

async fn app() -> (tempfile::TempDir, Arc<AppState>) {
    app_with_fault(StoreFault::None).await
}

async fn app_with_summary_failure() -> (tempfile::TempDir, Arc<AppState>) {
    let (directory, mut state) = app().await;
    Arc::get_mut(&mut Arc::get_mut(&mut state).unwrap().serving)
        .unwrap()
        .indexes
        .push(index(
            "summary-failure",
            IndexAcl::default(),
            IndexKind::Hosted { volatile: false },
        ));
    (directory, state)
}

async fn app_with_password_overload() -> (tempfile::TempDir, Arc<AppState>) {
    let (directory, mut state) = app().await;
    let serving = Arc::get_mut(&mut Arc::get_mut(&mut state).unwrap().serving).unwrap();
    serving.users = UserService::with_password_settings(serving.meta.clone(), PasswordPolicy::new(8, 1, 1).unwrap(), 0);
    (directory, state)
}

async fn app_with_fault(fault: StoreFault) -> (tempfile::TempDir, Arc<AppState>) {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("peryx.redb");
    let meta = MetaStore::open(&path).unwrap();
    let users = UserService::with_password_settings(meta.clone(), PasswordPolicy::new(8, 1, 1).unwrap(), 2);
    let authorization = AuthorizationService::new(meta.clone());
    for (name, role, scope) in [
        ("Alice", Role::Administrator, GrantScope::Server),
        ("Olivia", Role::Operator, GrantScope::Server),
        (
            "Rita",
            Role::RepositoryReader,
            GrantScope::Repository {
                name: "private".to_owned(),
            },
        ),
    ] {
        let user = users.create(name).unwrap();
        users.set_password(&user.id, USER_PASSWORD).await.unwrap();
        authorization.grant(&user.id, role, scope).unwrap();
    }
    drop(authorization);
    drop(users);
    drop(meta);
    if let Some(table) = match fault {
        StoreFault::None => None,
        StoreFault::Analytics => Some("analytics"),
        StoreFault::Authentication => Some("server_user_verifier"),
        StoreFault::Authorization => Some("role_grant"),
    } {
        let database = redb::Database::open(&path).unwrap();
        let transaction = database.begin_write().unwrap();
        transaction
            .delete_table(redb::TableDefinition::<&str, &[u8]>::new(table))
            .unwrap();
        transaction
            .open_table(redb::TableDefinition::<&str, u64>::new(table))
            .unwrap();
        transaction.commit().unwrap();
    }
    let meta = MetaStore::open_existing(path).unwrap();
    let blobs = peryx_storage::blob::BlobStore::new(dir.path().join("blobs"));
    let (indexes, upstream_route) = indexes().await;
    let mut state = AppState::new(meta.clone(), blobs, 60, indexes);
    super::support::register_example_driver(&mut state);
    let serving = Arc::get_mut(&mut state.serving).unwrap();
    serving.upstream_routes.insert("reachable".to_owned(), upstream_route);
    serving.users = UserService::with_password_settings(meta, PasswordPolicy::new(8, 1, 1).unwrap(), 2);
    (dir, Arc::new(state))
}

async fn indexes() -> (Vec<Index>, UpstreamRouter) {
    let server = wiremock::MockServer::start().await;
    let reachable = peryx_upstream::UpstreamClient::new(&server.uri()).unwrap();
    reachable.warm().await;
    let unreachable = peryx_upstream::UpstreamClient::new("http://127.0.0.1:1/").unwrap();
    unreachable.warm().await;
    let route = UpstreamRouter::new(vec![NamedUpstream::new("origin", reachable.clone())]).unwrap();
    let mut missing_driver = index(
        "missing-driver",
        IndexAcl::default(),
        IndexKind::Hosted { volatile: false },
    );
    missing_driver.ecosystem = Ecosystem::new("missing");
    (
        vec![
            index(
                "private",
                writer_acl(UPLOAD_SECRET),
                IndexKind::Hosted { volatile: false },
            ),
            index("other", IndexAcl::default(), IndexKind::Hosted { volatile: false }),
            index(
                "reachable",
                IndexAcl::default(),
                IndexKind::Cached {
                    client: reachable,
                    offline: false,
                },
            ),
            index(
                "unreachable",
                IndexAcl::default(),
                IndexKind::Cached {
                    client: unreachable,
                    offline: false,
                },
            ),
            index(
                "unknown",
                IndexAcl::default(),
                IndexKind::Cached {
                    client: peryx_upstream::UpstreamClient::new("https://unknown.example/artifacts/").unwrap(),
                    offline: false,
                },
            ),
            index(
                "offline",
                IndexAcl::default(),
                IndexKind::Cached {
                    client: peryx_upstream::UpstreamClient::new("https://offline.example/artifacts/").unwrap(),
                    offline: true,
                },
            ),
            index(
                "aggregate",
                IndexAcl::default(),
                IndexKind::Virtual {
                    layers: vec![0, 1],
                    write_target: Some(0),
                },
            ),
            missing_driver,
        ],
        route,
    )
}

fn index(route: &str, acl: IndexAcl, kind: IndexKind) -> Index {
    Index {
        name: route.to_owned(),
        route: route.to_owned(),
        ecosystem: Ecosystem::new("example"),
        kind,
        policy: Policy::default(),
        acl,
    }
}

async fn get(
    state: &Arc<AppState>,
    credential: Option<(&str, &str)>,
) -> (StatusCode, axum::http::HeaderMap, serde_json::Value) {
    let mut request = Request::builder().uri("/+status");
    if let Some((user, password)) = credential {
        request = request.header(
            header::AUTHORIZATION,
            format!("Basic {}", STANDARD.encode(format!("{user}:{password}"))),
        );
    }
    let response = crate::router(state.clone())
        .oneshot(request.body(Body::empty()).unwrap())
        .await
        .unwrap();
    let status = response.status();
    let headers = response.headers().clone();
    let body = axum::body::to_bytes(response.into_body(), usize::MAX).await.unwrap();
    (status, headers, serde_json::from_slice(&body).unwrap())
}

fn keys(body: &serde_json::Value) -> BTreeSet<String> {
    body.as_object().unwrap().keys().cloned().collect()
}

fn assert_keys(body: &serde_json::Value, present: &[&[&str]], absent: &[&[&str]]) {
    let keys = keys(body);
    for key in present.iter().flat_map(|group| group.iter()) {
        assert!(keys.contains(*key), "missing {key}: {keys:?}");
    }
    for key in absent.iter().flat_map(|group| group.iter()) {
        assert!(!keys.contains(*key), "leaked {key}: {keys:?}");
    }
}

fn indexes_carry_sensitive_fields(body: &serde_json::Value) -> bool {
    body["indexes"].as_array().unwrap().iter().any(|index| {
        index.get("hosted").is_some() || index.get("upstream").is_some() || index.get("resource_count").is_some()
    })
}

fn assert_basic_index_list(body: &serde_json::Value) {
    let routes = body["indexes"]
        .as_array()
        .unwrap()
        .iter()
        .map(|index| index["route"].as_str().unwrap().to_owned())
        .collect::<BTreeSet<_>>();
    assert_eq!(
        routes,
        BTreeSet::from([
            "aggregate".to_owned(),
            "missing-driver".to_owned(),
            "offline".to_owned(),
            "other".to_owned(),
            "private".to_owned(),
            "reachable".to_owned(),
            "unknown".to_owned(),
            "unreachable".to_owned(),
        ])
    );
}

#[rstest]
#[case::anonymous(None)]
#[case::wrong_password(Some(("Alice", "wrong")))]
#[case::unknown_user(Some(("Ghost", USER_PASSWORD)))]
#[case::repository_reader(Some(("Rita", USER_PASSWORD)))]
#[case::unknown_external_user(Some(("external", UPLOAD_SECRET)))]
#[tokio::test]
async fn test_status_reveals_only_public_fields_below_operator(#[case] credential: Option<(&str, &str)>) {
    let (_dir, state) = app().await;

    let (status, headers, body) = get(&state, credential).await;

    assert_eq!(status, StatusCode::OK);
    assert_eq!(headers[header::CACHE_CONTROL], "private, no-cache");
    assert_keys(&body, &[PUBLIC_KEYS], &[OPERATOR_KEYS]);
    assert_basic_index_list(&body);
    assert!(!indexes_carry_sensitive_fields(&body), "{body}");
}

#[tokio::test]
async fn test_status_operator_sees_counters_but_not_the_sensitive_index_fields() {
    let (_dir, state) = app().await;

    let (status, _, body) = get(&state, Some(("Olivia", USER_PASSWORD))).await;

    assert_eq!(status, StatusCode::OK);
    assert_keys(&body, &[PUBLIC_KEYS, OPERATOR_KEYS], &[]);
    assert_basic_index_list(&body);
    assert!(!indexes_carry_sensitive_fields(&body), "{body}");
}

#[tokio::test]
async fn test_status_administrator_sees_the_sensitive_index_fields() {
    let (_dir, state) = app().await;

    let (status, _, body) = get(&state, Some(("Alice", USER_PASSWORD))).await;

    assert_eq!(status, StatusCode::OK);
    assert_keys(&body, &[PUBLIC_KEYS, OPERATOR_KEYS], &[]);
    assert_basic_index_list(&body);
    assert!(indexes_carry_sensitive_fields(&body), "{body}");
    let index = &body["indexes"][0];
    assert_eq!(index["resource_count"], 1);
    assert_eq!(index["write_count"], 1);
    assert_eq!(index["recent_writes"][0]["artifact"], "artifact.bin");
    assert!(index["hosted"].is_object());
    assert_eq!(body["health"]["upstreams"]["reachable"], 1);
    assert_eq!(body["health"]["upstreams"]["unreachable"], 1);
    assert_eq!(body["health"]["upstreams"]["unknown"], 1);
    assert_eq!(body["health"]["upstreams"]["disabled"], 1);
    assert_eq!(body["indexes"][6]["precedence"].as_array().unwrap().len(), 2);
    let indexes = body["indexes"].as_array().unwrap();
    let missing_driver = indexes.iter().find(|index| index["route"] == "missing-driver").unwrap();
    assert_eq!(missing_driver["endpoint"], "/missing-driver/");
    let reachable = indexes.iter().find(|index| index["route"] == "reachable").unwrap();
    assert_eq!(reachable["upstream"]["sources"][0]["name"], "origin");
}

#[tokio::test]
async fn test_status_operator_sees_durable_metrics_startup_failure() {
    let (_dir, state) = app_with_fault(StoreFault::Analytics).await;

    let (_, _, body) = get(&state, Some(("Olivia", USER_PASSWORD))).await;

    assert!(
        body["metrics_durability_failure"]
            .as_str()
            .is_some_and(|error| error.contains("analytics")),
        "{body}"
    );
}

#[tokio::test]
async fn test_status_administrator_sees_summary_failures() {
    let (_dir, state) = app_with_summary_failure().await;

    let (status, _, body) = get(&state, Some(("Alice", USER_PASSWORD))).await;

    assert_eq!(status, StatusCode::OK);
    let index = body["indexes"]
        .as_array()
        .unwrap()
        .iter()
        .find(|index| index["route"] == "summary-failure")
        .unwrap();
    assert_eq!(
        index["summary"],
        serde_json::json!({"status": "unavailable", "error_class": "storage"})
    );
    assert_eq!((index.get("resource_count"), index.get("write_count")), (None, None));
    assert!(!body.to_string().contains("summary failed"), "{body}");
}

#[tokio::test]
async fn test_status_reports_password_overload() {
    let (_directory, state) = app_with_password_overload().await;

    let (status, headers, body) = get(&state, Some(("Unknown", USER_PASSWORD))).await;

    assert_eq!(
        (status, body),
        (
            StatusCode::SERVICE_UNAVAILABLE,
            serde_json::json!({"error": "identity service unavailable"}),
        )
    );
    assert_eq!(headers[header::CACHE_CONTROL], "no-store");
}

#[rstest]
#[case::authentication(StoreFault::Authentication)]
#[case::authorization(StoreFault::Authorization)]
#[tokio::test]
async fn test_status_fails_closed_to_public_on_store_faults(#[case] fault: StoreFault) {
    let (_dir, state) = app_with_fault(fault).await;

    let (status, _, body) = get(&state, Some(("Alice", USER_PASSWORD))).await;

    assert_eq!(status, StatusCode::OK);
    assert_keys(&body, &[PUBLIC_KEYS], &[OPERATOR_KEYS]);
    assert!(!indexes_carry_sensitive_fields(&body), "{body}");
}
