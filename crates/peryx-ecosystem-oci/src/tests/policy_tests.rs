//! An OCI index reuses the neutral policy allow/block-list: a blocked image name is hidden on reads
//! (served as absent, like a policy-denied pypi project) and refused on writes.

use std::sync::Arc;

use axum::http::{Method, StatusCode};
use peryx_core::Ecosystem;
use peryx_driver::AppState;
use peryx_http::router;
use peryx_index::{Index, IndexKind};
use peryx_policy::{Policy, PolicyConfig};
use peryx_storage::blob::BlobStore;
use peryx_storage::meta::MetaStore;

use super::{auth, oci_digest, send, send_body};
use crate::store::{self, Manifest};

const TOKEN: &str = "s3cret";
const MANIFEST_TYPE: &str = "application/vnd.oci.image.manifest.v1+json";

/// A hosted OCI index `store` whose policy blocks the `blocked/app` repository.
fn store_blocking(dir: &tempfile::TempDir) -> (Arc<AppState>, axum::Router) {
    let meta = MetaStore::open(dir.path().join("peryx.redb")).unwrap();
    let blobs = BlobStore::new(dir.path().join("blobs"));
    let policy = Policy::compile(
        &PolicyConfig {
            block_projects: vec!["blocked/app".to_owned()],
            ..PolicyConfig::default()
        },
        str::to_owned,
    );
    let mut state = AppState::with_clock(
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
            policy,
        }],
        Arc::new(|| 1000),
    );
    crate::install(&mut state, std::collections::HashMap::new());
    let state = Arc::new(state);
    (state.clone(), router(state))
}

#[tokio::test]
async fn test_policy_hides_a_blocked_manifest_on_serve() {
    let dir = tempfile::tempdir().unwrap();
    let (state, app) = store_blocking(&dir);
    let bytes = br#"{"schemaVersion":2}"#;
    let digest = oci_digest(bytes);
    let manifest = Manifest {
        media_type: MANIFEST_TYPE.to_owned(),
        bytes: bytes.to_vec(),
    };
    // Seed the same image under a blocked and an allowed repository.
    store::put_manifest(&state.meta, &digest, &manifest).unwrap();
    store::put_tag(&state.meta, "store", "blocked/app", "1.0", &digest).unwrap();
    store::put_tag(&state.meta, "store", "public/app", "1.0", &digest).unwrap();

    let (status, _, _) = send(&app, Method::GET, "/v2/store/blocked/app/manifests/1.0").await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    let (status, _, _) = send(&app, Method::GET, "/v2/store/public/app/manifests/1.0").await;
    assert_eq!(status, StatusCode::OK);
}

#[tokio::test]
async fn test_policy_refuses_a_blocked_push() {
    let dir = tempfile::tempdir().unwrap();
    let (_state, app) = store_blocking(&dir);
    let manifest = br#"{"schemaVersion":2}"#;
    let headers = [
        ("authorization", auth(TOKEN)),
        ("content-type", MANIFEST_TYPE.to_owned()),
    ];
    let refs: Vec<(&str, &str)> = headers.iter().map(|(k, v)| (*k, v.as_str())).collect();

    let (status, _, body) = send_body(
        &app,
        Method::PUT,
        "/v2/store/blocked/app/manifests/1.0",
        &refs,
        manifest.to_vec(),
    )
    .await;
    assert_eq!(status, StatusCode::FORBIDDEN);
    assert!(String::from_utf8_lossy(&body).contains("DENIED"));

    let (status, _, _) = send_body(
        &app,
        Method::PUT,
        "/v2/store/public/app/manifests/1.0",
        &refs,
        manifest.to_vec(),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED);
}

#[tokio::test]
async fn test_policy_hides_a_blocked_blob_on_serve() {
    let dir = tempfile::tempdir().unwrap();
    let (_state, app) = store_blocking(&dir);
    let blob = b"a-shared-layer";
    let digest = oci_digest(blob);
    // Push the blob through an allowed repository; blobs are content-addressed and shared.
    let (status, _, _) = send_body(
        &app,
        Method::POST,
        &format!("/v2/store/public/app/blobs/uploads/?digest={digest}"),
        &[("authorization", &auth(TOKEN))],
        blob.to_vec(),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED);

    // The same digest is hidden under the blocked repository but served under the allowed one.
    let (status, _, _) = send(&app, Method::GET, &format!("/v2/store/blocked/app/blobs/{digest}")).await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    let (status, _, got) = send(&app, Method::GET, &format!("/v2/store/public/app/blobs/{digest}")).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(got, &blob[..]);

    // The layer browser hides a blocked repository's layer the same way the blob route does.
    let (status, _, _) = send(
        &app,
        Method::GET,
        &format!("/v2/store/blocked/app/blobs/{digest}/contents"),
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn test_policy_refuses_a_blocked_cross_repo_mount() {
    let dir = tempfile::tempdir().unwrap();
    let (_state, app) = store_blocking(&dir);
    let blob = b"a-shared-layer";
    let digest = oci_digest(blob);
    let (status, _, _) = send_body(
        &app,
        Method::POST,
        &format!("/v2/store/public/app/blobs/uploads/?digest={digest}"),
        &[("authorization", &auth(TOKEN))],
        blob.to_vec(),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED);

    // Mounting the existing blob into a blocked repository must be denied, not silently created.
    let (status, _, body) = send_body(
        &app,
        Method::POST,
        &format!("/v2/store/blocked/app/blobs/uploads/?mount={digest}&from=public/app"),
        &[("authorization", &auth(TOKEN))],
        Vec::new(),
    )
    .await;
    assert_eq!(status, StatusCode::FORBIDDEN);
    assert!(String::from_utf8_lossy(&body).contains("DENIED"));
}

#[tokio::test]
async fn test_policy_hides_a_blocked_repository_from_tags_referrers_and_catalog() {
    let dir = tempfile::tempdir().unwrap();
    let (state, app) = store_blocking(&dir);
    let bytes = br#"{"schemaVersion":2}"#;
    let digest = oci_digest(bytes);
    let manifest = Manifest {
        media_type: MANIFEST_TYPE.to_owned(),
        bytes: bytes.to_vec(),
    };
    store::put_manifest(&state.meta, &digest, &manifest).unwrap();
    for repo in ["blocked/app", "public/app"] {
        store::put_tag(&state.meta, "store", repo, "1.0", &digest).unwrap();
        store::put_referrer(&state.meta, "store", repo, &digest, &digest, br#"{"digest":"x"}"#).unwrap();
    }

    // The tag list and referrers of a blocked repository are hidden, exactly as its manifest is.
    let (status, _, _) = send(&app, Method::GET, "/v2/store/blocked/app/tags/list").await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    let (status, _, _) = send(&app, Method::GET, &format!("/v2/store/blocked/app/referrers/{digest}")).await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    let (status, _, body) = send(&app, Method::GET, "/v2/store/public/app/tags/list").await;
    assert_eq!(status, StatusCode::OK);
    assert!(String::from_utf8_lossy(&body).contains("1.0"));

    // The catalog lists the allowed repository but omits the blocked one.
    let (status, _, body) = send(&app, Method::GET, "/v2/_catalog").await;
    assert_eq!(status, StatusCode::OK);
    let catalog = String::from_utf8_lossy(&body);
    assert!(catalog.contains("store/public/app"), "{catalog}");
    assert!(!catalog.contains("store/blocked/app"), "{catalog}");
}

/// A writable hosted store whose policy caps a blob at `max_file_size_bytes` bytes.
fn store_size_limited(dir: &tempfile::TempDir, limit: u64) -> (Arc<AppState>, axum::Router) {
    let meta = MetaStore::open(dir.path().join("peryx.redb")).unwrap();
    let blobs = BlobStore::new(dir.path().join("blobs"));
    let policy = Policy::compile(
        &PolicyConfig {
            max_file_size_bytes: Some(limit),
            ..PolicyConfig::default()
        },
        str::to_owned,
    );
    let mut state = AppState::with_clock(
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
            policy,
        }],
        Arc::new(|| 1000),
    );
    crate::install(&mut state, std::collections::HashMap::new());
    let state = Arc::new(state);
    (state.clone(), router(state))
}

#[tokio::test]
async fn test_policy_refuses_a_monolithic_blob_over_the_size_limit() {
    let dir = tempfile::tempdir().unwrap();
    let (_state, app) = store_size_limited(&dir, 4);

    let big = b"toobig";
    let big_digest = oci_digest(big);
    let (status, _, body) = send_body(
        &app,
        Method::POST,
        &format!("/v2/store/app/blobs/uploads/?digest={big_digest}"),
        &[("authorization", &auth(TOKEN))],
        big.to_vec(),
    )
    .await;
    assert_eq!(status, StatusCode::FORBIDDEN);
    assert!(String::from_utf8_lossy(&body).contains("DENIED"));

    let small = b"ok";
    let small_digest = oci_digest(small);
    let (status, _, _) = send_body(
        &app,
        Method::POST,
        &format!("/v2/store/app/blobs/uploads/?digest={small_digest}"),
        &[("authorization", &auth(TOKEN))],
        small.to_vec(),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED);
}

#[tokio::test]
async fn test_policy_refuses_a_chunked_blob_over_the_size_limit() {
    let dir = tempfile::tempdir().unwrap();
    let (_state, app) = store_size_limited(&dir, 4);

    let (status, headers, _) = send_body(
        &app,
        Method::POST,
        "/v2/store/app/blobs/uploads/",
        &[("authorization", &auth(TOKEN))],
        Vec::new(),
    )
    .await;
    assert_eq!(status, StatusCode::ACCEPTED);
    let location = headers["location"].to_str().unwrap().to_owned();
    let big = b"way too big";
    let digest = oci_digest(big);
    let (status, _, body) = send_body(
        &app,
        Method::PUT,
        &format!("{location}?digest={digest}"),
        &[("authorization", &auth(TOKEN))],
        big.to_vec(),
    )
    .await;
    assert_eq!(status, StatusCode::FORBIDDEN);
    assert!(String::from_utf8_lossy(&body).contains("DENIED"));
}
