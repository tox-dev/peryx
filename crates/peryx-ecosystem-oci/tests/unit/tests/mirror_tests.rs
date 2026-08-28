use axum::http::{Method, StatusCode};
use peryx_storage::blob::Digest;
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

use super::{oci_digest, proxy, search_total, send};
use crate::mirror::{MirrorMode, MirrorRow, mirror as mirror_with};
use crate::settings::IndexSettings;
use peryx_driver::ServingState;
use peryx_index::Index;
use std::sync::Arc;

async fn mirror(
    state: &Arc<ServingState>,
    index: &Index,
    refs: &[String],
    mode: MirrorMode,
) -> anyhow::Result<Vec<MirrorRow>> {
    mirror_with(state, index, IndexSettings::default(), refs, mode).await
}

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
    image_manifest_with_layers(config, &[layer])
}

fn image_manifest_with_layers(config: &[u8], layers: &[&[u8]]) -> Vec<u8> {
    let layers = layers
        .iter()
        .map(|layer| format!(r#"{{"mediaType":"{LAYER_TYPE}","digest":"{}"}}"#, oci_digest(layer)))
        .collect::<Vec<_>>()
        .join(",");
    format!(
        r#"{{"schemaVersion":2,"mediaType":"{MANIFEST_TYPE}","config":{{"mediaType":"{CONFIG_TYPE}","digest":"{}"}},"layers":[{layers}]}}"#,
        oci_digest(config),
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
    let rows = mirror(&state.serving, &state.serving.indexes[0], &refs, MirrorMode::Sync)
        .await
        .unwrap();

    let synced: Vec<_> = rows.iter().filter(|row| row.status == "synced").collect();
    assert_eq!(synced.iter().filter(|row| row.kind == "manifest").count(), 1);
    assert_eq!(synced.iter().filter(|row| row.kind == "blob").count(), 2);
    assert_eq!(rows.last().unwrap().kind, "summary");
    assert_eq!(rows.last().unwrap().status, "synced");
    assert!(state.serving.blobs.head(&store_digest(config)).await.unwrap().is_some());
    assert!(state.serving.blobs.head(&store_digest(layer)).await.unwrap().is_some());

    let verify = mirror(&state.serving, &state.serving.indexes[0], &refs, MirrorMode::Verify)
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
async fn test_mirror_summary_counts_mixed_results_and_bytes() {
    let server = MockServer::start().await;
    let cached = b"cached-config";
    let seed_layer = b"seed-layer";
    let seed_manifest = image_manifest(cached, seed_layer);
    mount_manifest(&server, "library/app", "seed", &seed_manifest, MANIFEST_TYPE).await;
    mount_blob(&server, "library/app", cached).await;
    mount_blob(&server, "library/app", seed_layer).await;
    let dir = tempfile::tempdir().unwrap();
    let (state, _) = proxy(&dir, &format!("{}/", server.uri()), false);
    mirror(
        &state.serving,
        &state.serving.indexes[0],
        &["library/app:seed".to_owned()],
        MirrorMode::Sync,
    )
    .await
    .unwrap();

    let downloaded = b"downloaded-layer";
    let missing = b"missing-layer";
    let manifest = image_manifest_with_layers(cached, &[downloaded, missing]);
    mount_manifest(&server, "library/app", "mixed", &manifest, MANIFEST_TYPE).await;
    mount_blob(&server, "library/app", downloaded).await;

    let rows = mirror(
        &state.serving,
        &state.serving.indexes[0],
        &["library/app:mixed".to_owned()],
        MirrorMode::Sync,
    )
    .await
    .unwrap();
    let summary = rows.last().unwrap();

    assert_eq!(summary.status, "error");
    assert_eq!(summary.bytes, manifest.len() as u64 + downloaded.len() as u64);
    assert_eq!(summary.reason, "2 synced, 1 cached, 1 errors");
}

#[tokio::test]
async fn test_empty_mirror_summary_reports_no_work() {
    let dir = tempfile::tempdir().unwrap();
    let (state, _) = proxy(&dir, "http://127.0.0.1:1/", false);

    let rows = mirror(&state.serving, &state.serving.indexes[0], &[], MirrorMode::Verify)
        .await
        .unwrap();

    assert_eq!(rows.last().unwrap().reason, "0 synced, 0 cached, 0 errors");
}

#[tokio::test]
async fn test_search_refreshes_after_mirror_inserts_tag() {
    let server = MockServer::start().await;
    mount_manifest(&server, "library/app", "latest", b"{}", MANIFEST_TYPE).await;
    let dir = tempfile::tempdir().unwrap();
    let (state, app) = proxy(&dir, &format!("{}/", server.uri()), false);
    let before = search_total(&app, "app").await;

    mirror(
        &state.serving,
        &state.serving.indexes[0],
        &["library/app:latest".to_owned()],
        MirrorMode::Sync,
    )
    .await
    .unwrap();

    assert_eq!((before, search_total(&app, "app").await), (0, 1));
}

#[tokio::test]
async fn test_mirror_records_tag_freshness_so_the_tag_serves_offline() {
    let server = MockServer::start().await;
    let config = b"{}";
    let layer = b"a-layer-of-bytes";
    let manifest = image_manifest(config, layer);
    let digest = oci_digest(&manifest);
    mount_manifest(&server, "library/app", "latest", &manifest, MANIFEST_TYPE).await;
    mount_blob(&server, "library/app", config).await;
    mount_blob(&server, "library/app", layer).await;

    let dir = tempfile::tempdir().unwrap();
    let (state, app) = proxy(&dir, &format!("{}/", server.uri()), false);
    mirror(
        &state.serving,
        &state.serving.indexes[0],
        &["library/app:latest".to_owned()],
        MirrorMode::Sync,
    )
    .await
    .unwrap();

    assert_eq!(
        crate::store::tag_freshness(&state.serving.meta, "hub", "library/app", "latest").unwrap(),
        Some((1000, digest.clone()))
    );

    drop(server);
    let (status, headers, _) = send(&app, Method::GET, "/v2/hub/library/app/manifests/latest").await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(headers.get("docker-content-digest").unwrap(), &digest);
}

#[tokio::test]
async fn test_mirror_by_digest_rejects_bytes_that_hash_to_something_else() {
    let server = MockServer::start().await;
    let requested = oci_digest(b"the-manifest-we-asked-for");
    let substituted = b"a-substituted-manifest";
    mount_manifest(&server, "library/app", &requested, substituted, MANIFEST_TYPE).await;

    let dir = tempfile::tempdir().unwrap();
    let (state, _app) = proxy(&dir, &format!("{}/", server.uri()), false);
    let by_digest = format!("library/app@{requested}");
    let rows = mirror(
        &state.serving,
        &state.serving.indexes[0],
        std::slice::from_ref(&by_digest),
        MirrorMode::Sync,
    )
    .await
    .unwrap();

    let row = rows
        .iter()
        .find(|row| row.kind == "manifest")
        .expect("a manifest row is reported");
    assert_eq!(row.status, "error");
    assert!(row.reason.contains("does not match requested"), "{}", row.reason);
    assert!(row.reason.contains(&requested), "{}", row.reason);
    assert!(
        crate::store::get_manifest(&state.serving.meta, &oci_digest(substituted))
            .unwrap()
            .is_none()
    );
}

#[tokio::test]
async fn test_mirror_by_digest_accepts_a_non_sha256_algorithm() {
    let server = MockServer::start().await;
    let config = b"{}";
    let layer = b"a-layer-of-bytes";
    let manifest = image_manifest(config, layer);
    let requested = format!("sha512:{}", "a".repeat(128));
    mount_manifest(&server, "library/app", &requested, &manifest, MANIFEST_TYPE).await;
    mount_blob(&server, "library/app", config).await;
    mount_blob(&server, "library/app", layer).await;

    let dir = tempfile::tempdir().unwrap();
    let (state, _app) = proxy(&dir, &format!("{}/", server.uri()), false);
    let by_digest = format!("library/app@{requested}");
    let rows = mirror(
        &state.serving,
        &state.serving.indexes[0],
        std::slice::from_ref(&by_digest),
        MirrorMode::Sync,
    )
    .await
    .unwrap();

    let row = rows
        .iter()
        .find(|row| row.kind == "manifest")
        .expect("a manifest row is reported");
    assert_eq!(row.status, "synced", "{}", row.reason);
    assert!(
        crate::store::get_manifest(&state.serving.meta, &oci_digest(&manifest))
            .unwrap()
            .is_some()
    );
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
        &state.serving,
        &state.serving.indexes[0],
        &["library/multi:latest".to_owned()],
        MirrorMode::Sync,
    )
    .await
    .unwrap();

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

/// The marker gives diamond parents distinct digests.
fn index_over(children: &[&str], marker: &str) -> Vec<u8> {
    let entries = children
        .iter()
        .map(|digest| format!(r#"{{"mediaType":"{MANIFEST_TYPE}","digest":"{digest}"}}"#))
        .collect::<Vec<_>>()
        .join(",");
    format!(
        r#"{{"schemaVersion":2,"mediaType":"{INDEX_TYPE}","annotations":{{"marker":"{marker}"}},"manifests":[{entries}]}}"#,
    )
    .into_bytes()
}

async fn manifest_fetches(server: &MockServer, repo: &str, reference: &str) -> usize {
    let target = format!("/v2/{repo}/manifests/{reference}");
    server
        .received_requests()
        .await
        .unwrap()
        .iter()
        .filter(|req| req.url.path() == target)
        .count()
}

#[tokio::test]
async fn test_mirror_terminates_on_a_self_referential_manifest() {
    let server = MockServer::start().await;
    let own = format!("sha512:{}", "a".repeat(128));
    let index = index_over(&[own.as_str()], "self");
    mount_manifest(&server, "library/loop", "latest", &index, INDEX_TYPE).await;
    mount_manifest(&server, "library/loop", &own, &index, INDEX_TYPE).await;

    let dir = tempfile::tempdir().unwrap();
    let (state, _app) = proxy(&dir, &format!("{}/", server.uri()), false);
    let rows = mirror(
        &state.serving,
        &state.serving.indexes[0],
        &["library/loop:latest".to_owned()],
        MirrorMode::Sync,
    )
    .await
    .unwrap();

    assert_eq!(rows.last().unwrap().status, "synced");
    assert_eq!(manifest_fetches(&server, "library/loop", &own).await, 1);
}

#[tokio::test]
async fn test_mirror_terminates_on_a_two_node_cycle() {
    let server = MockServer::start().await;
    let a = format!("sha512:{}", "a".repeat(128));
    let b = format!("sha512:{}", "b".repeat(128));
    let index_a = index_over(&[b.as_str()], "a");
    let index_b = index_over(&[a.as_str()], "b");
    mount_manifest(&server, "library/cycle", "latest", &index_a, INDEX_TYPE).await;
    mount_manifest(&server, "library/cycle", &a, &index_a, INDEX_TYPE).await;
    mount_manifest(&server, "library/cycle", &b, &index_b, INDEX_TYPE).await;

    let dir = tempfile::tempdir().unwrap();
    let (state, _app) = proxy(&dir, &format!("{}/", server.uri()), false);
    let rows = mirror(
        &state.serving,
        &state.serving.indexes[0],
        &["library/cycle:latest".to_owned()],
        MirrorMode::Sync,
    )
    .await
    .unwrap();

    assert_eq!(rows.last().unwrap().status, "synced");
    assert_eq!(manifest_fetches(&server, "library/cycle", &a).await, 1);
    assert_eq!(manifest_fetches(&server, "library/cycle", &b).await, 1);
}

#[tokio::test]
async fn test_mirror_deduplicates_a_diamond_of_shared_descendants() {
    let server = MockServer::start().await;
    let config = b"{}";
    let layer = b"shared-descendant-layer";
    let leaf = image_manifest(config, layer);
    let leaf_digest = oci_digest(&leaf);
    let left = index_over(&[leaf_digest.as_str()], "left");
    let right = index_over(&[leaf_digest.as_str()], "right");
    let left_digest = oci_digest(&left);
    let right_digest = oci_digest(&right);
    let root = index_over(&[left_digest.as_str(), right_digest.as_str()], "root");
    mount_manifest(&server, "library/diamond", "latest", &root, INDEX_TYPE).await;
    mount_manifest(&server, "library/diamond", &left_digest, &left, INDEX_TYPE).await;
    mount_manifest(&server, "library/diamond", &right_digest, &right, INDEX_TYPE).await;
    mount_manifest(&server, "library/diamond", &leaf_digest, &leaf, MANIFEST_TYPE).await;
    mount_blob(&server, "library/diamond", config).await;
    mount_blob(&server, "library/diamond", layer).await;

    let dir = tempfile::tempdir().unwrap();
    let (state, _app) = proxy(&dir, &format!("{}/", server.uri()), false);
    let rows = mirror(
        &state.serving,
        &state.serving.indexes[0],
        &["library/diamond:latest".to_owned()],
        MirrorMode::Sync,
    )
    .await
    .unwrap();

    assert_eq!(rows.last().unwrap().status, "synced");
    assert_eq!(manifest_fetches(&server, "library/diamond", &leaf_digest).await, 1);
}

#[tokio::test]
async fn test_mirror_bounds_an_over_wide_manifest_graph() {
    let server = MockServer::start().await;
    let children: Vec<String> = (0..1100).map(|index| format!("sha512:{index:0128x}")).collect();
    let refs: Vec<&str> = children.iter().map(String::as_str).collect();
    let root = index_over(&refs, "wide");
    mount_manifest(&server, "library/wide", "latest", &root, INDEX_TYPE).await;

    let dir = tempfile::tempdir().unwrap();
    let (state, _app) = proxy(&dir, &format!("{}/", server.uri()), false);
    let rows = mirror(
        &state.serving,
        &state.serving.indexes[0],
        &["library/wide:latest".to_owned()],
        MirrorMode::Sync,
    )
    .await
    .unwrap();

    assert!(
        rows.iter().any(|row| row.reason.contains("exceeds 1024 nodes")),
        "{rows:?}"
    );
    assert_eq!(manifest_fetches(&server, "library/wide", &children[0]).await, 0);
}

#[tokio::test]
async fn test_mirror_bounds_a_too_deep_manifest_graph() {
    let server = MockServer::start().await;
    let config = b"{}";
    let layer = b"deep-layer";
    let leaf = image_manifest(config, layer);
    let mut body = leaf.clone();
    let mut digest = oci_digest(&leaf);
    mount_manifest(&server, "library/deep", &digest, &body, MANIFEST_TYPE).await;
    for level in 0..40 {
        body = index_over(&[digest.as_str()], &format!("level-{level}"));
        digest = oci_digest(&body);
        mount_manifest(&server, "library/deep", &digest, &body, INDEX_TYPE).await;
    }
    mount_blob(&server, "library/deep", config).await;
    mount_blob(&server, "library/deep", layer).await;

    let dir = tempfile::tempdir().unwrap();
    let (state, _app) = proxy(&dir, &format!("{}/", server.uri()), false);
    let by_digest = format!("library/deep@{digest}");
    let rows = mirror(
        &state.serving,
        &state.serving.indexes[0],
        std::slice::from_ref(&by_digest),
        MirrorMode::Sync,
    )
    .await
    .unwrap();

    assert!(
        rows.iter().any(|row| row.reason.contains("exceeds depth 32")),
        "{rows:?}"
    );
    assert_eq!(manifest_fetches(&server, "library/deep", &oci_digest(&leaf)).await, 0);
}

#[tokio::test]
async fn test_mirror_continues_past_a_missing_child_manifest() {
    let server = MockServer::start().await;
    let child = format!("sha512:{}", "c".repeat(128));
    let index = index_over(&[child.as_str()], "root");
    mount_manifest(&server, "library/gap", "latest", &index, INDEX_TYPE).await;

    let dir = tempfile::tempdir().unwrap();
    let (state, _app) = proxy(&dir, &format!("{}/", server.uri()), false);
    let rows = mirror(
        &state.serving,
        &state.serving.indexes[0],
        &["library/gap:latest".to_owned()],
        MirrorMode::Sync,
    )
    .await
    .unwrap();

    assert!(
        rows.iter()
            .any(|row| row.kind == "manifest" && row.reference == "latest" && row.status == "synced")
    );
    assert!(
        rows.iter()
            .any(|row| row.kind == "manifest" && row.reference == child && row.status == "error")
    );
}

#[tokio::test]
async fn test_mirror_reports_an_unreachable_reference() {
    let server = MockServer::start().await;
    let dir = tempfile::tempdir().unwrap();
    let (state, _app) = proxy(&dir, &format!("{}/", server.uri()), false);
    let rows = mirror(
        &state.serving,
        &state.serving.indexes[0],
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
async fn test_mirror_rejects_malformed_references_before_network_access() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .respond_with(ResponseTemplate::new(200).set_body_raw(b"{}".to_vec(), MANIFEST_TYPE))
        .mount(&server)
        .await;
    for raw in [
        "",
        "team/app:",
        "team/app:sha256:1111111111111111111111111111111111111111111111111111111111111111",
        "team/app@latest",
        "team/app@sha256:1111111111111111111111111111111111111111111111111111111111111111@extra",
        " team/app:latest",
        "team/app@sha256:short",
        "registry.example.com/team/app:latest",
        "registry:5000/team/app:latest",
        "localhost/team/app:latest",
        "team//app:latest",
        "team/App:latest",
    ] {
        let dir = tempfile::tempdir().unwrap();
        let (state, app) = proxy(&dir, &format!("{}/", server.uri()), false);
        let reference = raw.to_owned();
        let catalog = send(&app, Method::GET, "/v2/_catalog").await;

        let rows = mirror(
            &state.serving,
            &state.serving.indexes[0],
            std::slice::from_ref(&reference),
            MirrorMode::Sync,
        )
        .await
        .unwrap();

        assert!(server.received_requests().await.unwrap().is_empty(), "{raw}");
        assert_eq!(send(&app, Method::GET, "/v2/_catalog").await, catalog, "{raw}");
        let index = state.serving.indexes[0].name.clone();
        assert_eq!(
            rows,
            vec![
                MirrorRow {
                    kind: "manifest",
                    index: index.clone(),
                    repo: reference,
                    reference: String::new(),
                    digest: String::new(),
                    status: "error",
                    bytes: 0,
                    reason: "not a valid image reference".to_owned(),
                },
                MirrorRow {
                    kind: "summary",
                    index,
                    repo: String::new(),
                    reference: String::new(),
                    digest: String::new(),
                    status: "error",
                    bytes: 0,
                    reason: "0 synced, 0 cached, 1 errors".to_owned(),
                },
            ],
            "{raw}"
        );
    }
}

#[tokio::test]
async fn test_mirror_needs_a_cached_upstream() {
    let dir = tempfile::tempdir().unwrap();
    let (state, _app) = super::hosted(&dir);
    let rows = mirror(
        &state.serving,
        &state.serving.indexes[0],
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
        &state.serving,
        &state.serving.indexes[0],
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
        &state.serving,
        &state.serving.indexes[0],
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

    let verify = mirror(
        &state.serving,
        &state.serving.indexes[0],
        &[by_digest],
        MirrorMode::Verify,
    )
    .await
    .unwrap();
    assert!(
        verify
            .iter()
            .any(|row| row.kind == "manifest" && row.status == "cached")
    );
    let absent = format!("library/app@{}", oci_digest(b"never-pushed"));
    let missing = mirror(&state.serving, &state.serving.indexes[0], &[absent], MirrorMode::Verify)
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
    let rows = mirror(
        &state.serving,
        &state.serving.indexes[0],
        &["alpine".to_owned()],
        MirrorMode::Sync,
    )
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

    let dir = tempfile::tempdir().unwrap();
    let (state, _app) = proxy(&dir, &format!("{}/", server.uri()), false);
    let refs = vec!["library/app:latest".to_owned()];
    let rows = mirror(&state.serving, &state.serving.indexes[0], &refs, MirrorMode::Sync)
        .await
        .unwrap();
    assert!(rows.iter().any(|row| row.kind == "blob" && row.status == "error"));

    let verify = mirror(&state.serving, &state.serving.indexes[0], &refs, MirrorMode::Verify)
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
        &state.serving,
        &state.serving.indexes[0],
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
    Mock::given(method("GET"))
        .and(path(format!("/v2/library/app/blobs/{}", oci_digest(layer))))
        .respond_with(ResponseTemplate::new(200).set_body_raw(b"tampered".to_vec(), "application/octet-stream"))
        .mount(&server)
        .await;

    let dir = tempfile::tempdir().unwrap();
    let (state, _app) = proxy(&dir, &format!("{}/", server.uri()), false);
    let rows = mirror(
        &state.serving,
        &state.serving.indexes[0],
        &["library/app:latest".to_owned()],
        MirrorMode::Sync,
    )
    .await
    .unwrap();
    let reason = &rows
        .iter()
        .find(|row| row.kind == "blob" && row.digest == oci_digest(layer))
        .unwrap()
        .reason;
    assert!(reason.contains("digest mismatch"), "{reason}");
}

#[tokio::test]
async fn test_mirror_reports_blob_body_failures() {
    use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};

    let config = b"{}";
    let manifest = format!(
        r#"{{"schemaVersion":2,"mediaType":"{MANIFEST_TYPE}","config":{{"mediaType":"{CONFIG_TYPE}","digest":"{}"}},"layers":[]}}"#,
        oci_digest(config),
    )
    .into_bytes();
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let base = format!("http://{}/", listener.local_addr().unwrap());
    let upstream = tokio::spawn(async move {
        let (mut socket, _) = listener.accept().await.unwrap();
        let mut request = [0; 1024];
        let _ = socket.read(&mut request).await.unwrap();
        socket
            .write_all(
                format!(
                    "HTTP/1.1 200 OK\r\ncontent-type: {MANIFEST_TYPE}\r\ncontent-length: {}\r\nconnection: close\r\n\r\n",
                    manifest.len(),
                )
                .as_bytes(),
            )
            .await
            .unwrap();
        socket.write_all(&manifest).await.unwrap();

        let (mut socket, _) = listener.accept().await.unwrap();
        let _ = socket.read(&mut request).await.unwrap();
        socket
            .write_all(b"HTTP/1.1 200 OK\r\ncontent-length: 4096\r\nconnection: close\r\n\r\nshort")
            .await
            .unwrap();
    });

    let dir = tempfile::tempdir().unwrap();
    let (state, _app) = proxy(&dir, &base, false);
    let rows = mirror(
        &state.serving,
        &state.serving.indexes[0],
        &["library/app:latest".to_owned()],
        MirrorMode::Sync,
    )
    .await
    .unwrap();
    tokio::time::timeout(std::time::Duration::from_secs(5), upstream)
        .await
        .unwrap()
        .unwrap();
    let reason = &rows.iter().find(|row| row.kind == "blob").unwrap().reason;
    assert!(reason.contains("blob body read failed"), "{reason}");
}

#[tokio::test]
async fn test_mirror_reports_blob_store_failures() {
    let server = MockServer::start().await;
    let config = b"{}";
    let manifest = format!(
        r#"{{"schemaVersion":2,"mediaType":"{MANIFEST_TYPE}","config":{{"mediaType":"{CONFIG_TYPE}","digest":"{}"}},"layers":[]}}"#,
        oci_digest(config),
    )
    .into_bytes();
    mount_manifest(&server, "library/app", "latest", &manifest, MANIFEST_TYPE).await;
    let dir = tempfile::tempdir().unwrap();
    let blob_root = dir.path().join("blobs");
    Mock::given(method("GET"))
        .and(path(format!("/v2/library/app/blobs/{}", oci_digest(config))))
        .respond_with(move |_: &wiremock::Request| {
            std::fs::write(&blob_root, b"not a directory").unwrap();
            ResponseTemplate::new(200).set_body_raw(config.to_vec(), "application/octet-stream")
        })
        .mount(&server)
        .await;
    let (state, _app) = proxy(&dir, &format!("{}/", server.uri()), false);
    let rows = mirror(
        &state.serving,
        &state.serving.indexes[0],
        &["library/app:latest".to_owned()],
        MirrorMode::Sync,
    )
    .await
    .unwrap();
    let reason = &rows.iter().find(|row| row.kind == "blob").unwrap().reason;
    assert!(reason.contains("blob store error"), "{reason}");
    assert!(reason.contains("I/O error"), "{reason}");
}

#[cfg(unix)]
#[tokio::test]
async fn test_mirror_reports_a_blob_head_failure() {
    let server = MockServer::start().await;
    let config = b"{}";
    let manifest = image_manifest(config, &[]);
    mount_manifest(&server, "library/app", "latest", &manifest, MANIFEST_TYPE).await;
    let dir = tempfile::tempdir().unwrap();
    let digest = Digest::of(config);
    let hex = digest.as_str();
    let path = dir
        .path()
        .join(format!("blobs/sha256/{}/{}/{}", &hex[..2], &hex[2..4], hex));
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    std::os::unix::fs::symlink(&path, &path).unwrap();
    let (state, _app) = proxy(&dir, &format!("{}/", server.uri()), false);

    let rows = mirror(
        &state.serving,
        &state.serving.indexes[0],
        &["library/app:latest".to_owned()],
        MirrorMode::Sync,
    )
    .await
    .unwrap();

    let reason = &rows.iter().find(|row| row.kind == "blob").unwrap().reason;
    assert!(reason.contains("filesystem blob backend head"), "{reason}");
}

#[tokio::test]
async fn test_mirror_tolerates_a_non_json_manifest() {
    let server = MockServer::start().await;
    mount_manifest(&server, "library/app", "latest", b"this is not json", MANIFEST_TYPE).await;

    let dir = tempfile::tempdir().unwrap();
    let (state, _app) = proxy(&dir, &format!("{}/", server.uri()), false);
    let rows = mirror(
        &state.serving,
        &state.serving.indexes[0],
        &["library/app:latest".to_owned()],
        MirrorMode::Sync,
    )
    .await
    .unwrap();
    assert!(rows.iter().any(|row| row.kind == "manifest" && row.status == "synced"));
    assert!(!rows.iter().any(|row| row.kind == "blob"));
}

#[tokio::test]
async fn test_mirror_bounds_an_oversized_manifest() {
    let server = MockServer::start().await;
    let body = vec![b'x'; 4 * 1024 * 1024 + 1];
    mount_manifest(&server, "library/app", "latest", &body, MANIFEST_TYPE).await;

    let dir = tempfile::tempdir().unwrap();
    let (state, _app) = proxy(&dir, &format!("{}/", server.uri()), false);
    let err = mirror(
        &state.serving,
        &state.serving.indexes[0],
        &["library/app:latest".to_owned()],
        MirrorMode::Sync,
    )
    .await
    .unwrap_err();
    assert!(err.to_string().contains("exceeds"), "{err}");
}

#[tokio::test]
async fn test_mirror_pulls_a_single_segment_name_under_the_library_prefix() {
    let server = MockServer::start().await;
    let config = b"{}";
    let layer = b"a-layer-of-bytes";
    let manifest = image_manifest(config, layer);
    mount_manifest(&server, "library/app", "latest", &manifest, MANIFEST_TYPE).await;
    mount_blob(&server, "library/app", config).await;
    mount_blob(&server, "library/app", layer).await;

    let dir = tempfile::tempdir().unwrap();
    let (state, app) = proxy(&dir, &format!("{}/", server.uri()), false);
    let settings = IndexSettings {
        library_prefix: crate::LibraryPrefix::Always,
    };
    let rows = mirror_with(
        &state.serving,
        &state.serving.indexes[0],
        settings,
        &["app:latest".to_owned()],
        MirrorMode::Sync,
    )
    .await
    .unwrap();

    assert_eq!(rows.last().unwrap().status, "synced");
    let (status, _, got) = send(&app, Method::GET, "/v2/hub/app/manifests/latest").await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(got, manifest);
}
