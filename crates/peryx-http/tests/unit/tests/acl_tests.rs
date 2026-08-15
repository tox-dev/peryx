//! `GET /+acl`: one index's tokens, grants, and read policy, gated on an index-administering token and
//! with every secret redacted.

use std::collections::BTreeSet;
use std::sync::Arc;

use axum::body::Body;
use axum::http::{Request, StatusCode, header};
use base64::Engine as _;
use base64::engine::general_purpose::STANDARD;
use tower::ServiceExt as _;

use peryx_core::Ecosystem;
use peryx_driver::state::{AppState, Index, IndexKind};
use peryx_identity::{Action, Glob, Grant, IndexAcl, NamedToken};

const ADMIN_SECRET: &str = "admin-secret";
const READER_SECRET: &str = "reader-secret";

fn acl_app() -> (tempfile::TempDir, axum::Router) {
    let dir = tempfile::tempdir().unwrap();
    let meta = peryx_storage::meta::MetaStore::open(dir.path().join("peryx.redb")).unwrap();
    let blobs = peryx_storage::blob::BlobStore::new(dir.path().join("blobs"));
    let index = Index {
        name: "hosted".to_owned(),
        route: "hosted".to_owned(),
        ecosystem: Ecosystem::new("example"),
        kind: IndexKind::Hosted { volatile: false },
        policy: peryx_policy::Policy::default(),
        acl: IndexAcl {
            anonymous_read: true,
            tokens: vec![
                NamedToken {
                    name: "uploader".to_owned(),
                    secret: ADMIN_SECRET.to_owned(),
                    grants: vec![Grant {
                        resources: vec![Glob::new("*")],
                        actions: BTreeSet::from([Action::Write, Action::Delete]),
                    }],
                    expires_at: None,
                },
                NamedToken {
                    name: "ci".to_owned(),
                    secret: READER_SECRET.to_owned(),
                    grants: vec![Grant {
                        resources: vec![Glob::new("team/*")],
                        actions: BTreeSet::from([Action::Read]),
                    }],
                    expires_at: Some(1_800_000_000),
                },
            ],
        },
    };
    let state = AppState::new(meta, blobs, 60, vec![index]);
    (dir, crate::router(Arc::new(state)))
}

async fn get(app: &axum::Router, uri: &str, secret: Option<&str>) -> (StatusCode, serde_json::Value) {
    let mut request = Request::builder().uri(uri);
    if let Some(secret) = secret {
        let credential = STANDARD.encode(format!("anyuser:{secret}"));
        request = request.header(header::AUTHORIZATION, format!("Basic {credential}"));
    }
    let response = app.clone().oneshot(request.body(Body::empty()).unwrap()).await.unwrap();
    let status = response.status();
    let body = axum::body::to_bytes(response.into_body(), usize::MAX).await.unwrap();
    let document = serde_json::from_slice(&body).unwrap_or(serde_json::Value::Null);
    (status, document)
}

#[tokio::test]
async fn test_acl_lists_tokens_and_read_policy_for_an_administering_token() {
    let (_dir, app) = acl_app();
    let (status, document) = get(&app, "/+acl?index=hosted", Some(ADMIN_SECRET)).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(document["index"], "hosted");
    assert_eq!(document["route"], "hosted");
    assert_eq!(document["anonymous_read"], true);
    assert_eq!(
        document["tokens"],
        serde_json::json!([
            {"name": "uploader", "secret": {"configured": true, "redacted": "<redacted>"},
             "expires_at": null, "grants": [{"resources": ["*"], "actions": ["write", "delete"]}]},
            {"name": "ci", "secret": {"configured": true, "redacted": "<redacted>"},
             "expires_at": 1_800_000_000, "grants": [{"resources": ["team/*"], "actions": ["read"]}]},
        ])
    );
}

#[tokio::test]
async fn test_acl_never_returns_a_token_secret() {
    let (_dir, app) = acl_app();
    let (_status, document) = get(&app, "/+acl?index=hosted", Some(ADMIN_SECRET)).await;
    let body = serde_json::to_string(&document).unwrap();
    assert!(!body.contains(ADMIN_SECRET));
    assert!(!body.contains(READER_SECRET));
}

#[tokio::test]
async fn test_acl_rejects_an_anonymous_request_with_401() {
    let (_dir, app) = acl_app();
    let (status, _document) = get(&app, "/+acl?index=hosted", None).await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);
}

#[tokio::test]
async fn test_acl_rejects_a_read_only_token_with_403() {
    let (_dir, app) = acl_app();
    let (status, _document) = get(&app, "/+acl?index=hosted", Some(READER_SECRET)).await;
    assert_eq!(status, StatusCode::FORBIDDEN);
}

#[tokio::test]
async fn test_acl_is_404_for_an_unknown_index() {
    let (_dir, app) = acl_app();
    let (status, _document) = get(&app, "/+acl?index=nope", Some(ADMIN_SECRET)).await;
    assert_eq!(status, StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn test_acl_response_is_not_cached() {
    let (_dir, app) = acl_app();
    let credential = STANDARD.encode(format!("anyuser:{ADMIN_SECRET}"));
    let response = app
        .oneshot(
            Request::builder()
                .uri("/+acl?index=hosted")
                .header(header::AUTHORIZATION, format!("Basic {credential}"))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(response.headers()[header::CACHE_CONTROL], "no-store");
}
