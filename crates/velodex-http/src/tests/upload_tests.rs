use std::io::Write as _;

use base64::Engine as _;
use base64::engine::general_purpose::STANDARD;
use blake2::Blake2bVar;
use blake2::digest::{Update as _, VariableOutput as _};
use flate2::Compression;
use flate2::write::GzEncoder;
use velodex_core::pypi::CoreMetadata;
use velodex_core::pypi::DistributionFilenameError;
use velodex_storage::blob::{BlobStore, Digest};

use crate::upload::{StagedUpload, UploadError, UploadForm, authorized, prepare};

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

fn full_form(filename: &str) -> UploadForm {
    UploadForm {
        action: Some("file_upload".to_owned()),
        name: Some("Flask".to_owned()),
        version: Some("1.0".to_owned()),
        requires_python: Some(">=3.8".to_owned()),
        filetype: Some("bdist_wheel".to_owned()),
        sha256_digest: None,
        blake2_256_digest: None,
        md5_digest: None,
        filename: Some(filename.to_owned()),
    }
}

#[test]
fn test_prepare_builds_content_addressed_record() {
    let wheel = wheel_metadata("Flask", "1.0");
    let (_dir, staged) = staged_upload(&wheel);

    let prepared = prepare(staged_form(&wheel), staged, "root/local", 1000).unwrap();
    let digest = Digest::of(&wheel);

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
    assert_eq!(prepared.record.file.size, Some(wheel.len() as u64));
    assert_eq!(
        prepared.record.file.upload_time.as_deref(),
        Some("1970-01-01T00:16:40Z")
    );
    assert_eq!(prepared.record.file.core_metadata, CoreMetadata::Absent);
    assert_eq!(
        prepared.metadata.as_deref(),
        Some(b"Metadata-Version: 2.1\nName: Flask\nVersion: 1.0\nRequires-Python: >=3.8\n".as_slice())
    );
}

#[test]
fn test_prepare_accepts_matching_declared_digests() {
    let wheel = wheel_metadata("Flask", "1.0");
    let (_dir, staged) = staged_upload(&wheel);
    let mut form = staged_form(&wheel);
    form.sha256_digest = Some(Digest::of(&wheel).as_str().to_owned());
    form.blake2_256_digest = Some(staged.blake2_256.clone());

    assert!(prepare(form, staged, "root/local", 1000).is_ok());
}

#[test]
fn test_prepare_accepts_valid_sdist() {
    let sdist = sdist_metadata("Flask", "1.0", ">=3.9");
    let (_dir, staged) = staged_upload(&sdist);
    let mut form = full_form("Flask-1.0.tar.gz");
    form.filetype = Some("sdist".to_owned());
    form.requires_python = None;

    let prepared = prepare(form, staged, "root/local", 1000).unwrap();

    assert_eq!(prepared.record.file.requires_python.as_deref(), Some(">=3.9"));
    assert!(prepared.metadata.is_none());
}

#[test]
fn test_prepare_rejects_wrong_action() {
    let wheel = wheel_metadata("Flask", "1.0");
    let (_dir, staged) = staged_upload(&wheel);
    let mut form = staged_form(&wheel);
    form.action = Some("submit".to_owned());

    assert_eq!(
        prepare(form, staged, "root/local", 1000).unwrap_err(),
        UploadError::NotFileUpload
    );
}

#[test]
fn test_prepare_rejects_invalid_form_identity() {
    for (mut form, expected) in [
        {
            let mut form = staged_form(&wheel_metadata("Flask", "1.0"));
            form.name = Some("-bad".to_owned());
            (form, UploadError::InvalidName("-bad".to_owned()))
        },
        {
            let mut form = staged_form(&wheel_metadata("Flask", "1.0"));
            form.version = Some("not a version".to_owned());
            (form, UploadError::InvalidVersion("not a version".to_owned()))
        },
    ] {
        let wheel = wheel_metadata("Flask", "1.0");
        let (_dir, staged) = staged_upload(&wheel);
        form.filename = Some("Flask-1.0-py3-none-any.whl".to_owned());
        assert_eq!(prepare(form, staged, "root/local", 1000).unwrap_err(), expected);
    }
}

#[test]
fn test_prepare_rejects_digest_problems() {
    for (configure, expected) in [
        (
            (|form: &mut UploadForm| form.sha256_digest = Some("00".repeat(32))) as fn(&mut UploadForm),
            UploadError::DigestMismatch("sha256_digest"),
        ),
        (
            |form| form.sha256_digest = Some("ABC".to_owned()),
            UploadError::InvalidDigest {
                field: "sha256_digest",
                value: "ABC".to_owned(),
            },
        ),
        (
            |form| {
                form.sha256_digest = None;
                form.md5_digest = Some("d41d8cd98f00b204e9800998ecf8427e".to_owned());
            },
            UploadError::Md5Only,
        ),
    ] {
        let wheel = wheel_metadata("Flask", "1.0");
        let (_dir, staged) = staged_upload(&wheel);
        let mut form = staged_form(&wheel);
        configure(&mut form);

        assert_eq!(prepare(form, staged, "root/local", 1000).unwrap_err(), expected);
    }
}

#[test]
fn test_prepare_rejects_filename_problems() {
    for (filename, expected) in [
        ("../pkg.whl", UploadError::InvalidFilename("../pkg.whl".to_owned())),
        (
            "pkg-1.0.egg",
            UploadError::InvalidDistributionFilename {
                filename: "pkg-1.0.egg".to_owned(),
                error: DistributionFilenameError::LegacyEgg,
            },
        ),
        (
            "pkg-1.0.zip",
            UploadError::InvalidDistributionFilename {
                filename: "pkg-1.0.zip".to_owned(),
                error: DistributionFilenameError::UnsupportedExtension,
            },
        ),
        (
            "pkg-1.0-py3-none.whl",
            UploadError::InvalidDistributionFilename {
                filename: "pkg-1.0-py3-none.whl".to_owned(),
                error: DistributionFilenameError::InvalidWheelShape,
            },
        ),
        (
            "pkg-1.0-py3-*-any.whl",
            UploadError::InvalidDistributionFilename {
                filename: "pkg-1.0-py3-*-any.whl".to_owned(),
                error: DistributionFilenameError::InvalidTag("*".to_owned()),
            },
        ),
    ] {
        let wheel = wheel_metadata("Flask", "1.0");
        let (_dir, staged) = staged_upload(&wheel);
        let mut form = staged_form(&wheel);
        form.filename = Some(filename.to_owned());

        assert_eq!(prepare(form, staged, "root/local", 1000).unwrap_err(), expected);
    }
}

#[test]
fn test_prepare_rejects_filename_form_mismatches() {
    for (filename, expected) in [
        (
            "Other-1.0-py3-none-any.whl",
            UploadError::FilenameNameMismatch {
                filename: "Other".to_owned(),
                form: "Flask".to_owned(),
            },
        ),
        (
            "Flask-2.0-py3-none-any.whl",
            UploadError::FilenameVersionMismatch {
                filename: "2.0".to_owned(),
                form: "1.0".to_owned(),
            },
        ),
    ] {
        let wheel = wheel_metadata("Flask", "1.0");
        let (_dir, staged) = staged_upload(&wheel);
        let mut form = staged_form(&wheel);
        form.filename = Some(filename.to_owned());

        assert_eq!(prepare(form, staged, "root/local", 1000).unwrap_err(), expected);
    }
}

#[test]
fn test_prepare_rejects_filetype_mismatch() {
    let wheel = wheel_metadata("Flask", "1.0");
    let (_dir, staged) = staged_upload(&wheel);
    let mut form = staged_form(&wheel);
    form.filetype = Some("sdist".to_owned());

    assert_eq!(
        prepare(form, staged, "root/local", 1000).unwrap_err(),
        UploadError::FiletypeMismatch {
            expected: "bdist_wheel".to_owned(),
            actual: "sdist".to_owned(),
        }
    );
}

#[test]
fn test_prepare_rejects_archive_content_problems() {
    for (bytes, expected) in [
        (
            b"not a zip".to_vec(),
            UploadError::InvalidContent("archive read failed: invalid Zip archive: Could not find EOCD".to_owned()),
        ),
        (wheel_without_metadata(), UploadError::MissingMetadata("METADATA")),
        (wheel_metadata_bytes(b"\xff"), UploadError::InvalidMetadataUtf8),
    ] {
        let (_dir, staged) = staged_upload(&bytes);

        assert_eq!(
            prepare(full_form("Flask-1.0-py3-none-any.whl"), staged, "root/local", 1000).unwrap_err(),
            expected
        );
    }
}

#[test]
fn test_prepare_rejects_sdist_archive_read_errors() {
    let (_dir, staged) = staged_upload(b"not a gzip");
    let mut form = full_form("Flask-1.0.tar.gz");
    form.filetype = Some("sdist".to_owned());

    let err = prepare(form, staged, "root/local", 1000).unwrap_err();

    assert!(matches!(err, UploadError::InvalidContent(message) if message.starts_with("archive read failed: ")));
}

#[test]
fn test_prepare_rejects_metadata_mismatches() {
    for (bytes, expected) in [
        (
            wheel_metadata("Other", "1.0"),
            UploadError::MetadataNameMismatch {
                metadata: "Other".to_owned(),
                form: "flask".to_owned(),
            },
        ),
        (
            wheel_metadata("Flask", "bad"),
            UploadError::MetadataVersionMismatch {
                metadata: "bad".to_owned(),
                form: "1.0".to_owned(),
            },
        ),
        (
            wheel_metadata("Flask", "2.0"),
            UploadError::MetadataVersionMismatch {
                metadata: "2.0".to_owned(),
                form: "1.0".to_owned(),
            },
        ),
    ] {
        let (_dir, staged) = staged_upload(&bytes);

        assert_eq!(
            prepare(full_form("Flask-1.0-py3-none-any.whl"), staged, "root/local", 1000).unwrap_err(),
            expected
        );
    }
}

#[test]
fn test_prepare_rejects_invalid_requires_python_and_clock() {
    let wheel = wheel_metadata("Flask", "1.0");
    let (_dir, staged) = staged_upload(&wheel);
    let mut form = staged_form(&wheel);
    form.requires_python = Some("=>3".to_owned());
    assert_eq!(
        prepare(form, staged, "root/local", 1000).unwrap_err(),
        UploadError::InvalidRequiresPython("=>3".to_owned())
    );

    let (_dir, staged) = staged_upload(&wheel);
    assert_eq!(
        prepare(staged_form(&wheel), staged, "root/local", i64::MAX).unwrap_err(),
        UploadError::InvalidUploadTime
    );
}

#[test]
fn test_prepare_requires_each_field() {
    for (clear, missing) in [
        (
            (|form: &mut UploadForm| form.name = None) as fn(&mut UploadForm),
            "name",
        ),
        (|form| form.version = None, "version"),
        (|form| form.filename = None, "filename"),
        (|form| form.filetype = None, "filetype"),
    ] {
        let wheel = wheel_metadata("Flask", "1.0");
        let (_dir, staged) = staged_upload(&wheel);
        let mut form = staged_form(&wheel);
        clear(&mut form);
        assert_eq!(
            prepare(form, staged, "root/local", 1000).unwrap_err(),
            UploadError::Missing(missing)
        );
    }
}

fn staged_form(bytes: &[u8]) -> UploadForm {
    let mut form = full_form("Flask-1.0-py3-none-any.whl");
    form.sha256_digest = Some(Digest::of(bytes).as_str().to_owned());
    form
}

fn staged_upload(bytes: &[u8]) -> (tempfile::TempDir, StagedUpload) {
    let dir = tempfile::tempdir().unwrap();
    let store = BlobStore::new(dir.path().join("blobs"));
    let mut pending = store.begin().unwrap();
    pending.write(bytes).unwrap();
    let mut blake2 = Blake2bVar::new(32).unwrap();
    blake2.update(bytes);
    let mut digest = [0; 32];
    blake2.finalize_variable(&mut digest).unwrap();
    (
        dir,
        StagedUpload {
            blob: pending.finish().unwrap(),
            blake2_256: hex(&digest),
        },
    )
}

fn wheel_metadata(name: &str, version: &str) -> Vec<u8> {
    wheel_metadata_bytes(
        format!("Metadata-Version: 2.1\nName: {name}\nVersion: {version}\nRequires-Python: >=3.8\n").as_bytes(),
    )
}

fn wheel_metadata_bytes(metadata: &[u8]) -> Vec<u8> {
    let mut buf = Vec::new();
    {
        let mut zip = zip::ZipWriter::new(std::io::Cursor::new(&mut buf));
        let options = zip::write::SimpleFileOptions::default().compression_method(zip::CompressionMethod::Stored);
        zip.start_file("Flask/__init__.py", options).unwrap();
        zip.write_all(b"VALUE = 1\n").unwrap();
        zip.start_file("Flask-1.0.dist-info/METADATA", options).unwrap();
        zip.write_all(metadata).unwrap();
        zip.finish().unwrap();
    }
    buf
}

fn wheel_without_metadata() -> Vec<u8> {
    let mut buf = Vec::new();
    {
        let mut zip = zip::ZipWriter::new(std::io::Cursor::new(&mut buf));
        zip.start_file("Flask/__init__.py", zip::write::SimpleFileOptions::default())
            .unwrap();
        zip.write_all(b"VALUE = 1\n").unwrap();
        zip.finish().unwrap();
    }
    buf
}

fn sdist_metadata(name: &str, version: &str, requires_python: &str) -> Vec<u8> {
    let mut buf = Vec::new();
    {
        let encoder = GzEncoder::new(&mut buf, Compression::default());
        let mut tar = tar::Builder::new(encoder);
        let content =
            format!("Metadata-Version: 2.1\nName: {name}\nVersion: {version}\nRequires-Python: {requires_python}\n");
        let mut header = tar::Header::new_gnu();
        header.set_size(content.len() as u64);
        header.set_mode(0o644);
        header.set_cksum();
        tar.append_data(&mut header, "Flask-1.0/PKG-INFO", content.as_bytes())
            .unwrap();
        tar.finish().unwrap();
    }
    buf
}

fn hex(bytes: &[u8]) -> String {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    let mut out = String::with_capacity(bytes.len() * 2);
    for &byte in bytes {
        out.push(HEX[(byte >> 4) as usize] as char);
        out.push(HEX[(byte & 0x0f) as usize] as char);
    }
    out
}
