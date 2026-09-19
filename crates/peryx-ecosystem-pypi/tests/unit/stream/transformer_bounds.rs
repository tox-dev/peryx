use std::collections::BTreeMap;

use super::{MAX_PAGE_BYTES, MAX_PAGE_FILES, Mode, PageTransformer, TransformError};
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

#[test]
fn test_max_page_bytes_is_sixty_four_mebibytes() {
    assert_eq!(MAX_PAGE_BYTES, 67_108_864);
}

#[test]
fn test_finish_rejects_an_unclosed_container_alone() {
    let mut transformer = transformer();
    transformer.depth = 1;

    assert!(matches!(transformer.finish().unwrap_err(), TransformError::Truncated));
}

#[test]
fn test_finish_rejects_an_active_string_alone() {
    let mut transformer = transformer();
    transformer.string.active = true;

    assert!(matches!(transformer.finish().unwrap_err(), TransformError::Truncated));
}

#[test]
fn test_finish_rejects_a_non_passthrough_mode_alone() {
    let mut transformer = transformer();
    transformer.mode = Mode::Meta;

    assert!(matches!(transformer.finish().unwrap_err(), TransformError::Truncated));
}

#[test]
fn test_step_passthrough_ignores_a_meta_key_captured_at_the_wrong_depth() {
    let mut transformer = transformer();
    transformer.key[..4].copy_from_slice(b"meta");
    transformer.key_len = 4;
    transformer.depth = 4;
    let mut out = Vec::new();

    transformer.step_passthrough(b'{', &mut out);

    assert_eq!(transformer.mode, Mode::Passthrough);
    assert_eq!(out, b"{");
}

#[test]
fn test_step_passthrough_ignores_a_files_key_captured_at_the_wrong_depth() {
    let mut transformer = transformer();
    transformer.key[..5].copy_from_slice(b"files");
    transformer.key_len = 5;
    transformer.depth = 4;
    let mut out = Vec::new();

    transformer.step_passthrough(b'[', &mut out);

    assert_eq!(transformer.mode, Mode::Passthrough);
    assert_eq!(out, b"[");
}

#[test]
fn test_step_passthrough_does_not_emit_project_status_when_closing_a_nested_object() {
    let mut transformer = transformer();
    transformer.project_status = Some("quarantined".to_owned());
    transformer.depth = 4;
    let mut out = Vec::new();

    transformer.step_passthrough(b'}', &mut out);

    assert_eq!(out, b"}", "closing a nested object at depth != 1 seeds nothing extra");
}

/// The file cap refuses the page only once a file arrives beyond it, so the file that reaches the cap
/// is still served and the next one is not. Driving this through a page would mean building half a
/// million file entries; the counter the check reads is a field, so the boundary arrives in one call.
#[test]
fn test_emit_local_files_records_a_served_version_before_the_page_declares_any() {
    let file = crate::File {
        filename: "demo-1.0-py3-none-any.whl".to_owned(),
        url: "https://example.test/demo.whl".to_owned(),
        hashes: BTreeMap::new(),
        requires_python: None,
        size: None,
        upload_time: None,
        yanked: crate::Yanked::No,
        core_metadata: crate::CoreMetadata::Absent,
        dist_info_metadata: crate::CoreMetadata::Absent,
        gpg_sig: None,
        provenance: crate::Provenance::Absent,
    };
    let mut transformer = PageTransformer::new(page_context(
        "root/pypi",
        "demo",
        peryx_policy::Policy::default(),
        vec![file],
        Vec::new(),
        &BTreeMap::new(),
    ));

    transformer.emit_local_files(&mut Vec::new());

    assert!(transformer.served_versions.contains("1.0"));
}

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
