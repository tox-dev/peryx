//! Mirroring pulls an image's manifest and every blob it names into the store, so a cached index can
//! serve it offline; verify reports whether the store already holds all of it.

use peryx_storage::blob::Digest;
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

use super::{oci_digest, proxy};
use crate::mirror::{MirrorMode, mirror};

const MANIFEST_TYPE: &str = "application/vnd.oci.image.manifest.v1+json";
const INDEX_TYPE: &str = "application/vnd.oci.image.index.v1+json";
const CONFIG_TYPE: &str = "application/vnd.oci.image.config.v1+json";
const LAYER_TYPE: &str = "application/vnd.oci.image.layer.v1.tar+gzip";

async fn mount_blob(server: &MockServer, repo: &str, bytes: &[u8]) {
    Mock::given(method("GET"))
        .and(path(format!("/v2/{repo}/blobs/{}", oci_digest(bytes))))
        .respond_with(ResponseTemplate::new(200).set_body_raw(bytes.to_vec(), "application/octet-stream"))
        .mount(server)
        .await;
}

async fn mount_manifest(server: &MockServer, repo: &str, reference: &str, body: &[u8], media_type: &str) {
    Mock::given(method("GET"))
        .and(path(format!("/v2/{repo}/manifests/{reference}")))
        .respond_with(ResponseTemplate::new(200).set_body_raw(body.to_vec(), media_type))
        .mount(server)
        .await;
}

fn image_manifest(config: &[u8], layer: &[u8]) -> Vec<u8> {
    format!(
        r#"{{"schemaVersion":2,"mediaType":"{MANIFEST_TYPE}","config":{{"mediaType":"{CONFIG_TYPE}","digest":"{}"}},"layers":[{{"mediaType":"{LAYER_TYPE}","digest":"{}"}}]}}"#,
        oci_digest(config),
        oci_digest(layer),
    )
    .into_bytes()
}

#[tokio::test]
async fn test_mirror_syncs_a_manifest_and_its_blobs() {
    let server = MockServer::start().await;
    let config = b"{}";
    let layer = b"a-layer-of-bytes";
    let manifest = image_manifest(config, layer);
    mount_manifest(&server, "library/app", "latest", &manifest, MANIFEST_TYPE).await;
    mount_blob(&server, "library/app", config).await;
    mount_blob(&server, "library/app", layer).await;

    let dir = tempfile::tempdir().unwrap();
    let (state, _app) = proxy(&dir, &format!("{}/", server.uri()), false);
    let refs = vec!["library/app:latest".to_owned()];
    let rows = mirror(&state, &state.indexes[0], &refs, MirrorMode::Sync)
        .await
        .unwrap();

    let synced: Vec<_> = rows.iter().filter(|row| row.status == "synced").collect();
    assert_eq!(synced.iter().filter(|row| row.kind == "manifest").count(), 1);
    assert_eq!(synced.iter().filter(|row| row.kind == "blob").count(), 2);
    assert_eq!(rows.last().unwrap().kind, "summary");
    assert_eq!(rows.last().unwrap().status, "synced");
    // Both blobs are now on disk.
    assert!(state.blobs.exists(&store_digest(config)));
    assert!(state.blobs.exists(&store_digest(layer)));

    // A second pass finds everything cached, touching no upstream error.
    let verify = mirror(&state, &state.indexes[0], &refs, MirrorMode::Verify)
        .await
        .unwrap();
    assert!(
        verify
            .iter()
            .filter(|row| row.kind != "summary")
            .all(|row| row.status == "cached")
    );
    assert_eq!(verify.last().unwrap().status, "synced");
}

#[tokio::test]
async fn test_mirror_follows_a_manifest_list() {
    let server = MockServer::start().await;
    let config = b"{}";
    let layer = b"platform-layer";
    let child = image_manifest(config, layer);
    let child_digest = oci_digest(&child);
    let index = format!(
        r#"{{"schemaVersion":2,"mediaType":"{INDEX_TYPE}","manifests":[{{"mediaType":"{MANIFEST_TYPE}","digest":"{child_digest}","platform":{{"os":"linux","architecture":"amd64"}}}}]}}"#,
    )
    .into_bytes();
    mount_manifest(&server, "library/multi", "latest", &index, INDEX_TYPE).await;
    mount_manifest(&server, "library/multi", &child_digest, &child, MANIFEST_TYPE).await;
    mount_blob(&server, "library/multi", config).await;
    mount_blob(&server, "library/multi", layer).await;

    let dir = tempfile::tempdir().unwrap();
    let (state, _app) = proxy(&dir, &format!("{}/", server.uri()), false);
    let rows = mirror(
        &state,
        &state.indexes[0],
        &["library/multi:latest".to_owned()],
        MirrorMode::Sync,
    )
    .await
    .unwrap();

    // The list, its one child manifest, and both blobs synced.
    assert_eq!(
        rows.iter()
            .filter(|row| row.kind == "manifest" && row.status == "synced")
            .count(),
        2
    );
    assert_eq!(
        rows.iter()
            .filter(|row| row.kind == "blob" && row.status == "synced")
            .count(),
        2
    );
    assert_eq!(rows.last().unwrap().status, "synced");
}

#[tokio::test]
async fn test_mirror_reports_an_unreachable_reference() {
    let server = MockServer::start().await;
    // No mounts: every fetch 404s.
    let dir = tempfile::tempdir().unwrap();
    let (state, _app) = proxy(&dir, &format!("{}/", server.uri()), false);
    let rows = mirror(
        &state,
        &state.indexes[0],
        &["library/missing:latest".to_owned()],
        MirrorMode::Sync,
    )
    .await
    .unwrap();
    assert_eq!(rows[0].kind, "manifest");
    assert_eq!(rows[0].status, "error");
    assert_eq!(rows.last().unwrap().status, "error");
    assert!(rows.last().unwrap().reason.contains("1 errors"));
}

#[tokio::test]
async fn test_mirror_rejects_a_bad_reference() {
    let dir = tempfile::tempdir().unwrap();
    let (state, _app) = proxy(&dir, "http://127.0.0.1:1/", false);
    let rows = mirror(&state, &state.indexes[0], &["@".to_owned()], MirrorMode::Sync)
        .await
        .unwrap();
    assert_eq!(rows[0].status, "error");
    assert!(rows[0].reason.contains("valid image reference"));
}

#[tokio::test]
async fn test_mirror_needs_a_cached_upstream() {
    let dir = tempfile::tempdir().unwrap();
    let (state, _app) = super::hosted(&dir);
    let rows = mirror(
        &state,
        &state.indexes[0],
        &["library/app:latest".to_owned()],
        MirrorMode::Sync,
    )
    .await
    .unwrap();
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].kind, "summary");
    assert!(rows[0].reason.contains("no cached upstream"));
}

#[tokio::test]
async fn test_verify_flags_a_missing_image() {
    let dir = tempfile::tempdir().unwrap();
    let (state, _app) = proxy(&dir, "http://127.0.0.1:1/", false);
    let rows = mirror(
        &state,
        &state.indexes[0],
        &["library/app:latest".to_owned()],
        MirrorMode::Verify,
    )
    .await
    .unwrap();
    assert_eq!(rows[0].status, "error");
    assert!(rows[0].reason.contains("tag not mirrored"));
}

fn store_digest(bytes: &[u8]) -> Digest {
    Digest::from_hex(Digest::of(bytes).as_str()).unwrap()
}

#[tokio::test]
async fn test_mirror_by_digest_then_verify_missing() {
    let server = MockServer::start().await;
    let config = b"{}";
    let layer = b"digest-layer";
    let manifest = image_manifest(config, layer);
    let manifest_digest = oci_digest(&manifest);
    mount_manifest(&server, "library/app", &manifest_digest, &manifest, MANIFEST_TYPE).await;
    mount_blob(&server, "library/app", config).await;
    mount_blob(&server, "library/app", layer).await;

    let dir = tempfile::tempdir().unwrap();
    let (state, _app) = proxy(&dir, &format!("{}/", server.uri()), false);
    let by_digest = format!("library/app@{manifest_digest}");
    let rows = mirror(
        &state,
        &state.indexes[0],
        std::slice::from_ref(&by_digest),
        MirrorMode::Sync,
    )
    .await
    .unwrap();
    assert_eq!(
        rows.iter()
            .filter(|row| row.status == "synced" && row.kind == "manifest")
            .count(),
        1
    );

    // Verify by the stored digest is cached; a never-seen digest is reported missing.
    let verify = mirror(&state, &state.indexes[0], &[by_digest], MirrorMode::Verify)
        .await
        .unwrap();
    assert!(
        verify
            .iter()
            .any(|row| row.kind == "manifest" && row.status == "cached")
    );
    let absent = format!("library/app@{}", oci_digest(b"never-pushed"));
    let missing = mirror(&state, &state.indexes[0], &[absent], MirrorMode::Verify)
        .await
        .unwrap();
    assert!(missing.iter().any(|row| row.reason == "manifest missing"));
}

#[tokio::test]
async fn test_mirror_bare_name_defaults_to_latest() {
    let server = MockServer::start().await;
    let config = b"{}";
    let layer = b"bare-layer";
    let manifest = image_manifest(config, layer);
    mount_manifest(&server, "alpine", "latest", &manifest, MANIFEST_TYPE).await;
    mount_blob(&server, "alpine", config).await;
    mount_blob(&server, "alpine", layer).await;

    let dir = tempfile::tempdir().unwrap();
    let (state, _app) = proxy(&dir, &format!("{}/", server.uri()), false);
    let rows = mirror(&state, &state.indexes[0], &["alpine".to_owned()], MirrorMode::Sync)
        .await
        .unwrap();
    assert!(
        rows.iter()
            .any(|row| row.kind == "manifest" && row.reference == "latest" && row.status == "synced")
    );
}

#[tokio::test]
async fn test_mirror_reports_a_missing_blob() {
    let server = MockServer::start().await;
    let config = b"{}";
    let layer = b"absent-layer";
    let manifest = image_manifest(config, layer);
    mount_manifest(&server, "library/app", "latest", &manifest, MANIFEST_TYPE).await;
    mount_blob(&server, "library/app", config).await;
    // The layer blob is never mounted, so its fetch 404s.

    let dir = tempfile::tempdir().unwrap();
    let (state, _app) = proxy(&dir, &format!("{}/", server.uri()), false);
    let refs = vec!["library/app:latest".to_owned()];
    let rows = mirror(&state, &state.indexes[0], &refs, MirrorMode::Sync)
        .await
        .unwrap();
    assert!(rows.iter().any(|row| row.kind == "blob" && row.status == "error"));

    // Verify then reports the manifest cached but the layer blob missing.
    let verify = mirror(&state, &state.indexes[0], &refs, MirrorMode::Verify)
        .await
        .unwrap();
    assert!(
        verify
            .iter()
            .any(|row| row.kind == "blob" && row.reason == "blob missing")
    );
}

#[tokio::test]
async fn test_mirror_rejects_an_unsupported_blob_digest() {
    let server = MockServer::start().await;
    let manifest = format!(
        r#"{{"schemaVersion":2,"mediaType":"{MANIFEST_TYPE}","config":{{"mediaType":"{CONFIG_TYPE}","digest":"md5:00112233445566778899aabbccddeeff"}},"layers":[]}}"#,
    )
    .into_bytes();
    mount_manifest(&server, "library/app", "latest", &manifest, MANIFEST_TYPE).await;

    let dir = tempfile::tempdir().unwrap();
    let (state, _app) = proxy(&dir, &format!("{}/", server.uri()), false);
    let rows = mirror(
        &state,
        &state.indexes[0],
        &["library/app:latest".to_owned()],
        MirrorMode::Sync,
    )
    .await
    .unwrap();
    assert!(
        rows.iter()
            .any(|row| row.kind == "blob" && row.reason == "unsupported digest")
    );
}

#[tokio::test]
async fn test_mirror_rejects_a_corrupt_blob() {
    let server = MockServer::start().await;
    let config = b"{}";
    let layer = b"honest-layer";
    let manifest = image_manifest(config, layer);
    mount_manifest(&server, "library/app", "latest", &manifest, MANIFEST_TYPE).await;
    mount_blob(&server, "library/app", config).await;
    // The layer blob is served with bytes that do not hash to its advertised digest.
    Mock::given(method("GET"))
        .and(path(format!("/v2/library/app/blobs/{}", oci_digest(layer))))
        .respond_with(ResponseTemplate::new(200).set_body_raw(b"tampered".to_vec(), "application/octet-stream"))
        .mount(&server)
        .await;

    let dir = tempfile::tempdir().unwrap();
    let (state, _app) = proxy(&dir, &format!("{}/", server.uri()), false);
    let rows = mirror(
        &state,
        &state.indexes[0],
        &["library/app:latest".to_owned()],
        MirrorMode::Sync,
    )
    .await
    .unwrap();
    assert!(rows.iter().any(|row| row.reason == "digest verification failed"));
}

#[tokio::test]
async fn test_mirror_tolerates_a_non_json_manifest() {
    let server = MockServer::start().await;
    mount_manifest(&server, "library/app", "latest", b"this is not json", MANIFEST_TYPE).await;

    let dir = tempfile::tempdir().unwrap();
    let (state, _app) = proxy(&dir, &format!("{}/", server.uri()), false);
    let rows = mirror(
        &state,
        &state.indexes[0],
        &["library/app:latest".to_owned()],
        MirrorMode::Sync,
    )
    .await
    .unwrap();
    // The manifest stores, but naming no blobs there is nothing else to fetch.
    assert!(rows.iter().any(|row| row.kind == "manifest" && row.status == "synced"));
    assert!(!rows.iter().any(|row| row.kind == "blob"));
}
