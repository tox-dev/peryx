use axum::http::{Method, StatusCode};
use peryx_driver::AppState;

use super::{auth, hosted_writable, image_manifest, oci_digest, seed_config, send, send_body};

const TOKEN: &str = "s3cret";
const MANIFEST_TYPE: &str = "application/vnd.oci.image.manifest.v1+json";

/// The barrier makes counter assertions deterministic.
fn settle(state: &AppState, done: impl Fn(&peryx_events::metrics::Counters) -> bool) {
    state.serving.metrics.flush().unwrap();
    let counters = state.serving.metrics.index_totals();
    let store = counters.get("store").expect("store counters present after settle");
    assert!(done(store), "metrics settled on an unexpected state: {store:?}");
}

#[tokio::test]
async fn test_oci_serving_records_page_download_and_upload() {
    let dir = tempfile::tempdir().unwrap();
    let (state, app) = hosted_writable(&dir, TOKEN);
    let blob = b"a-real-layer-of-bytes";
    let digest = oci_digest(blob);

    let (status, _, _) = send_body(
        &app,
        Method::POST,
        &format!("/v2/store/app/blobs/uploads/?digest={digest}"),
        &[("authorization", &auth(TOKEN))],
        blob.to_vec(),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED);
    seed_config(&app, "store/app", &auth(TOKEN)).await;
    let (status, _, _) = send_body(
        &app,
        Method::PUT,
        "/v2/store/app/manifests/1.0",
        &[("authorization", &auth(TOKEN)), ("content-type", MANIFEST_TYPE)],
        image_manifest(MANIFEST_TYPE, ""),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED);

    let (status, _, _) = send(&app, Method::GET, "/v2/store/app/manifests/1.0").await;
    assert_eq!(status, StatusCode::OK);
    let (status, _, got) = send(&app, Method::GET, &format!("/v2/store/app/blobs/{digest}")).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(got, &blob[..]);

    settle(&state, |c| {
        c.base.pages >= 1 && c.base.reads >= 1 && c.hosted.writes >= 1
    });
    let counters = state.serving.metrics.index_totals();
    let store = counters.get("store").expect("store counters");
    assert_eq!(store.hosted.writes, 1);
    assert_eq!(store.base.pages, 1);
    assert_eq!(store.base.reads, 1);
    assert_eq!(store.base.bytes, blob.len() as u64);

    let (status, _, metrics) = send(&app, Method::GET, "/metrics").await;
    assert_eq!(status, StatusCode::OK);
    let metrics = String::from_utf8(metrics.to_vec()).unwrap();
    assert!(metrics.contains("peryx_pages_served_total{ecosystem=\"oci\",role=\"hosted\"} 1"));
    assert!(metrics.contains("peryx_artifacts_served_total{ecosystem=\"oci\",role=\"hosted\"} 1"));
    for secret in [TOKEN, "store", "app"] {
        assert!(!metrics.contains(secret), "{secret} leaked into metrics:\n{metrics}");
    }
}

#[tokio::test]
async fn test_head_requests_do_not_count_as_page_or_download() {
    let dir = tempfile::tempdir().unwrap();
    let (state, app) = hosted_writable(&dir, TOKEN);
    let blob = b"layer";
    let digest = oci_digest(blob);
    send_body(
        &app,
        Method::POST,
        &format!("/v2/store/app/blobs/uploads/?digest={digest}"),
        &[("authorization", &auth(TOKEN))],
        blob.to_vec(),
    )
    .await;
    seed_config(&app, "store/app", &auth(TOKEN)).await;
    send_body(
        &app,
        Method::PUT,
        "/v2/store/app/manifests/1.0",
        &[("authorization", &auth(TOKEN)), ("content-type", MANIFEST_TYPE)],
        image_manifest(MANIFEST_TYPE, ""),
    )
    .await;

    let (status, _, _) = send(&app, Method::HEAD, "/v2/store/app/manifests/1.0").await;
    assert_eq!(status, StatusCode::OK);
    let (status, _, _) = send(&app, Method::HEAD, &format!("/v2/store/app/blobs/{digest}")).await;
    assert_eq!(status, StatusCode::OK);

    settle(&state, |c| c.hosted.writes >= 1);
    let counters = state.serving.metrics.index_totals();
    let store = counters.get("store").expect("store counters");
    assert_eq!(store.base.pages, 0);
    assert_eq!(store.base.reads, 0);
}
