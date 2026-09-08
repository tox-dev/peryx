use super::{CATALOG_TEXT_BYTES, CORE_METADATA_TEXT_BYTES, IDENTITY_TEXT_BYTES, push_text};
use peryx_search::INDEXED_TEXT_BYTES;

/// The three text budgets divide one indexed-document allowance between identity, core metadata and
/// catalog text, so they have to add back up to it. Derived by arithmetic and read nowhere else,
/// nothing else says what that arithmetic is for.
#[test]
fn test_the_text_budgets_divide_the_document_allowance() {
    assert_eq!(
        IDENTITY_TEXT_BYTES + CORE_METADATA_TEXT_BYTES + CATALOG_TEXT_BYTES,
        INDEXED_TEXT_BYTES
    );
    assert!(CATALOG_TEXT_BYTES > 0, "catalog text needs a share of the allowance");
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
