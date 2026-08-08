//! RECORD: the manifest of hashes and sizes, and the rules it must satisfy.

use super::support::*;

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
