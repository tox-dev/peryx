use super::*;

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
