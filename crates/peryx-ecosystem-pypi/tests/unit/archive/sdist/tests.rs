use super::*;

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
