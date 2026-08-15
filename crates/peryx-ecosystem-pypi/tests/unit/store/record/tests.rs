use super::{CachedIndex, CachedIndexSummary, FreshnessOverlay};

#[test]
fn test_freshness_overlay_encode_decode_roundtrips() {
    let overlay = FreshnessOverlay {
        fetched_at_unix: 1_800_000_000,
        fresh_secs: Some(600),
    };
    assert_eq!(FreshnessOverlay::decode(&overlay.encode()).unwrap(), overlay);
}

fn record() -> CachedIndex {
    CachedIndex {
        etag: Some("\"abc\"".to_owned()),
        last_serial: Some(42),
        fetched_at_unix: 1_700_000_000,
        content_type: None,
        fresh_secs: None,
        body: b"<html></html>".to_vec(),
    }
}

#[test]
fn test_encode_decode_roundtrips_a_framed_record() {
    let original = CachedIndex {
        fresh_secs: Some(600),
        ..record()
    };
    let bytes = original.encode();
    assert!(bytes.starts_with(b"peryx1\n"));
    assert!(bytes.ends_with(b"<html></html>"));
    assert_eq!(CachedIndex::decode(&bytes).unwrap(), original);
}

#[test]
fn test_decode_rejects_garbage() {
    assert!(CachedIndex::decode(b"not json").is_err());
}

#[test]
fn test_decode_accepts_a_legacy_plain_json_record() {
    let legacy = serde_json::to_vec(&record()).unwrap();
    assert_eq!(CachedIndex::decode(&legacy).unwrap(), record());
}

#[test]
fn test_summary_reads_a_framed_record_without_copying_the_body() {
    let bytes = record().encode();
    assert_eq!(
        CachedIndex::summary(&bytes).unwrap(),
        CachedIndexSummary {
            fetched_at_unix: 1_700_000_000,
            fresh_secs: None,
            body_bytes: 13,
            record_bytes: bytes.len() as u64,
            last_serial: Some(42),
            content_type: None,
        }
    );
}

#[test]
fn test_summary_reads_a_legacy_plain_json_record() {
    let legacy = serde_json::to_vec(&record()).unwrap();
    let summary = CachedIndex::summary(&legacy).unwrap();
    assert_eq!(summary.fetched_at_unix, 1_700_000_000);
    assert_eq!(summary.body_bytes, 13);
    assert_eq!(summary.record_bytes, legacy.len() as u64);
}

#[test]
fn test_summary_rejects_garbage() {
    assert!(CachedIndex::summary(b"not json").is_err());
}
