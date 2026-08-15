use peryx_storage::blob::BlobDurability;

use super::*;

#[test]
fn test_durable_response_is_final() {
    let response = ack_response(
        WriteDurability::Confirmed {
            scope: BlobDurability::Filesystem,
        },
        "op-1",
    );
    assert_eq!(
        (response.status, response.body, response.finalize),
        (StatusCode::OK, b"upload accepted".to_vec(), true)
    );
}

#[test]
fn test_unknown_response_carries_the_operation() {
    let response = ack_response(WriteDurability::Unavailable, "op-1");
    assert_eq!(response.status, StatusCode::ACCEPTED);
    assert!(String::from_utf8_lossy(&response.body).contains("op-1"));
    assert!(!response.finalize);
}

#[test]
fn test_pending_response_is_not_final() {
    let response = ack_response(WriteDurability::Pending, "op-1");
    assert_eq!(
        (response.status, response.body, response.finalize),
        (
            StatusCode::ACCEPTED,
            b"upload accepted; durability pending".to_vec(),
            false
        )
    );
}
