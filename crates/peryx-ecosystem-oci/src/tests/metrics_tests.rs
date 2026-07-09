//! OCI serving records the neutral per-index counters: a served manifest is a page, a served blob a
//! download, and a pushed manifest an upload, so an OCI index reports the same metrics a pypi one does.

use axum::http::{Method, StatusCode};
use peryx_http::AppState;

use super::{auth, hosted_writable, oci_digest, send, send_body};

const TOKEN: &str = "s3cret";
const MANIFEST_TYPE: &str = "application/vnd.oci.image.manifest.v1+json";

/// The aggregator runs on its own thread; poll the store's counters until the events land.
fn settle(state: &AppState, done: impl Fn(&peryx_http::metrics::Counters) -> bool) {
    for _ in 0..500 {
        if let Some(counters) = state.metrics.index_totals().get("store")
            && done(counters)
        {
            return;
        }
        std::thread::sleep(std::time::Duration::from_millis(2));
    }
    panic!("metrics aggregator never settled");
}

#[tokio::test]
async fn test_oci_serving_records_page_download_and_upload() {
    let dir = tempfile::tempdir().unwrap();
    let (state, app) = hosted_writable(&dir, TOKEN);
    let blob = b"a-real-layer-of-bytes";
    let digest = oci_digest(blob);

    // Push a blob monolithically, then a manifest that finalizes the image.
    let (status, _, _) = send_body(
        &app,
        Method::POST,
        &format!("/v2/store/app/blobs/uploads/?digest={digest}"),
        &[("authorization", &auth(TOKEN))],
        blob.to_vec(),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED);
    let manifest = br#"{"schemaVersion":2,"mediaType":"application/vnd.oci.image.manifest.v1+json"}"#;
    let (status, _, _) = send_body(
        &app,
        Method::PUT,
        "/v2/store/app/manifests/1.0",
        &[("authorization", &auth(TOKEN)), ("content-type", MANIFEST_TYPE)],
        manifest.to_vec(),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED);

    // A GET manifest counts as a page; a GET blob counts as a download of its bytes.
    let (status, _, _) = send(&app, Method::GET, "/v2/store/app/manifests/1.0").await;
    assert_eq!(status, StatusCode::OK);
    let (status, _, got) = send(&app, Method::GET, &format!("/v2/store/app/blobs/{digest}")).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(got, &blob[..]);

    settle(&state, |c| {
        c.base.pages >= 1 && c.base.downloads >= 1 && c.hosted.uploads >= 1
    });
    let counters = state.metrics.index_totals();
    let store = counters.get("store").expect("store counters");
    assert_eq!(store.hosted.uploads, 1);
    assert_eq!(store.base.pages, 1);
    assert_eq!(store.base.downloads, 1);
    assert_eq!(store.base.bytes, blob.len() as u64);
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
    let manifest = br#"{"schemaVersion":2}"#;
    send_body(
        &app,
        Method::PUT,
        "/v2/store/app/manifests/1.0",
        &[("authorization", &auth(TOKEN)), ("content-type", MANIFEST_TYPE)],
        manifest.to_vec(),
    )
    .await;

    // HEAD both: a metadata check, not a served body, so neither counter moves.
    let (status, _, _) = send(&app, Method::HEAD, "/v2/store/app/manifests/1.0").await;
    assert_eq!(status, StatusCode::OK);
    let (status, _, _) = send(&app, Method::HEAD, &format!("/v2/store/app/blobs/{digest}")).await;
    assert_eq!(status, StatusCode::OK);

    // The upload settled, so the aggregator has drained; the HEADs added nothing.
    settle(&state, |c| c.hosted.uploads >= 1);
    let counters = state.metrics.index_totals();
    let store = counters.get("store").expect("store counters");
    assert_eq!(store.base.pages, 0);
    assert_eq!(store.base.downloads, 0);
}
