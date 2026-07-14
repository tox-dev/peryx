//! Core-metadata validation against the declared form fields.

use super::support::*;
use rstest::rstest;

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
        b"Metadata-Version: 2.4\nName: Flask\nVersion: 1.0\nRequires-Python: >=3.8\nLicense-Expression: MIT\nLicense-File: LICENSE\nProvides-Extra: cli\nProject-URL: Source, https://example.test/source\nHome-Page: https://example.test/home\n",
    );
    let (_dir, staged) = staged_upload(&bytes);
    let mut form = staged_form(&bytes);
    form.metadata_version = Some("2.4".to_owned());
    form.license_expression = Some("MIT".to_owned());
    form.license_files.push("LICENSE".to_owned());
    form.provides_extra.push("cli".to_owned());
    form.project_urls.push("Source, https://example.test/source".to_owned());
    form.home_page = Some("https://example.test/home".to_owned());

    let prepared = prepare(form, staged, "root/hosted", 1000).unwrap();

    assert_eq!(prepared.display_name, "Flask");
}

#[rstest]
#[case::missing_comma("https://example.test", "", "https://example.test", None)]
#[case::empty_label(", https://example.test", "", "https://example.test", None)]
#[case::long_label(
    "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa, https://example.test",
    "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa",
    "https://example.test",
    None
)]
#[case::empty_url("Docs,", "Docs", "", None)]
#[case::malformed_url("Docs, https://example .test", "Docs", "https://example .test", None)]
#[case::unsupported_scheme(
    "Docs, irc://example.test",
    "Docs",
    "irc://example.test",
    Some("Docs, irc://example.test")
)]
fn test_prepare_rejects_invalid_project_url(
    #[case] project_url: &str,
    #[case] label: &str,
    #[case] url: &str,
    #[case] form_url: Option<&str>,
) {
    let bytes = wheel_metadata_bytes(
        format!("Metadata-Version: 2.1\nName: Flask\nVersion: 1.0\nProject-URL: {project_url}\n").as_bytes(),
    );
    let (_dir, staged) = staged_upload(&bytes);
    let mut form = staged_form(&bytes);
    form.requires_python = None;
    form.project_urls.extend(form_url.map(str::to_owned));

    assert_eq!(
        prepare(form, staged, "root/hosted", 1000).unwrap_err(),
        UploadError::InvalidProjectUrl {
            label: label.to_owned(),
            url: url.to_owned()
        }
    );
}

#[rstest]
#[case::unicode_label("éééééééééééééééééééééééééééééééé", "https://example.test")]
#[case::url_with_comma("Docs", "https://example.test/a,b")]
#[case::http_url("Docs", "http://example.test")]
fn test_prepare_accepts_valid_project_url(#[case] label: &str, #[case] url: &str) {
    let bytes = wheel_metadata_bytes(
        format!("Metadata-Version: 2.1\nName: Flask\nVersion: 1.0\nProject-URL: {label}, {url}\n").as_bytes(),
    );
    let (_dir, staged) = staged_upload(&bytes);
    let mut form = staged_form(&bytes);
    form.requires_python = None;

    assert_eq!(
        prepare(form, staged, "root/hosted", 1000).unwrap().display_name,
        "Flask"
    );
}

#[test]
fn test_prepare_rejects_conflicting_license_fields() {
    let bytes = wheel_metadata_bytes(
        b"Metadata-Version: 2.4\nName: Flask\nVersion: 1.0\nLicense: legacy\nLicense-Expression: MIT\n",
    );
    let (_dir, staged) = staged_upload(&bytes);

    assert_eq!(
        prepare(staged_form(&bytes), staged, "root/hosted", 1000).unwrap_err(),
        UploadError::ConflictingLicenseFields
    );
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
