use std::fmt::Write as _;
use std::io::Write as _;

use base64::Engine as _;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use peryx_storage::blob::BlobStorage;
use peryx_storage::meta::MetaStore;
use sha2::{Digest as _, Sha256};

use super::{import_dir, upload_error_reason};
use crate::store::PypiStore as _;
use crate::upload::UploadError;

fn wheel() -> Vec<u8> {
    let entries: [(&str, &[u8]); 3] = [
        ("demo/__init__.py", b""),
        (
            "demo-1.0.dist-info/METADATA",
            b"Metadata-Version: 2.1\nName: demo\nVersion: 1.0\n",
        ),
        (
            "demo-1.0.dist-info/WHEEL",
            b"Wheel-Version: 1.0\nRoot-Is-Purelib: true\nTag: py3-none-any\n",
        ),
    ];
    let mut record = String::new();
    for (path, bytes) in entries {
        let digest = URL_SAFE_NO_PAD.encode(Sha256::digest(bytes));
        writeln!(record, "{path},sha256={digest},{}", bytes.len()).unwrap();
    }
    record.push_str("demo-1.0.dist-info/RECORD,,\n");
    let mut bytes = Vec::new();
    let mut archive = zip::ZipWriter::new(std::io::Cursor::new(&mut bytes));
    let options = zip::write::SimpleFileOptions::default();
    for (path, content) in entries {
        archive.start_file(path, options).unwrap();
        archive.write_all(content).unwrap();
    }
    archive.start_file("demo-1.0.dist-info/RECORD", options).unwrap();
    archive.write_all(record.as_bytes()).unwrap();
    archive.finish().unwrap();
    bytes
}

fn sdist_tar(root: &str, name: &str, version: &str) -> Vec<u8> {
    let mut bytes = Vec::new();
    {
        let encoder = flate2::write::GzEncoder::new(&mut bytes, flate2::Compression::default());
        let mut archive = tar::Builder::new(encoder);
        let metadata = format!("Metadata-Version: 2.2\nName: {name}\nVersion: {version}\n");
        let mut header = tar::Header::new_gnu();
        header.set_size(metadata.len() as u64);
        header.set_mode(0o644);
        header.set_cksum();
        archive
            .append_data(&mut header, format!("{root}-{version}/PKG-INFO"), metadata.as_bytes())
            .unwrap();
        let pyproject = b"[build-system]\nrequires = []\n";
        let mut header = tar::Header::new_gnu();
        header.set_size(pyproject.len() as u64);
        header.set_mode(0o644);
        header.set_cksum();
        archive
            .append_data(
                &mut header,
                format!("{root}-{version}/pyproject.toml"),
                pyproject.as_slice(),
            )
            .unwrap();
        archive.into_inner().unwrap().finish().unwrap();
    }
    bytes
}

fn sdist_zip(name: &str, version: &str) -> Vec<u8> {
    let mut bytes = Vec::new();
    let mut archive = zip::ZipWriter::new(std::io::Cursor::new(&mut bytes));
    archive
        .start_file(
            format!("{name}-{version}/PKG-INFO"),
            zip::write::SimpleFileOptions::default(),
        )
        .unwrap();
    write!(archive, "Metadata-Version: 2.2\nName: {name}\nVersion: {version}\n").unwrap();
    archive
        .start_file(
            format!("{name}-{version}/pyproject.toml"),
            zip::write::SimpleFileOptions::default(),
        )
        .unwrap();
    archive.write_all(b"[build-system]\nrequires = []\n").unwrap();
    archive.finish().unwrap();
    bytes
}

#[test]
fn test_upload_error_reason_formats_metadata_field_and_fallback() {
    assert_eq!(
        upload_error_reason(&UploadError::MetadataFieldMismatch {
            field: "Project-URL",
            metadata: "Homepage, https://example.test".to_owned(),
            form: "Source, https://example.test/src".to_owned(),
        }),
        "metadata field Project-URL is \"Homepage, https://example.test\", expected \"Source, https://example.test/src\""
    );
    assert_eq!(upload_error_reason(&UploadError::NotFileUpload), "NotFileUpload");
}

#[test]
fn import_dir_reports_imported_skipped_rejected_and_existing_files() {
    let dir = tempfile::tempdir().unwrap();
    let input = dir.path().join("input");
    std::fs::create_dir_all(input.join("nested")).unwrap();
    std::fs::write(input.join("nested/demo-1.0-py3-none-any.whl"), wheel()).unwrap();
    std::fs::write(input.join("notes.txt"), b"ignored").unwrap();
    std::fs::write(input.join("broken-1.0-py3-none-any.whl"), b"not a wheel").unwrap();
    let meta = MetaStore::open(dir.path().join("peryx.redb")).unwrap();
    let blobs = BlobStorage::filesystem(dir.path().join("blobs"));

    let mut first = Vec::new();
    import_dir(&meta, &blobs, "hosted", "root/hosted", &input, &mut first).unwrap();
    let first = String::from_utf8(first).unwrap();
    assert!(first.contains("imported\tnested/demo-1.0-py3-none-any.whl"));
    assert!(first.contains("skipped\tnotes.txt"));
    assert!(first.contains("rejected\tbroken-1.0-py3-none-any.whl"));

    let mut second = Vec::new();
    import_dir(&meta, &blobs, "hosted", "root/hosted", &input, &mut second).unwrap();
    assert!(String::from_utf8(second).unwrap().contains("already present"));
}

#[test]
fn upload_error_reasons_cover_metadata_validation() {
    for (error, expected) in [
        (UploadError::InvalidContent("bad".to_owned()), "invalid content"),
        (UploadError::InvalidMetadataUtf8, "not UTF-8"),
        (
            UploadError::MalformedMetadata(crate::MetadataError::MissingHeaderSeparator("bad".to_owned())),
            "malformed metadata",
        ),
        (UploadError::ConflictingLicenseFields, "both License"),
        (UploadError::MissingMetadataVersion, "missing Metadata-Version"),
        (
            UploadError::UnsupportedMetadataVersion("3".to_owned()),
            "invalid Metadata-Version",
        ),
        (
            UploadError::InvalidRequiresPython("bad".to_owned()),
            "invalid Requires-Python",
        ),
        (
            UploadError::MetadataNameMismatch {
                metadata: "a".to_owned(),
                form: "b".to_owned(),
            },
            "metadata name",
        ),
        (
            UploadError::MetadataVersionMismatch {
                metadata: "1".to_owned(),
                form: "2".to_owned(),
            },
            "metadata version",
        ),
        (
            UploadError::InvalidMetadataValue {
                field: "Name",
                value: "bad".to_owned(),
                reason: "is invalid",
            },
            "metadata Name value",
        ),
        (
            UploadError::InvalidLicenseFile {
                value: "../LICENSE".to_owned(),
                reason: "escapes",
            },
            "invalid License-File",
        ),
    ] {
        assert!(upload_error_reason(&error).contains(expected));
    }
}

#[test]
fn import_dir_reports_invalid_names_and_store_failures() {
    let dir = tempfile::tempdir().unwrap();
    let input = dir.path().join("input");
    std::fs::create_dir(&input).unwrap();
    std::fs::write(input.join("bad-.whl"), b"bad").unwrap();
    std::fs::write(input.join("demo-1.0-py3-none-any.whl"), wheel()).unwrap();
    let meta = MetaStore::open(dir.path().join("peryx.redb")).unwrap();
    meta.put_upload("hosted", "demo", "demo-1.0-py3-none-any.whl", b"bad")
        .unwrap();
    let blobs = BlobStorage::filesystem(dir.path().join("blobs"));
    let mut output = Vec::new();
    import_dir(&meta, &blobs, "hosted", "root/hosted", &input, &mut output).unwrap();
    let output = String::from_utf8(output).unwrap();
    assert!(output.contains("invalid distribution filename"));
    assert!(output.contains("rejected\tdemo-1.0-py3-none-any.whl"));
}

#[test]
fn import_dir_validates_tar_and_zip_sdist_identities() {
    let dir = tempfile::tempdir().unwrap();
    let input = dir.path().join("input");
    std::fs::create_dir(&input).unwrap();
    std::fs::write(input.join("other-1.0.tar.gz"), sdist_tar("other", "actual", "1.0")).unwrap();
    std::fs::write(input.join("demo-1.0.zip"), sdist_zip("demo", "1.0")).unwrap();
    let meta = MetaStore::open(dir.path().join("peryx.redb")).unwrap();
    let blobs = BlobStorage::filesystem(dir.path().join("blobs"));
    let mut output = Vec::new();
    import_dir(&meta, &blobs, "hosted", "root/hosted", &input, &mut output).unwrap();
    let output = String::from_utf8(output).unwrap();
    assert!(output.contains("different project or version"), "{output}");
    assert!(output.contains("imported\tdemo-1.0.zip"));
}

#[cfg(unix)]
#[test]
fn import_dir_skips_symlink_entries() {
    let dir = tempfile::tempdir().unwrap();
    let input = dir.path().join("input");
    std::fs::create_dir(&input).unwrap();
    std::fs::write(dir.path().join("outside.whl"), wheel()).unwrap();
    std::os::unix::fs::symlink(dir.path().join("outside.whl"), input.join("linked.whl")).unwrap();
    let meta = MetaStore::open(dir.path().join("peryx.redb")).unwrap();
    let blobs = BlobStorage::filesystem(dir.path().join("blobs"));
    let mut output = Vec::new();
    import_dir(&meta, &blobs, "hosted", "root/hosted", &input, &mut output).unwrap();
    assert!(
        String::from_utf8(output)
            .unwrap()
            .contains("imported=0 skipped=0 rejected=0")
    );
}
