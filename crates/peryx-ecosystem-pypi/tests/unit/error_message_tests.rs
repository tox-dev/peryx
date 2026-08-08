use super::error_message;

#[test]
fn test_error_message_stringifies_io_and_store_faults() {
    assert_eq!(error_message(std::io::Error::other("disk")), "disk");
    let decode = serde_json::from_str::<u8>("x").unwrap_err();
    assert!(!error_message(peryx_storage::meta::MetaError::Decode(decode)).is_empty());
}
