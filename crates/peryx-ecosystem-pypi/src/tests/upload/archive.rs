//! Sdist and archive-content validation.

use super::support::*;

#[test]
fn test_prepare_accepts_valid_sdist() {
    let sdist = sdist_metadata("Flask", "1.0", ">=3.9");
    let (_dir, staged) = staged_upload(&sdist);
    let mut form = full_form("Flask-1.0.tar.gz");
    form.filetype = Some("sdist".to_owned());
    form.requires_python = None;

    let prepared = prepare(form, staged, "root/hosted", 1000).unwrap();

    assert_eq!(prepared.record.file.requires_python.as_deref(), Some(">=3.9"));
    assert!(
        prepared
            .metadata
            .starts_with(b"Metadata-Version: 2.2\nName: Flask\nVersion: 1.0\n")
    );
}
#[test]
fn test_prepare_rejects_archive_content_problems() {
    for (bytes, expected) in [
        (
            b"not a zip".to_vec(),
            UploadError::InvalidContent("archive read failed: invalid Zip archive: Could not find EOCD".to_owned()),
        ),
        (
            wheel_without_metadata(),
            UploadError::InvalidContent("invalid wheel: missing required flask-1.0.dist-info/METADATA".to_owned()),
        ),
        (wheel_metadata_bytes(b"\xff"), UploadError::InvalidMetadataUtf8),
    ] {
        let (_dir, staged) = staged_upload(&bytes);

        assert_eq!(
            prepare(full_form("Flask-1.0-py3-none-any.whl"), staged, "root/hosted", 1000).unwrap_err(),
            expected
        );
    }

    let sdist = sdist_metadata("Other", "1.0", ">=3.9");
    let (_dir, staged) = staged_upload(&sdist);
    let mut form = full_form("Flask-1.0.tar.gz");
    form.filetype = Some("sdist".to_owned());
    assert_eq!(
        prepare(form, staged, "root/hosted", 1000).unwrap_err(),
        UploadError::MetadataNameMismatch {
            metadata: "Other".to_owned(),
            form: "flask".to_owned(),
        }
    );
}
#[test]
fn test_prepare_rejects_sdist_archive_read_errors() {
    let (_dir, staged) = staged_upload(b"not a gzip");
    let mut form = full_form("Flask-1.0.tar.gz");
    form.filetype = Some("sdist".to_owned());

    let err = prepare(form, staged, "root/hosted", 1000).unwrap_err();

    assert!(matches!(err, UploadError::InvalidContent(message) if message.starts_with("archive read failed: ")));
}
