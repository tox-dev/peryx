//! Per-operation benchmarks for the OCI driver: the hot serve paths a registry client exercises on
//! every pull: the `/v2/` version check, a manifest fetched by digest, and a blob served from the
//! content-addressed store. Driven end to end through the router, so route classification, index
//! resolution, and the store read are all in the measured path.
#![allow(
    clippy::significant_drop_tightening,
    reason = "criterion_group! expands to a temporary the nursery lint flags"
)]

use std::sync::Arc;

use axum::Router;
use axum::body::Body;
use criterion::{Criterion, criterion_group, criterion_main};
use http::Request;
use http_body_util::BodyExt as _;
use tokio::runtime::Runtime;
use tower::ServiceExt as _;
use peryx_format::Ecosystem;
use peryx_http::{AppState, Index, IndexKind, router};
use peryx_policy::Policy;
use peryx_storage::blob::{BlobStore, Digest};
use peryx_storage::meta::MetaStore;

const TOKEN: &str = "bench-token";

fn runtime() -> Runtime {
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap()
}

fn auth() -> String {
    use base64::Engine as _;
    format!(
        "Basic {}",
        base64::engine::general_purpose::STANDARD.encode(format!("_:{TOKEN}"))
    )
}

/// A hosted OCI registry seeded with one blob and one tagged manifest; returns the app and the two
/// digests to fetch by.
fn seeded(runtime: &Runtime) -> (Router, String, String) {
    let dir = tempfile::tempdir().unwrap();
    let meta = MetaStore::open(dir.path().join("peryx.redb")).unwrap();
    let blobs = BlobStore::new(dir.path().join("blobs"));
    std::mem::forget(dir);
    let index = Index {
        name: "store".to_owned(),
        route: "store".to_owned(),
        ecosystem: Ecosystem::Oci,
        kind: IndexKind::Hosted {
            upload_token: Some(TOKEN.to_owned()),
            volatile: true,
        },
        policy: Policy::default(),
    };
    let mut state = AppState::with_clock(meta, blobs, 60, vec![index], Arc::new(|| 1000));
    peryx_ecosystem_oci::install(&mut state);
    let state = Arc::new(state);
    let blob = vec![0x7fu8; 4096];
    let blob_digest = format!("sha256:{}", state.blobs.write(&blob).unwrap().as_str());
    let app = router(state);

    let manifest = br#"{"schemaVersion":2,"mediaType":"application/vnd.oci.image.manifest.v1+json"}"#;
    let manifest_digest = format!("sha256:{}", Digest::of(manifest).as_str());
    let request = Request::builder()
        .method("PUT")
        .uri("/v2/store/app/manifests/v1")
        .header("authorization", auth())
        .header("content-type", "application/vnd.oci.image.manifest.v1+json")
        .body(Body::from(manifest.to_vec()))
        .unwrap();
    let response = runtime.block_on(app.clone().oneshot(request)).unwrap();
    assert_eq!(response.status(), 201);
    (app, manifest_digest, blob_digest)
}

async fn get(app: &Router, uri: &str) -> usize {
    let request = Request::builder().uri(uri).body(Body::empty()).unwrap();
    let response = app.clone().oneshot(request).await.unwrap();
    assert!(response.status().is_success());
    response.into_body().collect().await.unwrap().to_bytes().len()
}

fn operations(criterion: &mut Criterion) {
    let runtime = runtime();
    let (app, manifest_digest, blob_digest) = seeded(&runtime);
    let manifest_uri = format!("/v2/store/app/manifests/{manifest_digest}");
    let blob_uri = format!("/v2/store/app/blobs/{blob_digest}");

    criterion.bench_function("oci_version_check", |bencher| {
        bencher.to_async(&runtime).iter(|| get(&app, "/v2/"));
    });
    criterion.bench_function("oci_manifest_by_digest", |bencher| {
        bencher.to_async(&runtime).iter(|| get(&app, &manifest_uri));
    });
    criterion.bench_function("oci_blob_serve", |bencher| {
        bencher.to_async(&runtime).iter(|| get(&app, &blob_uri));
    });
    criterion.bench_function("oci_tags_list", |bencher| {
        bencher.to_async(&runtime).iter(|| get(&app, "/v2/store/app/tags/list"));
    });
}

criterion_group!(benches, operations);
criterion_main!(benches);
