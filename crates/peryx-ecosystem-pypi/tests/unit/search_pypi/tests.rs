use super::{CATALOG_TEXT_BYTES, CORE_METADATA_TEXT_BYTES, IDENTITY_TEXT_BYTES, push_text};
use peryx_search::INDEXED_TEXT_BYTES;
use rstest::rstest;

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
