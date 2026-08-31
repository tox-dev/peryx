use axum::http::{Method, StatusCode};
use peryx_core::Ecosystem;
use peryx_identity::{ArtifactDigest, IndexAcl, RevocationReason, UserId};
use peryx_index::{Index, IndexKind};
use peryx_policy::{Policy, PolicyConfig};
use peryx_search::{ContentSource, SearchDocumentProvider as _};
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

use super::{
    app_with_indexes, auth, hosted_writable, oci_digest, proxy, search_total, send, send_body, virtual_stack,
    writable_index,
};
use crate::OciIndexer;
use crate::store;

const TOKEN: &str = "s3cret";
const DIGEST: &str = "sha256:aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";
const OTHER_DIGEST: &str = "sha256:bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb";
// An index with no children is the cheapest manifest a push accepts: the search view does not read the
// image document, so the fixture names no blob to upload first.
const MANIFEST_TYPE: &str = "application/vnd.oci.image.index.v1+json";
const MANIFEST: &[u8] = br#"{"schemaVersion":2,"mediaType":"application/vnd.oci.image.index.v1+json","manifests":[]}"#;

#[tokio::test]
async fn test_oci_indexer_surfaces_repositories_and_tags() {
    let dir = tempfile::tempdir().unwrap();
    let (state, _) = hosted_writable(&dir, TOKEN);
    store::put_tag(&state.serving.meta, "store", "library/app", "1.0", DIGEST).unwrap();
    store::put_tag(&state.serving.meta, "store", "library/app", "2.0", DIGEST).unwrap();
    store::put_tag(&state.serving.meta, "store", "team/api", "latest", DIGEST).unwrap();

    let documents = OciIndexer.documents(&state.serving.indexer_ctx()).unwrap();
    let names: Vec<&str> = documents.iter().map(|doc| doc.display_label.as_str()).collect();
    assert!(names.contains(&"library/app"));
    assert!(names.contains(&"team/api"));

    let app = documents.iter().find(|doc| doc.display_label == "library/app").unwrap();
    assert_eq!(app.route, "store");
    assert_eq!(app.index, "store");
    assert_eq!(app.summary.as_deref(), Some("2 tags"));
    assert!(app.text.contains("library/app"));
    assert!(app.text.contains("1.0") && app.text.contains("2.0"));

    let api = documents.iter().find(|doc| doc.display_label == "team/api").unwrap();
    assert_eq!(api.summary.as_deref(), Some("1 tag"));
}

#[tokio::test]
async fn test_oci_search_returns_the_same_repository_for_short_and_long_tag_queries() {
    let dir = tempfile::tempdir().unwrap();
    let (state, app) = hosted_writable(&dir, TOKEN);
    // Enough tags to sort the marker tag past 32 KiB of repository text, yet inside the indexed window.
    for serial in 0..320 {
        store::put_tag(
            &state.serving.meta,
            "store",
            "library/app",
            &format!("a{serial:03}-{}", "x".repeat(118)),
            DIGEST,
        )
        .unwrap();
    }
    store::put_tag(
        &state.serving.meta,
        "store",
        "library/app",
        "z-release-candidate-2026",
        DIGEST,
    )
    .unwrap();

    assert_eq!(
        (
            search_total(&app, "candidate").await,
            search_total(&app, "release-candidate-2026").await,
        ),
        (1, 1)
    );
}

#[tokio::test]
async fn test_oci_indexer_is_empty_without_tags() {
    let dir = tempfile::tempdir().unwrap();
    let (state, _app) = hosted_writable(&dir, TOKEN);
    assert!(OciIndexer.documents(&state.serving.indexer_ctx()).unwrap().is_empty());
}

#[tokio::test]
async fn test_search_refreshes_after_hosted_tag_insert() {
    let dir = tempfile::tempdir().unwrap();
    let (_, app) = hosted_writable(&dir, TOKEN);
    let before = search_total(&app, "app").await;

    push_tag(&app, "store", "team/app", "latest").await;

    assert_eq!((before, search_total(&app, "app").await), (0, 1));
}

#[tokio::test]
async fn test_search_refreshes_a_virtual_document_after_hosted_push() {
    let dir = tempfile::tempdir().unwrap();
    let (_, app) = virtual_stack(&dir, "http://127.0.0.1:1/");
    assert_eq!(search_total(&app, "app").await, 0);

    push_tag(&app, "reg", "team/app", "latest").await;

    let response = send(&app, Method::GET, "/reg/+search?q=app&page_size=25").await;
    assert_eq!(response.0, StatusCode::OK);
    assert_eq!(
        serde_json::from_slice::<serde_json::Value>(&response.2).unwrap()["total"],
        1
    );
}

#[tokio::test]
async fn test_search_refreshes_after_placement_publication() {
    let dir = tempfile::tempdir().unwrap();
    let (state, app) = hosted_writable(&dir, TOKEN);
    let before = local_search_total(&app, "app").await;
    store::put_tag(&state.serving.meta, "store", "team/app", "latest", DIGEST).unwrap();
    let search_invalidation = crate::search_oci::SearchInvalidationGuard::arm(&state.serving, "team/app");
    let during = local_search_total(&app, "app").await;
    store::record_content_placement(&state.serving.meta, DIGEST, store::OciArtifactOrigin::Pushed, true).unwrap();

    drop(search_invalidation);

    assert_eq!((before, during, local_search_total(&app, "app").await), (0, 0, 1));
}

#[tokio::test]
async fn test_incomplete_publication_remains_dirty() {
    let dir = tempfile::tempdir().unwrap();
    let (state, app) = hosted_writable(&dir, TOKEN);
    assert_eq!(search_total(&app, "app").await, 0);
    store::put_tag(&state.serving.meta, "store", "team/app", "latest", DIGEST).unwrap();

    drop(crate::search_oci::SearchInvalidationGuard::arm(
        &state.serving,
        "team/app",
    ));

    assert_eq!(search_total(&app, "app").await, 1);
}

#[tokio::test]
async fn test_search_refreshes_after_proxy_tag_fill() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/v2/team/app/manifests/latest"))
        .respond_with(ResponseTemplate::new(200).set_body_raw(MANIFEST.to_vec(), MANIFEST_TYPE))
        .mount(&server)
        .await;
    let dir = tempfile::tempdir().unwrap();
    let (_, app) = proxy(&dir, &format!("{}/", server.uri()), false);
    let before = search_total(&app, "app").await;

    send(&app, Method::GET, "/v2/hub/team/app/manifests/latest").await;

    assert_eq!((before, search_total(&app, "app").await), (0, 1));
}

#[tokio::test]
async fn test_search_refreshes_after_proxy_tag_repoint() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/v2/team/app/tags/list"))
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_raw(br#"{"name":"team/app","tags":["latest"]}"#.to_vec(), "application/json"),
        )
        .mount(&server)
        .await;
    Mock::given(method("HEAD"))
        .and(path("/v2/team/app/manifests/latest"))
        .respond_with(ResponseTemplate::new(200).insert_header("docker-content-digest", OTHER_DIGEST))
        .mount(&server)
        .await;
    let dir = tempfile::tempdir().unwrap();
    let (state, app) = proxy(&dir, &format!("{}/", server.uri()), false);
    state
        .serving
        .revocations
        .put(
            &ArtifactDigest::from_sha256("c".repeat(64)).unwrap(),
            &RevocationReason::new("compromised builder").unwrap(),
            &UserId::random(),
            1_000,
        )
        .unwrap();
    store::put_tag(&state.serving.meta, "hub", "team/app", "latest", DIGEST).unwrap();
    store::record_content_placement(&state.serving.meta, DIGEST, store::OciArtifactOrigin::Mirrored, true).unwrap();
    let before = local_search_total(&app, "app").await;

    let response = send(&app, Method::GET, "/v2/hub/team/app/tags/list").await;

    assert_eq!(
        (response.0, before, local_search_total(&app, "app").await),
        (StatusCode::OK, 1, 0)
    );
}

#[rstest::rstest]
#[case::tag(false)]
#[case::digest(true)]
#[tokio::test]
async fn test_search_refreshes_after_manifest_delete_and_restore(#[case] by_digest: bool) {
    let dir = tempfile::tempdir().unwrap();
    let (_state, app) = hosted_writable(&dir, TOKEN);
    let digest = push_tag(&app, "store", "team/app", "latest").await;
    let before = search_total(&app, "app").await;
    let reference = if by_digest { digest.as_str() } else { "latest" };

    let delete = send_body(
        &app,
        Method::DELETE,
        &format!("/v2/store/team/app/manifests/{reference}"),
        &[("authorization", &auth(TOKEN))],
        Vec::new(),
    )
    .await;
    let deleted = search_total(&app, "app").await;
    let restore = send_body(
        &app,
        Method::PUT,
        &format!("/v2/store/team/app/manifests/{reference}/restore"),
        &[("authorization", &auth(TOKEN))],
        Vec::new(),
    )
    .await;

    assert_eq!(
        (delete.0, before, deleted, restore.0, search_total(&app, "app").await),
        (StatusCode::ACCEPTED, 1, 0, StatusCode::ACCEPTED, 1)
    );
}

#[tokio::test]
async fn test_oci_indexer_walks_a_virtual_index() {
    let dir = tempfile::tempdir().unwrap();
    let (state, _) = virtual_stack(&dir, "http://127.0.0.1:1/");
    store::put_tag(&state.serving.meta, "images", "team/app", "1.0", DIGEST).unwrap();
    peryx_ha::ArtifactPlacementStore::put_artifact_placement(
        &state.serving.meta,
        DIGEST,
        &peryx_ha::ArtifactPlacement::record(peryx_ha::ArtifactSource::Hosted, true),
    )
    .unwrap();

    let documents = OciIndexer.documents(&state.serving.indexer_ctx()).unwrap();
    let hosted = documents.iter().find(|doc| doc.index == "images").unwrap();
    assert_eq!(hosted.source, ContentSource::Uploaded);
    assert!(hosted.available_locally);
    let virtual_doc = documents.iter().find(|doc| doc.index == "reg").unwrap();
    assert_eq!(virtual_doc.display_label, "team/app");
    assert_eq!(virtual_doc.route, "reg");
    assert_eq!(virtual_doc.source, ContentSource::Cached);
    assert!(virtual_doc.available_locally);
    assert!(virtual_doc.text.contains("1.0"));
}

#[tokio::test]
async fn test_oci_indexer_omits_a_policy_blocked_repository() {
    let dir = tempfile::tempdir().unwrap();
    let policy = Policy::compile(
        &PolicyConfig {
            block_resources: vec!["blocked/app".to_owned()],
            ..PolicyConfig::default()
        },
        str::to_owned,
    );
    let index = Index {
        name: "store".to_owned(),
        route: "store".to_owned(),
        ecosystem: crate::ECOSYSTEM,
        kind: IndexKind::Hosted { volatile: true },
        policy,
        acl: crate::tests::writer_acl(TOKEN.to_owned()),
    };
    let (state, _) = app_with_indexes(&dir, vec![index]);
    store::put_tag(&state.serving.meta, "store", "blocked/app", "1.0", DIGEST).unwrap();
    store::put_tag(&state.serving.meta, "store", "public/app", "1.0", DIGEST).unwrap();

    let documents = OciIndexer.documents(&state.serving.indexer_ctx()).unwrap();
    let names: Vec<&str> = documents.iter().map(|doc| doc.display_label.as_str()).collect();
    assert_eq!(names, vec!["public/app"]);
}

#[tokio::test]
async fn test_oci_indexer_skips_non_oci_indexes() {
    let dir = tempfile::tempdir().unwrap();
    let non_oci = Index {
        name: "other".to_owned(),
        route: "other".to_owned(),
        ecosystem: Ecosystem::new("other"),
        kind: IndexKind::Hosted { volatile: false },
        policy: Policy::default(),
        acl: IndexAcl::default(),
    };
    let oci = writable_index("store", "store", true, TOKEN);
    let (state, _) = app_with_indexes(&dir, vec![non_oci, oci]);
    store::put_tag(&state.serving.meta, "store", "library/app", "1.0", DIGEST).unwrap();

    let documents = OciIndexer.documents(&state.serving.indexer_ctx()).unwrap();
    assert!(documents.iter().all(|doc| doc.index == "store"));
    assert!(documents.iter().any(|doc| doc.display_label == "library/app"));
}

#[tokio::test]
async fn test_oci_indexer_resource_update_scopes_to_one_repository() {
    let dir = tempfile::tempdir().unwrap();
    let non_oci = Index {
        name: "other".to_owned(),
        route: "other".to_owned(),
        ecosystem: Ecosystem::new("other"),
        kind: IndexKind::Hosted { volatile: false },
        policy: Policy::default(),
        acl: IndexAcl::default(),
    };
    let (state, _) = app_with_indexes(&dir, vec![non_oci, writable_index("store", "store", true, TOKEN)]);
    store::put_tag(&state.serving.meta, "store", "library/app", "1.0", DIGEST).unwrap();
    store::put_tag(&state.serving.meta, "store", "team/api", "latest", DIGEST).unwrap();

    let update = OciIndexer
        .resource_update(&state.serving.indexer_ctx(), "library/app")
        .unwrap();

    assert_eq!(update.keys, vec![peryx_search::document_key("store", "library/app")]);
    assert_eq!(update.documents.len(), 1);
    assert_eq!(update.documents[0].display_label, "library/app");
}

#[tokio::test]
async fn test_oci_indexer_resource_update_retires_an_absent_repository() {
    let dir = tempfile::tempdir().unwrap();
    let (state, _) = hosted_writable(&dir, TOKEN);
    store::put_tag(&state.serving.meta, "store", "library/app", "1.0", DIGEST).unwrap();

    let update = OciIndexer
        .resource_update(&state.serving.indexer_ctx(), "team/ghost")
        .unwrap();

    assert_eq!(
        (update.keys, update.documents.len()),
        (vec![peryx_search::document_key("store", "team/ghost")], 0)
    );
}

#[tokio::test]
async fn test_oci_indexer_resource_update_retires_a_policy_blocked_repository() {
    let dir = tempfile::tempdir().unwrap();
    let policy = Policy::compile(
        &PolicyConfig {
            block_resources: vec!["blocked/app".to_owned()],
            ..PolicyConfig::default()
        },
        str::to_owned,
    );
    let index = Index {
        policy,
        ..writable_index("store", "store", true, TOKEN)
    };
    let (state, _) = app_with_indexes(&dir, vec![index]);
    store::put_tag(&state.serving.meta, "store", "blocked/app", "1.0", DIGEST).unwrap();

    let update = OciIndexer
        .resource_update(&state.serving.indexer_ctx(), "blocked/app")
        .unwrap();

    assert_eq!(
        (update.keys, update.documents.len()),
        (vec![peryx_search::document_key("store", "blocked/app")], 0)
    );
}

#[tokio::test]
async fn test_oci_indexer_resource_update_follows_virtual_layers() {
    let dir = tempfile::tempdir().unwrap();
    let (state, _app) = virtual_stack(&dir, "https://example.test");
    store::put_tag(&state.serving.meta, "images", "team/app", "1.0", DIGEST).unwrap();

    let update = OciIndexer
        .resource_update(&state.serving.indexer_ctx(), "team/app")
        .unwrap();

    assert_eq!(
        update.keys,
        vec![
            peryx_search::document_key("images", "team/app"),
            peryx_search::document_key("hub", "team/app"),
            peryx_search::document_key("reg", "team/app"),
        ]
    );
    assert_eq!(update.documents.len(), 2);
}

#[tokio::test]
async fn test_oci_search_availability_filter_uses_manifest_placement() {
    let dir = tempfile::tempdir().unwrap();
    let (state, app) = hosted_writable(&dir, TOKEN);
    store::put_tag(&state.serving.meta, "store", "team/remote", "1.0", DIGEST).unwrap();
    push_tag(&app, "store", "team/local", "latest").await;

    let documents = OciIndexer.documents(&state.serving.indexer_ctx()).unwrap();
    let remote = documents.iter().find(|doc| doc.display_label == "team/remote").unwrap();
    let local = documents.iter().find(|doc| doc.display_label == "team/local").unwrap();
    assert!(!remote.available_locally);
    assert!(local.available_locally);

    let response = send(&app, Method::GET, "/+search?q=team&availability=local&page_size=25").await;
    assert_eq!(response.0, StatusCode::OK);
    let value: serde_json::Value = serde_json::from_slice(&response.2).unwrap();
    assert_eq!(value["availability"], "local");
    let names: Vec<&str> = value["results"]
        .as_array()
        .unwrap()
        .iter()
        .map(|result| result["resource_key"].as_str().unwrap())
        .collect();
    assert_eq!(names, vec!["team/local"]);
    assert_eq!(value["results"][0]["available"], true);
}

async fn push_tag(app: &axum::Router, route: &str, repo: &str, tag: &str) -> String {
    send_body(
        app,
        Method::PUT,
        &format!("/v2/{route}/{repo}/manifests/{tag}"),
        &[("authorization", &auth(TOKEN)), ("content-type", MANIFEST_TYPE)],
        MANIFEST.to_vec(),
    )
    .await;
    oci_digest(MANIFEST)
}

async fn local_search_total(app: &axum::Router, query: &str) -> u64 {
    let response = send(
        app,
        Method::GET,
        &format!("/+search?q={query}&availability=local&page_size=25"),
    )
    .await;
    assert_eq!(response.0, StatusCode::OK);
    serde_json::from_slice::<serde_json::Value>(&response.2).unwrap()["total"]
        .as_u64()
        .unwrap()
}
