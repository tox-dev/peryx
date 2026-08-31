use std::sync::Arc;

use async_trait::async_trait;
use axum::body::Body;
use axum::http::{Method, Request, StatusCode, header};
use base64::Engine as _;
use base64::engine::general_purpose::STANDARD;
use http_body_util::BodyExt as _;
use peryx_core::Ecosystem;
use peryx_driver::authz::AuthorizationService;
use peryx_driver::serving::{CachePurgeDriver, PurgeReport};
use peryx_driver::state::{AppState, Index, IndexKind, ServingState};
use peryx_driver::users::UserService;
use peryx_identity::{GrantScope, IndexAcl, PasswordPolicy, Role};
use peryx_policy::Policy;
use peryx_storage::meta::MetaStore;
use rstest::rstest;
use tower::ServiceExt as _;

const ADMIN_PASSWORD: &str = "administrator password";
const OPERATOR_PASSWORD: &str = "operator password";

/// Records what the handler asked for, so a test can pin the normalized repository and the `apply`
/// flag the request carried without reaching into the driver.
struct RecordingPurger {
    calls: Arc<std::sync::Mutex<Vec<(String, String, bool)>>>,
    failure: Option<&'static str>,
}

#[async_trait]
impl CachePurgeDriver for RecordingPurger {
    async fn purge_served_resource(
        &self,
        _state: Arc<ServingState>,
        index: &str,
        resource: &str,
        apply: bool,
    ) -> Result<PurgeReport, String> {
        self.calls
            .lock()
            .unwrap()
            .push((index.to_owned(), resource.to_owned(), apply));
        if let Some(failure) = self.failure {
            return Err(failure.to_owned());
        }
        Ok(PurgeReport {
            resource: resource.to_lowercase(),
            categories: vec![("index_pages".to_owned(), 1), ("project_records".to_owned(), 2)],
        })
    }
}

/// The ecosystem's cache-purge capability as a test sees it: registered and working, registered but
/// reporting a driver failure, or never registered at all.
#[derive(Clone, Copy)]
enum Purger {
    Working,
    Failing(&'static str),
    Absent,
}

struct Fixture {
    _dir: tempfile::TempDir,
    app: axum::Router,
    calls: Arc<std::sync::Mutex<Vec<(String, String, bool)>>>,
}

impl Fixture {
    async fn new() -> Self {
        Self::build(Purger::Working).await
    }

    async fn failing(reason: &'static str) -> Self {
        Self::build(Purger::Failing(reason)).await
    }

    async fn without_driver() -> Self {
        Self::build(Purger::Absent).await
    }

    async fn with_unreadable_users() -> Self {
        Self::assemble(Purger::Working, true).await
    }

    async fn build(purger: Purger) -> Self {
        Self::assemble(purger, false).await
    }

    async fn assemble(purger: Purger, corrupt_verifier: bool) -> Self {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("peryx.redb");
        let meta = MetaStore::open(&path).unwrap();
        let users = UserService::with_password_settings(meta.clone(), PasswordPolicy::new(8, 1, 1).unwrap(), 2);
        let authorization = AuthorizationService::new(meta.clone());
        let mut administrator = None;
        for (name, password, role) in [
            ("Alice", ADMIN_PASSWORD, Role::Administrator),
            ("Olivia", OPERATOR_PASSWORD, Role::Operator),
        ] {
            let account = users.create(name).unwrap().id;
            users.set_password(&account, password).await.unwrap();
            authorization.grant(&account, role, GrantScope::Server).unwrap();
            administrator.get_or_insert(account);
        }
        let administrator = administrator.expect("the fixture creates an administrator first");
        drop(users);
        drop(authorization);
        drop(meta);
        if corrupt_verifier {
            corrupt_verifier_row(&path, administrator.as_str());
        }
        let meta = MetaStore::open_existing(&path).unwrap();
        let blobs = peryx_storage::blob::BlobStore::new(dir.path().join("blobs"));
        let mut state = AppState::new(
            meta.clone(),
            blobs,
            60,
            vec![cached_index("pypi", Ecosystem::new("example"))],
        );
        let serving = Arc::get_mut(&mut state.serving).unwrap();
        serving.users = UserService::with_password_settings(meta, PasswordPolicy::new(8, 1, 1).unwrap(), 2);
        let calls = Arc::new(std::sync::Mutex::new(Vec::new()));
        if let Purger::Working | Purger::Failing(_) = purger {
            let driver = Arc::new(RecordingPurger {
                calls: calls.clone(),
                failure: match purger {
                    Purger::Failing(reason) => Some(reason),
                    Purger::Working | Purger::Absent => None,
                },
            });
            state.register_capabilities(|registrar| {
                registrar.register_cache_purge(Ecosystem::new("example"), driver);
            });
        }
        Self {
            _dir: dir,
            app: crate::router(Arc::new(state)),
            calls,
        }
    }

    async fn purge(&self, credential: Option<(&str, &str)>, body: &str, json: bool) -> (StatusCode, serde_json::Value) {
        let mut request = Request::builder().method(Method::POST).uri("/+cache/purge");
        if let Some((user, password)) = credential {
            request = request.header(
                header::AUTHORIZATION,
                format!("Basic {}", STANDARD.encode(format!("{user}:{password}"))),
            );
        }
        if json {
            request = request.header(header::CONTENT_TYPE, "application/json");
        }
        let response = self
            .app
            .clone()
            .oneshot(request.body(Body::from(body.to_owned())).unwrap())
            .await
            .unwrap();
        let status = response.status();
        let cached = response.headers().get(header::CACHE_CONTROL).cloned();
        assert_eq!(cached.as_ref().map(|value| value.to_str().unwrap()), Some("no-store"));
        let body = response.into_body().collect().await.unwrap().to_bytes();
        (status, serde_json::from_slice(&body).unwrap_or(serde_json::Value::Null))
    }
}

/// Leaves the administrator's stored password verifier undecodable, so authentication fails with a
/// directory error rather than a rejected credential.
fn corrupt_verifier_row(path: &std::path::Path, administrator: &str) {
    let database = redb::Database::open(path).unwrap();
    let transaction = database.begin_write().unwrap();
    {
        let mut table = transaction
            .open_table(redb::TableDefinition::<&str, &[u8]>::new("server_user_verifier"))
            .unwrap();
        table.insert(administrator, b"{ not json".as_slice()).unwrap();
    }
    transaction.commit().unwrap();
}

fn cached_index(name: &str, ecosystem: Ecosystem) -> Index {
    Index {
        name: name.to_owned(),
        route: name.to_owned(),
        ecosystem,
        kind: IndexKind::Hosted { volatile: false },
        policy: Policy::default(),
        acl: IndexAcl {
            anonymous_read: true,
            tokens: Vec::new(),
        },
    }
}

#[tokio::test]
async fn test_purge_removes_the_resource_and_reports_the_counts() {
    let fixture = Fixture::new().await;

    let (status, body) = fixture
        .purge(
            Some(("Alice", ADMIN_PASSWORD)),
            r#"{"repository":"pypi","resource":"Flask","apply":true}"#,
            true,
        )
        .await;

    assert_eq!(status, StatusCode::OK);
    assert_eq!(
        body,
        serde_json::json!({
            "repository": "pypi",
            "resource": "flask",
            "applied": true,
            "removed": {"index_pages": 1, "project_records": 2},
        })
    );
    assert_eq!(
        *fixture.calls.lock().unwrap(),
        vec![("pypi".to_owned(), "Flask".to_owned(), true)]
    );
}

#[tokio::test]
async fn test_purge_preview_reports_without_confirming() {
    let fixture = Fixture::new().await;

    let (status, body) = fixture
        .purge(
            Some(("Alice", ADMIN_PASSWORD)),
            r#"{"repository":"pypi","resource":"flask"}"#,
            true,
        )
        .await;

    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["applied"], serde_json::json!(false));
    assert_eq!(
        *fixture.calls.lock().unwrap(),
        vec![("pypi".to_owned(), "flask".to_owned(), false)]
    );
}

#[rstest]
#[case::no_credential(None, r#"{"repository":"pypi","resource":"flask"}"#, true, StatusCode::UNAUTHORIZED)]
#[case::wrong_password(
    Some(("Alice", "not the password")),
    r#"{"repository":"pypi","resource":"flask"}"#,
    true,
    StatusCode::UNAUTHORIZED
)]
#[case::operator_preview(
    Some(("Olivia", OPERATOR_PASSWORD)),
    r#"{"repository":"pypi","resource":"flask"}"#,
    true,
    StatusCode::NOT_FOUND
)]
#[case::operator_confirmation(
    Some(("Olivia", OPERATOR_PASSWORD)),
    r#"{"repository":"pypi","resource":"flask","apply":true}"#,
    true,
    StatusCode::NOT_FOUND
)]
#[case::not_json(
    Some(("Alice", ADMIN_PASSWORD)),
    r#"{"repository":"pypi","resource":"flask"}"#,
    false,
    StatusCode::UNSUPPORTED_MEDIA_TYPE
)]
#[case::unparseable(Some(("Alice", ADMIN_PASSWORD)), "{ not json", true, StatusCode::UNPROCESSABLE_ENTITY)]
#[case::unknown_field(
    Some(("Alice", ADMIN_PASSWORD)),
    r#"{"repository":"pypi","resource":"flask","force":true}"#,
    true,
    StatusCode::UNPROCESSABLE_ENTITY
)]
#[case::unknown_repository(
    Some(("Alice", ADMIN_PASSWORD)),
    r#"{"repository":"absent","resource":"flask"}"#,
    true,
    StatusCode::NOT_FOUND
)]
#[tokio::test]
async fn test_purge_rejects_a_malformed_request(
    #[case] credential: Option<(&str, &str)>,
    #[case] body: &str,
    #[case] json: bool,
    #[case] expected: StatusCode,
) {
    let fixture = Fixture::new().await;

    let (status, _) = fixture.purge(credential, body, json).await;

    assert_eq!(status, expected);
    assert!(fixture.calls.lock().unwrap().is_empty());
}

#[tokio::test]
async fn test_purge_rejects_a_body_past_the_limit() {
    let fixture = Fixture::new().await;
    let resource = "f".repeat(16 * 1024);

    let (status, body) = fixture
        .purge(
            Some(("Alice", ADMIN_PASSWORD)),
            &format!(r#"{{"repository":"pypi","resource":"{resource}"}}"#),
            true,
        )
        .await;

    assert_eq!(status, StatusCode::PAYLOAD_TOO_LARGE);
    assert_eq!(body["error"], serde_json::json!("request body is too large"));
}

#[tokio::test]
async fn test_purge_is_not_found_for_an_ecosystem_that_purges_no_cache() {
    let fixture = Fixture::without_driver().await;

    let (status, _) = fixture
        .purge(
            Some(("Alice", ADMIN_PASSWORD)),
            r#"{"repository":"pypi","resource":"flask"}"#,
            true,
        )
        .await;

    assert_eq!(status, StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn test_purge_surfaces_a_driver_failure() {
    let fixture = Fixture::failing("read cached project pypi/flask: corrupt").await;

    let (status, body) = fixture
        .purge(
            Some(("Alice", ADMIN_PASSWORD)),
            r#"{"repository":"pypi","resource":"flask","apply":true}"#,
            true,
        )
        .await;

    assert_eq!(status, StatusCode::INTERNAL_SERVER_ERROR);
    assert_eq!(
        body["error"],
        serde_json::json!("cache purge failed: read cached project pypi/flask: corrupt")
    );
}

#[tokio::test]
async fn test_purge_is_unavailable_when_the_user_directory_cannot_be_read() {
    let fixture = Fixture::with_unreadable_users().await;

    let (status, body) = fixture
        .purge(
            Some(("Alice", ADMIN_PASSWORD)),
            r#"{"repository":"pypi","resource":"flask"}"#,
            true,
        )
        .await;

    assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE);
    assert_eq!(body["error"], serde_json::json!("user directory unavailable"));
}
