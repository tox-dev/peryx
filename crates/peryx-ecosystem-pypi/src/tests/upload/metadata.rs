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

#[rstest]
#[case::missing("", None)]
#[case::empty("Metadata-Version:\n", None)]
#[case::withdrawn("Metadata-Version: 2.0\n", Some("2.0"))]
#[case::malformed("Metadata-Version: 2\n", Some("2"))]
#[case::newer_minor("Metadata-Version: 2.7\n", Some("2.7"))]
#[case::newer_major("Metadata-Version: 3.0\n", Some("3.0"))]
fn test_prepare_rejects_invalid_metadata_version(#[case] header: &str, #[case] expected: Option<&str>) {
    let bytes = wheel_metadata_bytes(format!("{header}Name: Flask\nVersion: 1.0\n").as_bytes());
    let (_dir, staged) = staged_upload(&bytes);
    let mut form = staged_form(&bytes);
    form.requires_python = None;

    assert_eq!(
        prepare(form, staged, "root/hosted", 1000).unwrap_err(),
        expected.map_or(UploadError::MissingMetadataVersion, |value| {
            UploadError::UnsupportedMetadataVersion(value.to_owned())
        })
    );
}

#[rstest]
#[case::v1_0("1.0")]
#[case::v1_1("1.1")]
#[case::v1_2("1.2")]
#[case::v2_1("2.1")]
#[case::v2_2("2.2")]
#[case::v2_3("2.3")]
#[case::v2_4("2.4")]
#[case::v2_5("2.5")]
#[case::v2_6("2.6")]
fn test_prepare_accepts_supported_metadata_version(#[case] metadata_version: &str) {
    let bytes =
        wheel_metadata_bytes(format!("Metadata-Version: {metadata_version}\nName: Flask\nVersion: 1.0\n").as_bytes());
    let (_dir, staged) = staged_upload(&bytes);
    let mut form = staged_form(&bytes);
    form.requires_python = None;

    assert_eq!(
        prepare(form, staged, "root/hosted", 1000).unwrap().display_name,
        "Flask"
    );
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
            "Metadata-Version: 2.4\nName: Flask\nVersion: 1.0\nRequires-Python: >=3.8\nLicense-Expression: MIT\n",
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
    let bytes = wheel_metadata_bytes_with_licenses(
        b"Metadata-Version: 2.4\nName: Flask\nVersion: 1.0\nRequires-Python: >=3.8\nLicense-Expression: MIT\nLicense-File: LICENSE\nProvides-Extra: cli\nProject-URL: Source, https://example.test/source\nHome-Page: https://example.test/home\n",
        &["LICENSE"],
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

#[rstest]
#[case::invalid_legacy_name(
    "Metadata-Version: 2.1\nName: Flask\nVersion: 1.0\nProvides-Extra: -dev\n",
    "-dev",
    "must be a valid project or extra name"
)]
#[case::unnormalized_modern_name(
    "Metadata-Version: 2.3\nName: Flask\nVersion: 1.0\nProvides-Extra: Dev_Test\n",
    "Dev_Test",
    "must match ^[a-z0-9]+(-[a-z0-9]+)*$"
)]
#[case::unnormalized_latest_name(
    "Metadata-Version: 2.6\nName: Flask\nVersion: 1.0\nProvides-Extra: Dev_Test\n",
    "Dev_Test",
    "must match ^[a-z0-9]+(-[a-z0-9]+)*$"
)]
#[case::invalid_modern_name(
    "Metadata-Version: 2.3\nName: Flask\nVersion: 1.0\nProvides-Extra: -dev\n",
    "-dev",
    "must match ^[a-z0-9]+(-[a-z0-9]+)*$"
)]
fn test_prepare_rejects_invalid_provided_extra(
    #[case] metadata: &str,
    #[case] value: &str,
    #[case] reason: &'static str,
) {
    let bytes = wheel_metadata_bytes(metadata.as_bytes());
    let (_dir, staged) = staged_upload(&bytes);

    assert_eq!(
        prepare(staged_form(&bytes), staged, "root/hosted", 1000).unwrap_err(),
        UploadError::InvalidMetadataValue {
            field: "Provides-Extra",
            value: value.to_owned(),
            reason,
        }
    );
}

#[test]
fn test_prepare_rejects_normalized_provided_extra_collision() {
    let bytes = wheel_metadata_bytes(
        b"Metadata-Version: 2.1\nName: Flask\nVersion: 1.0\nProvides-Extra: Dev.Test\nProvides-Extra: dev_test\n",
    );
    let (_dir, staged) = staged_upload(&bytes);

    assert_eq!(
        prepare(staged_form(&bytes), staged, "root/hosted", 1000).unwrap_err(),
        UploadError::InvalidMetadataValue {
            field: "Provides-Extra",
            value: "dev_test".to_owned(),
            reason: "duplicates an earlier value after normalization",
        }
    );
}

#[test]
fn test_prepare_preserves_legacy_provided_extras() {
    let bytes = wheel_metadata_bytes(
        b"Metadata-Version: 2.1\nName: Flask\nVersion: 1.0\nRequires-Python: >=3.8\nProvides-Extra: Dev.Test\nProvides-Extra: docs\n",
    );
    let (_dir, staged) = staged_upload(&bytes);
    let mut form = staged_form(&bytes);
    form.provides_extra = vec!["Dev.Test".to_owned(), "docs".to_owned()];
    let prepared = prepare(form, staged, "root/hosted", 1000).unwrap();

    assert_eq!(
        crate::parse_metadata(std::str::from_utf8(&prepared.metadata).unwrap()).provides_extra,
        ["Dev.Test", "docs"]
    );
}

#[rstest]
#[case::parent("../LICENSE", "parent directory components are not allowed")]
#[case::relative_parent("./../LICENSE", "parent directory components are not allowed")]
#[case::unresolved_parent("licenses/../LICENSE", "parent directory components are not allowed")]
#[case::glob("licenses/*", "paths must be resolved")]
#[case::absolute("/licenses/LICENSE", "paths must be relative")]
#[case::windows_drive("C:/licenses/LICENSE", "paths must be relative")]
#[case::windows_drive_backslash("C:\\licenses\\LICENSE", "paths must be relative")]
#[case::backslash("licenses\\LICENSE", "paths must use the '/' delimiter")]
fn test_prepare_rejects_invalid_license_file(#[case] license_file: &str, #[case] reason: &'static str) {
    let bytes = wheel_metadata_bytes(
        format!(
            "Metadata-Version: 2.4\nName: Flask\nVersion: 1.0\nRequires-Python: >=3.8\nLicense-File: {license_file}\n"
        )
        .as_bytes(),
    );
    let (_dir, staged) = staged_upload(&bytes);

    assert_eq!(
        prepare(staged_form(&bytes), staged, "root/hosted", 1000).unwrap_err(),
        UploadError::InvalidLicenseFile {
            value: license_file.to_owned(),
            reason,
        }
    );
}

#[rstest]
#[case::root(&["LICENSE"])]
#[case::nested(&["licenses/LICENSE.MIT"])]
#[case::multiple(&["licenses/LICENSE.MIT", "licenses/LICENSE.CC0"])]
fn test_prepare_accepts_valid_license_file(#[case] license_files: &[&str]) {
    let bytes = wheel_with_license_files(license_files, license_files);
    let (_dir, staged) = staged_upload(&bytes);

    assert_eq!(
        prepare(staged_form(&bytes), staged, "root/hosted", 1000)
            .unwrap()
            .display_name,
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

#[rstest]
#[case::identifier("MIT")]
#[case::compound("MIT OR (Apache-2.0 AND BSD-3-Clause)")]
#[case::exception("GPL-3.0-or-later WITH Bison-exception-2.2")]
#[case::or_later("Apache-1.0+")]
#[case::reference("LicenseRef-Proprietary")]
fn test_prepare_accepts_valid_license_expression(#[case] expression: &str) {
    let bytes = license_expression_wheel("2.4", expression);
    let (_dir, staged) = staged_upload(&bytes);

    assert_eq!(
        prepare(staged_form(&bytes), staged, "root/hosted", 1000)
            .unwrap()
            .display_name,
        "Flask"
    );
}

#[rstest]
#[case::unclosed_parens("(MIT OR Apache-2.0", "is not a valid SPDX license expression")]
#[case::dangling_operator("MIT OR", "is not a valid SPDX license expression")]
#[case::unknown_identifier(
    "Totally-Made-Up-1.0",
    "is not a known SPDX license identifier in its reference case"
)]
#[case::unnormalized_case("mit", "is not a known SPDX license identifier in its reference case")]
#[case::unknown_exception(
    "GPL-3.0-or-later WITH Made-Up-exception",
    "is not a known SPDX license identifier in its reference case"
)]
#[case::deprecated_identifier("GPL-3.0", "uses a deprecated SPDX license identifier")]
fn test_prepare_rejects_invalid_license_expression(#[case] expression: &str, #[case] reason: &'static str) {
    let bytes = license_expression_wheel("2.4", expression);
    let (_dir, staged) = staged_upload(&bytes);

    assert_eq!(
        prepare(staged_form(&bytes), staged, "root/hosted", 1000).unwrap_err(),
        UploadError::InvalidMetadataValue {
            field: "License-Expression",
            value: expression.to_owned(),
            reason,
        }
    );
}

#[rstest]
#[case::v1_0("1.0")]
#[case::v1_2("1.2")]
#[case::v2_3("2.3")]
fn test_prepare_rejects_license_expression_before_metadata_2_4(#[case] metadata_version: &str) {
    let bytes = license_expression_wheel(metadata_version, "MIT");
    let (_dir, staged) = staged_upload(&bytes);

    assert_eq!(
        prepare(staged_form(&bytes), staged, "root/hosted", 1000).unwrap_err(),
        UploadError::InvalidMetadataValue {
            field: "License-Expression",
            value: "MIT".to_owned(),
            reason: "requires Metadata-Version 2.4 or later",
        }
    );
}

fn license_expression_wheel(metadata_version: &str, expression: &str) -> Vec<u8> {
    wheel_metadata_bytes(
        format!(
            "Metadata-Version: {metadata_version}\nName: Flask\nVersion: 1.0\nRequires-Python: >=3.8\nLicense-Expression: {expression}\n"
        )
        .as_bytes(),
    )
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
