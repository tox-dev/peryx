use super::*;

#[test]
fn test_record_allows_exactly_the_entry_cap() {
    let mut members = SdistMembers::new("pkg-1.0".to_owned());
    members.entries = MAX_SDIST_ENTRIES - 1;

    assert!(
        members
            .record("pkg-1.0/last".to_owned(), tar::EntryType::Regular)
            .is_ok()
    );
}

#[test]
fn test_record_rejects_the_first_entry_past_the_cap() {
    let mut members = SdistMembers::new("pkg-1.0".to_owned());
    members.entries = MAX_SDIST_ENTRIES;

    let message = members
        .record("pkg-1.0/overflow".to_owned(), tar::EntryType::Regular)
        .unwrap_err()
        .to_string();

    assert!(message.contains("more than 100000 entries"), "{message}");
}

#[test]
fn test_validate_sdist_member_path_requires_the_root_entry_to_be_a_directory() {
    let error = validate_sdist_member_path("pkg-1.0", "pkg-1.0", tar::EntryType::Regular).unwrap_err();

    assert!(
        error
            .to_string()
            .contains("outside required top-level directory \"pkg-1.0\""),
        "{error}"
    );
}

#[test]
fn test_read_sdist_member_limited_allows_a_member_exactly_at_the_limit() {
    let mut reader = std::io::Cursor::new(b"abcd".to_vec());

    assert_eq!(read_sdist_member_limited(&mut reader, "path", 4, 4).unwrap(), b"abcd");
}

#[test]
fn test_metadata_version_at_least_accepts_a_newer_major_version() {
    assert!(metadata_version_at_least((3, 0), (2, 2)));
}

#[test]
fn test_account_sdist_expansion_sums_within_budget() {
    assert_eq!(account_sdist_expansion(10, 5).unwrap(), 15);
}

#[test]
fn test_account_sdist_expansion_rejects_members_crossing_budget() {
    let message = account_sdist_expansion(MAX_SDIST_EXPANDED_BYTES, 1)
        .unwrap_err()
        .to_string();
    assert!(message.contains("expand to more than"), "{message}");
}

// A gzip tar cannot carry a member whose declared size overflows the running sum: the tar reader
// guards its own size arithmetic first, so the checked add is exercised at its boundary here.
#[test]
fn test_account_sdist_expansion_rejects_size_sum_overflow() {
    let message = account_sdist_expansion(1, u64::MAX).unwrap_err().to_string();
    assert!(message.contains("expand to more than"), "{message}");
}

#[test]
fn test_account_zip_sdist_expansion_allows_max_ratio() {
    assert_eq!(
        account_zip_sdist_expansion(0, "pkg-1.0/data.bin", MAX_SDIST_COMPRESSION_RATIO, 1).unwrap(),
        MAX_SDIST_COMPRESSION_RATIO
    );
}

#[test]
fn test_account_zip_sdist_expansion_rejects_zero_compressed_with_content() {
    let message = account_zip_sdist_expansion(0, "pkg-1.0/bomb.bin", 1, 0)
        .unwrap_err()
        .to_string();
    assert!(message.contains("expansion limit"), "{message}");
}
