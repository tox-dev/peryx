//! The OCI indexer turns stored repositories and their tags into neutral search documents.

use peryx_format::Ecosystem;
use peryx_http::search::{PackageIndexer as _, PackageSource};
use peryx_http::{Index, IndexKind};
use peryx_policy::Policy;

use super::{app_with_indexes, hosted_writable, oci_index, virtual_stack};
use crate::OciIndexer;
use crate::store;

const TOKEN: &str = "s3cret";
const DIGEST: &str = "sha256:aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";

#[tokio::test]
async fn test_oci_indexer_surfaces_repositories_and_tags() {
    let dir = tempfile::tempdir().unwrap();
    let (state, _app) = hosted_writable(&dir, TOKEN);
    store::put_tag(&state.meta, "store", "library/app", "1.0", DIGEST).unwrap();
    store::put_tag(&state.meta, "store", "library/app", "2.0", DIGEST).unwrap();
    store::put_tag(&state.meta, "store", "team/api", "latest", DIGEST).unwrap();

    let documents = OciIndexer.documents(&state).unwrap();
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
    assert!(OciIndexer.documents(&state).unwrap().is_empty());
}

#[tokio::test]
async fn test_oci_indexer_walks_a_virtual_index() {
    let dir = tempfile::tempdir().unwrap();
    let (state, _app) = virtual_stack(&dir, "http://127.0.0.1:1/");
    // Seed a tag on the hosted member `images`; the virtual `reg` unions its members.
    store::put_tag(&state.meta, "images", "team/app", "1.0", DIGEST).unwrap();

    let documents = OciIndexer.documents(&state).unwrap();
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
async fn test_oci_indexer_skips_non_oci_indexes() {
    let dir = tempfile::tempdir().unwrap();
    let pypi = Index {
        name: "pypi".to_owned(),
        route: "pypi".to_owned(),
        ecosystem: Ecosystem::Pypi,
        kind: IndexKind::Hosted {
            upload_token: None,
            volatile: false,
        },
        policy: Policy::default(),
    };
    let oci = oci_index(
        "store",
        "store",
        IndexKind::Hosted {
            upload_token: Some(TOKEN.to_owned()),
            volatile: true,
        },
    );
    let (state, _app) = app_with_indexes(&dir, vec![pypi, oci]);
    store::put_tag(&state.meta, "store", "library/app", "1.0", DIGEST).unwrap();

    let documents = OciIndexer.documents(&state).unwrap();
    // Only the OCI index yields documents; the PyPI index is skipped, not misread.
    assert!(documents.iter().all(|doc| doc.index == "store"));
    assert!(documents.iter().any(|doc| doc.display_name == "library/app"));
}
