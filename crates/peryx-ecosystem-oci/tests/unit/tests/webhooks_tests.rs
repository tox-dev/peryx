use std::sync::Arc;

use axum::body::Body;
use axum::http::{Method, Request, StatusCode, header};
use peryx_driver::AppState;
use peryx_events::webhook::{WebhookRuntime, WebhookTargetConfig};
use peryx_http::router;
use peryx_index::{Index, IndexKind};
use peryx_policy::Policy;
use peryx_storage::blob::BlobStore;
use peryx_storage::meta::{MetaStore, WebhookDeliveryRecord};
use tower::ServiceExt as _;

use super::{auth, oci_digest};

const TOKEN: &str = "s3cret";
const MANIFEST_TYPE: &str = "application/vnd.oci.image.manifest.v1+json";

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
            ecosystem: crate::ECOSYSTEM,
            kind: IndexKind::Hosted { volatile: true },
            policy: Policy::default(),
            acl: crate::tests::writer_acl(TOKEN.to_owned()),
        }],
        Arc::new(|| 1000),
        webhooks,
    );
    crate::tests::install_oci(&mut state, std::collections::HashMap::new(), false);
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

/// Delivery records remain available after an attempt.
fn enqueued_delivery(state: &AppState) -> WebhookDeliveryRecord {
    state
        .serving
        .meta
        .list_webhook_deliveries()
        .unwrap()
        .into_iter()
        .next()
        .expect("the push enqueued a webhook delivery")
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
async fn test_manifest_push_fires_manifest_push_webhook() {
    let dir = tempfile::tempdir().unwrap();
    let (state, app) = hosted_with_webhook(&dir, &["manifest-push"]);
    let manifest = br#"{"schemaVersion":2,"mediaType":"application/vnd.oci.image.manifest.v1+json"}"#;
    push_manifest(&app, b"layer-bytes", manifest, "1.0").await;

    let delivery = enqueued_delivery(&state);
    assert_eq!(delivery.event, "manifest-push");
    let payload: serde_json::Value = serde_json::from_str(&delivery.payload).unwrap();
    assert_eq!(payload["schema"], "oci.v1");
    assert_eq!(payload["event"], "manifest-push");
    assert_eq!(payload["index"], "store");
    assert_eq!(payload["data"]["repository"], "app");
    assert_eq!(payload["data"]["reference"], "1.0");
    assert_eq!(payload["actor"], "_");
    assert_eq!(payload["request_id"], "req-42");
}

#[tokio::test]
async fn test_manifest_delete_fires_a_delete_webhook() {
    let dir = tempfile::tempdir().unwrap();
    let (state, app) = hosted_with_webhook(&dir, &["manifest-delete"]);
    let manifest = br#"{"schemaVersion":2}"#;
    push_manifest(&app, b"layer", manifest, "2.0").await;

    let status = send_body(
        &app,
        Method::DELETE,
        "/v2/store/app/manifests/2.0",
        &[("authorization", &auth(TOKEN))],
        Vec::new(),
    )
    .await;
    assert_eq!(status, StatusCode::ACCEPTED);

    let delivery = enqueued_delivery(&state);
    assert_eq!(delivery.event, "manifest-delete");
    let payload: serde_json::Value = serde_json::from_str(&delivery.payload).unwrap();
    assert_eq!(payload["schema"], "oci.v1");
    assert_eq!(payload["data"]["repository"], "app");
    assert_eq!(payload["data"]["reference"], "2.0");
}

#[tokio::test]
async fn test_manifest_restore_fires_a_restore_webhook() {
    let dir = tempfile::tempdir().unwrap();
    let (state, app) = hosted_with_webhook(&dir, &["manifest-restore"]);
    push_manifest(&app, b"layer", br#"{"schemaVersion":2}"#, "2.0").await;
    send_body(
        &app,
        Method::DELETE,
        "/v2/store/app/manifests/2.0",
        &[("authorization", &auth(TOKEN))],
        Vec::new(),
    )
    .await;

    let status = send_body(
        &app,
        Method::PUT,
        "/v2/store/app/manifests/2.0/restore",
        &[("authorization", &auth(TOKEN))],
        Vec::new(),
    )
    .await;
    assert_eq!(status, StatusCode::ACCEPTED);

    let delivery = enqueued_delivery(&state);
    assert_eq!(delivery.event, "manifest-restore");
    let payload: serde_json::Value = serde_json::from_str(&delivery.payload).unwrap();
    assert_eq!(payload["schema"], "oci.v1");
    assert_eq!(payload["data"]["repository"], "app");
    assert_eq!(payload["data"]["reference"], "2.0");
    assert!(payload["data"]["digest"].as_str().is_some());
}

#[tokio::test]
async fn test_blob_delete_fires_a_delete_webhook() {
    let dir = tempfile::tempdir().unwrap();
    let (state, app) = hosted_with_webhook(&dir, &["blob-delete"]);
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

    let delivery = enqueued_delivery(&state);
    assert_eq!(delivery.event, "blob-delete");
    let payload: serde_json::Value = serde_json::from_str(&delivery.payload).unwrap();
    assert_eq!(payload["schema"], "oci.v1");
    assert_eq!(payload["data"]["digest"], digest);
}
