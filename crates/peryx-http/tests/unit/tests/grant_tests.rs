use std::sync::Arc;

use axum::body::Body;
use axum::http::{Method, Request, StatusCode, header};
use base64::Engine as _;
use base64::engine::general_purpose::STANDARD;
use http_body_util::BodyExt as _;
use peryx_driver::authz::AuthorizationService;
use peryx_driver::state::AppState;
use peryx_driver::users::UserService;
use peryx_identity::{GrantScope, PasswordPolicy, Role, RoleGrant, UserId};
use peryx_storage::meta::MetaStore;
use rstest::rstest;
use serde_json::{Value, json};
use tower::ServiceExt as _;

const ADMIN: &str = "administrator password";
const RITA: &str = "repository admin password";
const PAUL: &str = "publisher password";
const REPO: &str = "root/alpha";

#[derive(Clone, Copy, PartialEq, Eq)]
enum StoreFault {
    None,
    Authentication,
    Grants,
    ScopeIndex,
    Record,
}

struct Fixture {
    _dir: tempfile::TempDir,
    app: axum::Router,
    administrator: UserId,
    ted: UserId,
    dan: UserId,
    seeded: String,
}

impl Fixture {
    async fn new() -> Self {
        Self::with_fault(StoreFault::None).await
    }

    async fn with_fault(fault: StoreFault) -> Self {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("peryx.redb");
        let meta = MetaStore::open(&path).unwrap();
        let users = UserService::with_password_settings(meta.clone(), PasswordPolicy::new(8, 1, 1).unwrap(), 2);
        let authz = AuthorizationService::new(meta.clone());

        let administrator = users.create("Alice").unwrap().id;
        users.set_password(&administrator, ADMIN).await.unwrap();
        authz
            .grant(&administrator, Role::Administrator, GrantScope::Server)
            .unwrap();

        let rita = users.create("Rita").unwrap().id;
        users.set_password(&rita, RITA).await.unwrap();
        authz.grant(&rita, Role::Administrator, repository(REPO)).unwrap();

        let publisher = users.create("Paul").unwrap().id;
        users.set_password(&publisher, PAUL).await.unwrap();
        authz
            .grant(&publisher, Role::RepositoryPublisher, repository(REPO))
            .unwrap();

        let ted = users.create("Ted").unwrap().id;
        let dan = users.create("Dan").unwrap().id;
        meta.set_user_state(&dan, peryx_identity::UserState::Disabled).unwrap();

        let seeded = authz
            .create_managed_grant(
                &RoleGrant::new(ted.clone(), Role::RepositoryReader, repository(REPO)),
                &administrator,
                10,
            )
            .unwrap()
            .record
            .id();

        drop((users, authz, meta));
        apply_fault(&path, fault, &administrator, &ted);

        let meta = MetaStore::open_existing(&path).unwrap();
        let users = UserService::with_password_settings(meta.clone(), PasswordPolicy::new(8, 1, 1).unwrap(), 2);
        let blobs = peryx_storage::blob::BlobStore::new(dir.path().join("blobs"));
        let mut state = AppState::with_clock(meta, blobs, 60, Vec::new(), Arc::new(|| 42));
        Arc::get_mut(&mut state.serving).unwrap().users = users;
        Self {
            _dir: dir,
            app: crate::router(Arc::new(state)),
            administrator,
            ted,
            dan,
            seeded,
        }
    }

    async fn request(
        &self,
        method: Method,
        uri: &str,
        credential: Option<(&str, &str)>,
        body: Option<Value>,
    ) -> (StatusCode, axum::http::HeaderMap, Value) {
        let body = body.map(|value| serde_json::to_vec(&value).unwrap());
        self.raw(
            method,
            uri,
            RawRequest {
                credential,
                body,
                content_type: Some("application/json"),
                ..RawRequest::default()
            },
        )
        .await
    }

    async fn raw(
        &self,
        method: Method,
        uri: &str,
        input: RawRequest<'_>,
    ) -> (StatusCode, axum::http::HeaderMap, Value) {
        let mut request = Request::builder().method(method).uri(uri);
        if let Some((user, password)) = input.credential {
            request = request.header(
                header::AUTHORIZATION,
                format!("Basic {}", STANDARD.encode(format!("{user}:{password}"))),
            );
        }
        if let Some(content_type) = input.content_type {
            request = request.header(header::CONTENT_TYPE, content_type);
        }
        if let Some(if_match) = input.if_match {
            request = request.header(header::IF_MATCH, if_match);
        }
        let body = input.body.map_or_else(Body::empty, Body::from);
        let response = self.app.clone().oneshot(request.body(body).unwrap()).await.unwrap();
        let status = response.status();
        let headers = response.headers().clone();
        let bytes = response.into_body().collect().await.unwrap().to_bytes();
        let document = serde_json::from_slice(&bytes).unwrap_or(Value::Null);
        (status, headers, document)
    }
}

#[derive(Default)]
struct RawRequest<'a> {
    credential: Option<(&'a str, &'a str)>,
    body: Option<Vec<u8>>,
    content_type: Option<&'a str>,
    if_match: Option<&'a str>,
}

fn repository(name: &str) -> GrantScope {
    GrantScope::Repository { name: name.to_owned() }
}

fn body(user: &UserId, role: &str, scope: Value) -> Value {
    Value::Object(serde_json::Map::from_iter([
        ("user".to_owned(), json!(user.as_str())),
        ("role".to_owned(), json!(role)),
        ("scope".to_owned(), scope),
    ]))
}

fn repo_scope() -> Value {
    json!({"kind": "repository", "name": REPO})
}

fn apply_fault(path: &std::path::Path, fault: StoreFault, administrator: &UserId, ted: &UserId) {
    match fault {
        StoreFault::None => {}
        StoreFault::Authentication => corrupt_value(path, "server_user_verifier", administrator.as_str()),
        StoreFault::Grants => corrupt_prefix(path, "role_grant", &format!("{administrator}/")),
        StoreFault::Record => corrupt_prefix(path, "role_grant", &format!("{ted}/")),
        StoreFault::ScopeIndex => {
            let database = redb::Database::open(path).unwrap();
            let txn = database.begin_write().unwrap();
            txn.delete_table(redb::TableDefinition::<&str, &[u8]>::new("role_grant_by_scope"))
                .unwrap();
            txn.open_table(redb::TableDefinition::<&str, u64>::new("role_grant_by_scope"))
                .unwrap();
            txn.commit().unwrap();
        }
    }
}

fn corrupt_value(path: &std::path::Path, table: &'static str, key: &str) {
    let database = redb::Database::open(path).unwrap();
    let txn = database.begin_write().unwrap();
    txn.open_table(redb::TableDefinition::<&str, &[u8]>::new(table))
        .unwrap()
        .insert(key, b"{".as_slice())
        .unwrap();
    txn.commit().unwrap();
}

fn corrupt_prefix(path: &std::path::Path, table: &'static str, prefix: &str) {
    let definition = redb::TableDefinition::<&str, &[u8]>::new(table);
    let database = redb::Database::open(path).unwrap();
    let txn = database.begin_write().unwrap();
    let keys = {
        let handle = txn.open_table(definition).unwrap();
        redb::ReadableTable::iter(&handle)
            .unwrap()
            .map(|entry| entry.unwrap().0.value().to_owned())
            .filter(|key| key.starts_with(prefix))
            .collect::<Vec<_>>()
    };
    let mut handle = txn.open_table(definition).unwrap();
    for key in keys {
        handle.insert(key.as_str(), b"{".as_slice()).unwrap();
    }
    drop(handle);
    txn.commit().unwrap();
}

#[tokio::test]
async fn test_an_administrator_creates_inspects_lists_and_revokes_a_grant() {
    let fixture = Fixture::new().await;
    let admin = Some(("Alice", ADMIN));

    let (status, headers, created) = fixture
        .request(
            Method::POST,
            "/+grants",
            admin,
            Some(body(&fixture.ted, "repository_publisher", repo_scope())),
        )
        .await;
    assert_eq!(status, StatusCode::CREATED);
    let id = created["id"].as_str().unwrap().to_owned();
    assert_eq!(headers[header::ETAG], "\"1\"");
    assert_eq!(headers[header::LOCATION], format!("/+grants/{id}"));
    assert_eq!(headers[header::CACHE_CONTROL], "no-store");
    assert_eq!(created["user"], fixture.ted.as_str());
    assert_eq!(created["role"], "repository_publisher");
    assert_eq!(created["scope"], repo_scope());
    assert_eq!(created["version"], 1);
    assert_eq!(created["granted_by"], fixture.administrator.as_str());
    assert_eq!(created["granted_at_unix"], 42);

    let reassert = fixture
        .request(
            Method::POST,
            "/+grants",
            admin,
            Some(body(&fixture.ted, "repository_publisher", repo_scope())),
        )
        .await;
    assert_eq!(reassert.0, StatusCode::OK);
    assert_eq!(reassert.2["version"], 2);

    let inspect = fixture
        .request(Method::GET, &format!("/+grants/{id}"), admin, None)
        .await;
    assert_eq!(inspect.0, StatusCode::OK);
    assert_eq!(inspect.1[header::ETAG], "\"2\"");
    assert_eq!(inspect.2["version"], 2);

    let listed = fixture
        .request(Method::GET, &format!("/+grants?user={}", fixture.ted), admin, None)
        .await;
    assert_eq!(listed.0, StatusCode::OK);
    assert_eq!(listed.1[header::CACHE_CONTROL], "no-store");
    let ids = listed.2["grants"]
        .as_array()
        .unwrap()
        .iter()
        .map(|grant| grant["id"].as_str().unwrap().to_owned())
        .collect::<Vec<_>>();
    assert_eq!(ids.len(), 2);
    assert!(ids.contains(&id));

    let stale = fixture
        .raw(
            Method::DELETE,
            &format!("/+grants/{id}"),
            RawRequest {
                credential: admin,
                if_match: Some("\"1\""),
                ..RawRequest::default()
            },
        )
        .await;
    assert_eq!(stale.0, StatusCode::PRECONDITION_FAILED);
    assert_eq!(stale.1[header::ETAG], "\"2\"");

    let removed = fixture
        .raw(
            Method::DELETE,
            &format!("/+grants/{id}"),
            RawRequest {
                credential: admin,
                if_match: Some("\"2\""),
                ..RawRequest::default()
            },
        )
        .await;
    assert_eq!(removed.0, StatusCode::NO_CONTENT);
    assert_eq!(
        fixture
            .request(Method::GET, &format!("/+grants/{id}"), admin, None)
            .await
            .0,
        StatusCode::NOT_FOUND
    );
    let again = fixture
        .raw(
            Method::DELETE,
            &format!("/+grants/{id}"),
            RawRequest {
                credential: admin,
                if_match: Some("\"2\""),
                ..RawRequest::default()
            },
        )
        .await;
    assert_eq!(again.0, StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn test_an_administrator_grants_over_the_whole_server() {
    let fixture = Fixture::new().await;
    let created = fixture
        .request(
            Method::POST,
            "/+grants",
            Some(("Alice", ADMIN)),
            Some(body(&fixture.ted, "operator", json!({"kind": "server"}))),
        )
        .await;
    assert_eq!(created.0, StatusCode::CREATED);
    assert_eq!(created.2["scope"], json!({"kind": "server"}));
}

#[tokio::test]
async fn test_an_inert_role_and_scope_pairing_is_rejected() {
    let fixture = Fixture::new().await;
    let rejected = fixture
        .request(
            Method::POST,
            "/+grants",
            Some(("Alice", ADMIN)),
            Some(body(&fixture.ted, "operator", repo_scope())),
        )
        .await;
    assert_eq!(rejected.0, StatusCode::UNPROCESSABLE_ENTITY);
    assert_eq!(rejected.2["error"], "role does not apply to the scope");
}

#[tokio::test]
async fn test_a_repository_administrator_manages_only_its_repository() {
    let fixture = Fixture::new().await;
    let rita = Some(("Rita", RITA));

    let own = fixture
        .request(
            Method::POST,
            "/+grants",
            rita,
            Some(body(&fixture.ted, "repository_reader", repo_scope())),
        )
        .await;
    assert_eq!(own.0, StatusCode::OK);

    let sibling = fixture
        .request(
            Method::POST,
            "/+grants",
            rita,
            Some(body(
                &fixture.ted,
                "repository_reader",
                json!({"kind": "repository", "name": "team/web"}),
            )),
        )
        .await;
    assert_eq!(sibling.0, StatusCode::FORBIDDEN);

    let server = fixture
        .request(
            Method::POST,
            "/+grants",
            rita,
            Some(body(&fixture.ted, "operator", json!({"kind": "server"}))),
        )
        .await;
    assert_eq!(server.0, StatusCode::FORBIDDEN);

    assert_eq!(
        fixture.request(Method::GET, "/+grants", rita, None).await.0,
        StatusCode::FORBIDDEN
    );
    assert_eq!(
        fixture
            .request(Method::GET, &format!("/+grants?resource=repository/{REPO}"), rita, None)
            .await
            .0,
        StatusCode::OK
    );
}

#[tokio::test]
async fn test_a_publisher_holds_no_delegation_authority() {
    let fixture = Fixture::new().await;
    let paul = Some(("Paul", PAUL));
    assert_eq!(
        fixture
            .request(
                Method::POST,
                "/+grants",
                paul,
                Some(body(&fixture.ted, "repository_reader", repo_scope()))
            )
            .await
            .0,
        StatusCode::FORBIDDEN
    );
    assert_eq!(
        fixture
            .request(Method::GET, &format!("/+grants/{}", fixture.seeded), paul, None)
            .await
            .0,
        StatusCode::NOT_FOUND
    );
    assert_eq!(
        fixture
            .raw(
                Method::DELETE,
                &format!("/+grants/{}", fixture.seeded),
                RawRequest {
                    credential: paul,
                    if_match: Some("\"1\""),
                    ..RawRequest::default()
                },
            )
            .await
            .0,
        StatusCode::NOT_FOUND
    );
}

#[rstest]
#[case::no_credential(None)]
#[case::wrong_password(Some(("Alice", "wrong")))]
#[tokio::test]
async fn test_an_unauthenticated_request_is_challenged(#[case] credential: Option<(&'static str, &'static str)>) {
    let fixture = Fixture::new().await;
    assert_eq!(
        fixture.request(Method::GET, "/+grants", credential, None).await.0,
        StatusCode::UNAUTHORIZED
    );
}

#[tokio::test]
async fn test_every_mutating_route_challenges_an_anonymous_request() {
    let fixture = Fixture::new().await;
    let uri = format!("/+grants/{}", fixture.seeded);
    assert_eq!(
        fixture
            .request(
                Method::POST,
                "/+grants",
                None,
                Some(body(&fixture.ted, "operator", json!({"kind": "server"})))
            )
            .await
            .0,
        StatusCode::UNAUTHORIZED
    );
    assert_eq!(
        fixture.request(Method::GET, &uri, None, None).await.0,
        StatusCode::UNAUTHORIZED
    );
    assert_eq!(
        fixture
            .raw(
                Method::DELETE,
                &uri,
                RawRequest {
                    if_match: Some("\"1\""),
                    ..RawRequest::default()
                },
            )
            .await
            .0,
        StatusCode::UNAUTHORIZED
    );
}

#[tokio::test]
async fn test_a_grant_to_an_unknown_or_disabled_user_is_rejected() {
    let fixture = Fixture::new().await;
    let admin = Some(("Alice", ADMIN));

    let unknown = fixture
        .request(
            Method::POST,
            "/+grants",
            admin,
            Some(body(&UserId::random(), "operator", json!({"kind": "server"}))),
        )
        .await;
    assert_eq!(unknown.0, StatusCode::UNPROCESSABLE_ENTITY);
    assert_eq!(unknown.2["error"], "user does not exist");

    let disabled = fixture
        .request(
            Method::POST,
            "/+grants",
            admin,
            Some(body(&fixture.dan, "operator", json!({"kind": "server"}))),
        )
        .await;
    assert_eq!(disabled.0, StatusCode::UNPROCESSABLE_ENTITY);
    assert_eq!(disabled.2["error"], "user is disabled");
}

#[tokio::test]
async fn test_revocation_demands_a_well_formed_precondition() {
    let fixture = Fixture::new().await;
    let admin = Some(("Alice", ADMIN));
    let uri = format!("/+grants/{}", fixture.seeded);

    let missing = fixture
        .raw(
            Method::DELETE,
            &uri,
            RawRequest {
                credential: admin,
                ..RawRequest::default()
            },
        )
        .await;
    assert_eq!(missing.0, StatusCode::PRECONDITION_REQUIRED);

    let malformed = fixture
        .raw(
            Method::DELETE,
            &uri,
            RawRequest {
                credential: admin,
                if_match: Some("not-a-version"),
                ..RawRequest::default()
            },
        )
        .await;
    assert_eq!(malformed.0, StatusCode::BAD_REQUEST);
}

#[tokio::test]
async fn test_a_create_request_must_carry_a_valid_json_body() {
    let fixture = Fixture::new().await;
    let admin = Some(("Alice", ADMIN));

    let wrong_type = fixture
        .raw(
            Method::POST,
            "/+grants",
            RawRequest {
                credential: admin,
                body: Some(b"{}".to_vec()),
                content_type: Some("text/plain"),
                ..RawRequest::default()
            },
        )
        .await;
    assert_eq!(wrong_type.0, StatusCode::UNSUPPORTED_MEDIA_TYPE);

    let too_large = fixture
        .raw(
            Method::POST,
            "/+grants",
            RawRequest {
                credential: admin,
                body: Some(vec![b'x'; 5 * 1024]),
                content_type: Some("application/json"),
                ..RawRequest::default()
            },
        )
        .await;
    assert_eq!(too_large.0, StatusCode::PAYLOAD_TOO_LARGE);

    let invalid = fixture
        .request(
            Method::POST,
            "/+grants",
            admin,
            Some(json!({"user": fixture.ted.as_str()})),
        )
        .await;
    assert_eq!(invalid.0, StatusCode::UNPROCESSABLE_ENTITY);
}

#[tokio::test]
async fn test_listing_filters_are_validated() {
    let fixture = Fixture::new().await;
    let admin = Some(("Alice", ADMIN));

    assert_eq!(
        fixture
            .request(
                Method::GET,
                &format!("/+grants?user={}&resource=server", fixture.ted),
                admin,
                None
            )
            .await
            .0,
        StatusCode::BAD_REQUEST
    );
    assert_eq!(
        fixture
            .request(Method::GET, "/+grants?resource=team", admin, None)
            .await
            .0,
        StatusCode::BAD_REQUEST
    );
    assert_eq!(
        fixture.request(Method::GET, "/+grants?limit=0", admin, None).await.0,
        StatusCode::BAD_REQUEST
    );
    assert_eq!(
        fixture
            .request(Method::GET, "/+grants?resource=server", admin, None)
            .await
            .0,
        StatusCode::OK
    );
}

#[rstest]
#[case::absent_id("rg_00")]
#[case::malformed_id("not-an-id")]
#[tokio::test]
async fn test_an_unknown_grant_is_not_found(#[case] id: &str) {
    let fixture = Fixture::new().await;
    let admin = Some(("Alice", ADMIN));
    assert_eq!(
        fixture
            .request(Method::GET, &format!("/+grants/{id}"), admin, None)
            .await
            .0,
        StatusCode::NOT_FOUND
    );
    assert_eq!(
        fixture
            .raw(
                Method::DELETE,
                &format!("/+grants/{id}"),
                RawRequest {
                    credential: admin,
                    if_match: Some("\"1\""),
                    ..RawRequest::default()
                },
            )
            .await
            .0,
        StatusCode::NOT_FOUND
    );
}

#[rstest]
#[case::authentication(StoreFault::Authentication, Method::GET, "/+grants")]
#[case::grants(StoreFault::Grants, Method::GET, "/+grants")]
#[tokio::test]
async fn test_a_read_fault_before_authorization_is_unavailable(
    #[case] fault: StoreFault,
    #[case] method: Method,
    #[case] uri: &str,
) {
    let fixture = Fixture::with_fault(fault).await;
    assert_eq!(
        fixture.request(method, uri, Some(("Alice", ADMIN)), None).await.0,
        StatusCode::SERVICE_UNAVAILABLE
    );
}

#[tokio::test]
async fn test_a_scope_index_fault_makes_mutations_and_resource_lists_unavailable() {
    let fixture = Fixture::with_fault(StoreFault::ScopeIndex).await;
    let admin = Some(("Alice", ADMIN));

    assert_eq!(
        fixture
            .request(
                Method::POST,
                "/+grants",
                admin,
                Some(body(&fixture.ted, "operator", json!({"kind": "server"})))
            )
            .await
            .0,
        StatusCode::SERVICE_UNAVAILABLE
    );
    assert_eq!(
        fixture
            .request(
                Method::GET,
                &format!("/+grants?resource=repository/{REPO}"),
                admin,
                None
            )
            .await
            .0,
        StatusCode::SERVICE_UNAVAILABLE
    );
    assert_eq!(
        fixture
            .raw(
                Method::DELETE,
                &format!("/+grants/{}", fixture.seeded),
                RawRequest {
                    credential: admin,
                    if_match: Some("\"1\""),
                    ..RawRequest::default()
                },
            )
            .await
            .0,
        StatusCode::SERVICE_UNAVAILABLE
    );
}

#[tokio::test]
async fn test_a_corrupt_record_makes_its_inspection_unavailable() {
    let fixture = Fixture::with_fault(StoreFault::Record).await;
    assert_eq!(
        fixture
            .request(
                Method::GET,
                &format!("/+grants/{}", fixture.seeded),
                Some(("Alice", ADMIN)),
                None
            )
            .await
            .0,
        StatusCode::SERVICE_UNAVAILABLE
    );
}
