use std::io::Write as _;

use super::*;

#[test]
fn test_wheel_record_and_expanded_budgets_are_computed_by_multiplication() {
    assert_eq!(MAX_WHEEL_RECORD_BYTES, 67_108_864);
    assert_eq!(MAX_WHEEL_EXPANDED_BYTES, 8_589_934_592);
}

fn zip_with_one_stored_entry(name: &str, content: &[u8]) -> zip::ZipArchive<std::io::Cursor<Vec<u8>>> {
    let mut buf = Vec::new();
    {
        let mut writer = zip::ZipWriter::new(std::io::Cursor::new(&mut buf));
        let options = zip::write::SimpleFileOptions::default().compression_method(zip::CompressionMethod::Stored);
        writer.start_file(name, options).unwrap();
        writer.write_all(content).unwrap();
        writer.finish().unwrap();
    }
    zip::ZipArchive::new(std::io::Cursor::new(buf)).unwrap()
}

#[test]
fn test_read_zip_member_limited_allows_a_member_exactly_at_the_limit() {
    let mut archive = zip_with_one_stored_entry("member", b"abcd");

    assert_eq!(read_zip_member_limited(&mut archive, "member", 4).unwrap(), b"abcd");
}

#[test]
fn test_account_member_expansion_sums_within_budget() {
    assert_eq!(account_member_expansion(10, "pkg/a", 5, 5).unwrap(), 15);
}

#[test]
fn test_account_member_expansion_allows_max_ratio() {
    assert_eq!(
        account_member_expansion(0, "pkg/a", MAX_WHEEL_COMPRESSION_RATIO, 1).unwrap(),
        MAX_WHEEL_COMPRESSION_RATIO
    );
}

#[test]
fn test_account_member_expansion_rejects_high_ratio() {
    let message = account_member_expansion(0, "pkg/bomb", MAX_WHEEL_COMPRESSION_RATIO + 1, 1)
        .unwrap_err()
        .to_string();
    assert!(message.contains("above the 1000:1 expansion limit"), "{message}");
}

#[test]
fn test_account_member_expansion_rejects_zero_compressed_with_content() {
    let message = account_member_expansion(0, "pkg/bomb", 1, 0).unwrap_err().to_string();
    assert!(message.contains("expansion limit"), "{message}");
}

#[test]
fn test_account_member_expansion_rejects_over_budget() {
    let message = account_member_expansion(MAX_WHEEL_EXPANDED_BYTES, "pkg/a", 1, 1)
        .unwrap_err()
        .to_string();
    assert!(message.contains("expand to more than"), "{message}");
}

#[test]
fn test_account_member_expansion_rejects_size_overflow() {
    let message = account_member_expansion(1, "pkg/a", u64::MAX, u64::MAX)
        .unwrap_err()
        .to_string();
    assert!(message.contains("expand to more than"), "{message}");
}
