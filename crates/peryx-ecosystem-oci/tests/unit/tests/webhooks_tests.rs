use std::sync::Arc;

use axum::body::Body;
use axum::http::{Method, Request, StatusCode, header};
use http_body_util::BodyExt as _;
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
const CONFIG: &[u8] = b"a-config-document";

/// A schema-valid image manifest naming [`CONFIG`], the blob [`push_manifest`] uploads first.
fn image_manifest() -> Vec<u8> {
    format!(
        r#"{{"schemaVersion":2,"mediaType":"{MANIFEST_TYPE}","config":{{"mediaType":"application/vnd.oci.image.config.v1+json","digest":"{}","size":{}}},"layers":[]}}"#,
        oci_digest(CONFIG),
        CONFIG.len()
    )
    .into_bytes()
}

fn hosted_with_meta_and_webhook(
    dir: &tempfile::TempDir,
    meta: MetaStore,
    events: &[&str],
) -> (Arc<AppState>, axum::Router) {
    let blobs = BlobStore::new(dir.path().join("blobs"));
    let webhooks = WebhookRuntime::new(vec![WebhookTargetConfig {
        index: "store".to_owned(),
        name: "ci".to_owned(),
        url: "http://127.0.0.1:1/hook".to_owned(),
        secret: "test-webhook-signing-secret-32-bytes".to_owned(),
        events: events.iter().map(|event| (*event).to_owned()).collect(),
        allowed_events: crate::registration().registration.webhook_events(),
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

fn hosted_with_webhook(dir: &tempfile::TempDir, events: &[&str]) -> (Arc<AppState>, axum::Router) {
    hosted_with_meta_and_webhook(dir, MetaStore::open(dir.path().join("peryx.redb")).unwrap(), events)
}

async fn send_response(
    app: &axum::Router,
    method: Method,
    uri: &str,
    headers: &[(&str, &str)],
    body: Vec<u8>,
) -> axum::response::Response {
    let mut builder = Request::builder().method(method).uri(uri);
    for (name, value) in headers {
        builder = builder.header(*name, *value);
    }
    app.clone()
        .oneshot(builder.body(Body::from(body)).unwrap())
        .await
        .unwrap()
}

async fn send_body(
    app: &axum::Router,
    method: Method,
    uri: &str,
    headers: &[(&str, &str)],
    body: Vec<u8>,
) -> StatusCode {
    send_response(app, method, uri, headers, body).await.status()
}

/// Delivery records remain available after an attempt.
fn enqueued_delivery(state: &AppState) -> WebhookDeliveryRecord {
    while let Some(id) = state.serving.meta.next_webhook_event_id().unwrap() {
        state.serving.meta.fan_out_webhook_event(&id).unwrap();
    }
    let mut deliveries = state.serving.meta.list_webhook_deliveries().unwrap();
    assert_eq!(deliveries.len(), 1);
    deliveries.pop().unwrap()
}

async fn push_manifest(app: &axum::Router, manifest: &[u8], reference: &str) {
    let digest = oci_digest(CONFIG);
    send_body(
        app,
        Method::POST,
        &format!("/v2/store/app/blobs/uploads/?digest={digest}"),
        &[("authorization", &auth(TOKEN))],
        CONFIG.to_vec(),
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
    push_manifest(&app, &image_manifest(), "1.0").await;

    let delivery = enqueued_delivery(&state);
    assert_eq!(delivery.event, "manifest-push");
    let payload: serde_json::Value = serde_json::from_str(&delivery.payload).unwrap();
    assert_eq!(payload["schema"], "oci.v1");
    assert_eq!(payload["event"], "manifest-push");
    assert_eq!(payload["index"], "store");
    assert_eq!(payload["data"]["repository"], "app");
    assert_eq!(payload["data"]["reference"], "1.0");
    assert_eq!(payload["actor"], "uploader");
    assert_eq!(payload["request_id"], "req-42");
}

#[tokio::test]
async fn test_manifest_push_by_digest_fires_manifest_push_webhook() {
    let dir = tempfile::tempdir().unwrap();
    let (state, app) = hosted_with_webhook(&dir, &["manifest-push"]);
    let manifest = image_manifest();
    let digest = oci_digest(&manifest);
    push_manifest(&app, &manifest, &digest).await;

    let payload: serde_json::Value = serde_json::from_str(&enqueued_delivery(&state).payload).unwrap();
    assert_eq!(payload["data"]["digest"], digest);
    assert_eq!(payload["data"]["reference"], serde_json::Value::Null);
}

#[tokio::test]
async fn test_manifest_delete_fires_a_delete_webhook() {
    let dir = tempfile::tempdir().unwrap();
    let (state, app) = hosted_with_webhook(&dir, &["manifest-delete"]);
    push_manifest(&app, &image_manifest(), "2.0").await;

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
async fn test_manifest_delete_by_digest_fires_a_delete_webhook() {
    let dir = tempfile::tempdir().unwrap();
    let (state, app) = hosted_with_webhook(&dir, &["manifest-delete"]);
    let manifest = image_manifest();
    let digest = oci_digest(&manifest);
    push_manifest(&app, &manifest, "2.0").await;

    let status = send_body(
        &app,
        Method::DELETE,
        &format!("/v2/store/app/manifests/{digest}"),
        &[("authorization", &auth(TOKEN))],
        Vec::new(),
    )
    .await;
    assert_eq!(status, StatusCode::ACCEPTED);

    let payload: serde_json::Value = serde_json::from_str(&enqueued_delivery(&state).payload).unwrap();
    assert_eq!(payload["event"], "manifest-delete");
    assert_eq!(payload["data"]["digest"], digest);
    assert_eq!(payload["data"]["reference"], serde_json::Value::Null);
}

#[tokio::test]
async fn test_manifest_restore_fires_a_restore_webhook() {
    let dir = tempfile::tempdir().unwrap();
    let (state, app) = hosted_with_webhook(&dir, &["manifest-restore"]);
    push_manifest(&app, &image_manifest(), "2.0").await;
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
async fn test_manifest_restore_by_digest_fires_a_restore_webhook() {
    let dir = tempfile::tempdir().unwrap();
    let (state, app) = hosted_with_webhook(&dir, &["manifest-restore"]);
    let manifest = image_manifest();
    let digest = oci_digest(&manifest);
    push_manifest(&app, &manifest, "2.0").await;
    send_body(
        &app,
        Method::DELETE,
        &format!("/v2/store/app/manifests/{digest}"),
        &[("authorization", &auth(TOKEN))],
        Vec::new(),
    )
    .await;

    let status = send_body(
        &app,
        Method::PUT,
        &format!("/v2/store/app/manifests/{digest}/restore"),
        &[("authorization", &auth(TOKEN))],
        Vec::new(),
    )
    .await;
    assert_eq!(status, StatusCode::ACCEPTED);

    let payload: serde_json::Value = serde_json::from_str(&enqueued_delivery(&state).payload).unwrap();
    assert_eq!(payload["event"], "manifest-restore");
    assert_eq!(payload["data"]["digest"], digest);
    assert_eq!(payload["data"]["reference"], serde_json::Value::Null);
}

#[tokio::test]
async fn test_manifest_restore_by_digest_reports_a_store_failure() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("read-only.redb");
    drop(MetaStore::open(&path).unwrap());
    let (_state, app) = hosted_with_meta_and_webhook(
        &dir,
        MetaStore::open_existing_read_only(path).unwrap(),
        &["manifest-restore"],
    );
    let digest = format!("sha256:{}", "a".repeat(64));

    let response = send_response(
        &app,
        Method::PUT,
        &format!("/v2/store/app/manifests/{digest}/restore"),
        &[("authorization", &auth(TOKEN))],
        Vec::new(),
    )
    .await;

    assert_eq!(response.status(), StatusCode::BAD_GATEWAY);
    let body = response.into_body().collect().await.unwrap().to_bytes();
    let payload: serde_json::Value = serde_json::from_slice(&body).unwrap();
    assert_eq!(payload["errors"][0]["code"], "UNKNOWN");
    assert!(
        payload["errors"][0]["message"]
            .as_str()
            .unwrap()
            .starts_with("metadata store error:")
    );
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
