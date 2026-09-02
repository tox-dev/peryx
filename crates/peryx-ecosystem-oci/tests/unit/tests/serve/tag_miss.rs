//! What peryx does with a tag a registry says it does not have: one existence check settles the
//! lookups that follow it, while a registry that failed for any other reason keeps its status and is
//! asked again next time.

use super::support::*;

const MANIFEST: &[u8] = br#"{"schemaVersion":2,"mediaType":"application/vnd.oci.image.manifest.v1+json"}"#;
const TAG_PATH: &str = "/v2/library/nginx/manifests/rc";
const TAG_URI: &str = "/v2/hub/library/nginx/manifests/rc";

/// The body fetch a registry must not receive, so a test fails on the second call rather than on a
/// count read afterwards.
async fn refuse_body_fetch(server: &MockServer) {
    Mock::given(method("GET"))
        .and(path(TAG_PATH))
        .respond_with(ResponseTemplate::new(200).set_body_raw(MANIFEST.to_vec(), MANIFEST_TYPE))
        .expect(0)
        .mount(server)
        .await;
}

/// A deployment client polling for a tag before it is published used to spend two authenticated calls
/// per poll. The second lookup inside the window asks nothing.
#[tokio::test]
async fn test_a_second_lookup_of_an_unknown_tag_asks_the_registry_once() {
    let server = MockServer::start().await;
    Mock::given(method("HEAD"))
        .and(path(TAG_PATH))
        .respond_with(ResponseTemplate::new(404))
        .expect(1)
        .mount(&server)
        .await;
    refuse_body_fetch(&server).await;
    let dir = tempfile::tempdir().unwrap();
    let (_state, app) = proxy(&dir, &format!("{}/", server.uri()), false);

    let first = send(&app, Method::GET, TAG_URI).await;
    let second = send(&app, Method::GET, TAG_URI).await;

    assert_eq!((first.0, second.0), (StatusCode::NOT_FOUND, StatusCode::NOT_FOUND));
    let body = second.2;
    assert!(body_has_code(&body, "MANIFEST_UNKNOWN"), "{body:?}");
}

/// Callers that arrive together still share one registry operation, and the ones behind the leader
/// read the miss it recorded instead of starting a check of their own.
#[tokio::test]
async fn test_concurrent_lookups_of_an_unknown_tag_share_one_existence_check() {
    let server = MockServer::start().await;
    Mock::given(method("HEAD"))
        .and(path(TAG_PATH))
        .respond_with(ResponseTemplate::new(404))
        .expect(1)
        .mount(&server)
        .await;
    refuse_body_fetch(&server).await;
    let dir = tempfile::tempdir().unwrap();
    let (_state, app) = proxy(&dir, &format!("{}/", server.uri()), false);

    let (first, second) = tokio::join!(send(&app, Method::GET, TAG_URI), send(&app, Method::GET, TAG_URI));

    assert_eq!((first.0, second.0), (StatusCode::NOT_FOUND, StatusCode::NOT_FOUND));
}

/// A tag published while a miss stands is invisible for the length of the window and served once it
/// closes, which is what bounds how far behind a publication a mirror can be.
#[tokio::test]
async fn test_a_tag_published_after_a_miss_appears_once_the_window_closes() {
    use std::sync::Arc;
    use std::sync::atomic::{AtomicI64, Ordering};
    let server = MockServer::start().await;
    Mock::given(method("HEAD"))
        .and(path(TAG_PATH))
        .respond_with(ResponseTemplate::new(404))
        .up_to_n_times(1)
        .mount(&server)
        .await;
    mount_head_without_digest(&server, TAG_PATH).await;
    Mock::given(method("GET"))
        .and(path(TAG_PATH))
        .respond_with(ResponseTemplate::new(200).set_body_raw(MANIFEST.to_vec(), MANIFEST_TYPE))
        .expect(1)
        .mount(&server)
        .await;
    let now = Arc::new(AtomicI64::new(1000));
    let ticking = now.clone();
    let (_state, app) = crate::tests::proxy_with_clock(
        &tempfile::tempdir().unwrap(),
        &format!("{}/", server.uri()),
        Arc::new(move || ticking.load(Ordering::Relaxed)),
    );

    assert_eq!(send(&app, Method::GET, TAG_URI).await.0, StatusCode::NOT_FOUND);
    now.store(1029, Ordering::Relaxed);
    assert_eq!(send(&app, Method::GET, TAG_URI).await.0, StatusCode::NOT_FOUND);
    now.store(1031, Ordering::Relaxed);
    let (status, _, body) = send(&app, Method::GET, TAG_URI).await;

    assert_eq!((status, body.as_ref()), (StatusCode::OK, MANIFEST));
}

/// A registry that refused for a reason other than absence keeps its own status, is not asked for a
/// body it already declined, and leaves no miss behind, so the next lookup puts the question again.
#[rstest]
#[case::throttled(429, StatusCode::TOO_MANY_REQUESTS)]
#[case::unauthorized(401, StatusCode::UNAUTHORIZED)]
#[case::server_error(500, StatusCode::BAD_GATEWAY)]
#[case::hidden_repository(403, StatusCode::NOT_FOUND)]
#[tokio::test]
async fn test_a_refused_existence_check_is_not_followed_by_a_body_fetch(
    #[case] upstream: u16,
    #[case] expected: StatusCode,
) {
    let server = MockServer::start().await;
    Mock::given(method("HEAD"))
        .and(path(TAG_PATH))
        .respond_with(ResponseTemplate::new(upstream))
        .expect(2)
        .mount(&server)
        .await;
    refuse_body_fetch(&server).await;
    let dir = tempfile::tempdir().unwrap();
    let (_state, app) = proxy(&dir, &format!("{}/", server.uri()), false);

    let first = send(&app, Method::GET, TAG_URI).await;
    let second = send(&app, Method::GET, TAG_URI).await;

    assert_eq!((first.0, second.0), (expected, expected));
}

/// A tag the registry stops serving between the existence check and the body fetch is absent on the
/// same terms, so the fetch that watched it go records the miss.
#[tokio::test]
async fn test_a_tag_withdrawn_before_the_body_fetch_records_the_miss() {
    let server = MockServer::start().await;
    mount_head_without_digest(&server, TAG_PATH).await;
    Mock::given(method("GET"))
        .and(path(TAG_PATH))
        .respond_with(ResponseTemplate::new(404))
        .expect(1)
        .mount(&server)
        .await;
    let dir = tempfile::tempdir().unwrap();
    let (_state, app) = proxy(&dir, &format!("{}/", server.uri()), false);

    let first = send(&app, Method::GET, TAG_URI).await;
    let second = send(&app, Method::GET, TAG_URI).await;

    assert_eq!((first.0, second.0), (StatusCode::NOT_FOUND, StatusCode::NOT_FOUND));
}
