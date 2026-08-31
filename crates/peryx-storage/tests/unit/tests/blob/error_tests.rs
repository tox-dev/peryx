use std::error::Error as _;

use crate::blob::{BlobError, BlobOperation, BlobScanError, Digest};

#[test]
fn test_blob_operations_use_backend_verbs() {
    for (operation, expected) in [
        (BlobOperation::Health, "health"),
        (BlobOperation::Open, "open"),
        (BlobOperation::Head, "head"),
        (BlobOperation::Write, "write"),
        (BlobOperation::Commit, "commit"),
        (BlobOperation::Delete, "delete"),
        (BlobOperation::Verify, "verify"),
        (BlobOperation::List, "list"),
        (BlobOperation::Materialize, "materialize"),
    ] {
        assert_eq!(operation.to_string(), expected);
    }
}

#[test]
fn test_scan_store_error_reports_source() {
    let digest = Digest::of(b"missing");
    let err: BlobScanError<std::io::Error> = BlobError::not_found(&digest).into();
    assert_eq!(err.to_string(), format!("blob {} not found", digest.as_str()));
    assert!(err.source().is_some());
}

#[test]
fn test_mismatch_returns_only_digest_pairs() {
    let expected = Digest::of(b"expected");
    let actual = Digest::of(b"actual");
    assert_eq!(
        BlobError::digest_mismatch(&expected, &actual).mismatch(),
        Some((expected.as_str(), actual.as_str()))
    );
    assert_eq!(BlobError::io(std::io::Error::other("disk")).mismatch(), None);
    assert_eq!(BlobError::size_mismatch(7, 9).mismatch(), None);
}

#[test]
fn test_size_mismatch_is_a_content_mismatch() {
    assert_eq!(
        BlobError::size_mismatch(7, 9).kind(),
        crate::blob::BlobErrorKind::DigestMismatch
    );
}

#[test]
fn test_blob_error_formats_every_detail_with_optional_context() {
    let expected = Digest::of(b"expected");
    let actual = Digest::of(b"actual");
    for (error, message) in [
        (BlobError::io(std::io::Error::other("disk")), "I/O error".to_owned()),
        (
            BlobError::not_found(&expected),
            format!("blob {} not found", expected.as_str()),
        ),
        (
            BlobError::digest_mismatch(&expected, &actual),
            format!(
                "digest mismatch: expected {}, got {}",
                expected.as_str(),
                actual.as_str()
            ),
        ),
        (
            BlobError::size_mismatch(7, 9),
            "size mismatch: expected 7 bytes, got 9".to_owned(),
        ),
        (
            BlobError::invalid_range(3, 9, 7),
            "range 3..9 exceeds 7 bytes".to_owned(),
        ),
        (
            BlobError::limit_exceeded(6, 7),
            "blob size 7 exceeds 6 byte limit".to_owned(),
        ),
        (
            BlobError::unsupported("streaming"),
            "streaming is unsupported".to_owned(),
        ),
    ] {
        assert_eq!(error.to_string(), message);
    }
    assert_eq!(
        BlobError::not_found(&expected)
            .with_context("filesystem", BlobOperation::Open, Some(&expected))
            .to_string(),
        format!(
            "filesystem blob backend open for {}: blob {} not found",
            expected.as_str(),
            expected.as_str()
        )
    );
    assert_eq!(
        BlobError::unsupported("streaming")
            .with_context("s3", BlobOperation::Open, None)
            .to_string(),
        "s3 blob backend open: streaming is unsupported"
    );
}

#[test]
fn test_non_range_error_returns_no_range_values() {
    assert_eq!(
        BlobError::io(std::io::Error::other("disk")).invalid_range_values(),
        None
    );
}

#[test]
fn test_range_error_includes_its_values() {
    assert_eq!(
        BlobError::invalid_range(3, 9, 7).to_string(),
        "range 3..9 exceeds 7 bytes"
    );
}

#[test]
fn test_limit_error_includes_its_values() {
    assert_eq!(
        BlobError::limit_exceeded(6, 7).to_string(),
        "blob size 7 exceeds 6 byte limit"
    );
}

#[test]
fn test_io_error_reports_a_source() {
    assert!(BlobError::io(std::io::Error::other("disk")).source().is_some());
}

#[test]
fn test_not_found_error_has_no_source() {
    assert!(BlobError::not_found(&Digest::of(b"missing")).source().is_none());
}

#[test]
fn test_digest_mismatch_error_names_both_digests() {
    let expected = Digest::of(b"expected");
    let actual = Digest::of(b"actual");
    assert_eq!(
        BlobError::digest_mismatch(&expected, &actual).to_string(),
        format!(
            "digest mismatch: expected {}, got {}",
            expected.as_str(),
            actual.as_str()
        )
    );
}

#[tokio::test]
async fn test_join_error_converts_to_an_io_error() {
    let join_error = tokio::spawn(async { panic!("worker failed") }).await.unwrap_err();
    let error = BlobError::from(join_error);

    assert_eq!(error.kind(), crate::blob::BlobErrorKind::Io);
    assert!(error.source().is_some());
}
