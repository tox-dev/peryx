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
use rstest::rstest;
use tower::ServiceExt as _;

fn writer_acl(secret: impl Into<String>) -> IndexAcl {
    IndexAcl {
        anonymous_read: true,
        tokens: vec![NamedToken {
            name: "uploader".to_owned(),
            secret: secret.into(),
            grants: vec![Grant {
                projects: vec![Glob::new("*")],
                actions: BTreeSet::from([Action::Write, Action::Delete]),
            }],
            expires_at: None,
        }],
    }
}

const UPLOAD_SECRET: &str = "upload-secret";
const USER_PASSWORD: &str = "local password";

const PUBLIC_KEYS: &[&str] = &["version", "role", "health", "indexes"];
const OPERATOR_KEYS: &[&str] = &["serial", "requests", "blob_storage", "by_ecosystem", "metric_families"];

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
    let mut state = AppState::new(meta.clone(), blobs, 60, indexes());
    state.users = UserService::with_password_settings(meta, PasswordPolicy::new(8, 1, 1).unwrap(), 2);
    (dir, Arc::new(state))
}

fn indexes() -> Vec<Index> {
    vec![
        index("private", writer_acl(UPLOAD_SECRET)),
        index("other", IndexAcl::default()),
    ]
}

fn index(route: &str, acl: IndexAcl) -> Index {
    Index {
        name: route.to_owned(),
        route: route.to_owned(),
        ecosystem: Ecosystem::new("example"),
        kind: IndexKind::Hosted { volatile: false },
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

/// Whether any index in the document carries an administrator-class field. The basic route stays
/// public; the upstream, hosted upload-token, and per-repository counts appear only for an admin.
fn indexes_carry_sensitive_fields(body: &serde_json::Value) -> bool {
    body["indexes"].as_array().unwrap().iter().any(|index| {
        index.get("hosted").is_some() || index.get("upstream").is_some() || index.get("project_count").is_some()
    })
}

/// The basic route is always present, so callers below the administrator class still navigate.
fn assert_basic_index_list(body: &serde_json::Value) {
    let routes = body["indexes"]
        .as_array()
        .unwrap()
        .iter()
        .map(|index| index["route"].as_str().unwrap().to_owned())
        .collect::<BTreeSet<_>>();
    assert_eq!(routes, BTreeSet::from(["private".to_owned(), "other".to_owned()]));
}

#[rstest]
#[case::anonymous(None)]
#[case::wrong_password(Some(("Alice", "wrong")))]
#[case::unknown_user(Some(("Ghost", USER_PASSWORD)))]
#[case::repository_reader(Some(("Rita", USER_PASSWORD)))]
#[case::legacy_token(Some(("__token__", UPLOAD_SECRET)))]
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
    assert_eq!(index["project_count"], 0);
    assert_eq!(index["recent_uploads"], serde_json::json!([]));
    assert!(index["hosted"].is_object());
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
