use std::sync::Arc;

use axum::body::Body;
use axum::http::{Method, Request, StatusCode, header};
use base64::Engine as _;
use base64::engine::general_purpose::STANDARD;
use peryx_driver::state::AppState;
use peryx_driver::users::UserService;
use peryx_identity::PasswordPolicy;
use peryx_storage::blob::BlobStore;
use peryx_storage::meta::MetaStore;
use rstest::rstest;
use tower::ServiceExt as _;

#[rstest]
#[case::jobs(Method::POST, "/+jobs/jr_0000000000000001/cancel", None)]
#[case::trash(Method::GET, "/+trash", None)]
#[case::repositories(Method::GET, "/+repositories", None)]
#[case::retention(Method::POST, "/+retention/plan", Some("{}"))]
#[case::analytics(Method::GET, "/+analytics/top-resources", None)]
#[case::status(Method::GET, "/+status", None)]
#[case::stats(Method::GET, "/+stats", None)]
#[case::revocations(Method::GET, "/+revocations", None)]
#[case::tokens(Method::GET, "/+tokens", None)]
#[case::pql(Method::POST, "/+query", Some(r#"{"query":"from policy.decisions"}"#))]
#[case::grants(Method::GET, "/+grants", None)]
#[case::quota(Method::GET, "/+quota", None)]
#[case::policy_decisions(Method::GET, "/+policy/decisions", None)]
#[tokio::test]
async fn local_password_overload_is_service_unavailable(
    #[case] method: Method,
    #[case] uri: &str,
    #[case] body: Option<&str>,
) {
    let directory = tempfile::tempdir().unwrap();
    let meta = MetaStore::open(directory.path().join("peryx.redb")).unwrap();
    let mut state = AppState::new(
        meta.clone(),
        BlobStore::new(directory.path().join("blobs")),
        60,
        Vec::new(),
    );
    Arc::get_mut(&mut state.serving).unwrap().users =
        UserService::with_password_settings(meta, PasswordPolicy::new(8, 1, 1).unwrap(), 0);
    let mut request = Request::builder().method(method).uri(uri).header(
        header::AUTHORIZATION,
        format!("Basic {}", STANDARD.encode("Unknown:password")),
    );
    if body.is_some() {
        request = request.header(header::CONTENT_TYPE, "application/json");
    }

    let response = crate::router(Arc::new(state))
        .oneshot(
            request
                .body(body.map_or_else(Body::empty, |body| Body::from(body.to_owned())))
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(response.status(), StatusCode::SERVICE_UNAVAILABLE);
}
