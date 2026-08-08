//! Wheel structure: dist-info layout, WHEEL file, and entry points.

use super::support::*;
use rstest::rstest;

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
    for (dist_info, reason) in [
        (
            "other-1.0",
            "other-1.0.dist-info does not match expected flask-1.0.dist-info",
        ),
        (
            "flask-2.0",
            "flask-2.0.dist-info does not match expected flask-1.0.dist-info",
        ),
        ("flask", "flask.dist-info does not match expected flask-1.0.dist-info"),
    ] {
        assert_wheel_invalid(
            &wheel_zip(
                &[
                    ("Flask/__init__.py", init.as_slice()),
                    (&format!("{dist_info}.dist-info/METADATA"), metadata.as_slice()),
                    (&format!("{dist_info}.dist-info/WHEEL"), wheel.as_slice()),
                ],
                Some(&format!("{dist_info}.dist-info/RECORD")),
                None,
            ),
            reason,
        );
    }
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
fn test_prepare_accepts_unnormalized_dist_info() {
    let metadata = b"Metadata-Version: 2.1\nName: Flask\nVersion: 1.0\nRequires-Python: >=3.8\n";
    let wheel = b"Wheel-Version: 1.0\nGenerator: peryx-test\nRoot-Is-Purelib: true\nTag: py3-none-any\n";
    let init = b"VALUE = 1\n";
    let bytes = wheel_zip(
        &[
            ("Flask/__init__.py", init.as_slice()),
            ("Flask-1.0.dist-info/METADATA", metadata.as_slice()),
            ("Flask-1.0.dist-info/WHEEL", wheel.as_slice()),
        ],
        Some("Flask-1.0.dist-info/RECORD"),
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
fn test_prepare_accepts_wheel_carrying_declared_license_file() {
    let bytes = wheel_with_license_files(&["LICENSE"], &["LICENSE"]);
    let (_dir, staged) = staged_upload(&bytes);

    assert_eq!(
        prepare(staged_form(&bytes), staged, "root/hosted", 1000)
            .unwrap()
            .display_name,
        "Flask"
    );
}

#[rstest]
#[case::no_license_files(&[])]
#[case::other_license_file(&["NOTICE"])]
fn test_prepare_rejects_wheel_missing_declared_license_file(#[case] present: &[&str]) {
    let bytes = wheel_with_license_files(&["LICENSE"], present);
    let (_dir, staged) = staged_upload(&bytes);

    assert_eq!(
        prepare(staged_form(&bytes), staged, "root/hosted", 1000).unwrap_err(),
        UploadError::InvalidLicenseFile {
            value: "LICENSE".to_owned(),
            reason: "the archive does not carry the declared file",
        }
    );
}
