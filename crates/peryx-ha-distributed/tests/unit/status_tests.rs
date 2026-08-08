use crate::status::{OperationStatus, WriteRecord};

fn rec(published: bool, failed: bool, expiry: Option<u64>) -> WriteRecord {
    WriteRecord {
        published,
        failed,
        expiry,
    }
}

#[test]
fn test_published_is_terminal_even_past_expiry() {
    assert_eq!(rec(true, false, None).status(0), OperationStatus::Published);
    assert_eq!(rec(true, false, Some(50)).status(1_000), OperationStatus::Published);
}

#[test]
fn test_failed_is_terminal_even_past_expiry() {
    assert_eq!(rec(false, true, None).status(0), OperationStatus::Failed);
    assert_eq!(rec(false, true, Some(50)).status(1_000), OperationStatus::Failed);
}

#[test]
fn test_unfinalized_at_or_past_expiry_is_expired() {
    assert_eq!(rec(false, false, Some(100)).status(100), OperationStatus::Expired);
    assert_eq!(rec(false, false, Some(100)).status(200), OperationStatus::Expired);
}

#[test]
fn test_unfinalized_before_expiry_is_pending() {
    assert_eq!(rec(false, false, Some(100)).status(99), OperationStatus::Pending);
}

#[test]
fn test_unfinalized_without_expiry_is_pending() {
    assert_eq!(rec(false, false, None).status(u64::MAX), OperationStatus::Pending);
}

#[test]
fn test_published_outranks_failed() {
    assert_eq!(rec(true, true, None).status(0), OperationStatus::Published);
}
