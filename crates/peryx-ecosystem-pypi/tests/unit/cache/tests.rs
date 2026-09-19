use super::*;

#[test]
fn test_cache_error_converts_to_its_user_message_string() {
    assert_eq!(
        String::from(CacheError::Unavailable),
        "upstream is unavailable and no cached page exists"
    );
}

#[test]
fn test_cache_error_archive_message_is_user_visible() {
    assert_eq!(
        CacheError::Archive(crate::archive::ArchiveError::Unsupported).user_message(),
        "unsupported archive type"
    );
}

#[test]
fn test_freshness_secs_clamps_a_negative_ttl_to_zero() {
    assert_eq!(freshness_secs(-1, None), 0);
}

#[test]
fn test_freshness_secs_clamps_a_negative_granted_lifetime_to_zero() {
    assert_eq!(freshness_secs(10, Some(-5)), 0);
}

#[test]
fn test_freshness_secs_honors_a_shorter_granted_lifetime() {
    assert_eq!(freshness_secs(60, Some(10)), 10);
}

#[test]
fn test_freshness_secs_caps_a_longer_granted_lifetime_at_the_ttl() {
    assert_eq!(freshness_secs(60, Some(120)), 60);
}

#[test]
fn test_cache_error_maps_upload_store_errors() {
    let err = upload::UploadStoreError::Meta(peryx_storage::meta::MetaError::Decode(
        serde_json::from_str::<serde_json::Value>("{").unwrap_err(),
    ));
    assert!(matches!(CacheError::from(err), CacheError::Meta(_)));

    let err = upload::UploadStoreError::Blob(peryx_storage::blob::BlobError::not_found(
        &peryx_storage::blob::Digest::of(b"missing"),
    ));
    assert!(matches!(CacheError::from(err), CacheError::Blob(_)));
}
