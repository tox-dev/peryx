use std::alloc::System;
use std::sync::Arc;

use axum::body::Body;
use axum::http::{Method, Request, StatusCode};
use base64::Engine as _;
use peryx_core::Ecosystem;
use peryx_driver::AppState;
use peryx_identity::IndexAcl;
use peryx_index::{Index, IndexKind};
use peryx_policy::Policy;
use peryx_storage::blob::{BlobStorage, Digest};
use peryx_storage::meta::MetaStore;
use stats_alloc::{INSTRUMENTED_SYSTEM, Region, StatsAlloc};
use tower::ServiceExt as _;

const LARGE_BLOB_BYTES: u64 = 64 << 20;
const MAX_DELETE_ALLOCATION: usize = 1 << 20;
const TOKEN: &str = "upload-token";

#[global_allocator]
static ALLOCATOR: &StatsAlloc<System> = &INSTRUMENTED_SYSTEM;

#[tokio::test]
async fn test_blob_delete_uses_bounded_memory() {
    let dir = tempfile::tempdir().unwrap();
    let meta = MetaStore::open(dir.path().join("peryx.redb")).unwrap();
    let blobs = BlobStorage::filesystem(dir.path().join("blobs"));
    let index = Index {
        name: "store".to_owned(),
        route: "store".to_owned(),
        ecosystem: Ecosystem::Oci,
        kind: IndexKind::Hosted { volatile: true },
        policy: Policy::default(),
        acl: IndexAcl::upload_token(TOKEN),
    };
    let mut state = AppState::with_clock(meta, blobs, 60, vec![index], Arc::new(|| 1000));
    peryx_ecosystem_oci::install(&mut state, std::iter::empty());
    let state = Arc::new(state);
    let app = peryx_http::router(Arc::clone(&state));
    let bytes = b"layer";
    let digest = Digest::of(bytes);
    let canonical = format!("sha256:{}", digest.as_str());
    let upload = Request::builder()
        .method(Method::POST)
        .uri(format!("/v2/store/app/blobs/uploads/?digest={canonical}"))
        .header("authorization", authorization())
        .body(Body::from(bytes.as_slice()))
        .unwrap();
    assert_eq!(app.clone().oneshot(upload).await.unwrap().status(), StatusCode::CREATED);
    let lease = state.blobs.materialize(&digest).await.unwrap();
    std::fs::OpenOptions::new()
        .write(true)
        .open(lease.path())
        .unwrap()
        .set_len(LARGE_BLOB_BYTES)
        .unwrap();
    let delete = Request::builder()
        .method(Method::DELETE)
        .uri(format!("/v2/store/app/blobs/{canonical}"))
        .header("authorization", authorization())
        .body(Body::empty())
        .unwrap();

    let region = Region::new(ALLOCATOR);
    let response = app.oneshot(delete).await.unwrap();
    let allocated = region.change().bytes_allocated;

    assert_eq!(response.status(), StatusCode::ACCEPTED);
    assert!(allocated < MAX_DELETE_ALLOCATION, "delete allocated {allocated} bytes");
    assert!(state.blobs.head(&digest).await.unwrap().is_some());
}

fn authorization() -> String {
    format!(
        "Basic {}",
        base64::engine::general_purpose::STANDARD.encode(format!("_:{TOKEN}"))
    )
}
