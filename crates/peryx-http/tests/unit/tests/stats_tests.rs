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
use peryx_events::metrics::Observation;
use peryx_identity::{Action, Glob, Grant, GrantScope, IndexAcl, NamedToken, PasswordPolicy, Role};
use peryx_policy::Policy;
use peryx_storage::meta::MetaStore;
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

#[derive(Clone, Copy)]
enum StoreFault {
    None,
    Authentication,
    Authorization,
}

async fn app() -> (tempfile::TempDir, Arc<AppState>) {
    app_with_fault(StoreFault::None).await
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
    let mut state = AppState::new(
        meta.clone(),
        blobs,
        60,
        vec![Index {
            name: "private".to_owned(),
            route: "private".to_owned(),
            ecosystem: Ecosystem::new("example"),
            kind: IndexKind::Hosted { volatile: false },
            policy: Policy::default(),
            acl: writer_acl(UPLOAD_SECRET),
        }],
    );
    Arc::get_mut(&mut state.serving).unwrap().users =
        UserService::with_password_settings(meta, PasswordPolicy::new(8, 1, 1).unwrap(), 2);
    (dir, Arc::new(state))
}

async fn get(
    state: &Arc<AppState>,
    uri: &str,
    credential: Option<(&str, &str)>,
) -> (StatusCode, axum::http::HeaderMap, serde_json::Value) {
    let mut request = Request::builder().uri(uri);
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
    (
        status,
        headers,
        serde_json::from_slice(&body).unwrap_or(serde_json::Value::Null),
    )
}

#[rstest]
#[case::operator(Some(("Olivia", USER_PASSWORD)), StatusCode::OK)]
#[case::administrator(Some(("Alice", USER_PASSWORD)), StatusCode::OK)]
#[case::repository_reader(Some(("Rita", USER_PASSWORD)), StatusCode::NOT_FOUND)]
#[case::anonymous(None, StatusCode::UNAUTHORIZED)]
#[case::unknown_user(Some(("external", UPLOAD_SECRET)), StatusCode::UNAUTHORIZED)]
#[case::wrong_password(Some(("Alice", "wrong")), StatusCode::UNAUTHORIZED)]
#[tokio::test]
async fn test_stats_requires_operator_scope(#[case] credential: Option<(&str, &str)>, #[case] expected: StatusCode) {
    let (_dir, state) = app().await;

    let (status, headers, _) = get(&state, "/+stats", credential).await;

    assert_eq!(status, expected);
    assert_eq!(headers[header::CACHE_CONTROL], "no-store");
    if expected == StatusCode::UNAUTHORIZED {
        assert_eq!(headers[header::WWW_AUTHENTICATE], "Basic realm=\"peryx-stats\"");
    }
}

#[tokio::test]
async fn test_stats_operator_receives_the_drill_tree() {
    let (_dir, state) = app().await;

    let (status, _, body) = get(&state, "/+stats", Some(("Olivia", USER_PASSWORD))).await;

    assert_eq!(status, StatusCode::OK);
    assert!(body.is_object(), "stats tree is an object: {body}");
}

#[tokio::test]
async fn test_stats_repository_query_scopes_tree() {
    let (_dir, state) = app().await;
    seed_usage(&state);

    let (status, _, body) = get(&state, "/+stats?repository=private", Some(("Olivia", USER_PASSWORD))).await;

    assert_eq!(
        (status, body),
        (
            StatusCode::OK,
            serde_json::json!({
                "totals": read_counters(2, 30),
                "resources": {
                    "alpha": read_counters(1, 10),
                    "beta": read_counters(1, 20),
                },
            }),
        )
    );
}

#[tokio::test]
async fn test_stats_resource_query_scopes_tree() {
    let (_dir, state) = app().await;
    seed_usage(&state);

    let (status, _, body) = get(
        &state,
        "/+stats?repository=private&resource=alpha",
        Some(("Olivia", USER_PASSWORD)),
    )
    .await;

    assert_eq!(
        (status, body),
        (
            StatusCode::OK,
            serde_json::json!({
                "totals": read_counters(1, 10),
                "artifacts": {
                    "alpha.bin": {"reads": 1, "bytes": 10, "ecosystem": {}},
                },
            }),
        )
    );
}

#[rstest]
#[case::authentication(StoreFault::Authentication)]
#[case::authorization(StoreFault::Authorization)]
#[tokio::test]
async fn test_stats_fails_closed_on_store_faults(#[case] fault: StoreFault) {
    let (_dir, state) = app_with_fault(fault).await;

    let (status, headers, body) = get(&state, "/+stats", Some(("Alice", USER_PASSWORD))).await;

    assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE);
    assert_eq!(headers[header::CACHE_CONTROL], "no-store");
    assert_eq!(body, serde_json::json!({"error": "stats service unavailable"}));
}

fn seed_usage(state: &AppState) {
    for (repository, resource, artifact, bytes) in [
        ("private", "alpha", "alpha.bin", 10),
        ("private", "beta", "beta.bin", 20),
        ("other", "gamma", "gamma.bin", 30),
    ] {
        state.serving.metrics.record(Observation::Read {
            repository: repository.to_owned(),
            resource: resource.to_owned(),
            artifact: artifact.to_owned(),
            group: None,
            source: None,
            bytes,
        });
    }
    state.serving.metrics.flush().unwrap();
}

fn read_counters(reads: u64, bytes: u64) -> serde_json::Value {
    serde_json::json!({
        "base": {"pages": 0, "reads": reads, "bytes": bytes, "rejected": 0},
        "cached": {"refreshes": 0, "changed": 0, "stale_served": 0, "upstream_errors": 0},
        "hosted": {"writes": 0},
        "ecosystem": {},
    })
}
