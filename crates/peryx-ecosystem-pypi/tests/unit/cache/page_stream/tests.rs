use peryx_storage::blob::BlobStore;
use peryx_storage::meta::MetaStore;
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

use super::*;

#[test]
fn test_transform_error_maps_parse_and_truncated_errors() {
    let err = serde_json::from_str::<serde_json::Value>("{").unwrap_err();
    assert!(matches!(transform_error(err.into()), CacheError::Parse(_)));
    assert!(matches!(
        transform_error(crate::stream::TransformError::Truncated),
        CacheError::Unavailable
    ));
    assert!(matches!(
        transform_error(crate::stream::TransformError::TooLarge),
        CacheError::Unavailable
    ));
}

fn flask_body(versions: &[&str]) -> Vec<u8> {
    crate::to_json(&crate::ProjectDetail {
        meta: crate::Meta::default(),
        name: "flask".to_owned(),
        versions: versions.iter().map(|version| (*version).to_owned()).collect(),
        files: vec![],
    })
    .into_bytes()
}

fn stale_flask_state(dir: &tempfile::TempDir, upstream: &str, fetched_at: i64) -> (Arc<ServingState>, UpstreamClient) {
    let meta = MetaStore::open(dir.path().join("peryx.redb")).unwrap();
    let blobs = BlobStore::new(dir.path().join("blobs"));
    let client = UpstreamClient::new(upstream).unwrap();
    let indexes = vec![Index {
        name: "pypi".to_owned(),
        route: "pypi".to_owned(),
        ecosystem: crate::ECOSYSTEM,
        kind: IndexKind::Cached {
            client: client.clone(),
            offline: false,
        },
        policy: peryx_policy::Policy::default(),
        acl: peryx_identity::IndexAcl::default(),
    }];
    let mut app = peryx_driver::state::AppState::with_clock(meta, blobs, 60, indexes, Arc::new(|| 2000));
    crate::tests::install(&mut app);
    let state = app.serving.clone();
    state
        .meta
        .put_index(
            "pypi/flask",
            &CachedIndex {
                source: None,
                last_modified: None,
                etag: None,
                last_serial: None,
                fetched_at_unix: fetched_at,
                content_type: None,
                fresh_secs: None,
                body: flask_body(&["1.0"]),
            },
        )
        .unwrap();
    (state, client)
}

#[tokio::test]
async fn test_spawn_revalidation_refreshes_the_cached_page() {
    let dir = tempfile::tempdir().unwrap();
    let server = MockServer::start().await;
    let (state, client) = stale_flask_state(&dir, &format!("{}/simple/", server.uri()), 1000);
    Mock::given(method("GET"))
        .and(path("/simple/flask/"))
        .respond_with(
            ResponseTemplate::new(200).set_body_raw(flask_body(&["1.0", "2.0"]), "application/vnd.pypi.simple.v1+json"),
        )
        .mount(&server)
        .await;

    spawn_revalidation(
        state.clone(),
        "pypi/flask".to_owned(),
        "pypi".to_owned(),
        "flask".to_owned(),
        client,
    )
    .expect("the free gate lets the refresh run")
    .await
    .unwrap();

    let body = state.meta.get_index("pypi/flask").unwrap().unwrap().body;
    assert!(String::from_utf8(body).unwrap().contains("2.0"));
    drop(flight_gate(&state, "pypi/flask").try_lock_owned().unwrap());
}

#[tokio::test]
async fn test_spawn_revalidation_skips_when_a_refresh_is_already_in_flight() {
    let dir = tempfile::tempdir().unwrap();
    let (state, client) = stale_flask_state(&dir, "https://example.invalid/simple/", 1000);
    let held = flight_gate(&state, "pypi/flask").lock_owned().await;

    let outcome = spawn_revalidation(
        state.clone(),
        "pypi/flask".to_owned(),
        "pypi".to_owned(),
        "flask".to_owned(),
        client,
    );

    assert!(outcome.is_none());
    drop(held);
}

#[tokio::test]
async fn test_revalidation_keeps_the_stale_page_when_upstream_is_unparseable() {
    let dir = tempfile::tempdir().unwrap();
    let server = MockServer::start().await;
    let (state, client) = stale_flask_state(&dir, &format!("{}/simple/", server.uri()), 1000);
    Mock::given(method("GET"))
        .and(path("/simple/flask/"))
        .respond_with(
            ResponseTemplate::new(200).set_body_raw(b"not json".to_vec(), "application/vnd.pypi.simple.v1+json"),
        )
        .mount(&server)
        .await;

    spawn_revalidation(
        state.clone(),
        "pypi/flask".to_owned(),
        "pypi".to_owned(),
        "flask".to_owned(),
        client,
    )
    .expect("the free gate lets the refresh run")
    .await
    .unwrap();

    let body = state.meta.get_index("pypi/flask").unwrap().unwrap().body;
    assert!(String::from_utf8(body).unwrap().contains("1.0"));
    drop(flight_gate(&state, "pypi/flask").try_lock_owned().unwrap());
}
