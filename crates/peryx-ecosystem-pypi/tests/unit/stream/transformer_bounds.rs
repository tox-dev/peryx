use std::collections::BTreeMap;

use super::{MAX_PAGE_FILES, PageTransformer, TransformError};
use crate::stream::page_context;

fn transformer() -> PageTransformer {
    PageTransformer::new(page_context(
        "root/pypi",
        "demo",
        peryx_policy::Policy::default(),
        Vec::new(),
        Vec::new(),
        &BTreeMap::new(),
    ))
}

/// The file cap refuses the page only once a file arrives beyond it, so the file that reaches the cap
/// is still served and the next one is not. Driving this through a page would mean building half a
/// million file entries; the counter the check reads is a field, so the boundary arrives in one call.
#[test]
fn test_the_file_cap_admits_the_last_file_it_allows() {
    let mut transformer = transformer();
    transformer.files_seen = MAX_PAGE_FILES - 1;

    let refusal = transformer.emit_file(&mut Vec::new()).unwrap_err();

    assert!(
        !matches!(refusal, TransformError::TooLarge),
        "the file that reaches the cap is within it, so it fails on its own contents instead"
    );
}

#[test]
fn test_the_file_cap_refuses_the_first_file_past_it() {
    let mut transformer = transformer();
    transformer.files_seen = MAX_PAGE_FILES;

    let refusal = transformer.emit_file(&mut Vec::new()).unwrap_err();

    assert!(matches!(refusal, TransformError::TooLarge));
}
