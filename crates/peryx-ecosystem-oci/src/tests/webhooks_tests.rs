//! OCI mutations fire webhooks through the neutral subsystem: a manifest push enqueues an `upload`
//! delivery, a manifest or blob delete a `delete` one, so a hosted OCI index notifies like a pypi one.

use std::sync::Arc;

use axum::body::Body;
use axum::http::{Method, Request, StatusCode, header};
use peryx_core::Ecosystem;
use peryx_http::webhook::{WebhookRuntime, WebhookTargetConfig};
use peryx_http::{AppState, router};
use peryx_index::{Index, IndexKind};
use peryx_policy::Policy;
use peryx_storage::blob::BlobStore;
use peryx_storage::meta::{MetaStore, WebhookDeliveryRecord};
use tower::ServiceExt as _;

use super::{auth, oci_digest};

const TOKEN: &str = "s3cret";
const MANIFEST_TYPE: &str = "application/vnd.oci.image.manifest.v1+json";

/// A hosted OCI index `store` with a webhook target subscribed to `events`, pointed at a URL that
/// never answers, deliveries still enqueue, and the queued record is what the tests assert on.
fn hosted_with_webhook(dir: &tempfile::TempDir, events: &[&str]) -> (Arc<AppState>, axum::Router) {
    let meta = MetaStore::open(dir.path().join("peryx.redb")).unwrap();
    let blobs = BlobStore::new(dir.path().join("blobs"));
    let webhooks = WebhookRuntime::new(vec![WebhookTargetConfig {
        index: "store".to_owned(),
        name: "ci".to_owned(),
        url: "http://127.0.0.1:1/hook".to_owned(),
        secret: "hook-secret".to_owned(),
        events: events.iter().map(|event| (*event).to_owned()).collect(),
    }])
    .unwrap();
    let mut state = AppState::with_clock_and_webhooks(
        meta,
        blobs,
        60,
        vec![Index {
            name: "store".to_owned(),
            route: "store".to_owned(),
            ecosystem: Ecosystem::Oci,
            kind: IndexKind::Hosted {
                upload_token: Some(TOKEN.to_owned()),
                volatile: true,
            },
            policy: Policy::default(),
        }],
        Arc::new(|| 1000),
        webhooks,
    );
    crate::install(&mut state);
    let state = Arc::new(state);
    (state.clone(), router(state))
}

async fn send_body(
    app: &axum::Router,
    method: Method,
    uri: &str,
    headers: &[(&str, &str)],
    body: Vec<u8>,
) -> StatusCode {
    let mut builder = Request::builder().method(method).uri(uri);
    for (name, value) in headers {
        builder = builder.header(*name, *value);
    }
    app.clone()
        .oneshot(builder.body(Body::from(body)).unwrap())
        .await
        .unwrap()
        .status()
}

/// Poll the delivery queue until one lands, since enqueue-then-send runs off the request.
fn wait_for_delivery(state: &AppState) -> WebhookDeliveryRecord {
    for _ in 0..500 {
        if let Some(record) = state.meta.list_webhook_deliveries().unwrap().into_iter().next() {
            return record;
        }
        std::thread::sleep(std::time::Duration::from_millis(2));
    }
    panic!("no webhook delivery enqueued");
}

async fn push_manifest(app: &axum::Router, blob: &[u8], manifest: &[u8], reference: &str) {
    let digest = oci_digest(blob);
    send_body(
        app,
        Method::POST,
        &format!("/v2/store/app/blobs/uploads/?digest={digest}"),
        &[("authorization", &auth(TOKEN))],
        blob.to_vec(),
    )
    .await;
    let status = send_body(
        app,
        Method::PUT,
        &format!("/v2/store/app/manifests/{reference}"),
        &[
            ("authorization", &auth(TOKEN)),
            (header::CONTENT_TYPE.as_str(), MANIFEST_TYPE),
            ("x-request-id", "req-42"),
        ],
        manifest.to_vec(),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED);
}

#[tokio::test]
async fn test_manifest_push_fires_an_upload_webhook() {
    let dir = tempfile::tempdir().unwrap();
    let (state, app) = hosted_with_webhook(&dir, &["upload"]);
    let manifest = br#"{"schemaVersion":2,"mediaType":"application/vnd.oci.image.manifest.v1+json"}"#;
    push_manifest(&app, b"layer-bytes", manifest, "1.0").await;

    let delivery = wait_for_delivery(&state);
    assert_eq!(delivery.event, "upload");
    let payload: serde_json::Value = serde_json::from_str(&delivery.payload).unwrap();
    assert_eq!(payload["event"], "upload");
    assert_eq!(payload["index"], "store");
    assert_eq!(payload["project"], "app");
    assert_eq!(payload["version"], "1.0");
    assert_eq!(payload["actor"], "_");
    assert_eq!(payload["request_id"], "req-42");
}

#[tokio::test]
async fn test_manifest_delete_fires_a_delete_webhook() {
    let dir = tempfile::tempdir().unwrap();
    let (state, app) = hosted_with_webhook(&dir, &["delete"]);
    let manifest = br#"{"schemaVersion":2}"#;
    push_manifest(&app, b"layer", manifest, "2.0").await;

    // Only the delete subscribes, so the upload enqueues nothing; the first delivery is the delete.
    let status = send_body(
        &app,
        Method::DELETE,
        "/v2/store/app/manifests/2.0",
        &[("authorization", &auth(TOKEN))],
        Vec::new(),
    )
    .await;
    assert_eq!(status, StatusCode::ACCEPTED);

    let delivery = wait_for_delivery(&state);
    assert_eq!(delivery.event, "delete");
    let payload: serde_json::Value = serde_json::from_str(&delivery.payload).unwrap();
    assert_eq!(payload["project"], "app");
    assert_eq!(payload["version"], "2.0");
}

#[tokio::test]
async fn test_blob_delete_fires_a_delete_webhook() {
    let dir = tempfile::tempdir().unwrap();
    let (state, app) = hosted_with_webhook(&dir, &["delete"]);
    let blob = b"a-blob-to-remove";
    let digest = oci_digest(blob);
    send_body(
        &app,
        Method::POST,
        &format!("/v2/store/app/blobs/uploads/?digest={digest}"),
        &[("authorization", &auth(TOKEN))],
        blob.to_vec(),
    )
    .await;

    let status = send_body(
        &app,
        Method::DELETE,
        &format!("/v2/store/app/blobs/{digest}"),
        &[("authorization", &auth(TOKEN))],
        Vec::new(),
    )
    .await;
    assert_eq!(status, StatusCode::ACCEPTED);

    let delivery = wait_for_delivery(&state);
    assert_eq!(delivery.event, "delete");
    let payload: serde_json::Value = serde_json::from_str(&delivery.payload).unwrap();
    assert_eq!(payload["file"]["sha256"], digest);
}
