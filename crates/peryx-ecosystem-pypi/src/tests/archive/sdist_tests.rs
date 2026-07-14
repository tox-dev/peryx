use std::io::Write as _;

use super::{temp_archive, valid_sdist, valid_zip_sdist};
use crate::archive::{ArchiveError, validate_sdist_path, validate_zip_sdist_path};

fn valid_sdist_with_link(path: &str, target: &str, entry_type: tar::EntryType) -> Vec<u8> {
    let mut tarball = Vec::new();
    {
        let encoder = flate2::write::GzEncoder::new(&mut tarball, flate2::Compression::fast());
        let mut builder = tar::Builder::new(encoder);
        for (path, bytes) in [
            (
                "pkg-1.0/PKG-INFO",
                b"Metadata-Version: 2.2\nName: pkg\nVersion: 1.0\n".as_slice(),
            ),
            ("pkg-1.0/pyproject.toml", b"[build-system]\n".as_slice()),
        ] {
            let mut header = tar::Header::new_gnu();
            header.set_size(bytes.len() as u64);
            header.set_mode(0o644);
            header.set_cksum();
            builder.append_data(&mut header, path, bytes).unwrap();
        }
        let mut header = tar::Header::new_gnu();
        header.set_entry_type(entry_type);
        header.set_size(0);
        header.set_mode(0o777);
        header.set_cksum();
        builder.append_link(&mut header, path, target).unwrap();
        builder.into_inner().unwrap().finish().unwrap();
    }
    tarball
}

fn sdist_with_file_path(path: &str) -> Vec<u8> {
    let mut tarball = Vec::new();
    {
        let encoder = flate2::write::GzEncoder::new(&mut tarball, flate2::Compression::fast());
        let mut builder = tar::Builder::new(encoder);
        let mut header = tar::Header::new_gnu();
        header.set_size(0);
        header.set_mode(0o644);
        header.as_mut_bytes()[..path.len()].copy_from_slice(path.as_bytes());
        header.set_cksum();
        builder.append(&header, std::io::empty()).unwrap();
        builder.into_inner().unwrap().finish().unwrap();
    }
    tarball
}

fn sdist_with_large_pkg_info_header(size: u64) -> Vec<u8> {
    let mut tarball = Vec::new();
    {
        let mut encoder = flate2::write::GzEncoder::new(&mut tarball, flate2::Compression::fast());
        let mut header = tar::Header::new_gnu();
        header.set_path("pkg-1.0/PKG-INFO").unwrap();
        header.set_entry_type(tar::EntryType::Regular);
        header.set_size(size);
        header.set_mode(0o644);
        header.set_cksum();
        encoder.write_all(header.as_bytes()).unwrap();
        encoder.finish().unwrap();
    }
    tarball
}

fn sdist_with_too_many_entries() -> Vec<u8> {
    let mut tarball = Vec::new();
    {
        let encoder = flate2::write::GzEncoder::new(&mut tarball, flate2::Compression::fast());
        let mut builder = tar::Builder::new(encoder);
        for path in ["pkg-1.0/PKG-INFO".to_owned(), "pkg-1.0/pyproject.toml".to_owned()]
            .into_iter()
            .chain((0..99_999).map(|index| format!("pkg-1.0/empty-{index}.txt")))
        {
            let mut header = tar::Header::new_gnu();
            header.set_size(0);
            header.set_mode(0o644);
            header.set_cksum();
            builder.append_data(&mut header, path, std::io::empty()).unwrap();
        }
        builder.into_inner().unwrap().finish().unwrap();
    }
    tarball
}

fn sdist_with_link(path: &str, target: &str) -> Vec<u8> {
    sdist_with_link_type(path, target, tar::EntryType::symlink())
}

fn sdist_with_hard_link(path: &str, target: &str) -> Vec<u8> {
    sdist_with_link_type(path, target, tar::EntryType::hard_link())
}

fn sdist_with_link_type(path: &str, target: &str, entry_type: tar::EntryType) -> Vec<u8> {
    let mut tarball = Vec::new();
    {
        let encoder = flate2::write::GzEncoder::new(&mut tarball, flate2::Compression::fast());
        let mut builder = tar::Builder::new(encoder);
        let mut header = tar::Header::new_gnu();
        header.set_entry_type(entry_type);
        header.set_size(0);
        header.set_mode(0o777);
        header.set_cksum();
        builder.append_link(&mut header, path, target).unwrap();
        builder.into_inner().unwrap().finish().unwrap();
    }
    tarball
}

fn sdist_with_link_without_target(path: &str) -> Vec<u8> {
    let mut tarball = Vec::new();
    {
        let encoder = flate2::write::GzEncoder::new(&mut tarball, flate2::Compression::fast());
        let mut builder = tar::Builder::new(encoder);
        let mut header = tar::Header::new_gnu();
        header.set_path(path).unwrap();
        header.set_entry_type(tar::EntryType::symlink());
        header.set_size(0);
        header.set_mode(0o777);
        header.set_cksum();
        builder.append(&header, std::io::empty()).unwrap();
        builder.into_inner().unwrap().finish().unwrap();
    }
    tarball
}

fn sdist_with_special(path: &str) -> Vec<u8> {
    let mut tarball = Vec::new();
    {
        let encoder = flate2::write::GzEncoder::new(&mut tarball, flate2::Compression::fast());
        let mut builder = tar::Builder::new(encoder);
        let mut header = tar::Header::new_gnu();
        header.set_entry_type(tar::EntryType::character_special());
        header.set_device_major(1).unwrap();
        header.set_device_minor(3).unwrap();
        header.set_size(0);
        header.set_mode(0o644);
        header.set_cksum();
        builder.append_data(&mut header, path, std::io::empty()).unwrap();
        builder.into_inner().unwrap().finish().unwrap();
    }
    tarball
}

#[test]
fn test_validate_sdist_path_accepts_modern_layout_and_license_files() {
    let metadata = b"Metadata-Version: 2.4\nName: pkg\nVersion: 1.0\nLicense-File: LICENSE\n";
    let file = temp_archive(&valid_sdist(&[
        ("pkg-1.0/PKG-INFO", metadata.as_slice()),
        ("pkg-1.0/pyproject.toml", b"[build-system]\n".as_slice()),
        ("pkg-1.0/LICENSE", b"MIT\n".as_slice()),
    ]));

    let archive = validate_sdist_path("pkg-1.0.tar.gz", file.path()).unwrap();

    assert_eq!(archive.metadata, metadata);
    assert!(archive.missing_license_files.is_empty());
}

#[test]
fn test_validate_sdist_path_rejects_missing_required_members() {
    for (entries, expected) in [
        (
            vec![(
                "pkg-1.0/PKG-INFO",
                b"Metadata-Version: 2.2\nName: pkg\nVersion: 1.0\n".as_slice(),
            )],
            "missing required pkg-1.0/pyproject.toml",
        ),
        (
            vec![("pkg-1.0/pyproject.toml", b"[build-system]\n".as_slice())],
            "missing required pkg-1.0/PKG-INFO",
        ),
        (
            vec![
                (
                    "other-1.0/PKG-INFO",
                    b"Metadata-Version: 2.2\nName: pkg\nVersion: 1.0\n".as_slice(),
                ),
                ("pkg-1.0/pyproject.toml", b"[build-system]\n".as_slice()),
            ],
            "outside required top-level directory",
        ),
    ] {
        let file = temp_archive(&valid_sdist(&entries));

        assert!(matches!(
            validate_sdist_path("pkg-1.0.tar.gz", file.path()),
            Err(ArchiveError::Invalid(message)) if message.contains(expected)
        ));
    }
}

#[test]
fn test_validate_sdist_path_rejects_invalid_sdist_filename() {
    let file = temp_archive(&valid_sdist(&[(
        "pkg-1.0/PKG-INFO",
        b"Metadata-Version: 2.2\nName: pkg\nVersion: 1.0\n".as_slice(),
    )]));

    assert!(matches!(
        validate_sdist_path("pkg.tar.gz", file.path()),
        Err(ArchiveError::Invalid(message)) if message.contains("invalid sdist filename")
    ));
    assert!(matches!(
        validate_sdist_path("pkg-1.0-py3-none-any.whl", file.path()),
        Err(ArchiveError::Invalid(message)) if message == "invalid sdist: \"pkg-1.0-py3-none-any.whl\" is not an sdist filename"
    ));
}

#[test]
fn test_validate_sdist_path_rejects_duplicate_pkg_info() {
    let metadata = b"Metadata-Version: 2.2\nName: pkg\nVersion: 1.0\n";
    let file = temp_archive(&valid_sdist(&[
        ("pkg-1.0/PKG-INFO", metadata.as_slice()),
        ("pkg-1.0/PKG-INFO", metadata.as_slice()),
        ("pkg-1.0/pyproject.toml", b"[build-system]\n".as_slice()),
    ]));

    assert!(matches!(
        validate_sdist_path("pkg-1.0.tar.gz", file.path()),
        Err(ArchiveError::Invalid(message)) if message == "invalid sdist: multiple pkg-1.0/PKG-INFO entries found"
    ));
}

#[test]
fn test_validate_sdist_path_rejects_unsafe_tar_members() {
    for (bytes, expected) in [
        (sdist_with_file_path("/pkg-1.0/module.py"), "archive member"),
        (sdist_with_file_path("PKG-INFO"), "outside required top-level directory"),
        (sdist_with_file_path("C:pkg-1.0/PKG-INFO"), "archive member"),
        (sdist_with_file_path("pkg-1.0/../module.py"), "archive member"),
        (sdist_with_link("pkg-1.0/link.py", "../module.py"), "archive member"),
        (sdist_with_link_without_target("pkg-1.0/link.py"), "missing its target"),
        (
            sdist_with_hard_link("pkg-1.0/link.py", "other-1.0/module.py"),
            "points outside",
        ),
        (sdist_with_special("pkg-1.0/device"), "unsupported tar entry"),
    ] {
        let file = temp_archive(&bytes);
        let err = validate_sdist_path("pkg-1.0.tar.gz", file.path()).expect_err("unsafe sdist was accepted");

        assert!(err.to_string().contains(expected), "{err}");
    }
}

#[test]
fn test_validate_sdist_path_accepts_links_within_root() {
    for bytes in [
        valid_sdist_with_link("pkg-1.0/sub/link.py", "module.py", tar::EntryType::symlink()),
        valid_sdist_with_link("pkg-1.0/link.py", "pkg-1.0/module.py", tar::EntryType::hard_link()),
    ] {
        let file = temp_archive(&bytes);

        assert!(validate_sdist_path("pkg-1.0.tar.gz", file.path()).is_ok());
    }
}

#[test]
fn test_validate_sdist_path_rejects_metadata_version_problems() {
    for (metadata_version, expected) in [
        ("2.1", "PKG-INFO Metadata-Version 2.1 is older than the required 2.2"),
        ("2", "invalid Metadata-Version \"2\""),
        ("2.", "invalid Metadata-Version \"2.\""),
        ("x.2", "invalid Metadata-Version \"x.2\""),
        (
            "999999999999999999999999999999999999999999.0",
            "invalid Metadata-Version \"999999999999999999999999999999999999999999.0\"",
        ),
    ] {
        let file = temp_archive(&valid_sdist(&[
            (
                "pkg-1.0/PKG-INFO",
                format!("Metadata-Version: {metadata_version}\nName: pkg\nVersion: 1.0\n").as_bytes(),
            ),
            ("pkg-1.0/pyproject.toml", b"[build-system]\n".as_slice()),
        ]));

        assert!(matches!(
            validate_sdist_path("pkg-1.0.tar.gz", file.path()),
            Err(ArchiveError::Invalid(message)) if message == format!("invalid sdist: {expected}")
        ));
    }
}

#[test]
fn test_validate_sdist_path_rejects_invalid_pkg_info_metadata() {
    for (metadata, expected) in [
        (b"\xff".as_slice(), "PKG-INFO is not valid UTF-8"),
        (
            b"Name: pkg\nVersion: 1.0\n".as_slice(),
            "PKG-INFO is missing Metadata-Version",
        ),
    ] {
        let file = temp_archive(&valid_sdist(&[
            ("pkg-1.0/PKG-INFO", metadata),
            ("pkg-1.0/pyproject.toml", b"[build-system]\n".as_slice()),
        ]));

        assert!(matches!(
            validate_sdist_path("pkg-1.0.tar.gz", file.path()),
            Err(ArchiveError::Invalid(message)) if message == format!("invalid sdist: {expected}")
        ));
    }
}

#[test]
fn test_validate_sdist_path_rejects_large_pkg_info_before_reading_body() {
    let file = temp_archive(&sdist_with_large_pkg_info_header(16 * 1024 * 1024 + 1));

    assert!(matches!(
        validate_sdist_path("pkg-1.0.tar.gz", file.path()),
        Err(ArchiveError::Invalid(message))
            if message == "invalid sdist: pkg-1.0/PKG-INFO is 16777217 bytes, above the upload validation limit of 16777216 bytes"
    ));
}

#[test]
fn test_validate_sdist_path_rejects_too_many_entries() {
    let file = temp_archive(&sdist_with_too_many_entries());

    assert!(matches!(
        validate_sdist_path("pkg-1.0.tar.gz", file.path()),
        Err(ArchiveError::Invalid(message)) if message == "invalid sdist: archive has more than 100000 entries"
    ));
}

#[test]
fn test_validate_zip_sdist_path_extracts_pkg_info_from_modern_layout() {
    let metadata = b"Metadata-Version: 2.4\nName: pkg\nVersion: 1.0\nLicense-File: LICENSE\n";
    let file = temp_archive(&valid_zip_sdist(&[
        ("pkg-1.0/", b"".as_slice()),
        ("pkg-1.0/PKG-INFO", metadata.as_slice()),
        ("pkg-1.0/pyproject.toml", b"[build-system]\n".as_slice()),
        ("pkg-1.0/LICENSE", b"MIT\n".as_slice()),
        ("pkg-1.0/module.py", b"x = 1\n".as_slice()),
    ]));

    let archive = validate_zip_sdist_path("pkg-1.0.zip", file.path()).unwrap();

    assert_eq!(archive.metadata, metadata);
    assert!(archive.missing_license_files.is_empty());
}

#[test]
fn test_validate_zip_sdist_path_accepts_hyphenated_project_name() {
    // The last-dash rule keeps the version off a hyphenated project name, so the root must match.
    let metadata = b"Metadata-Version: 2.2\nName: my-pkg\nVersion: 1.0\n";
    let file = temp_archive(&valid_zip_sdist(&[
        ("my-pkg-1.0/PKG-INFO", metadata.as_slice()),
        ("my-pkg-1.0/pyproject.toml", b"[build-system]\n".as_slice()),
    ]));

    assert_eq!(
        validate_zip_sdist_path("my-pkg-1.0.zip", file.path()).unwrap().metadata,
        metadata
    );
}

#[test]
fn test_validate_zip_sdist_path_rejects_bad_filenames_and_content() {
    let bytes = valid_zip_sdist(&[
        (
            "pkg-1.0/PKG-INFO",
            b"Metadata-Version: 2.2\nName: pkg\nVersion: 1.0\n".as_slice(),
        ),
        ("pkg-1.0/pyproject.toml", b"[build-system]\n".as_slice()),
    ]);
    for (filename, archive, expected) in [
        ("pkg.zip", bytes.as_slice(), "invalid sdist filename"),
        (
            "pkg-1.0.tar.gz",
            bytes.as_slice(),
            "\"pkg-1.0.tar.gz\" is not an sdist filename",
        ),
        ("pkg-1.0.zip", b"not a zip".as_slice(), "archive read failed"),
    ] {
        let file = temp_archive(archive);
        let err = validate_zip_sdist_path(filename, file.path()).expect_err("zip sdist was accepted");

        assert!(err.to_string().contains(expected), "{err}");
    }
}

#[test]
fn test_validate_zip_sdist_path_rejects_unsafe_and_out_of_root_members() {
    for (entries, expected) in [
        (
            vec![
                ("pkg-1.0/../evil.py", b"x = 1\n".as_slice()),
                (
                    "pkg-1.0/PKG-INFO",
                    b"Metadata-Version: 2.2\nName: pkg\nVersion: 1.0\n".as_slice(),
                ),
                ("pkg-1.0/pyproject.toml", b"[build-system]\n".as_slice()),
            ],
            "archive member",
        ),
        (
            vec![
                (
                    "other-1.0/PKG-INFO",
                    b"Metadata-Version: 2.2\nName: pkg\nVersion: 1.0\n".as_slice(),
                ),
                ("pkg-1.0/pyproject.toml", b"[build-system]\n".as_slice()),
            ],
            "outside required top-level directory",
        ),
    ] {
        let file = temp_archive(&valid_zip_sdist(&entries));
        let err = validate_zip_sdist_path("pkg-1.0.zip", file.path()).expect_err("unsafe zip sdist was accepted");

        assert!(err.to_string().contains(expected), "{err}");
    }
}

#[test]
fn test_validate_sdist_path_reports_license_file_missing_from_the_archive() {
    let file = temp_archive(&valid_sdist(&[
        (
            "pkg-1.0/PKG-INFO",
            b"Metadata-Version: 2.4\nName: pkg\nVersion: 1.0\nLicense-File: LICENSE\n".as_slice(),
        ),
        ("pkg-1.0/pyproject.toml", b"[build-system]\n".as_slice()),
    ]));

    assert_eq!(
        validate_sdist_path("pkg-1.0.tar.gz", file.path())
            .unwrap()
            .missing_license_files,
        ["LICENSE"]
    );
}
