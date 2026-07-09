use super::record;
use crate::meta::CachedIndex;

#[test]
fn test_cached_index_encode_decode_roundtrip() {
    assert_eq!(CachedIndex::decode(&record().encode()).unwrap(), record());
}

#[test]
fn test_cached_index_decode_rejects_garbage() {
    assert!(CachedIndex::decode(b"not json").is_err());
}

#[test]
fn test_encode_decode_roundtrips_framed_record() {
    let original = CachedIndex {
        fresh_secs: Some(600),
        ..record()
    };
    let bytes = original.encode();
    assert!(bytes.starts_with(b"velodex1\n"));
    assert!(bytes.ends_with(b"<html></html>"));
    assert_eq!(CachedIndex::decode(&bytes).unwrap(), original);
}

#[test]
fn test_decode_accepts_legacy_json_records() {
    let legacy = serde_json::to_vec(&record()).unwrap();
    assert_eq!(CachedIndex::decode(&legacy).unwrap(), record());
}

#[test]
fn test_cached_index_summary_reports_body_and_record_size() {
    let bytes = record().encode();
    assert_eq!(
        CachedIndex::summary(&bytes).unwrap(),
        crate::meta::CachedIndexSummary {
            fetched_at_unix: 1_700_000_000,
            fresh_secs: None,
            body_bytes: 13,
            record_bytes: bytes.len() as u64,
            last_serial: Some(42),
            content_type: None,
        }
    );
}
