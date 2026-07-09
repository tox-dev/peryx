use std::fmt::Write as _;
use std::io::Write as _;

use crate::CoreMetadata;
use crate::DistributionFilenameError;
use base64::Engine as _;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use blake2::Blake2bVar;
use blake2::digest::{Update as _, VariableOutput as _};
use flate2::Compression;
use flate2::write::GzEncoder;
use peryx_storage::blob::{BlobStore, Digest};
use sha2::{Digest as _, Sha256, Sha384, Sha512};

use crate::upload::{StagedUpload, UploadError, UploadForm, prepare};

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
        ..UploadForm::default()
    }
}

#[test]
fn test_prepare_builds_content_addressed_record() {
    let wheel = wheel_metadata("Flask", "1.0");
    let (_dir, staged) = staged_upload(&wheel);

    let prepared = prepare(staged_form(&wheel), staged, "root/hosted", 1000).unwrap();
    let digest = Digest::of(&wheel);

    assert_eq!(prepared.normalized, "flask");
    assert_eq!(prepared.display_name, "Flask");
    assert_eq!(prepared.digest, digest);
    assert_eq!(prepared.record.version, "1.0");
    assert_eq!(
        prepared.record.file.url,
        format!("/root/hosted/files/{}/Flask-1.0-py3-none-any.whl", digest.as_str())
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
        prepared.metadata.as_slice(),
        b"Metadata-Version: 2.1\nName: Flask\nVersion: 1.0\nRequires-Python: >=3.8\n"
    );
}

#[test]
fn test_prepare_accepts_matching_declared_digests() {
    let wheel = wheel_metadata("Flask", "1.0");
    let (_dir, staged) = staged_upload(&wheel);
    let mut form = staged_form(&wheel);
    form.sha256_digest = Some(Digest::of(&wheel).as_str().to_owned());
    form.blake2_256_digest = Some(staged.blake2_256.clone());

    assert!(prepare(form, staged, "root/hosted", 1000).is_ok());
}

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
fn test_prepare_rejects_wrong_action() {
    let wheel = wheel_metadata("Flask", "1.0");
    let (_dir, staged) = staged_upload(&wheel);
    let mut form = staged_form(&wheel);
    form.action = Some("submit".to_owned());

    assert_eq!(
        prepare(form, staged, "root/hosted", 1000).unwrap_err(),
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
        assert_eq!(prepare(form, staged, "root/hosted", 1000).unwrap_err(), expected);
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

        assert_eq!(prepare(form, staged, "root/hosted", 1000).unwrap_err(), expected);
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

        assert_eq!(prepare(form, staged, "root/hosted", 1000).unwrap_err(), expected);
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

        assert_eq!(prepare(form, staged, "root/hosted", 1000).unwrap_err(), expected);
    }
}

#[test]
fn test_prepare_rejects_filetype_mismatch() {
    let wheel = wheel_metadata("Flask", "1.0");
    let (_dir, staged) = staged_upload(&wheel);
    let mut form = staged_form(&wheel);
    form.filetype = Some("sdist".to_owned());

    assert_eq!(
        prepare(form, staged, "root/hosted", 1000).unwrap_err(),
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
fn test_prepare_rejects_invalid_wheel_structure() {
    let metadata = b"Metadata-Version: 2.1\nName: Flask\nVersion: 1.0\nRequires-Python: >=3.8\n";
    let wheel = b"Wheel-Version: 1.0\nGenerator: peryx-test\nRoot-Is-Purelib: true\nTag: py3-none-any\n";
    let init = b"VALUE = 1\n";

    assert_wheel_invalid(
        &wheel_zip(
            &[
                ("Flask/__init__.py", init.as_slice()),
                ("flask-1.0.dist-info/METADATA", metadata.as_slice()),
            ],
            Some("flask-1.0.dist-info/RECORD"),
            None,
        ),
        "missing required flask-1.0.dist-info/WHEEL",
    );
    assert_wheel_invalid(
        &wheel_zip(
            &[
                ("Flask/__init__.py", init.as_slice()),
                ("flask-1.0.dist-info/METADATA", metadata.as_slice()),
                ("flask-1.0.dist-info/WHEEL", wheel.as_slice()),
            ],
            None,
            None,
        ),
        "missing required flask-1.0.dist-info/RECORD",
    );
    assert_wheel_invalid(
        &wheel_zip(
            &[
                ("Flask/__init__.py", init.as_slice()),
                ("Flask-1.0.dist-info/METADATA", metadata.as_slice()),
                ("Flask-1.0.dist-info/WHEEL", wheel.as_slice()),
            ],
            Some("Flask-1.0.dist-info/RECORD"),
            None,
        ),
        ".dist-info directory Flask-1.0.dist-info does not match expected flask-1.0.dist-info",
    );
    assert_wheel_invalid(
        &wheel_zip(
            &[
                ("Flask/__init__.py", init.as_slice()),
                ("flask-1.0.dist-info/METADATA", metadata.as_slice()),
                ("flask-1.0.dist-info/WHEEL", wheel.as_slice()),
                ("other-1.0.dist-info/METADATA", metadata.as_slice()),
            ],
            Some("flask-1.0.dist-info/RECORD"),
            None,
        ),
        "multiple .dist-info directories found",
    );

    assert_wheel_invalid(
        &wheel_zip(
            &[
                ("Flask/__init__.py", init.as_slice()),
                ("flask-1.0/METADATA", metadata.as_slice()),
                ("flask-1.0/WHEEL", wheel.as_slice()),
            ],
            Some("flask-1.0/RECORD"),
            None,
        ),
        "missing .dist-info directory",
    );
}

#[test]
fn test_prepare_accepts_wheel_with_directory_entries() {
    let metadata = b"Metadata-Version: 2.1\nName: Flask\nVersion: 1.0\nRequires-Python: >=3.8\n";
    let wheel = b"Wheel-Version: 1.0\nGenerator: peryx-test\nRoot-Is-Purelib: true\nTag: py3-none-any\n";
    let init = b"VALUE = 1\n";
    let bytes = wheel_zip_with_directories(
        &[
            ("Flask/__init__.py", init.as_slice()),
            ("flask-1.0.dist-info/METADATA", metadata.as_slice()),
            ("flask-1.0.dist-info/WHEEL", wheel.as_slice()),
        ],
        &["flask-1.0.dist-info/"],
        Some("flask-1.0.dist-info/RECORD"),
        None,
    );
    let (_dir, staged) = staged_upload(&bytes);

    let prepared = prepare(staged_form(&bytes), staged, "root/hosted", 1000).unwrap();

    assert_eq!(prepared.metadata.as_slice(), metadata);
}

#[test]
fn test_prepare_rejects_invalid_wheel_file() {
    assert_wheel_invalid(&wheel_with_wheel_file(b"\xff"), "WHEEL is not valid UTF-8");
    assert_wheel_invalid(
        &wheel_with_wheel_file(b"Generator: peryx-test\nRoot-Is-Purelib: true\nTag: py3-none-any\n"),
        "WHEEL must contain exactly one Wheel-Version field",
    );
    assert_wheel_invalid(
        &wheel_with_wheel_file(
            b"Wheel-Version: 2.0\nGenerator: peryx-test\nRoot-Is-Purelib: true\nTag: py3-none-any\n",
        ),
        "Wheel-Version 2.0 is newer than supported",
    );
    assert_wheel_invalid(
        &wheel_with_wheel_file(
            b"Wheel-Version: 1.x\nGenerator: peryx-test\nRoot-Is-Purelib: true\nTag: py3-none-any\n",
        ),
        "invalid Wheel-Version \"1.x\"",
    );
    assert_wheel_invalid(
        &wheel_with_wheel_file(
            b"Wheel-Version: 999999999999999999999999999999999999999999999999999999999999999.0\nGenerator: peryx-test\nRoot-Is-Purelib: true\nTag: py3-none-any\n",
        ),
        "invalid Wheel-Version \"999999999999999999999999999999999999999999999999999999999999999.0\"",
    );
    assert_wheel_invalid(
        &wheel_with_wheel_file(b"Wheel-Version: 1\nGenerator: peryx-test\nRoot-Is-Purelib: true\nTag: py3-none-any\n"),
        "invalid Wheel-Version \"1\"",
    );
    assert_wheel_invalid(
        &wheel_with_wheel_file(b"Wheel-Version: 1.0\nGenerator: peryx-test\nTag: py3-none-any\n"),
        "WHEEL must contain exactly one Root-Is-Purelib field",
    );
    assert_wheel_invalid(
        &wheel_with_wheel_file(
            b"Wheel-Version: 1.0\nGenerator: peryx-test\nRoot-Is-Purelib: maybe\nTag: py3-none-any\n",
        ),
        "Root-Is-Purelib has invalid value",
    );
    assert_wheel_invalid(
        &wheel_with_wheel_file(b"Wheel-Version: 1.0\nGenerator: peryx-test\nRoot-Is-Purelib: true\n"),
        "WHEEL must contain at least one Tag field",
    );
    assert_wheel_invalid(
        &wheel_with_wheel_file(b"Wheel-Version: 1.0\nGenerator: peryx-test\nRoot-Is-Purelib: true\nTag: py3-none\n"),
        "invalid WHEEL Tag \"py3-none\"",
    );
    assert_wheel_invalid(
        &wheel_with_wheel_file(
            b"Wheel-Version: 1.0\nGenerator: peryx-test\nRoot-Is-Purelib: true\nTag: py3-no/ne-any\n",
        ),
        "invalid WHEEL Tag \"py3-no/ne-any\"",
    );
    assert_wheel_invalid(
        &wheel_with_wheel_file(
            b"Wheel-Version: 1.0\nGenerator: peryx-test\nRoot-Is-Purelib: true\nTag: py2-none-any\n",
        ),
        "WHEEL Tag fields do not match filename tags",
    );
    assert_wheel_invalid(
        &wheel_with_wheel_file(
            b"Wheel-Version: 1.0\nGenerator: peryx-test\nRoot-Is-Purelib: true\nTag: py3-none-any\nBuild: 1\n",
        ),
        "filename has no build tag",
    );
    assert_wheel_invalid(
        &wheel_with_wheel_file(
            b"Wheel-Version: 1.0\nGenerator: peryx-test\nRoot-Is-Purelib: true\nTag: py3-none-any\nBuild: 1\nBuild: 2\n",
        ),
        "WHEEL must contain at most one Build field",
    );
    assert_wheel_invalid_for(
        "Flask-1.0-1-py3-none-any.whl",
        &wheel_with_wheel_file(
            b"Wheel-Version: 1.0\nGenerator: peryx-test\nRoot-Is-Purelib: true\nTag: py3-none-any\n",
        ),
        "missing Build field",
    );
    assert_wheel_invalid_for(
        "Flask-1.0-1-py3-none-any.whl",
        &wheel_with_wheel_file(
            b"Wheel-Version: 1.0\nGenerator: peryx-test\nRoot-Is-Purelib: true\nTag: py3-none-any\nBuild: 2\n",
        ),
        "does not match filename build tag",
    );
}

#[test]
fn test_prepare_rejects_record_missing_or_mismatched_file_entries() {
    let entries = wheel_record_entries();
    let init = entries[0].1;
    assert_wheel_invalid(
        &wheel_zip(
            &entries,
            Some("flask-1.0.dist-info/RECORD"),
            Some(record(&entries[1..], "flask-1.0.dist-info/RECORD")),
        ),
        "RECORD is missing entry for Flask/__init__.py",
    );
    assert_wheel_invalid(
        &wheel_zip(
            &entries,
            Some("flask-1.0.dist-info/RECORD"),
            Some(format!(
                "Flask/__init__.py,sha256=AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA,{}\n{}",
                init.len(),
                record(&entries[1..], "flask-1.0.dist-info/RECORD")
            )),
        ),
        "RECORD hash mismatch for Flask/__init__.py",
    );
    assert_wheel_invalid(
        &wheel_zip(
            &entries,
            Some("flask-1.0.dist-info/RECORD"),
            Some(record(&entries, "flask-1.0.dist-info/RECORD").replace(
                &record_line("Flask/__init__.py", init, init.len()),
                &record_line("Flask/__init__.py", init, 999),
            )),
        ),
        "has size 999, but archive member is 10 bytes",
    );
}

#[test]
fn test_prepare_rejects_record_csv_and_duplicate_rows() {
    let entries = wheel_record_entries();
    let init = entries[0].1;

    assert_wheel_invalid(
        &wheel_zip(
            &entries,
            Some("flask-1.0.dist-info/RECORD"),
            Some("Flask/__init__.py,sha256=x\n".to_owned()),
        ),
        "RECORD rows must contain path, hash, and size",
    );
    assert_wheel_invalid(
        &wheel_zip(
            &entries,
            Some("flask-1.0.dist-info/RECORD"),
            Some(format!(
                "{}bad,row\n",
                record_line("Flask/__init__.py", init, init.len())
            )),
        ),
        "invalid RECORD CSV",
    );
    assert_wheel_invalid(
        &wheel_zip(
            &entries,
            Some("flask-1.0.dist-info/RECORD"),
            Some(format!(
                "{}{}{}",
                record_line("Flask/__init__.py", init, init.len()),
                record_line("Flask/__init__.py", init, init.len()),
                record(&entries[1..], "flask-1.0.dist-info/RECORD")
            )),
        ),
        "RECORD contains duplicate entry for Flask/__init__.py",
    );
    assert_wheel_invalid(
        &wheel_zip(&entries, Some("flask-1.0.dist-info/RECORD"), Some(String::new())),
        "RECORD is empty",
    );
}

#[test]
fn test_prepare_rejects_record_membership_rules() {
    let entries = wheel_record_entries();
    let init = entries[0].1;

    assert_wheel_invalid(
        &wheel_zip(
            &[
                ("Flask/__init__.py", init),
                ("flask-1.0.dist-info/METADATA", entries[1].1),
                ("flask-1.0.dist-info/WHEEL", entries[2].1),
                ("flask-1.0.dist-info/RECORD.jws", b"signature".as_slice()),
            ],
            Some("flask-1.0.dist-info/RECORD"),
            Some(format!(
                "{}{}",
                record_line("flask-1.0.dist-info/RECORD.jws", b"signature", b"signature".len()),
                record(&entries, "flask-1.0.dist-info/RECORD")
            )),
        ),
        "deprecated signature file flask-1.0.dist-info/RECORD.jws must not be listed",
    );
    assert_wheel_invalid(
        &wheel_zip(
            &entries,
            Some("flask-1.0.dist-info/RECORD"),
            Some(format!(
                "{}{}",
                record_line("missing.py", b"", 0),
                record(&entries, "flask-1.0.dist-info/RECORD")
            )),
        ),
        "RECORD entry missing.py is not present in the archive",
    );
    assert_wheel_invalid(
        &wheel_zip(
            &entries,
            Some("flask-1.0.dist-info/RECORD"),
            Some(format!(
                "{}flask-1.0.dist-info/RECORD,sha256={},\n",
                record(&entries, "flask-1.0.dist-info/RECORD").replace("flask-1.0.dist-info/RECORD,,\n", ""),
                URL_SAFE_NO_PAD.encode(Sha256::digest(b"record"))
            )),
        ),
        "RECORD must not contain a hash for itself",
    );
    assert_wheel_invalid(
        &wheel_zip(
            &entries,
            Some("flask-1.0.dist-info/RECORD"),
            Some(record(&entries, "flask-1.0.dist-info/RECORD").replace("flask-1.0.dist-info/RECORD,,\n", "")),
        ),
        "RECORD is missing entry for flask-1.0.dist-info/RECORD",
    );
}

#[test]
fn test_prepare_accepts_record_entry_without_size() {
    let entries = wheel_record_entries();
    let init = entries[0].1;
    let bytes = wheel_zip(
        &entries,
        Some("flask-1.0.dist-info/RECORD"),
        Some(record(&entries, "flask-1.0.dist-info/RECORD").replace(
            &record_line("Flask/__init__.py", init, init.len()),
            &format!(
                "Flask/__init__.py,sha256={},\n",
                URL_SAFE_NO_PAD.encode(Sha256::digest(init))
            ),
        )),
    );
    let (_dir, staged) = staged_upload(&bytes);

    let prepared = prepare(staged_form(&bytes), staged, "root/hosted", 1000).unwrap();

    assert_eq!(prepared.metadata.as_slice(), entries[1].1);
}

#[test]
fn test_prepare_rejects_record_hash_and_size_fields() {
    let entries = wheel_record_entries();
    let init = entries[0].1;

    assert_wheel_invalid(
        &wheel_zip(
            &entries,
            Some("flask-1.0.dist-info/RECORD"),
            Some(record(&entries, "flask-1.0.dist-info/RECORD").replace(
                &record_line("Flask/__init__.py", init, init.len()),
                &format!(
                    "Flask/__init__.py,sha256={},NaN\n",
                    URL_SAFE_NO_PAD.encode(Sha256::digest(init))
                ),
            )),
        ),
        "RECORD entry Flask/__init__.py has invalid size \"NaN\"",
    );
    assert_wheel_invalid(
        &wheel_zip(
            &entries,
            Some("flask-1.0.dist-info/RECORD"),
            Some(record(&entries, "flask-1.0.dist-info/RECORD").replace(
                &record_line("Flask/__init__.py", init, init.len()),
                &format!(
                    "Flask/__init__.py,sha256{},{}\n",
                    URL_SAFE_NO_PAD.encode(Sha256::digest(init)),
                    init.len()
                ),
            )),
        ),
        "RECORD entry Flask/__init__.py is missing hash algorithm",
    );
    assert_wheel_invalid(
        &wheel_zip(
            &entries,
            Some("flask-1.0.dist-info/RECORD"),
            Some(record(&entries, "flask-1.0.dist-info/RECORD").replace(
                &record_line("Flask/__init__.py", init, init.len()),
                &format!("Flask/__init__.py,sha256=,{}\n", init.len()),
            )),
        ),
        "RECORD entry Flask/__init__.py is missing hash value",
    );
    assert_wheel_invalid(
        &wheel_zip(
            &entries,
            Some("flask-1.0.dist-info/RECORD"),
            Some(record(&entries, "flask-1.0.dist-info/RECORD").replace(
                &record_line("Flask/__init__.py", init, init.len()),
                &format!("Flask/__init__.py,sha256=!,{}\n", init.len()),
            )),
        ),
        "has invalid base64 hash",
    );
    assert_wheel_invalid(
        &wheel_zip(
            &entries,
            Some("flask-1.0.dist-info/RECORD"),
            Some(record(&entries, "flask-1.0.dist-info/RECORD").replace(
                &record_line("Flask/__init__.py", init, init.len()),
                &format!(
                    "Flask/__init__.py,sha224={},{}\n",
                    URL_SAFE_NO_PAD.encode(Sha256::digest(init)),
                    init.len()
                ),
            )),
        ),
        "uses unsupported hash algorithm \"sha224\"",
    );
}

#[test]
fn test_prepare_accepts_record_self_size_and_stronger_hashes() {
    let metadata = b"Metadata-Version: 2.1\nName: Flask\nVersion: 1.0\nRequires-Python: >=3.8\n";
    let wheel = b"Wheel-Version: 1.0\nGenerator: peryx-test\nRoot-Is-Purelib: true\nTag: py3-none-any\n";
    let init = b"VALUE = 1\n";
    let entries = [
        ("Flask/__init__.py", init.as_slice()),
        ("flask-1.0.dist-info/METADATA", metadata.as_slice()),
        ("flask-1.0.dist-info/WHEEL", wheel.as_slice()),
    ];
    let bytes = wheel_zip(
        &entries,
        Some("flask-1.0.dist-info/RECORD"),
        Some(record_with_self_size(
            &[
                ("Flask/__init__.py", init.as_slice(), "sha384"),
                ("flask-1.0.dist-info/METADATA", metadata.as_slice(), "sha512"),
                ("flask-1.0.dist-info/WHEEL", wheel.as_slice(), "sha256"),
            ],
            "flask-1.0.dist-info/RECORD",
        )),
    );
    let (_dir, staged) = staged_upload(&bytes);

    let prepared = prepare(staged_form(&bytes), staged, "root/hosted", 1000).unwrap();

    assert_eq!(prepared.metadata.as_slice(), metadata);
}

#[test]
fn test_prepare_rejects_invalid_entry_points() {
    let metadata = b"Metadata-Version: 2.1\nName: Flask\nVersion: 1.0\nRequires-Python: >=3.8\n";
    let wheel = b"Wheel-Version: 1.0\nGenerator: peryx-test\nRoot-Is-Purelib: true\nTag: py3-none-any\n";
    let init = b"VALUE = 1\n";
    for (entry_points, expected) in [
        (&b"\xff"[..], "entry_points.txt is not valid UTF-8"),
        (b"  continued\n".as_slice(), "continuation on line 1 has no section"),
        (b"[]\nflask = flask:main\n".as_slice(), "empty section on line 1"),
        (
            b"[console_scripts]\nflask flask:main\n".as_slice(),
            "line 2 is not a key=value entry",
        ),
        (
            b"[console_scripts]\n= flask:main\n".as_slice(),
            "line 2 has an empty entry point name",
        ),
        (b"flask = flask:main\n".as_slice(), "entry on line 1 has no section"),
        (
            b"[console_scripts]\n../flask = flask:main\n".as_slice(),
            "entry_points.txt has invalid entry point name",
        ),
    ] {
        assert_wheel_invalid(
            &wheel_zip(
                &[
                    ("Flask/__init__.py", init.as_slice()),
                    ("flask-1.0.dist-info/METADATA", metadata.as_slice()),
                    ("flask-1.0.dist-info/WHEEL", wheel.as_slice()),
                    ("flask-1.0.dist-info/entry_points.txt", entry_points),
                ],
                Some("flask-1.0.dist-info/RECORD"),
                None,
            ),
            expected,
        );
    }

    let bytes = wheel_zip(
        &[
            ("Flask/__init__.py", init.as_slice()),
            ("flask-1.0.dist-info/METADATA", metadata.as_slice()),
            ("flask-1.0.dist-info/WHEEL", wheel.as_slice()),
            (
                "flask-1.0.dist-info/entry_points.txt",
                b"# generated\n[console_scripts]\nflask = flask:main\n  :continued\n".as_slice(),
            ),
        ],
        Some("flask-1.0.dist-info/RECORD"),
        None,
    );
    let (_dir, staged) = staged_upload(&bytes);

    assert!(prepare(staged_form(&bytes), staged, "root/hosted", 1000).is_ok());
}

#[test]
fn test_prepare_rejects_large_wheel_validation_members() {
    let metadata = b"Metadata-Version: 2.1\nName: Flask\nVersion: 1.0\nRequires-Python: >=3.8\n";
    let wheel = b"Wheel-Version: 1.0\nGenerator: peryx-test\nRoot-Is-Purelib: true\nTag: py3-none-any\n";
    let init = b"VALUE = 1\n";
    let entry_points = vec![b'a'; 1024 * 1024 + 1];

    assert_wheel_invalid(
        &wheel_zip(
            &[
                ("Flask/__init__.py", init.as_slice()),
                ("flask-1.0.dist-info/METADATA", metadata.as_slice()),
                ("flask-1.0.dist-info/WHEEL", wheel.as_slice()),
                ("flask-1.0.dist-info/entry_points.txt", entry_points.as_slice()),
            ],
            Some("flask-1.0.dist-info/RECORD"),
            None,
        ),
        "flask-1.0.dist-info/entry_points.txt is 1048577 bytes, above the upload validation limit of 1048576 bytes",
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
            prepare(full_form("Flask-1.0-py3-none-any.whl"), staged, "root/hosted", 1000).unwrap_err(),
            expected
        );
    }
}

#[test]
fn test_prepare_rejects_metadata_field_mismatches() {
    for (configure, metadata, expected) in [
        (
            (|form: &mut UploadForm| form.metadata_version = Some("2.0".to_owned())) as fn(&mut UploadForm),
            "Metadata-Version: 2.1\nName: Flask\nVersion: 1.0\nRequires-Python: >=3.8\n",
            UploadError::MetadataFieldMismatch {
                field: "Metadata-Version",
                metadata: "2.1".to_owned(),
                form: "2.0".to_owned(),
            },
        ),
        (
            |form| form.requires_python = Some(">=3.9".to_owned()),
            "Metadata-Version: 2.1\nName: Flask\nVersion: 1.0\nRequires-Python: >=3.8\n",
            UploadError::MetadataFieldMismatch {
                field: "Requires-Python",
                metadata: ">=3.8".to_owned(),
                form: ">=3.9".to_owned(),
            },
        ),
        (
            |form| form.license_expression = Some("Apache-2.0".to_owned()),
            "Metadata-Version: 2.1\nName: Flask\nVersion: 1.0\nRequires-Python: >=3.8\nLicense-Expression: MIT\n",
            UploadError::MetadataFieldMismatch {
                field: "License-Expression",
                metadata: "MIT".to_owned(),
                form: "Apache-2.0".to_owned(),
            },
        ),
        (
            |form| form.license_files.push("NOTICE".to_owned()),
            "Metadata-Version: 2.1\nName: Flask\nVersion: 1.0\nRequires-Python: >=3.8\nLicense-File: LICENSE\n",
            UploadError::MetadataFieldMismatch {
                field: "License-File",
                metadata: "LICENSE".to_owned(),
                form: "NOTICE".to_owned(),
            },
        ),
        (
            |form| form.provides_extra.push("dev".to_owned()),
            "Metadata-Version: 2.1\nName: Flask\nVersion: 1.0\nRequires-Python: >=3.8\nProvides-Extra: cli\n",
            UploadError::MetadataFieldMismatch {
                field: "Provides-Extra",
                metadata: "cli".to_owned(),
                form: "dev".to_owned(),
            },
        ),
        (
            |form| {
                form.project_urls.push("Docs, https://example.test/docs".to_owned());
                form.home_page = Some("https://example.test/home".to_owned());
            },
            "Metadata-Version: 2.1\nName: Flask\nVersion: 1.0\nRequires-Python: >=3.8\nProject-URL: Source, https://example.test/source\n",
            UploadError::MetadataFieldMismatch {
                field: "Project-URL",
                metadata: "Source, https://example.test/source".to_owned(),
                form: "Docs, https://example.test/docs; Homepage, https://example.test/home".to_owned(),
            },
        ),
    ] {
        let bytes = wheel_metadata_bytes(metadata.as_bytes());
        let (_dir, staged) = staged_upload(&bytes);
        let mut form = staged_form(&bytes);
        configure(&mut form);

        assert_eq!(prepare(form, staged, "root/hosted", 1000).unwrap_err(), expected);
    }
}

#[test]
fn test_prepare_accepts_matching_metadata_form_fields() {
    let bytes = wheel_metadata_bytes(
        b"Metadata-Version: 2.1\nName: Flask\nVersion: 1.0\nRequires-Python: >=3.8\nLicense: MIT\nLicense-Expression: MIT\nLicense-File: LICENSE\nProvides-Extra: cli\nProject-URL: Source, https://example.test/source\nHome-Page: https://example.test/home\n",
    );
    let (_dir, staged) = staged_upload(&bytes);
    let mut form = staged_form(&bytes);
    form.metadata_version = Some("2.1".to_owned());
    form.license = Some("MIT".to_owned());
    form.license_expression = Some("MIT".to_owned());
    form.license_files.push("LICENSE".to_owned());
    form.provides_extra.push("cli".to_owned());
    form.project_urls.push("Source, https://example.test/source".to_owned());
    form.home_page = Some("https://example.test/home".to_owned());

    let prepared = prepare(form, staged, "root/hosted", 1000).unwrap();

    assert_eq!(prepared.display_name, "Flask");
}

#[test]
fn test_prepare_rejects_invalid_requires_python_and_clock() {
    let wheel = wheel_metadata("Flask", "1.0");
    let (_dir, staged) = staged_upload(&wheel);
    let mut form = staged_form(&wheel);
    form.requires_python = Some("=>3".to_owned());
    assert_eq!(
        prepare(form, staged, "root/hosted", 1000).unwrap_err(),
        UploadError::InvalidRequiresPython("=>3".to_owned())
    );

    let (_dir, staged) = staged_upload(&wheel);
    assert_eq!(
        prepare(staged_form(&wheel), staged, "root/hosted", i64::MAX).unwrap_err(),
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
            prepare(form, staged, "root/hosted", 1000).unwrap_err(),
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

fn wheel_without_metadata() -> Vec<u8> {
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

fn wheel_record_entries() -> [(&'static str, &'static [u8]); 3] {
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

fn record(entries: &[(&str, &[u8])], record_path: &str) -> String {
    let mut record = String::new();
    for (path, bytes) in entries {
        record.push_str(&record_line(path, bytes, bytes.len()));
    }
    writeln!(record, "{record_path},,").unwrap();
    record
}

fn record_line(path: &str, bytes: &[u8], size: usize) -> String {
    record_line_with_algorithm(path, bytes, size, "sha256")
}

fn record_with_self_size(entries: &[(&str, &[u8], &str)], record_path: &str) -> String {
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

fn record_line_with_algorithm(path: &str, bytes: &[u8], size: usize, algorithm: &str) -> String {
    let digest = match algorithm {
        "sha256" => URL_SAFE_NO_PAD.encode(Sha256::digest(bytes)),
        "sha384" => URL_SAFE_NO_PAD.encode(Sha384::digest(bytes)),
        "sha512" => URL_SAFE_NO_PAD.encode(Sha512::digest(bytes)),
        _ => unreachable!("unsupported test hash algorithm"),
    };
    format!("{path},{algorithm}={digest},{size}\n")
}

fn wheel_with_wheel_file(wheel: &[u8]) -> Vec<u8> {
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

fn wheel_zip(entries: &[(&str, &[u8])], record_path: Option<&str>, record_body: Option<String>) -> Vec<u8> {
    wheel_zip_with_directories(entries, &[], record_path, record_body)
}

fn wheel_zip_with_directories(
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

fn assert_wheel_invalid(bytes: &[u8], expected: &str) {
    assert_wheel_invalid_for("Flask-1.0-py3-none-any.whl", bytes, expected);
}

fn assert_wheel_invalid_for(filename: &str, bytes: &[u8], expected: &str) {
    let (_dir, staged) = staged_upload(bytes);
    let err = prepare(full_form(filename), staged, "root/hosted", 1000)
        .expect_err(&format!("upload unexpectedly succeeded; expected {expected:?}"));
    assert!(
        matches!(err, UploadError::InvalidContent(ref message) if message.contains(expected)),
        "expected {expected:?}, got {err:?}"
    );
}

fn sdist_metadata(name: &str, version: &str, requires_python: &str) -> Vec<u8> {
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

fn hex(bytes: &[u8]) -> String {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    let mut out = String::with_capacity(bytes.len() * 2);
    for &byte in bytes {
        out.push(HEX[(byte >> 4) as usize] as char);
        out.push(HEX[(byte & 0x0f) as usize] as char);
    }
    out
}
