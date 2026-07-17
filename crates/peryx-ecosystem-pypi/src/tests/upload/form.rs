//! The multipart form itself: action, identity, declared digest and filename.

use super::support::*;

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
fn test_prepare_accepts_only_md5_for_a_large_upload() {
    let padding = vec![b'x'; 128 * 1024 + 1];
    let [module, metadata, wheel_file] = wheel_record_entries();
    let wheel = wheel_zip(
        &[module, metadata, wheel_file, ("Flask/padding.bin", &padding)],
        Some("flask-1.0.dist-info/RECORD"),
        None,
    );
    let (_dir, staged) = staged_upload(&wheel);
    let mut form = staged_form(&wheel);
    form.sha256_digest = None;
    form.md5_digest = Some(md5_hex(&wheel));

    assert!(prepare(form, staged, "root/hosted", 1000).is_ok());
}
#[test]
fn test_prepare_rejects_unreadable_content_for_declared_md5() {
    let wheel = wheel_metadata("Flask", "1.0");
    let (_dir, staged) = staged_upload(&wheel);
    staged
        .blob
        .with_materialized(|path| std::fs::remove_file(path).unwrap());
    let mut form = staged_form(&wheel);
    form.sha256_digest = None;
    form.md5_digest = Some(md5_hex(&wheel));

    assert!(matches!(
        prepare(form, staged, "root/hosted", 1000).unwrap_err(),
        UploadError::InvalidContent(_)
    ));
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
                form.md5_digest = Some("00".repeat(16));
            },
            UploadError::DigestMismatch("md5_digest"),
        ),
        (
            |form| {
                form.sha256_digest = None;
                form.md5_digest = Some("nothex".to_owned());
            },
            UploadError::InvalidDigest {
                field: "md5_digest",
                value: "nothex".to_owned(),
            },
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
            "pkg-1.0.tar.bz2",
            UploadError::InvalidDistributionFilename {
                filename: "pkg-1.0.tar.bz2".to_owned(),
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
