use std::sync::Arc;

use axum::body::Body;
use axum::http::{HeaderMap, Method, Request, StatusCode, header};
use base64::Engine as _;
use base64::engine::general_purpose::STANDARD;
use http_body_util::BodyExt as _;
use peryx_core::Ecosystem;
use peryx_driver::authz::AuthorizationService;
use peryx_driver::state::{AppState, Index, IndexKind};
use peryx_driver::users::UserService;
use std::collections::BTreeSet;

use peryx_identity::{Action, GrantScope, IndexAcl, PasswordPolicy, Role, TokenName, TokenSecret};
use peryx_policy::Policy;
use peryx_storage::meta::{MetaStore, NewScopedToken};
use rstest::rstest;
use serde_json::{Value, json};
use tower::ServiceExt as _;

const PASSWORD: &str = "local password";

#[derive(Clone, Copy, PartialEq, Eq)]
enum StoreFault {
    None,
    Authentication,
    Authorization,
    Tokens,
    TokenIndex,
}

struct Fixture {
    _dir: tempfile::TempDir,
    app: axum::Router,
    token: Option<String>,
}

impl Fixture {
    async fn new() -> Self {
        Self::build(StoreFault::None, false).await
    }

    async fn with_fault(fault: StoreFault) -> Self {
        Self::build(fault, false).await
    }

    async fn build(fault: StoreFault, preload: bool) -> Self {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("peryx.redb");
        let meta = MetaStore::open(&path).unwrap();
        let users = UserService::with_password_settings(meta.clone(), PasswordPolicy::new(8, 1, 1).unwrap(), 2);
        let authorization = AuthorizationService::new(meta.clone());
        let mut alice = None;
        for (name, role, scope) in [
            ("Alice", Role::Administrator, GrantScope::Server),
            ("Olivia", Role::Operator, GrantScope::Server),
            (
                "Peter",
                Role::RepositoryPublisher,
                GrantScope::Repository {
                    name: "hosted".to_owned(),
                },
            ),
            (
                "Rita",
                Role::RepositoryReader,
                GrantScope::Repository {
                    name: "hosted".to_owned(),
                },
            ),
        ] {
            let user = users.create(name).unwrap();
            users.set_password(&user.id, PASSWORD).await.unwrap();
            authorization.grant(&user.id, role, scope).unwrap();
            if name == "Alice" {
                alice = Some(user.id);
            }
        }
        let token = preload.then(|| {
            meta.create_scoped_token(NewScopedToken {
                name: TokenName::new("preloaded").unwrap(),
                reach: GrantScope::Server,
                actions: BTreeSet::from([Action::Read]),
                expires_at: None,
                verifier: TokenSecret::generate().verifier(),
                created_by: alice.unwrap(),
                created_at_unix: 1000,
            })
            .unwrap()
            .id
            .to_string()
        });
        drop(authorization);
        drop(users);
        drop(meta);
        if let Some(table) = match fault {
            StoreFault::None => None,
            StoreFault::Authentication => Some("server_user_verifier"),
            StoreFault::Authorization => Some("role_grant"),
            StoreFault::Tokens => Some("scoped_token"),
            StoreFault::TokenIndex => Some("scoped_token_verifier"),
        } {
            let database = redb::Database::open(&path).unwrap();
            let txn = database.begin_write().unwrap();
            txn.delete_table(redb::TableDefinition::<&str, &[u8]>::new(table))
                .unwrap();
            txn.open_table(redb::TableDefinition::<&str, u64>::new(table)).unwrap();
            txn.commit().unwrap();
        }
        let meta = MetaStore::open_existing(path).unwrap();
        let users = UserService::with_password_settings(meta.clone(), PasswordPolicy::new(8, 1, 1).unwrap(), 2);
        let blobs = peryx_storage::blob::BlobStore::new(dir.path().join("blobs"));
        let mut state = AppState::with_clock(
            meta,
            blobs,
            60,
            vec![index("hosted"), index("cached")],
            Arc::new(|| 1000),
        );
        Arc::get_mut(&mut state.serving).unwrap().users = users;
        Self {
            _dir: dir,
            app: crate::router(Arc::new(state)),
            token,
        }
    }

    async fn call(
        &self,
        method: Method,
        uri: &str,
        credential: Option<&str>,
        content_type: Option<&str>,
        body: Option<Vec<u8>>,
    ) -> (StatusCode, HeaderMap, Value) {
        let mut request = Request::builder().method(method).uri(uri);
        if let Some(user) = credential {
            request = request.header(
                header::AUTHORIZATION,
                format!("Basic {}", STANDARD.encode(format!("{user}:{PASSWORD}"))),
            );
        }
        if let Some(content_type) = content_type {
            request = request.header(header::CONTENT_TYPE, content_type);
        }
        let body = body.map_or_else(Body::empty, Body::from);
        let response = self.app.clone().oneshot(request.body(body).unwrap()).await.unwrap();
        let status = response.status();
        let headers = response.headers().clone();
        let bytes = response.into_body().collect().await.unwrap().to_bytes();
        (status, headers, serde_json::from_slice(&bytes).unwrap_or(Value::Null))
    }

    async fn create(&self, user: &str, body: Value) -> (StatusCode, HeaderMap, Value) {
        self.call(
            Method::POST,
            "/+tokens",
            Some(user),
            Some("application/json"),
            Some(serde_json::to_vec(&body).unwrap()),
        )
        .await
    }

    async fn get(&self, uri: &str, user: Option<&str>) -> (StatusCode, HeaderMap, Value) {
        self.call(Method::GET, uri, user, None, None).await
    }
}

fn index(name: &str) -> Index {
    Index {
        name: name.to_owned(),
        route: name.to_owned(),
        ecosystem: Ecosystem::new("example"),
        kind: IndexKind::Hosted { volatile: false },
        policy: Policy::default(),
        acl: IndexAcl::default(),
    }
}

#[tokio::test]
async fn test_token_lifecycle_reveals_the_secret_once() {
    let fixture = Fixture::new().await;
    let (status, headers, created) = fixture
        .create("Alice", json!({"name": "ci", "actions": ["read", "write"]}))
        .await;
    assert_eq!(status, StatusCode::CREATED);
    assert_eq!(headers[header::CACHE_CONTROL], "no-store");
    let secret = created["secret"].as_str().unwrap().to_owned();
    assert!(secret.starts_with("peryx_"));
    let id = created["token"]["id"].as_str().unwrap().to_owned();
    assert_eq!(created["token"]["reach"], json!({"kind": "server"}));
    assert_eq!(created["token"]["revoked_at"], Value::Null);
    assert_eq!(created["token"].get("verifier"), None);

    let inspected = fixture.get(&format!("/+tokens/{id}"), Some("Alice")).await;
    assert_eq!(inspected.0, StatusCode::OK);
    assert_eq!(inspected.2.get("secret"), None);
    assert_eq!(inspected.2.get("verifier"), None);
    assert_eq!(inspected.2["id"], id);

    let listed = fixture.get("/+tokens", Some("Alice")).await;
    assert_eq!(listed.0, StatusCode::OK);
    assert_eq!(listed.2["tokens"].as_array().unwrap().len(), 1);

    let rotated = fixture
        .call(
            Method::POST,
            &format!("/+tokens/{id}/rotate"),
            Some("Alice"),
            None,
            None,
        )
        .await;
    assert_eq!(rotated.0, StatusCode::OK);
    let new_secret = rotated.2["secret"].as_str().unwrap();
    assert!(new_secret.starts_with("peryx_"));
    assert_ne!(new_secret, secret);
    assert_eq!(rotated.2["token"]["revision"], 2);

    let revoked = fixture
        .call(Method::DELETE, &format!("/+tokens/{id}"), Some("Alice"), None, None)
        .await;
    assert_eq!(revoked.0, StatusCode::OK);
    assert!(revoked.2["revoked_at"].is_i64());
    let revoke_again = fixture
        .call(Method::DELETE, &format!("/+tokens/{id}"), Some("Alice"), None, None)
        .await;
    assert_eq!(revoke_again.0, StatusCode::OK);
    let rotate_revoked = fixture
        .call(
            Method::POST,
            &format!("/+tokens/{id}/rotate"),
            Some("Alice"),
            None,
            None,
        )
        .await;
    assert_eq!(rotate_revoked.0, StatusCode::NOT_FOUND);
}

#[rstest]
#[case::admin_server("Alice", json!({"name": "t", "actions": ["read"]}), StatusCode::CREATED)]
#[case::admin_repository("Alice", json!({"name": "t", "repository": "hosted", "actions": ["read", "write", "delete"]}), StatusCode::CREATED)]
#[case::publisher_repository("Peter", json!({"name": "t", "repository": "hosted", "actions": ["read", "write", "delete"]}), StatusCode::CREATED)]
#[case::publisher_server("Peter", json!({"name": "t", "actions": ["read"]}), StatusCode::NOT_FOUND)]
#[case::publisher_cross_repository("Peter", json!({"name": "t", "repository": "cached", "actions": ["read"]}), StatusCode::NOT_FOUND)]
#[case::reader_read_only("Rita", json!({"name": "t", "repository": "hosted", "actions": ["read"]}), StatusCode::CREATED)]
#[case::reader_write("Rita", json!({"name": "t", "repository": "hosted", "actions": ["write"]}), StatusCode::NOT_FOUND)]
#[case::operator_server("Olivia", json!({"name": "t", "actions": ["read"]}), StatusCode::NOT_FOUND)]
#[tokio::test]
async fn test_create_validates_the_reach_against_the_callers_authority(
    #[case] user: &str,
    #[case] body: Value,
    #[case] expected: StatusCode,
) {
    let fixture = Fixture::new().await;
    assert_eq!(fixture.create(user, body).await.0, expected);
}

#[rstest]
#[case::create(Method::POST, "/+tokens")]
#[case::list(Method::GET, "/+tokens")]
#[case::inspect(Method::GET, "/+tokens/tok_1")]
#[case::rotate(Method::POST, "/+tokens/tok_1/rotate")]
#[case::revoke(Method::DELETE, "/+tokens/tok_1")]
#[tokio::test]
async fn test_operations_require_a_valid_credential(#[case] method: Method, #[case] uri: &str) {
    let fixture = Fixture::new().await;
    let post = method == Method::POST && uri == "/+tokens";
    for header_value in [
        None,
        Some(format!("Basic {}", STANDARD.encode("Alice:wrong password"))),
        Some(format!("Basic {}", STANDARD.encode(format!("Ghost:{PASSWORD}")))),
    ] {
        let mut request = Request::builder().method(method.clone()).uri(uri);
        if let Some(value) = &header_value {
            request = request.header(header::AUTHORIZATION, value);
        }
        let body = if post {
            request = request.header(header::CONTENT_TYPE, "application/json");
            Body::from(serde_json::to_vec(&json!({"name": "t", "actions": ["read"]})).unwrap())
        } else {
            Body::empty()
        };
        let response = fixture.app.clone().oneshot(request.body(body).unwrap()).await.unwrap();
        assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
        assert_eq!(
            response.headers()[header::WWW_AUTHENTICATE],
            "Basic realm=\"peryx-administration\""
        );
    }
}

#[tokio::test]
async fn test_create_rejects_malformed_requests() {
    let fixture = Fixture::new().await;
    let good = || serde_json::to_vec(&json!({"name": "t", "actions": ["read"]})).unwrap();
    assert_eq!(
        fixture
            .call(
                Method::POST,
                "/+tokens",
                Some("Alice"),
                Some("text/plain"),
                Some(good())
            )
            .await
            .0,
        StatusCode::UNSUPPORTED_MEDIA_TYPE
    );
    assert_eq!(
        fixture
            .call(
                Method::POST,
                "/+tokens",
                Some("Alice"),
                Some("application/json"),
                Some(vec![b'a'; 5 * 1024])
            )
            .await
            .0,
        StatusCode::PAYLOAD_TOO_LARGE
    );
    assert_eq!(
        fixture
            .call(
                Method::POST,
                "/+tokens",
                Some("Alice"),
                Some("application/json; charset=utf-8"),
                Some(b"not json".to_vec())
            )
            .await
            .0,
        StatusCode::UNPROCESSABLE_ENTITY
    );
    assert_eq!(
        fixture
            .create(
                "Alice",
                json!({"name": "t", "actions": ["read"], "created_by": "spoof"})
            )
            .await
            .0,
        StatusCode::UNPROCESSABLE_ENTITY
    );
    assert_eq!(
        fixture
            .create("Alice", json!({"name": "  ", "actions": ["read"]}))
            .await
            .0,
        StatusCode::BAD_REQUEST
    );
    assert_eq!(
        fixture.create("Alice", json!({"name": "t", "actions": []})).await.0,
        StatusCode::BAD_REQUEST
    );
    assert_eq!(
        fixture
            .create("Alice", json!({"name": "t", "actions": ["read"], "expires_at": 1000}))
            .await
            .0,
        StatusCode::BAD_REQUEST
    );
    assert!(
        fixture
            .create("Alice", json!({"name": "t", "actions": ["read"], "expires_at": 5000}))
            .await
            .0
            .is_success()
    );
    assert_eq!(
        fixture
            .create(
                "Alice",
                json!({"name": "t", "repository": "ghost", "actions": ["read"]})
            )
            .await
            .0,
        StatusCode::NOT_FOUND
    );
}

#[tokio::test]
async fn test_list_scopes_by_reach_and_bounds_the_limit() {
    let fixture = Fixture::new().await;
    fixture.create("Alice", json!({"name": "s", "actions": ["read"]})).await;
    fixture
        .create(
            "Peter",
            json!({"name": "h", "repository": "hosted", "actions": ["read"]}),
        )
        .await;

    let server = fixture.get("/+tokens", Some("Alice")).await;
    assert_eq!(server.2["tokens"].as_array().unwrap().len(), 1);
    let hosted = fixture.get("/+tokens?repository=hosted", Some("Peter")).await;
    assert_eq!(hosted.2["tokens"].as_array().unwrap().len(), 1);

    assert_eq!(
        fixture.get("/+tokens?limit=0", Some("Alice")).await.0,
        StatusCode::BAD_REQUEST
    );
    assert_eq!(
        fixture.get("/+tokens?repository=ghost", Some("Alice")).await.0,
        StatusCode::NOT_FOUND
    );
    // A repository reader cannot manage tokens.
    assert_eq!(
        fixture.get("/+tokens?repository=hosted", Some("Rita")).await.0,
        StatusCode::NOT_FOUND
    );
}

#[tokio::test]
async fn test_manage_hides_tokens_from_unauthorized_and_unknown() {
    let fixture = Fixture::new().await;
    let created = fixture
        .create(
            "Peter",
            json!({"name": "h", "repository": "hosted", "actions": ["read"]}),
        )
        .await;
    let id = created.2["token"]["id"].as_str().unwrap().to_owned();

    // A reader cannot inspect a token they cannot manage.
    assert_eq!(
        fixture.get(&format!("/+tokens/{id}"), Some("Rita")).await.0,
        StatusCode::NOT_FOUND
    );
    // Unknown ids answer not found for inspect, rotate, and revoke.
    assert_eq!(
        fixture.get("/+tokens/tok_ghost", Some("Alice")).await.0,
        StatusCode::NOT_FOUND
    );
    assert_eq!(
        fixture
            .call(Method::POST, "/+tokens/tok_ghost/rotate", Some("Alice"), None, None)
            .await
            .0,
        StatusCode::NOT_FOUND
    );
    assert_eq!(
        fixture
            .call(Method::DELETE, "/+tokens/tok_ghost", Some("Alice"), None, None)
            .await
            .0,
        StatusCode::NOT_FOUND
    );
}

#[rstest]
#[case::authentication(StoreFault::Authentication)]
#[case::tokens(StoreFault::Tokens)]
#[case::authorization(StoreFault::Authorization)]
#[tokio::test]
async fn test_store_faults_fail_closed_on_create(#[case] fault: StoreFault) {
    let fixture = Fixture::with_fault(fault).await;
    let response = fixture.create("Alice", json!({"name": "t", "actions": ["read"]})).await;
    assert_eq!(response.0, StatusCode::SERVICE_UNAVAILABLE);
    assert_eq!(response.2, json!({"error": "token service unavailable"}));
}

#[rstest]
#[case::list(Method::GET, "/+tokens")]
#[case::inspect(Method::GET, "/+tokens/tok_1")]
#[case::rotate(Method::POST, "/+tokens/tok_1/rotate")]
#[case::revoke(Method::DELETE, "/+tokens/tok_1")]
#[tokio::test]
async fn test_token_store_faults_fail_closed(#[case] method: Method, #[case] uri: &str) {
    let fixture = Fixture::with_fault(StoreFault::Tokens).await;
    let response = fixture.call(method, uri, Some("Alice"), None, None).await;
    assert_eq!(response.0, StatusCode::SERVICE_UNAVAILABLE);
}

#[tokio::test]
async fn test_mutations_fail_closed_when_the_index_write_fails() {
    // The token resolves for authorization, but rotating and revoking touch a corrupt verifier index.
    let fixture = Fixture::build(StoreFault::TokenIndex, true).await;
    let id = fixture.token.clone().unwrap();
    assert_eq!(
        fixture
            .call(
                Method::POST,
                &format!("/+tokens/{id}/rotate"),
                Some("Alice"),
                None,
                None
            )
            .await
            .0,
        StatusCode::SERVICE_UNAVAILABLE
    );
    assert_eq!(
        fixture
            .call(Method::DELETE, &format!("/+tokens/{id}"), Some("Alice"), None, None)
            .await
            .0,
        StatusCode::SERVICE_UNAVAILABLE
    );
}
