//! The OCI indexer turns stored repositories and their tags into neutral search documents.

use axum::http::Method;
use peryx_core::Ecosystem;
use peryx_identity::IndexAcl;
use peryx_index::{Index, IndexKind};
use peryx_policy::{Policy, PolicyConfig};
use peryx_search::{PackageIndexer as _, PackageSource};
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
const MANIFEST_TYPE: &str = "application/vnd.oci.image.manifest.v1+json";
const MANIFEST: &[u8] = br#"{"schemaVersion":2,"mediaType":"application/vnd.oci.image.manifest.v1+json"}"#;

#[tokio::test]
async fn test_oci_indexer_surfaces_repositories_and_tags() {
    let dir = tempfile::tempdir().unwrap();
    let (state, _app) = hosted_writable(&dir, TOKEN);
    store::put_tag(&state.meta, "store", "library/app", "1.0", DIGEST).unwrap();
    store::put_tag(&state.meta, "store", "library/app", "2.0", DIGEST).unwrap();
    store::put_tag(&state.meta, "store", "team/api", "latest", DIGEST).unwrap();

    let documents = OciIndexer.documents(&state.indexer_ctx()).unwrap();
    let names: Vec<&str> = documents.iter().map(|doc| doc.display_name.as_str()).collect();
    assert!(names.contains(&"library/app"));
    assert!(names.contains(&"team/api"));

    let app = documents.iter().find(|doc| doc.display_name == "library/app").unwrap();
    assert_eq!(app.route, "store");
    assert_eq!(app.index, "store");
    assert_eq!(app.summary.as_deref(), Some("2 tags"));
    assert!(app.text.contains("library/app"));
    assert!(app.text.contains("1.0") && app.text.contains("2.0"));

    let api = documents.iter().find(|doc| doc.display_name == "team/api").unwrap();
    assert_eq!(api.summary.as_deref(), Some("1 tag"));
}

#[tokio::test]
async fn test_oci_indexer_is_empty_without_tags() {
    let dir = tempfile::tempdir().unwrap();
    let (state, _app) = hosted_writable(&dir, TOKEN);
    assert!(OciIndexer.documents(&state.indexer_ctx()).unwrap().is_empty());
}

#[tokio::test]
async fn test_search_refreshes_after_hosted_tag_insert() {
    let dir = tempfile::tempdir().unwrap();
    let (_state, app) = hosted_writable(&dir, TOKEN);
    let before = search_total(&app, "app").await;

    push_tag(&app, "team/app", "latest").await;

    assert_eq!((before, search_total(&app, "app").await), (0, 1));
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
    let (_state, app) = proxy(&dir, &format!("{}/", server.uri()), false);
    let before = search_total(&app, "app").await;

    send(&app, Method::GET, "/v2/hub/team/app/manifests/latest").await;

    assert_eq!((before, search_total(&app, "app").await), (0, 1));
}

#[rstest::rstest]
#[case::tag(false)]
#[case::digest(true)]
#[tokio::test]
async fn test_search_refreshes_after_manifest_delete_removes_tag(#[case] by_digest: bool) {
    let dir = tempfile::tempdir().unwrap();
    let (_state, app) = hosted_writable(&dir, TOKEN);
    let digest = push_tag(&app, "team/app", "latest").await;
    let before = search_total(&app, "app").await;
    let reference = if by_digest { digest.as_str() } else { "latest" };

    send_body(
        &app,
        Method::DELETE,
        &format!("/v2/store/team/app/manifests/{reference}"),
        &[("authorization", &auth(TOKEN))],
        Vec::new(),
    )
    .await;

    assert_eq!((before, search_total(&app, "app").await), (1, 0));
}

#[tokio::test]
async fn test_oci_indexer_walks_a_virtual_index() {
    let dir = tempfile::tempdir().unwrap();
    let (state, _app) = virtual_stack(&dir, "http://127.0.0.1:1/");
    // Seed a tag on the hosted member `images`; the virtual `reg` unions its members.
    store::put_tag(&state.meta, "images", "team/app", "1.0", DIGEST).unwrap();

    let documents = OciIndexer.documents(&state.indexer_ctx()).unwrap();
    // The hosted member surfaces it as uploaded, the virtual index as a cached aggregation.
    let hosted = documents.iter().find(|doc| doc.index == "images").unwrap();
    assert_eq!(hosted.source, PackageSource::Uploaded);
    let virtual_doc = documents.iter().find(|doc| doc.index == "reg").unwrap();
    assert_eq!(virtual_doc.display_name, "team/app");
    assert_eq!(virtual_doc.route, "reg");
    assert_eq!(virtual_doc.source, PackageSource::Cached);
    assert!(virtual_doc.text.contains("1.0"));
}

#[tokio::test]
async fn test_oci_indexer_omits_a_policy_blocked_repository() {
    let dir = tempfile::tempdir().unwrap();
    let policy = Policy::compile(
        &PolicyConfig {
            block_projects: vec!["blocked/app".to_owned()],
            ..PolicyConfig::default()
        },
        str::to_owned,
    );
    let index = Index {
        name: "store".to_owned(),
        route: "store".to_owned(),
        ecosystem: Ecosystem::Oci,
        kind: IndexKind::Hosted { volatile: true },
        policy,
        acl: IndexAcl::upload_token(TOKEN.to_owned()),
    };
    let (state, _app) = app_with_indexes(&dir, vec![index]);
    store::put_tag(&state.meta, "store", "blocked/app", "1.0", DIGEST).unwrap();
    store::put_tag(&state.meta, "store", "public/app", "1.0", DIGEST).unwrap();

    // A blocked repository is hidden on reads, so it must not surface through search either.
    let documents = OciIndexer.documents(&state.indexer_ctx()).unwrap();
    let names: Vec<&str> = documents.iter().map(|doc| doc.display_name.as_str()).collect();
    assert_eq!(names, vec!["public/app"]);
}

#[tokio::test]
async fn test_oci_indexer_skips_non_oci_indexes() {
    let dir = tempfile::tempdir().unwrap();
    let pypi = Index {
        name: "pypi".to_owned(),
        route: "pypi".to_owned(),
        ecosystem: Ecosystem::Pypi,
        kind: IndexKind::Hosted { volatile: false },
        policy: Policy::default(),
        acl: IndexAcl::default(),
    };
    let oci = writable_index("store", "store", true, TOKEN);
    let (state, _app) = app_with_indexes(&dir, vec![pypi, oci]);
    store::put_tag(&state.meta, "store", "library/app", "1.0", DIGEST).unwrap();

    let documents = OciIndexer.documents(&state.indexer_ctx()).unwrap();
    // Only the OCI index yields documents; the PyPI index is skipped, not misread.
    assert!(documents.iter().all(|doc| doc.index == "store"));
    assert!(documents.iter().any(|doc| doc.display_name == "library/app"));
}

async fn push_tag(app: &axum::Router, repo: &str, tag: &str) -> String {
    send_body(
        app,
        Method::PUT,
        &format!("/v2/store/{repo}/manifests/{tag}"),
        &[("authorization", &auth(TOKEN)), ("content-type", MANIFEST_TYPE)],
        MANIFEST.to_vec(),
    )
    .await;
    oci_digest(MANIFEST)
}
