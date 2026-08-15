use crate::{
    CHANGELOG_PAGE_SIZE, ChangelogEntry, ChangelogPage, ChangelogPageError, SerialStamp, UpstreamSerialError,
    compose_serial_watermarks, validate_upstream_serial,
};

fn entry(serial: u64) -> ChangelogEntry {
    ChangelogEntry {
        project: format!("project-{serial}"),
        version: None,
        timestamp: 0,
        action: "add project".to_owned(),
        serial,
    }
}

#[rstest::rstest]
#[case::unversioned(None)]
#[case::first_serial(Some(10))]
fn test_validate_upstream_serial_accepts_without_a_cached_watermark(#[case] received: Option<u64>) {
    assert_eq!(validate_upstream_serial(None, received), Ok(received));
}

#[rstest::rstest]
#[case::same(10)]
#[case::advanced(11)]
fn test_validate_upstream_serial_accepts_a_non_regressing_watermark(#[case] received: u64) {
    assert_eq!(validate_upstream_serial(Some(10), Some(received)), Ok(Some(received)));
}

#[test]
fn test_validate_upstream_serial_rejects_a_missing_watermark() {
    assert_eq!(
        validate_upstream_serial(Some(10), None),
        Err(UpstreamSerialError::Missing { required: 10 })
    );
}

#[test]
fn test_validate_upstream_serial_rejects_a_regression() {
    assert_eq!(
        validate_upstream_serial(Some(10), Some(9)),
        Err(UpstreamSerialError::Regressed {
            required: 10,
            received: 9,
        })
    );
}

#[rstest::rstest]
#[case::missing(
    UpstreamSerialError::Missing { required: 10 },
    "upstream response omitted the serial watermark; required at least 10"
)]
#[case::regressed(
    UpstreamSerialError::Regressed { required: 10, received: 9 },
    "upstream serial 9 precedes required serial 10"
)]
fn test_upstream_serial_error_display(#[case] error: UpstreamSerialError, #[case] expected: &str) {
    assert_eq!(error.to_string(), expected);
}

#[test]
fn test_compose_serial_watermarks_uses_the_shared_low_watermark() {
    assert_eq!(
        compose_serial_watermarks([
            Some(SerialStamp {
                domain: "pypi.org".to_owned(),
                serial: 12,
            }),
            Some(SerialStamp {
                domain: "pypi.org".to_owned(),
                serial: 10,
            }),
            Some(SerialStamp {
                domain: "pypi.org".to_owned(),
                serial: 11,
            }),
        ]),
        Some(SerialStamp {
            domain: "pypi.org".to_owned(),
            serial: 10,
        })
    );
}

#[rstest::rstest]
#[case::empty(Vec::new())]
#[case::missing(vec![
    Some(SerialStamp { domain: "pypi.org".to_owned(), serial: 10 }),
    None,
])]
#[case::mixed_domains(vec![
    Some(SerialStamp { domain: "local".to_owned(), serial: 10 }),
    Some(SerialStamp { domain: "pypi.org".to_owned(), serial: 10 }),
])]
fn test_compose_serial_watermarks_omits_an_unsafe_scalar(#[case] stamps: Vec<Option<SerialStamp>>) {
    assert_eq!(compose_serial_watermarks(stamps), None);
}

#[test]
fn test_compose_serial_watermarks_preserves_one_layer() {
    let stamp = SerialStamp {
        domain: "local".to_owned(),
        serial: 42,
    };
    assert_eq!(compose_serial_watermarks([Some(stamp.clone())]), Some(stamp));
}

#[test]
fn test_changelog_page_accepts_a_strict_snapshot_page() {
    let page = ChangelogPage::new(10, 15, vec![entry(11), entry(13)]).unwrap();
    assert_eq!(page.current_serial(), 15);
    assert_eq!(page.entries(), [entry(11), entry(13)]);
    assert_eq!(page.resume_serial(), 13);
}

#[rstest::rstest]
#[case::same(10)]
#[case::earlier(9)]
fn test_changelog_page_rejects_an_entry_not_after_the_cursor(#[case] serial: u64) {
    assert_eq!(
        ChangelogPage::new(10, 15, vec![entry(serial)]),
        Err(ChangelogPageError::AtOrBeforeCursor { after: 10, serial })
    );
}

#[rstest::rstest]
#[case::duplicate(vec![entry(11), entry(11)], 11, 11)]
#[case::regression(vec![entry(12), entry(11)], 12, 11)]
fn test_changelog_page_rejects_non_increasing_entries(
    #[case] entries: Vec<ChangelogEntry>,
    #[case] previous: u64,
    #[case] serial: u64,
) {
    assert_eq!(
        ChangelogPage::new(10, 15, entries),
        Err(ChangelogPageError::NotIncreasing { previous, serial })
    );
}

#[test]
fn test_changelog_page_rejects_an_entry_beyond_the_snapshot() {
    assert_eq!(
        ChangelogPage::new(10, 15, vec![entry(16)]),
        Err(ChangelogPageError::BeyondSnapshot {
            current: 15,
            serial: 16,
        })
    );
}

#[test]
fn test_changelog_page_rejects_more_than_the_warehouse_limit() {
    assert_eq!(
        ChangelogPage::new(-1, CHANGELOG_PAGE_SIZE as u64 + 1, (1..=50_001).map(entry).collect()),
        Err(ChangelogPageError::TooLarge { actual: 50_001 })
    );
}

#[rstest::rstest]
#[case::behind(10, 15, 15)]
#[case::ahead(20, 15, 20)]
#[case::negative(-1, 0, 0)]
fn test_changelog_page_empty_resume_never_regresses(
    #[case] after: i64,
    #[case] current_serial: u64,
    #[case] expected: u64,
) {
    assert_eq!(
        ChangelogPage::new(after, current_serial, Vec::new())
            .unwrap()
            .resume_serial(),
        expected
    );
}

#[rstest::rstest]
#[case::too_large(
    ChangelogPageError::TooLarge { actual: 50_001 },
    "changelog page has 50001 entries; limit is 50000"
)]
#[case::cursor(
    ChangelogPageError::AtOrBeforeCursor { after: 10, serial: 9 },
    "changelog serial 9 is not after cursor 10"
)]
#[case::order(
    ChangelogPageError::NotIncreasing { previous: 11, serial: 10 },
    "changelog serial 10 does not follow 11"
)]
#[case::snapshot(
    ChangelogPageError::BeyondSnapshot { current: 10, serial: 11 },
    "changelog serial 11 exceeds snapshot 10"
)]
fn test_changelog_page_error_display(#[case] error: ChangelogPageError, #[case] expected: &str) {
    assert_eq!(error.to_string(), expected);
}
