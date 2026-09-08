use super::{CATALOG_TEXT_BYTES, CORE_METADATA_TEXT_BYTES, IDENTITY_TEXT_BYTES, push_text};
use peryx_search::INDEXED_TEXT_BYTES;
use rstest::rstest;
use std::collections::BTreeMap;

use crate::{CoreMetadata, File, Provenance, Yanked};

/// The three text budgets divide one indexed-document allowance between identity, core metadata and
/// catalog text, so they have to add back up to it. Derived by arithmetic and read nowhere else,
/// nothing else says what that arithmetic is for.
#[test]
fn test_the_text_budgets_divide_the_document_allowance() {
    assert_eq!(
        IDENTITY_TEXT_BYTES + CORE_METADATA_TEXT_BYTES + CATALOG_TEXT_BYTES,
        INDEXED_TEXT_BYTES
    );
    assert!(
        [IDENTITY_TEXT_BYTES, CORE_METADATA_TEXT_BYTES, CATALOG_TEXT_BYTES]
            .iter()
            .all(|share| *share * 8 >= INDEXED_TEXT_BYTES),
        "a share too small to hold a project name is not a share of anything"
    );
}

#[test]
fn test_push_text_separates_appended_values_but_not_the_first() {
    let mut out = String::new();
    push_text(&mut out, "alpha", 100);
    assert_eq!(out, "alpha", "the first value starts the document rather than a gap");

    push_text(&mut out, "beta", 100);
    assert_eq!(out, "alpha beta");
}

#[test]
fn test_push_text_leaves_the_document_alone_for_a_blank_value() {
    let mut out = "alpha".to_owned();

    push_text(&mut out, "   ", 100);

    assert_eq!(out, "alpha", "a value that trims to nothing adds neither text nor a separator");
}

#[test]
fn test_push_text_leaves_the_document_alone_once_it_reaches_the_limit() {
    let mut out = "alpha".to_owned();

    push_text(&mut out, "beta", 5);

    assert_eq!(out, "alpha");
}

/// A project record's key is the index, then one path segment. A key that names an empty project or
/// carries a further segment is not one, and both halves of that have to hold: keeping a key that
/// fails either would index a project under a name it does not have.
#[rstest]
#[case::project("images/flask", Some("flask"))]
#[case::empty_project("images/", None)]
#[case::nested_segment("images/flask/extra", None)]
#[case::other_index("other/flask", None)]
fn test_a_project_record_key_names_one_segment(#[case] key: &str, #[case] expected: Option<&str>) {
    assert_eq!(super::project_record_key(key, "images"), expected);
}

/// An upload's key is the index, then a project and a filename. Either one missing leaves nothing to
/// index, so both have to be present rather than just one.
#[rstest]
#[case::pair("images/flask/flask-1.0.whl", Some(("flask", "flask-1.0.whl")))]
#[case::empty_project("images//flask-1.0.whl", None)]
#[case::empty_filename("images/flask/", None)]
#[case::no_filename("images/flask", None)]
fn test_an_upload_key_names_a_project_and_a_file(#[case] key: &str, #[case] expected: Option<(&str, &str)>) {
    assert_eq!(super::upload_key(key, "images"), expected);
}

fn presented(url: &str, core_metadata: CoreMetadata) -> File {
    File {
        filename: "flask-1.0-py3-none-any.whl".to_owned(),
        url: url.to_owned(),
        hashes: BTreeMap::from([("sha256".to_owned(), "a".repeat(64))]),
        requires_python: None,
        size: Some(10),
        upload_time: None,
        yanked: Yanked::No,
        core_metadata,
        dist_info_metadata: CoreMetadata::Absent,
        gpg_sig: None,
        provenance: Provenance::Absent,
    }
}

/// A sidecar peryx can verify is one that names its own digest. A claim of `Available` says a sidecar
/// exists without saying which bytes it is, so it cannot be served and is dropped; hashes naming
/// `sha256` are kept.
#[rstest]
#[case::names_its_digest(CoreMetadata::Hashes(BTreeMap::from([("sha256".to_owned(), "b".repeat(64))])), true)]
#[case::names_no_digest(CoreMetadata::Available, false)]
#[case::names_another_hash(CoreMetadata::Hashes(BTreeMap::from([("md5".to_owned(), "c".repeat(32))])), false)]
fn test_present_file_keeps_only_metadata_that_names_its_digest(
    #[case] metadata: CoreMetadata,
    #[case] kept: bool,
) {
    let file = super::present_file(presented("https://files.example/flask.whl", metadata), "root/pypi");

    assert_eq!(*file.metadata() != CoreMetadata::Absent, kept);
}

/// An upstream URL is rewritten to this node's own route so a reader fetches through peryx, and a URL
/// that already points here is left as it is rather than routed a second time.
#[rstest]
#[case::upstream("https://files.example/flask-1.0-py3-none-any.whl", true)]
#[case::already_local("/root/pypi/files/aaa/flask-1.0-py3-none-any.whl", false)]
fn test_present_file_routes_only_a_url_that_points_away(#[case] url: &str, #[case] rewritten: bool) {
    let file = super::present_file(presented(url, CoreMetadata::Absent), "root/pypi");

    assert_eq!(file.url != url, rewritten, "url became {}", file.url);
    assert!(file.url.starts_with('/'), "either way it points at this node: {}", file.url);
}
