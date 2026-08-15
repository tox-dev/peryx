use std::sync::Arc;

use axum::body::Body;
use axum::http::{Method, Request, StatusCode, header};
use base64::Engine as _;
use base64::engine::general_purpose::STANDARD;
use http_body_util::BodyExt as _;
use peryx_driver::authz::AuthorizationService;
use peryx_driver::state::AppState;
use peryx_driver::users::UserService;
use peryx_identity::{GrantScope, PasswordPolicy, Role};
use peryx_storage::meta::MetaStore;
use rstest::rstest;
use tower::ServiceExt as _;

const ADMIN_PASSWORD: &str = "administrator password";
const OPERATOR_PASSWORD: &str = "operator password";
const DIGEST: &str = "sha256:0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef";

struct Fixture {
    _dir: tempfile::TempDir,
    app: axum::Router,
    administrator: peryx_identity::UserId,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum StoreFault {
    None,
    Authentication,
    Revocations,
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
        let administrator = users.create("Alice").unwrap().id;
        users.set_password(&administrator, ADMIN_PASSWORD).await.unwrap();
        AuthorizationService::new(meta.clone())
            .grant(&administrator, Role::Administrator, GrantScope::Server)
            .unwrap();
        let operator = users.create("Olivia").unwrap().id;
        users.set_password(&operator, OPERATOR_PASSWORD).await.unwrap();
        AuthorizationService::new(meta.clone())
            .grant(&operator, Role::Operator, GrantScope::Server)
            .unwrap();
        drop(users);
        drop(meta);
        match fault {
            StoreFault::None => {}
            StoreFault::Authentication => {
                let database = redb::Database::open(&path).unwrap();
                let txn = database.begin_write().unwrap();
                txn.open_table(redb::TableDefinition::<&str, &[u8]>::new("server_user_verifier"))
                    .unwrap()
                    .insert(administrator.as_str(), b"{".as_slice())
                    .unwrap();
                txn.commit().unwrap();
            }
            StoreFault::Revocations => {
                let database = redb::Database::open(&path).unwrap();
                let txn = database.begin_write().unwrap();
                txn.delete_table(redb::TableDefinition::<&str, &[u8]>::new("digest_revocation"))
                    .unwrap();
                txn.open_table(redb::TableDefinition::<&str, u64>::new("digest_revocation"))
                    .unwrap();
                txn.commit().unwrap();
            }
        }
        let meta = MetaStore::open_existing(path).unwrap();
        let users = UserService::with_password_settings(meta.clone(), PasswordPolicy::new(8, 1, 1).unwrap(), 2);
        let blobs = peryx_storage::blob::BlobStore::new(dir.path().join("blobs"));
        let mut state = AppState::with_clock(meta, blobs, 60, Vec::new(), Arc::new(|| 42));
        Arc::get_mut(&mut state.serving).unwrap().users = users;
        Self {
            _dir: dir,
            app: crate::router(Arc::new(state)),
            administrator,
        }
    }

    async fn request(
        &self,
        method: Method,
        uri: &str,
        credential: Option<(&str, &str)>,
        body: Option<serde_json::Value>,
    ) -> (StatusCode, axum::http::HeaderMap, serde_json::Value) {
        let body = body.map(|value| serde_json::to_vec(&value).unwrap());
        self.raw_request(method, uri, credential, body, Some("application/json"))
            .await
    }

    async fn raw_request(
        &self,
        method: Method,
        uri: &str,
        credential: Option<(&str, &str)>,
        body: Option<Vec<u8>>,
        content_type: Option<&str>,
    ) -> (StatusCode, axum::http::HeaderMap, serde_json::Value) {
        let mut request = Request::builder().method(method).uri(uri);
        if let Some((user, password)) = credential {
            request = request.header(
                header::AUTHORIZATION,
                format!("Basic {}", STANDARD.encode(format!("{user}:{password}"))),
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
        let document = serde_json::from_slice(&bytes).unwrap_or(serde_json::Value::Null);
        (status, headers, document)
    }
}

#[tokio::test]
async fn test_revocation_http_runs_create_inspect_list_lift_and_reopen() {
    let fixture = Fixture::new().await;
    let credential = Some(("Alice", ADMIN_PASSWORD));
    let (status, headers, created) = fixture
        .request(
            Method::PUT,
            &format!("/+revocations/{DIGEST}"),
            credential,
            Some(serde_json::json!({"reason": "compromised build host"})),
        )
        .await;
    assert_eq!(status, StatusCode::CREATED);
    assert_eq!(headers[header::LOCATION], format!("/+revocations/{DIGEST}"));
    assert_eq!(headers[header::CACHE_CONTROL], "no-store");
    assert_eq!(created["digest"], serde_json::json!({"sha256": &DIGEST[7..]}));
    assert_eq!(created["reason"], "compromised build host");
    assert_eq!(created["created_by"], fixture.administrator.as_str());
    assert_eq!(created["created_at_unix"], 42);
    assert_eq!(created["state"], serde_json::json!({"status": "active"}));
    assert_eq!(created["revision"], 1);

    let retry = fixture
        .request(
            Method::PUT,
            &format!("/+revocations/{DIGEST}"),
            credential,
            Some(serde_json::json!({"reason": "compromised build host"})),
        )
        .await;
    assert_eq!((retry.0, retry.2), (StatusCode::OK, created.clone()));
    assert_eq!(
        fixture
            .request(Method::GET, &format!("/+revocations/{DIGEST}"), credential, None)
            .await
            .2,
        created
    );
    let active_page = fixture
        .request(Method::GET, "/+revocations?status=active&limit=1", credential, None)
        .await;
    assert_eq!(active_page.0, StatusCode::OK);
    assert_eq!(active_page.1[header::CACHE_CONTROL], "no-store");
    assert_eq!(active_page.2["revocations"].as_array().unwrap().len(), 1);
    assert_eq!(
        fixture
            .request(Method::GET, &format!("/+revocations?cursor={DIGEST}"), credential, None,)
            .await
            .2["revocations"],
        serde_json::json!([])
    );

    let lifted = fixture
        .request(Method::POST, &format!("/+revocations/{DIGEST}/lift"), credential, None)
        .await;
    assert_eq!(lifted.0, StatusCode::OK);
    assert_eq!(lifted.2["state"]["status"], "lifted");
    assert_eq!(lifted.2["state"]["lifted_by"], fixture.administrator.as_str());
    assert_eq!(lifted.2["revision"], 2);
    let lift_retry = fixture
        .request(Method::POST, &format!("/+revocations/{DIGEST}/lift"), credential, None)
        .await;
    assert_eq!(lift_retry.2, lifted.2);

    let reopened = fixture
        .request(
            Method::PUT,
            &format!("/+revocations/{DIGEST}"),
            credential,
            Some(serde_json::json!({"reason": "second incident"})),
        )
        .await;
    assert_eq!(reopened.0, StatusCode::CREATED);
    assert_eq!(reopened.2["reason"], "second incident");
    assert_eq!(reopened.2["state"]["status"], "active");
    assert_eq!(reopened.2["revision"], 3);
}

#[rstest]
#[case::put(Method::PUT, format!("/+revocations/{DIGEST}"), Some(serde_json::json!({"reason": "incident"})))]
#[case::inspect(Method::GET, format!("/+revocations/{DIGEST}"), None)]
#[case::list(Method::GET, "/+revocations".to_owned(), None)]
#[case::lift(Method::POST, format!("/+revocations/{DIGEST}/lift"), None)]
#[tokio::test]
async fn test_revocation_http_hides_records_from_non_administrators(
    #[case] method: Method,
    #[case] uri: String,
    #[case] body: Option<serde_json::Value>,
) {
    let fixture = Fixture::new().await;
    for (credential, expected) in [
        (None, StatusCode::UNAUTHORIZED),
        (Some(("Alice", "wrong password")), StatusCode::UNAUTHORIZED),
        (Some(("Unknown", ADMIN_PASSWORD)), StatusCode::UNAUTHORIZED),
        (Some(("Olivia", OPERATOR_PASSWORD)), StatusCode::NOT_FOUND),
    ] {
        let response = fixture.request(method.clone(), &uri, credential, body.clone()).await;
        assert_eq!(response.0, expected);
        assert_eq!(response.2, serde_json::Value::Null);
    }
    let anonymous = fixture.request(method, &uri, None, body).await;
    assert_eq!(
        anonymous.1[header::WWW_AUTHENTICATE],
        "Basic realm=\"peryx-administration\""
    );
}

#[rstest]
#[case::put(Method::PUT, format!("/+revocations/{DIGEST}"), Some(serde_json::json!({"reason": "incident"})))]
#[case::inspect(Method::GET, format!("/+revocations/{DIGEST}"), None)]
#[case::list(Method::GET, "/+revocations".to_owned(), None)]
#[case::lift(Method::POST, format!("/+revocations/{DIGEST}/lift"), None)]
#[tokio::test]
async fn test_revocation_http_fails_closed_on_revocation_store_errors(
    #[case] method: Method,
    #[case] uri: String,
    #[case] body: Option<serde_json::Value>,
) {
    let fixture = Fixture::with_fault(StoreFault::Revocations).await;

    let response = fixture
        .request(method, &uri, Some(("Alice", ADMIN_PASSWORD)), body)
        .await;

    assert_eq!(response.0, StatusCode::SERVICE_UNAVAILABLE);
    assert_eq!(
        response.2,
        serde_json::json!({"error": "revocation service unavailable"})
    );
}

#[tokio::test]
async fn test_revocation_http_fails_closed_on_authentication_store_errors() {
    let fixture = Fixture::with_fault(StoreFault::Authentication).await;

    let response = fixture
        .request(
            Method::GET,
            &format!("/+revocations/{DIGEST}"),
            Some(("Alice", ADMIN_PASSWORD)),
            None,
        )
        .await;

    assert_eq!(response.0, StatusCode::SERVICE_UNAVAILABLE);
    assert_eq!(
        response.2,
        serde_json::json!({"error": "revocation service unavailable"})
    );
}

#[rstest]
#[case::put(Method::PUT, "/+revocations/not-a-digest", Some(serde_json::json!({"reason": "incident"})))]
#[case::inspect(Method::GET, "/+revocations/not-a-digest", None)]
#[case::lift(Method::POST, "/+revocations/not-a-digest/lift", None)]
#[tokio::test]
async fn test_revocation_http_rejects_invalid_digests(
    #[case] method: Method,
    #[case] uri: &str,
    #[case] body: Option<serde_json::Value>,
) {
    let fixture = Fixture::new().await;

    let response = fixture
        .request(method, uri, Some(("Alice", ADMIN_PASSWORD)), body)
        .await;

    assert_eq!(response.0, StatusCode::BAD_REQUEST);
    assert_eq!(response.2, serde_json::json!({"error": "invalid digest"}));
}

#[tokio::test]
async fn test_revocation_http_rejects_invalid_inputs_and_conflicts() {
    let fixture = Fixture::new().await;
    let credential = Some(("Alice", ADMIN_PASSWORD));
    assert_eq!(
        fixture
            .raw_request(
                Method::PUT,
                &format!("/+revocations/{DIGEST}"),
                credential,
                Some(b"{}".to_vec()),
                None,
            )
            .await
            .0,
        StatusCode::UNSUPPORTED_MEDIA_TYPE
    );
    assert_eq!(
        fixture
            .raw_request(
                Method::PUT,
                &format!("/+revocations/{DIGEST}"),
                credential,
                Some(b"not json".to_vec()),
                Some("application/json; charset=utf-8"),
            )
            .await
            .0,
        StatusCode::UNPROCESSABLE_ENTITY
    );
    assert_eq!(
        fixture
            .raw_request(
                Method::PUT,
                &format!("/+revocations/{DIGEST}"),
                credential,
                Some(vec![b'a'; 3 * 1024 + 1]),
                Some("application/json"),
            )
            .await
            .0,
        StatusCode::PAYLOAD_TOO_LARGE
    );
    assert_eq!(
        fixture
            .request(
                Method::PUT,
                &format!("/+revocations/{DIGEST}"),
                credential,
                Some(serde_json::json!({"reason": "incident", "created_by": "spoofed"})),
            )
            .await
            .0,
        StatusCode::UNPROCESSABLE_ENTITY
    );
    assert_eq!(
        fixture
            .request(Method::GET, "/+revocations?cursor=bad", credential, None)
            .await
            .0,
        StatusCode::BAD_REQUEST
    );
    assert_eq!(
        fixture
            .request(Method::GET, "/+revocations?limit=0", credential, None)
            .await
            .0,
        StatusCode::BAD_REQUEST
    );
    assert_eq!(
        fixture
            .request(
                Method::PUT,
                &format!("/+revocations/{DIGEST}"),
                credential,
                Some(serde_json::json!({"reason": "   "})),
            )
            .await
            .0,
        StatusCode::BAD_REQUEST
    );
    fixture
        .request(
            Method::PUT,
            &format!("/+revocations/{DIGEST}"),
            credential,
            Some(serde_json::json!({"reason": "first"})),
        )
        .await;
    assert_eq!(
        fixture
            .request(
                Method::PUT,
                &format!("/+revocations/{DIGEST}"),
                credential,
                Some(serde_json::json!({"reason": "different"})),
            )
            .await
            .0,
        StatusCode::CONFLICT
    );
}

#[tokio::test]
async fn test_revocation_http_authenticates_before_parsing_a_put_body() {
    let fixture = Fixture::new().await;

    assert_eq!(
        fixture
            .raw_request(
                Method::PUT,
                "/+revocations/not-a-digest",
                None,
                Some(b"not json".to_vec()),
                None,
            )
            .await
            .0,
        StatusCode::UNAUTHORIZED
    );
}

#[tokio::test]
async fn test_revocation_http_returns_not_found_for_unknown_digest_and_lift() {
    let fixture = Fixture::new().await;
    let credential = Some(("Alice", ADMIN_PASSWORD));
    assert_eq!(
        fixture
            .request(Method::GET, &format!("/+revocations/{DIGEST}"), credential, None)
            .await
            .0,
        StatusCode::NOT_FOUND
    );
    assert_eq!(
        fixture
            .request(Method::POST, &format!("/+revocations/{DIGEST}/lift"), credential, None,)
            .await
            .0,
        StatusCode::NOT_FOUND
    );
}
