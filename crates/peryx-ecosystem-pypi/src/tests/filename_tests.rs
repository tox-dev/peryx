use crate::{DistributionFilenameError, DistributionKind, distribution_version_segment, parse_distribution_filename};

#[test]
fn test_distribution_version_segment_reads_sdist_version_after_the_last_dash() {
    // An sdist name may keep a `-`, so the version is the last segment; a wheel escapes its name, so
    // the version follows the first dash even with a build tag; legacy shapes escape their names too.
    for (filename, expected) in [
        ("python-dateutil-2.8.2.tar.gz", Some("2.8.2")),
        ("proj-1.0.tar.gz", Some("1.0")),
        ("proj-1.1.zip", Some("1.1")),
        ("Flask-1.0-py3-none-any.whl", Some("1.0")),
        ("proj-2.1-1-py3-none-any.whl", Some("2.1")),
        ("proj-0.9-py3-none-any.egg", Some("0.9")),
        ("noversion.whl", None),
        ("README", None),
    ] {
        assert_eq!(distribution_version_segment(filename), expected, "{filename}");
    }
}

#[test]
fn test_parse_distribution_filename_accepts_upload_formats() {
    for (filename, kind, name, version) in [
        ("Flask-1.0-py3-none-any.whl", DistributionKind::Wheel, "Flask", "1.0"),
        ("Flask-1.0-1-py3-none-any.whl", DistributionKind::Wheel, "Flask", "1.0"),
        (
            "zope.interface-7.2-cp313-cp313-macosx_11_0_arm64.whl",
            DistributionKind::Wheel,
            "zope.interface",
            "7.2",
        ),
        ("Flask-1.0.tar.gz", DistributionKind::SdistTarGz, "Flask", "1.0"),
        ("Flask-1.0.zip", DistributionKind::SdistZip, "Flask", "1.0"),
        (
            "python-dateutil-2.8.2.zip",
            DistributionKind::SdistZip,
            "python-dateutil",
            "2.8.2",
        ),
        ("Flask-1.0-PY3-NONE-ANY.WHL", DistributionKind::Wheel, "Flask", "1.0"),
        ("Flask-1.0.TAR.GZ", DistributionKind::SdistTarGz, "Flask", "1.0"),
        ("Flask-1.0.ZIP", DistributionKind::SdistZip, "Flask", "1.0"),
    ] {
        let parsed = parse_distribution_filename(filename).unwrap();
        assert_eq!(parsed.kind, kind);
        assert_eq!(
            parsed.kind.upload_filetype(),
            match kind {
                DistributionKind::Wheel => "bdist_wheel",
                DistributionKind::SdistTarGz | DistributionKind::SdistZip => "sdist",
            }
        );
        assert_eq!(parsed.name, name);
        assert_eq!(parsed.normalized_name, crate::normalize_name(name));
        assert_eq!(parsed.version.to_string(), version);
    }
}

#[test]
fn test_parse_distribution_filename_rejects_bad_shapes() {
    for (filename, expected) in [
        ("pkg-1.0.egg", DistributionFilenameError::LegacyEgg),
        ("pkg-1.0.tar.bz2", DistributionFilenameError::UnsupportedExtension),
        ("pkg.zip", DistributionFilenameError::InvalidSdistShape),
        ("pkg-1.0-py3-none.whl", DistributionFilenameError::InvalidWheelShape),
        (
            "pkg-1.0-py3-*-any.whl",
            DistributionFilenameError::InvalidTag("*".to_owned()),
        ),
        (
            "pkg-1.0--py3-none-any.whl",
            DistributionFilenameError::InvalidWheelShape,
        ),
        (
            "pkg-1.0-build-py3-none-any.whl",
            DistributionFilenameError::InvalidTag("build".to_owned()),
        ),
        (
            "pkg!-1.0-py3-none-any.whl",
            DistributionFilenameError::InvalidName("pkg!".to_owned()),
        ),
        (
            "pkg-bad-py3-none-any.whl",
            DistributionFilenameError::InvalidVersion("bad".to_owned()),
        ),
        ("pkg.tar.gz", DistributionFilenameError::InvalidSdistShape),
    ] {
        assert_eq!(parse_distribution_filename(filename).unwrap_err(), expected);
    }
}
