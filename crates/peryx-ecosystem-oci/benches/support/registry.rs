use std::hash::BuildHasher;
use std::sync::Arc;

use axum::Router;
use axum::body::Body;
use http::Request;
use http_body_util::BodyExt as _;
use peryx_core::Ecosystem;
use peryx_driver::AppState;
use peryx_ecosystem_oci::{OCI_LEXICON, OciIndexer, OciRegistryWithHasher};
use peryx_http::router;
use peryx_identity::IndexAcl;
use peryx_index::{Index, IndexKind};
use peryx_policy::Policy;
use peryx_storage::blob::{BlobStore, Digest};
use peryx_storage::meta::MetaStore;
use tokio::runtime::Runtime;
use tower::ServiceExt as _;

const TOKEN: &str = "bench-token";

pub fn runtime() -> Runtime {
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap()
}

pub fn seeded<S>(runtime: &Runtime, registry: OciRegistryWithHasher<S>) -> (tempfile::TempDir, Router, String, String)
where
    S: BuildHasher + Default + Send + Sync + 'static,
{
    let dir = tempfile::tempdir().unwrap();
    let meta = MetaStore::open(dir.path().join("peryx.redb")).unwrap();
    let blobs = BlobStore::new(dir.path().join("blobs"));
    let index = Index {
        name: "store".to_owned(),
        route: "store".to_owned(),
        ecosystem: Ecosystem::Oci,
        kind: IndexKind::Hosted { volatile: true },
        policy: Policy::default(),
        acl: IndexAcl::upload_token(TOKEN.to_owned()),
    };
    let mut state = AppState::with_clock(meta, blobs, 60, vec![index], Arc::new(|| 1000));
    state.register_ecosystem(Arc::new(registry), Arc::new(OciIndexer));
    state.register_lexicon(Ecosystem::Oci, &OCI_LEXICON);
    let blob = vec![0x7fu8; 4096];
    let blob_digest = format!("sha256:{}", Digest::of(&blob).as_str());
    let app = router(Arc::new(state));

    let request = Request::builder()
        .method("POST")
        .uri(format!("/v2/store/app/blobs/uploads/?digest={blob_digest}"))
        .header("authorization", auth())
        .body(Body::from(blob))
        .unwrap();
    let response = runtime.block_on(app.clone().oneshot(request)).unwrap();
    assert_eq!(response.status(), 201);

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
    (dir, app, manifest_digest, blob_digest)
}

pub async fn get(app: &Router, uri: &str) -> usize {
    let request = Request::builder().uri(uri).body(Body::empty()).unwrap();
    let response = app.clone().oneshot(request).await.unwrap();
    assert!(response.status().is_success());
    response.into_body().collect().await.unwrap().to_bytes().len()
}

fn auth() -> String {
    use base64::Engine as _;
    format!(
        "Basic {}",
        base64::engine::general_purpose::STANDARD.encode(format!("_:{TOKEN}"))
    )
}
