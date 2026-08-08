use std::collections::BTreeSet;

use peryx_driver::rate_limit::{RateLimitConfig, RateLimiter, RouteLimit};
use peryx_identity::{Action, Glob, Grant, IndexAcl, NamedToken};
use peryx_storage::meta::MetaError;

use super::*;
use crate::store::JournalEntry;

const LAST_SERIAL: &str =
    "<?xml version=\"1.0\"?><methodCall><methodName>changelog_last_serial</methodName></methodCall>";

fn since(serial: i64) -> String {
    format!(
        "<?xml version=\"1.0\"?><methodCall><methodName>changelog_since_serial</methodName><params><param><value><i8>{serial}</i8></value></param></params></methodCall>"
    )
}

fn entry(serial: u64) -> Vec<u8> {
    serde_json::to_vec(&JournalEntry {
        serial,
        submitted_at_unix: 1_700_000_000 + i64::try_from(serial).unwrap(),
        action: "add-file".to_owned(),
        project: format!("project-{serial}"),
        version: Some("1.0".to_owned()),
        filename: Some(format!("project_{serial}-1.0-py3-none-any.whl")),
    })
    .unwrap()
}

async fn post_xml(
    state: &Arc<AppState>,
    uri: &str,
    body: impl Into<Body>,
    auth: Option<&str>,
) -> (StatusCode, HeaderMap, String) {
    let mut request = Request::builder()
        .method("POST")
        .uri(uri)
        .header(header::CONTENT_TYPE, "text/xml");
    if let Some(auth) = auth {
        request = request.header(header::AUTHORIZATION, auth);
    }
    let response = router(state.clone())
        .oneshot(request.body(body.into()).unwrap())
        .await
        .unwrap();
    let status = response.status();
    let headers = response.headers().clone();
    let body = response.into_body().collect().await.unwrap().to_bytes();
    (status, headers, String::from_utf8(body.to_vec()).unwrap())
}

fn read_acl(projects: &str) -> IndexAcl {
    IndexAcl {
        anonymous_read: false,
        tokens: vec![NamedToken {
            name: "mirror".to_owned(),
            secret: "read-secret".to_owned(),
            grants: vec![Grant {
                projects: vec![Glob::new(projects)],
                actions: BTreeSet::from([Action::Read]),
            }],
            expires_at: None,
        }],
    }
}

async fn state_with_hosted_acl(acl: IndexAcl) -> Arc<AppState> {
    let Harness { state, .. } = harness().await;
    let mut state = Arc::try_unwrap(state).ok().unwrap();
    state.indexes[1].acl = acl;
    Arc::new(state)
}

#[tokio::test]
async fn test_changelog_last_serial_uses_every_warehouse_route() {
    let h = harness().await;
    h.state
        .meta
        .commit_driver_txn(|_| Ok::<_, MetaError>(((), vec![entry(0)])))
        .unwrap();

    for route in ["/pypi", "/pypi/", "/RPC2"] {
        let (status, headers, body) = post_xml(&h.state, route, LAST_SERIAL, None).await;
        assert_eq!(status, StatusCode::OK, "{route}");
        assert_eq!(headers[header::CONTENT_TYPE], "text/xml; charset=utf-8", "{route}");
        assert!(body.contains("<int>1</int>"), "{route}: {body}");
    }
}

#[tokio::test]
async fn test_changelog_routes_leave_multipart_posts_to_the_index() {
    let h = harness().await;
    let (content_type, body) = multipart_body(&upload_fields(), None);

    let (status, body) = post_upload_response(&h.state, "/pypi", Some(&upload_auth()), &content_type, body).await;

    assert_eq!(status, StatusCode::METHOD_NOT_ALLOWED);
    assert!(!body.contains("methodResponse"));
}

#[tokio::test]
async fn test_changelog_remains_readable_on_a_read_only_replica() {
    let h = harness().await;
    let Harness { state, .. } = h;
    let mut state = Arc::try_unwrap(state).ok().unwrap();
    state.read_only = true;
    let state = Arc::new(state);

    assert_eq!(post_xml(&state, "/RPC2", LAST_SERIAL, None).await.0, StatusCode::OK);

    let (content_type, body) = multipart_body(&upload_fields(), None);
    assert_eq!(
        post_upload(&state, "/pypi", Some(&upload_auth()), &content_type, body).await,
        StatusCode::SERVICE_UNAVAILABLE
    );
}

#[tokio::test]
async fn test_changelog_uses_the_listing_rate_limit() {
    let h = harness().await;
    let Harness { state, .. } = h;
    let mut state = Arc::try_unwrap(state).ok().unwrap();
    state.rate_limits = RateLimiter::new(RateLimitConfig {
        listing: RouteLimit::new(2, 60),
        upload: RouteLimit::new(1, 60),
        ..RateLimitConfig::enabled_defaults()
    });
    let state = Arc::new(state);

    let first = post_xml(&state, "/RPC2", LAST_SERIAL, None).await.0;
    let second = post_xml(&state, "/RPC2", LAST_SERIAL, None).await.0;
    let third = post_xml(&state, "/RPC2", LAST_SERIAL, None).await.0;

    assert_eq!(
        (first, second, third),
        (StatusCode::OK, StatusCode::OK, StatusCode::TOO_MANY_REQUESTS)
    );
}

#[rstest]
#[case("<methodCall>", "-32700")]
#[case("<methodCall><methodName>list_packages</methodName></methodCall>", "-32601")]
#[case("<methodCall><methodName>changelog_since_serial</methodName></methodCall>", "-32602")]
#[tokio::test]
async fn test_changelog_returns_client_faults(#[case] request: &str, #[case] code: &str) {
    let h = harness().await;

    let (status, _, body) = post_xml(&h.state, "/RPC2", request.to_owned(), None).await;

    assert_eq!(status, StatusCode::OK);
    assert!(body.contains(&format!("<int>{code}</int>")), "{body}");
}

#[tokio::test]
async fn test_changelog_rejects_the_body_before_parsing_past_the_limit() {
    let h = harness().await;

    let (status, _, body) = post_xml(&h.state, "/RPC2", vec![b'x'; 64 * 1024 + 1], None).await;

    assert_eq!(status, StatusCode::OK);
    assert!(body.contains("<int>-32600</int>"));
}

#[tokio::test]
async fn test_changelog_requires_catalog_read_access() {
    let state = state_with_hosted_acl(read_acl("*")).await;
    let auth = format!("Basic {}", STANDARD.encode("mirror:read-secret"));

    assert_eq!(
        post_xml(&state, "/RPC2", LAST_SERIAL, None).await.0,
        StatusCode::UNAUTHORIZED
    );
    assert_eq!(
        post_xml(&state, "/RPC2", LAST_SERIAL, Some("Basic bWlycm9yOndyb25n"))
            .await
            .0,
        StatusCode::UNAUTHORIZED
    );
    assert_eq!(
        post_xml(&state, "/RPC2", LAST_SERIAL, Some(&auth)).await.0,
        StatusCode::OK
    );
}

#[tokio::test]
async fn test_changelog_rejects_a_project_scoped_read_grant() {
    let state = state_with_hosted_acl(read_acl("project-*")).await;
    let auth = format!("Basic {}", STANDARD.encode("mirror:read-secret"));

    assert_eq!(
        post_xml(&state, "/RPC2", LAST_SERIAL, Some(&auth)).await.0,
        StatusCode::FORBIDDEN
    );
}

#[tokio::test]
async fn test_changelog_returns_an_opaque_fault_for_bad_storage() {
    let capture = LogCapture::default();
    let _guard = capture.install();
    let h = harness().await;
    h.state
        .meta
        .commit_driver_txn(|_| Ok::<_, MetaError>(((), vec![b"{".to_vec()])))
        .unwrap();

    let (status, _, body) = post_xml(&h.state, "/RPC2", since(0), None).await;

    assert_eq!(status, StatusCode::OK);
    assert!(body.contains("<int>-32403</int>"));
    assert!(body.contains("server error; service unavailable"));
    assert!(capture.text().contains("failed to read the PyPI changelog"));
}

#[tokio::test]
async fn test_changelog_paginates_at_the_warehouse_boundary() {
    let h = harness().await;
    h.state
        .meta
        .commit_driver_txn(|_| Ok::<_, MetaError>(((), (0..50_001).map(entry).collect())))
        .unwrap();

    let (_, _, first) = post_xml(&h.state, "/RPC2", since(0), None).await;
    let (_, _, second) = post_xml(&h.state, "/RPC2", since(50_000), None).await;

    assert_eq!(first.matches("<value><array><data>").count(), 50_001);
    assert!(first.contains("<int>50000</int>"));
    assert!(!first.contains("<int>50001</int>"));
    assert_eq!(second.matches("<value><array><data>").count(), 2);
    assert!(second.contains("<int>50001</int>"));
}

#[tokio::test]
async fn test_changelog_supports_the_bandersnatch_cursor_sequence() {
    let h = harness().await;
    h.state
        .meta
        .commit_driver_txn(|_| Ok::<_, MetaError>(((), vec![entry(0), entry(0)])))
        .unwrap();

    let (_, _, head) = post_xml(&h.state, "/pypi", LAST_SERIAL, None).await;
    let (_, _, changes) = post_xml(&h.state, "/pypi", since(0), None).await;
    let (_, _, resumed) = post_xml(&h.state, "/pypi", since(2), None).await;

    assert!(head.contains("<int>2</int>"));
    assert!(changes.contains("<int>1</int>"));
    assert!(changes.contains("<int>2</int>"));
    assert_eq!(resumed.matches("<value><array><data>").count(), 1);
}
