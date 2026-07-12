//! The upload-preparation harness and fixtures the validation tests share.

pub(super) use std::fmt::Write as _;
pub(super) use std::io::Write as _;

pub(super) use crate::CoreMetadata;
pub(super) use crate::DistributionFilenameError;
pub(super) use base64::Engine as _;
pub(super) use base64::engine::general_purpose::URL_SAFE_NO_PAD;
pub(super) use blake2::Blake2bVar;
pub(super) use blake2::digest::{FixedOutput as _, Update as _, VariableOutput as _};
pub(super) use flate2::Compression;
pub(super) use flate2::write::GzEncoder;
pub(super) use md5::Md5;
pub(super) use peryx_storage::blob::{BlobStore, Digest};
pub(super) use sha2::{Digest as _, Sha256, Sha384, Sha512};

pub(super) use crate::upload::{StagedUpload, UploadError, UploadForm, prepare};

pub(super) fn full_form(filename: &str) -> UploadForm {
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
        ..UploadForm::default()
    }
}

pub(super) fn staged_form(bytes: &[u8]) -> UploadForm {
    let mut form = full_form("Flask-1.0-py3-none-any.whl");
    form.sha256_digest = Some(Digest::of(bytes).as_str().to_owned());
    form
}

pub(super) fn staged_upload(bytes: &[u8]) -> (tempfile::TempDir, StagedUpload) {
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

pub(super) fn md5_hex(bytes: &[u8]) -> String {
    let mut hasher = Md5::default();
    hasher.update(bytes);
    hex(hasher.finalize_fixed().as_slice())
}

pub(super) fn wheel_metadata(name: &str, version: &str) -> Vec<u8> {
    wheel_metadata_bytes(
        format!("Metadata-Version: 2.1\nName: {name}\nVersion: {version}\nRequires-Python: >=3.8\n").as_bytes(),
    )
}

pub(super) fn wheel_metadata_bytes(metadata: &[u8]) -> Vec<u8> {
    let wheel = b"Wheel-Version: 1.0\nGenerator: peryx-test\nRoot-Is-Purelib: true\nTag: py3-none-any\n";
    wheel_zip(
        &[
            ("Flask/__init__.py", b"VALUE = 1\n".as_slice()),
            ("flask-1.0.dist-info/METADATA", metadata),
            ("flask-1.0.dist-info/WHEEL", wheel.as_slice()),
        ],
        Some("flask-1.0.dist-info/RECORD"),
        None,
    )
}

pub(super) fn wheel_without_metadata() -> Vec<u8> {
    let wheel = b"Wheel-Version: 1.0\nGenerator: peryx-test\nRoot-Is-Purelib: true\nTag: py3-none-any\n";
    wheel_zip(
        &[
            ("Flask/__init__.py", b"VALUE = 1\n".as_slice()),
            ("flask-1.0.dist-info/WHEEL", wheel.as_slice()),
        ],
        Some("flask-1.0.dist-info/RECORD"),
        None,
    )
}

pub(super) fn wheel_record_entries() -> [(&'static str, &'static [u8]); 3] {
    [
        ("Flask/__init__.py", b"VALUE = 1\n".as_slice()),
        (
            "flask-1.0.dist-info/METADATA",
            b"Metadata-Version: 2.1\nName: Flask\nVersion: 1.0\nRequires-Python: >=3.8\n".as_slice(),
        ),
        (
            "flask-1.0.dist-info/WHEEL",
            b"Wheel-Version: 1.0\nGenerator: peryx-test\nRoot-Is-Purelib: true\nTag: py3-none-any\n".as_slice(),
        ),
    ]
}

pub(super) fn record(entries: &[(&str, &[u8])], record_path: &str) -> String {
    let mut record = String::new();
    for (path, bytes) in entries {
        record.push_str(&record_line(path, bytes, bytes.len()));
    }
    writeln!(record, "{record_path},,").unwrap();
    record
}

pub(super) fn record_line(path: &str, bytes: &[u8], size: usize) -> String {
    record_line_with_algorithm(path, bytes, size, "sha256")
}

pub(super) fn record_with_self_size(entries: &[(&str, &[u8], &str)], record_path: &str) -> String {
    let mut record = String::new();
    for (path, bytes, algorithm) in entries {
        record.push_str(&record_line_with_algorithm(path, bytes, bytes.len(), algorithm));
    }
    let mut size = 0;
    loop {
        let candidate = format!("{record}{record_path},,{size}\n");
        if candidate.len() == size {
            return candidate;
        }
        size = candidate.len();
    }
}

pub(super) fn record_line_with_algorithm(path: &str, bytes: &[u8], size: usize, algorithm: &str) -> String {
    let digest = match algorithm {
        "sha256" => URL_SAFE_NO_PAD.encode(Sha256::digest(bytes)),
        "sha384" => URL_SAFE_NO_PAD.encode(Sha384::digest(bytes)),
        "sha512" => URL_SAFE_NO_PAD.encode(Sha512::digest(bytes)),
        _ => unreachable!("unsupported test hash algorithm"),
    };
    format!("{path},{algorithm}={digest},{size}\n")
}

pub(super) fn wheel_with_wheel_file(wheel: &[u8]) -> Vec<u8> {
    wheel_zip(
        &[
            ("Flask/__init__.py", b"VALUE = 1\n".as_slice()),
            (
                "flask-1.0.dist-info/METADATA",
                b"Metadata-Version: 2.1\nName: Flask\nVersion: 1.0\nRequires-Python: >=3.8\n".as_slice(),
            ),
            ("flask-1.0.dist-info/WHEEL", wheel),
        ],
        Some("flask-1.0.dist-info/RECORD"),
        None,
    )
}

pub(super) fn wheel_zip(entries: &[(&str, &[u8])], record_path: Option<&str>, record_body: Option<String>) -> Vec<u8> {
    wheel_zip_with_directories(entries, &[], record_path, record_body)
}

pub(super) fn wheel_zip_with_directories(
    entries: &[(&str, &[u8])],
    directories: &[&str],
    record_path: Option<&str>,
    record_body: Option<String>,
) -> Vec<u8> {
    let mut buf = Vec::new();
    {
        let mut zip = zip::ZipWriter::new(std::io::Cursor::new(&mut buf));
        let options = zip::write::SimpleFileOptions::default().compression_method(zip::CompressionMethod::Stored);
        for path in directories {
            zip.add_directory(*path, options).unwrap();
        }
        for (path, bytes) in entries {
            zip.start_file(path, options).unwrap();
            zip.write_all(bytes).unwrap();
        }
        if let Some(record_path) = record_path {
            zip.start_file(record_path, options).unwrap();
            zip.write_all(record_body.unwrap_or_else(|| record(entries, record_path)).as_bytes())
                .unwrap();
        }
        zip.finish().unwrap();
    }
    buf
}

pub(super) fn assert_wheel_invalid(bytes: &[u8], expected: &str) {
    assert_wheel_invalid_for("Flask-1.0-py3-none-any.whl", bytes, expected);
}

pub(super) fn assert_wheel_invalid_for(filename: &str, bytes: &[u8], expected: &str) {
    let (_dir, staged) = staged_upload(bytes);
    let err = prepare(full_form(filename), staged, "root/hosted", 1000)
        .expect_err(&format!("upload unexpectedly succeeded; expected {expected:?}"));
    assert!(
        matches!(err, UploadError::InvalidContent(ref message) if message.contains(expected)),
        "expected {expected:?}, got {err:?}"
    );
}

pub(super) fn sdist_metadata(name: &str, version: &str, requires_python: &str) -> Vec<u8> {
    let mut buf = Vec::new();
    {
        let encoder = GzEncoder::new(&mut buf, Compression::default());
        let mut tar = tar::Builder::new(encoder);
        let content =
            format!("Metadata-Version: 2.2\nName: {name}\nVersion: {version}\nRequires-Python: {requires_python}\n");
        let mut header = tar::Header::new_gnu();
        header.set_size(content.len() as u64);
        header.set_mode(0o644);
        header.set_cksum();
        tar.append_data(&mut header, "Flask-1.0/PKG-INFO", content.as_bytes())
            .unwrap();
        let pyproject = b"[build-system]\nrequires = []\n";
        let mut header = tar::Header::new_gnu();
        header.set_size(pyproject.len() as u64);
        header.set_mode(0o644);
        header.set_cksum();
        tar.append_data(&mut header, "Flask-1.0/pyproject.toml", pyproject.as_slice())
            .unwrap();
        tar.finish().unwrap();
    }
    buf
}

pub(super) fn hex(bytes: &[u8]) -> String {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    let mut out = String::with_capacity(bytes.len() * 2);
    for &byte in bytes {
        out.push(HEX[(byte >> 4) as usize] as char);
        out.push(HEX[(byte & 0x0f) as usize] as char);
    }
    out
}
