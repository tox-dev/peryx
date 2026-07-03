use base64::Engine as _;
use base64::engine::general_purpose::STANDARD;
use velodex_core::pypi::CoreMetadata;
use velodex_storage::blob::Digest;

use crate::upload::{UploadError, UploadForm, authorized, prepare};

fn basic(credentials: &[u8]) -> String {
    format!("Basic {}", STANDARD.encode(credentials))
}

#[test]
fn test_authorized_accepts_any_user_with_the_token() {
    assert!(authorized(Some(&basic(b"__token__:s3cret")), "s3cret"));
    assert!(authorized(Some(&basic(b"alice:s3cret")), "s3cret"));
}

#[test]
fn test_authorized_rejects_wrong_password() {
    assert!(!authorized(Some(&basic(b"alice:nope")), "s3cret"));
}

#[test]
fn test_authorized_rejects_missing_or_non_basic_header() {
    assert!(!authorized(None, "s3cret"));
    assert!(!authorized(Some("Bearer s3cret"), "s3cret"));
}

#[test]
fn test_authorized_rejects_malformed_base64() {
    assert!(!authorized(Some("Basic !!!not-base64!!!"), "s3cret"));
}

#[test]
fn test_authorized_rejects_non_utf8_and_missing_colon() {
    assert!(!authorized(Some(&basic(&[0xff, 0xfe])), "s3cret"));
    assert!(!authorized(Some(&basic(b"nocolonhere")), "s3cret"));
}

fn full_form() -> UploadForm {
    UploadForm {
        action: Some("file_upload".to_owned()),
        name: Some("Flask".to_owned()),
        version: Some("1.0".to_owned()),
        requires_python: Some(">=3.8".to_owned()),
        sha256_digest: None,
        filename: Some("Flask-1.0-py3-none-any.whl".to_owned()),
        content: Some(b"wheel-bytes".to_vec()),
    }
}

#[test]
fn test_prepare_builds_content_addressed_record() {
    let prepared = prepare(full_form(), "root/local").unwrap();
    let digest = Digest::of(b"wheel-bytes");
    assert_eq!(prepared.normalized, "flask");
    assert_eq!(prepared.display_name, "Flask");
    assert_eq!(prepared.digest, digest);
    assert_eq!(prepared.record.version, "1.0");
    assert_eq!(
        prepared.record.file.url,
        format!("/root/local/files/{}/Flask-1.0-py3-none-any.whl", digest.as_str())
    );
    assert_eq!(
        prepared.record.file.hashes.get("sha256").map(String::as_str),
        Some(digest.as_str())
    );
    assert_eq!(prepared.record.file.requires_python.as_deref(), Some(">=3.8"));
    assert_eq!(prepared.record.file.size, Some(11));
    assert_eq!(prepared.record.file.core_metadata, CoreMetadata::Absent);
}

#[test]
fn test_prepare_accepts_matching_declared_digest() {
    let mut form = full_form();
    form.sha256_digest = Some(Digest::of(b"wheel-bytes").as_str().to_owned());
    assert!(prepare(form, "root/local").is_ok());
}

#[test]
fn test_prepare_rejects_wrong_action() {
    let mut form = full_form();
    form.action = Some("submit".to_owned());
    assert_eq!(prepare(form, "root/local").unwrap_err(), UploadError::NotFileUpload);
}

#[test]
fn test_prepare_rejects_digest_mismatch() {
    let mut form = full_form();
    form.sha256_digest = Some("00".repeat(32));
    assert_eq!(prepare(form, "root/local").unwrap_err(), UploadError::DigestMismatch);
}

#[test]
fn test_prepare_requires_each_field() {
    for (clear, missing) in [
        ((|f: &mut UploadForm| f.name = None) as fn(&mut UploadForm), "name"),
        (|f| f.version = None, "version"),
        (|f| f.filename = None, "filename"),
        (|f| f.content = None, "content"),
    ] {
        let mut form = full_form();
        clear(&mut form);
        assert_eq!(prepare(form, "root/local").unwrap_err(), UploadError::Missing(missing));
    }
}
