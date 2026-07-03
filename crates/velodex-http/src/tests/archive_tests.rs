use std::io::Write as _;

use crate::archive::{
    ArchiveError, DEFAULT_MEMBER_CHUNK, MAX_CONTAINER_DEPTH, MAX_LISTED_ENTRIES, MAX_NESTED_ARCHIVE_SIZE, Member,
    MemberKind, list_members, list_members_nested_path, list_members_path, read_member, read_member_chunk,
    read_member_chunk_path, read_text_member_chunk_nested_path, sdist_metadata_path, wheel_metadata,
    wheel_metadata_path,
};

fn small_zip() -> Vec<u8> {
    let mut buf = Vec::new();
    {
        let mut zip = zip::ZipWriter::new(std::io::Cursor::new(&mut buf));
        let options = zip::write::SimpleFileOptions::default();
        zip.add_directory("pkg/", options).unwrap();
        zip.start_file("big.bin", options).unwrap();
        zip.write_all(&vec![0_u8; usize::try_from(DEFAULT_MEMBER_CHUNK + 1).unwrap()])
            .unwrap();
        zip.finish().unwrap();
    }
    buf
}

fn zip_with_file(path: &str, bytes: &[u8], compression: zip::CompressionMethod) -> Vec<u8> {
    let mut buf = Vec::new();
    {
        let mut zip = zip::ZipWriter::new(std::io::Cursor::new(&mut buf));
        let options = zip::write::SimpleFileOptions::default().compression_method(compression);
        zip.start_file(path, options).unwrap();
        zip.write_all(bytes).unwrap();
        zip.finish().unwrap();
    }
    buf
}

fn tar_gz_with_file(path: &str, bytes: &[u8]) -> Vec<u8> {
    let mut tarball = Vec::new();
    {
        let encoder = flate2::write::GzEncoder::new(&mut tarball, flate2::Compression::fast());
        let mut builder = tar::Builder::new(encoder);
        let mut header = tar::Header::new_gnu();
        header.set_size(bytes.len() as u64);
        header.set_cksum();
        builder.append_data(&mut header, path, bytes).unwrap();
        builder.into_inner().unwrap().finish().unwrap();
    }
    tarball
}

fn tar_gz_with_directory_and_file(path: &str, bytes: &[u8]) -> Vec<u8> {
    let mut tarball = Vec::new();
    {
        let encoder = flate2::write::GzEncoder::new(&mut tarball, flate2::Compression::fast());
        let mut builder = tar::Builder::new(encoder);
        let mut header = tar::Header::new_gnu();
        header.set_entry_type(tar::EntryType::Directory);
        header.set_size(0);
        header.set_mode(0o755);
        header.set_cksum();
        builder.append_data(&mut header, "pkg-1.0", std::io::empty()).unwrap();
        let mut header = tar::Header::new_gnu();
        header.set_size(bytes.len() as u64);
        header.set_cksum();
        builder.append_data(&mut header, path, bytes).unwrap();
        builder.into_inner().unwrap().finish().unwrap();
    }
    tarball
}

#[test]
fn test_extracts_metadata_documents_without_buffering_archives() {
    let wheel = zip_with_file(
        "pkg-1.0.dist-info/METADATA",
        b"Metadata-Version: 2.1\nName: pkg\n",
        zip::CompressionMethod::Stored,
    );
    assert_eq!(
        wheel_metadata("pkg-1.0-py3-none-any.whl", &wheel).as_deref(),
        Some(b"Metadata-Version: 2.1\nName: pkg\n".as_slice())
    );
    let mut file = tempfile::NamedTempFile::new().unwrap();
    file.write_all(&wheel).unwrap();
    file.flush().unwrap();
    assert_eq!(
        wheel_metadata_path("pkg-1.0-py3-none-any.whl", file.path())
            .unwrap()
            .as_deref(),
        Some(b"Metadata-Version: 2.1\nName: pkg\n".as_slice())
    );
    assert!(wheel_metadata("pkg-1.0.zip", &wheel).is_none());

    let mut wheel = Vec::new();
    {
        let mut zip = zip::ZipWriter::new(std::io::Cursor::new(&mut wheel));
        let options = zip::write::SimpleFileOptions::default();
        zip.add_directory("pkg-1.0.dist-info/", options).unwrap();
        zip.start_file("pkg-1.0.dist-info/METADATA", options).unwrap();
        zip.write_all(b"Metadata-Version: 2.1\nName: pkg\n").unwrap();
        zip.finish().unwrap();
    }
    assert_eq!(
        wheel_metadata("pkg-1.0-py3-none-any.whl", &wheel).as_deref(),
        Some(b"Metadata-Version: 2.1\nName: pkg\n".as_slice())
    );

    let sdist = tar_gz_with_file("pkg-1.0/PKG-INFO", b"Metadata-Version: 2.1\nName: pkg\n");
    let mut file = tempfile::NamedTempFile::new().unwrap();
    file.write_all(&sdist).unwrap();
    file.flush().unwrap();
    assert_eq!(
        sdist_metadata_path("pkg-1.0.tar.gz", file.path()).unwrap().as_deref(),
        Some(b"Metadata-Version: 2.1\nName: pkg\n".as_slice())
    );
    assert!(sdist_metadata_path("pkg-1.0.zip", file.path()).unwrap().is_none());

    let sdist = tar_gz_with_directory_and_file("pkg-1.0/PKG-INFO", b"Metadata-Version: 2.1\nName: pkg\n");
    let mut file = tempfile::NamedTempFile::new().unwrap();
    file.write_all(&sdist).unwrap();
    file.flush().unwrap();
    assert_eq!(
        sdist_metadata_path("pkg-1.0.tar.gz", file.path()).unwrap().as_deref(),
        Some(b"Metadata-Version: 2.1\nName: pkg\n".as_slice())
    );

    let sdist = tar_gz_with_file("pkg-1.0/module.py", b"x = 1\n");
    let mut file = tempfile::NamedTempFile::new().unwrap();
    file.write_all(&sdist).unwrap();
    file.flush().unwrap();
    assert!(sdist_metadata_path("pkg-1.0.tar.gz", file.path()).unwrap().is_none());
}

#[test]
fn test_read_member_unsupported_type() {
    assert!(matches!(
        read_member("file.txt", b"data", "x"),
        Err(ArchiveError::Unsupported)
    ));
}

#[test]
fn test_list_members_unsupported_type() {
    assert!(matches!(
        list_members("file.egg", b"data"),
        Err(ArchiveError::Unsupported)
    ));
    assert_eq!(MemberKind::Unknown.as_str(), "unknown");
}

#[test]
fn test_list_zip_members() {
    let members = list_members("pkg-1.0-py3-none-any.whl", &small_zip()).unwrap();
    assert_eq!(
        members,
        vec![Member {
            path: "big.bin".to_owned(),
            size: DEFAULT_MEMBER_CHUNK + 1,
            kind: MemberKind::Binary,
            previewable: false,
        }]
    );
}

#[test]
fn test_list_members_classifies_previewable_archives_and_binary_files() {
    let inner = zip_with_file("pkg/mod.py", b"x = 1\n", zip::CompressionMethod::Stored);
    let mut buf = Vec::new();
    {
        let mut zip = zip::ZipWriter::new(std::io::Cursor::new(&mut buf));
        let options = zip::write::SimpleFileOptions::default();
        zip.start_file("METADATA", options).unwrap();
        zip.write_all(b"Metadata-Version: 2.1\n").unwrap();
        zip.start_file("data.bin", options).unwrap();
        zip.write_all(&[0xff, 0xfe]).unwrap();
        zip.start_file("vendor/inner.zip", options).unwrap();
        zip.write_all(&inner).unwrap();
        zip.start_file("payload.dat", options).unwrap();
        zip.write_all(b"opaque").unwrap();
        zip.finish().unwrap();
    }

    assert_eq!(
        list_members("pkg-1.0-py3-none-any.whl", &buf).unwrap(),
        vec![
            Member {
                path: "METADATA".to_owned(),
                size: 22,
                kind: MemberKind::Text,
                previewable: true,
            },
            Member {
                path: "data.bin".to_owned(),
                size: 2,
                kind: MemberKind::Binary,
                previewable: false,
            },
            Member {
                path: "payload.dat".to_owned(),
                size: 6,
                kind: MemberKind::Unknown,
                previewable: false,
            },
            Member {
                path: "vendor/inner.zip".to_owned(),
                size: inner.len() as u64,
                kind: MemberKind::Archive,
                previewable: false,
            },
        ]
    );
}

#[test]
fn test_path_archive_rejects_unsupported_type() {
    let file = tempfile::NamedTempFile::new().unwrap();
    assert!(matches!(
        list_members_path("file.txt", file.path()),
        Err(ArchiveError::Unsupported)
    ));
    assert!(matches!(
        read_member_chunk_path("file.txt", file.path(), "x", 0, DEFAULT_MEMBER_CHUNK),
        Err(ArchiveError::Unsupported)
    ));
    assert!(matches!(
        read_text_member_chunk_nested_path("file.txt", file.path(), &[], "x.txt", 0, DEFAULT_MEMBER_CHUNK),
        Err(ArchiveError::Unsupported)
    ));
}

#[test]
fn test_nested_stored_zip_lists_without_copying_member() {
    let inner = zip_with_file("pkg/mod.py", b"x = 1\n", zip::CompressionMethod::Stored);
    let outer = zip_with_file("vendor/inner.zip", &inner, zip::CompressionMethod::Stored);
    let mut file = tempfile::NamedTempFile::new().unwrap();
    file.write_all(&outer).unwrap();
    file.flush().unwrap();

    let containers = vec!["vendor/inner.zip".to_owned()];
    assert_eq!(
        list_members_nested_path("outer.zip", file.path(), &containers).unwrap(),
        vec![Member {
            path: "pkg/mod.py".to_owned(),
            size: 6,
            kind: MemberKind::Text,
            previewable: true,
        }]
    );
    assert_eq!(
        read_text_member_chunk_nested_path(
            "outer.zip",
            file.path(),
            &containers,
            "pkg/mod.py",
            0,
            DEFAULT_MEMBER_CHUNK
        )
        .unwrap()
        .bytes,
        b"x = 1\n"
    );
}

#[test]
fn test_nested_deflated_zip_streams_member_to_temp_archive() {
    let inner = zip_with_file("pkg/mod.py", b"x = 1\n", zip::CompressionMethod::Stored);
    let outer = zip_with_file("vendor/inner.zip", &inner, zip::CompressionMethod::Deflated);
    let mut file = tempfile::NamedTempFile::new().unwrap();
    file.write_all(&outer).unwrap();
    file.flush().unwrap();

    assert_eq!(
        list_members_nested_path("outer.zip", file.path(), &["vendor/inner.zip".to_owned()])
            .unwrap()
            .first()
            .map(|member| member.path.as_str()),
        Some("pkg/mod.py")
    );
}

#[test]
fn test_nested_tar_streams_member_to_temp_archive() {
    let inner = zip_with_file("pkg/mod.py", b"x = 1\n", zip::CompressionMethod::Stored);
    let mut tarball = Vec::new();
    {
        let encoder = flate2::write::GzEncoder::new(&mut tarball, flate2::Compression::fast());
        let mut builder = tar::Builder::new(encoder);
        let mut dir = tar::Header::new_gnu();
        dir.set_entry_type(tar::EntryType::Directory);
        dir.set_size(0);
        dir.set_cksum();
        builder
            .append_data(&mut dir, "pkg-1.0/vendor/", std::io::empty())
            .unwrap();
        let mut file = tar::Header::new_gnu();
        file.set_size(inner.len() as u64);
        file.set_cksum();
        builder
            .append_data(&mut file, "pkg-1.0/vendor/inner.zip", inner.as_slice())
            .unwrap();
        builder.into_inner().unwrap().finish().unwrap();
    }
    let mut file = tempfile::NamedTempFile::new().unwrap();
    file.write_all(&tarball).unwrap();
    file.flush().unwrap();

    assert_eq!(
        list_members_nested_path("pkg-1.0.tar.gz", file.path(), &["pkg-1.0/vendor/inner.zip".to_owned()])
            .unwrap()
            .first()
            .map(|member| member.path.as_str()),
        Some("pkg/mod.py")
    );
}

#[test]
fn test_nested_archive_limits_reject_depth_unsafe_paths_and_unsupported_containers() {
    let file = tempfile::NamedTempFile::new().unwrap();
    assert!(matches!(
        list_members_nested_path(
            "outer.zip",
            file.path(),
            &vec!["inner.zip".to_owned(); MAX_CONTAINER_DEPTH + 1]
        ),
        Err(ArchiveError::NestingTooDeep { .. })
    ));
    assert!(matches!(
        list_members_nested_path("outer.zip", file.path(), &["../inner.zip".to_owned()]),
        Err(ArchiveError::UnsafeMember(path)) if path == "../inner.zip"
    ));
    assert!(matches!(
        list_members_nested_path("outer.zip", file.path(), &["inner.txt".to_owned()]),
        Err(ArchiveError::UnsupportedNestedArchive(path)) if path == "inner.txt"
    ));
    assert!(matches!(
        list_members_nested_path("outer.egg", file.path(), &["inner.zip".to_owned()]),
        Err(ArchiveError::Unsupported)
    ));
}

#[test]
fn test_nested_archive_rejects_missing_and_non_file_zip_members() {
    let outer = zip_with_file("pkg/mod.py", b"x = 1\n", zip::CompressionMethod::Stored);
    let mut file = tempfile::NamedTempFile::new().unwrap();
    file.write_all(&outer).unwrap();
    file.flush().unwrap();
    assert!(matches!(
        list_members_nested_path("outer.zip", file.path(), &["vendor/missing.zip".to_owned()]),
        Err(ArchiveError::MemberNotFound)
    ));

    let mut buf = Vec::new();
    {
        let mut zip = zip::ZipWriter::new(std::io::Cursor::new(&mut buf));
        let options = zip::write::SimpleFileOptions::default();
        zip.add_symlink("link.zip", "target", options).unwrap();
        zip.finish().unwrap();
    }
    let mut file = tempfile::NamedTempFile::new().unwrap();
    file.write_all(&buf).unwrap();
    file.flush().unwrap();
    assert!(matches!(
        list_members_nested_path("outer.zip", file.path(), &["link.zip".to_owned()]),
        Err(ArchiveError::MemberNotFound)
    ));
}

#[test]
fn test_nested_tar_rejects_missing_and_large_members_from_headers() {
    let mut tarball = Vec::new();
    {
        let encoder = flate2::write::GzEncoder::new(&mut tarball, flate2::Compression::fast());
        let mut builder = tar::Builder::new(encoder);
        let mut header = tar::Header::new_gnu();
        header.set_size(0);
        header.set_cksum();
        builder
            .append_data(&mut header, "pkg-1.0/empty.txt", std::io::empty())
            .unwrap();
        builder.into_inner().unwrap().finish().unwrap();
    }
    let mut file = tempfile::NamedTempFile::new().unwrap();
    file.write_all(&tarball).unwrap();
    file.flush().unwrap();
    assert!(matches!(
        list_members_nested_path("pkg-1.0.tar.gz", file.path(), &["pkg-1.0/missing.zip".to_owned()]),
        Err(ArchiveError::MemberNotFound)
    ));

    let mut tarball = Vec::new();
    {
        let mut encoder = flate2::write::GzEncoder::new(&mut tarball, flate2::Compression::fast());
        let mut header = tar::Header::new_gnu();
        header.set_path("pkg-1.0/big.zip").unwrap();
        header.set_entry_type(tar::EntryType::Regular);
        header.set_size(MAX_NESTED_ARCHIVE_SIZE + 1);
        header.set_cksum();
        encoder.write_all(header.as_bytes()).unwrap();
        encoder.finish().unwrap();
    }
    let mut file = tempfile::NamedTempFile::new().unwrap();
    file.write_all(&tarball).unwrap();
    file.flush().unwrap();
    assert!(matches!(
        list_members_nested_path("pkg-1.0.tar.gz", file.path(), &["pkg-1.0/big.zip".to_owned()]),
        Err(ArchiveError::NestedArchiveTooLarge { member, size, limit })
            if member == "pkg-1.0/big.zip" && size == MAX_NESTED_ARCHIVE_SIZE + 1 && limit == MAX_NESTED_ARCHIVE_SIZE
    ));
}

#[test]
fn test_archive_listing_rejects_too_many_entries() {
    let mut buf = Vec::new();
    {
        let mut zip = zip::ZipWriter::new(std::io::Cursor::new(&mut buf));
        let options = zip::write::SimpleFileOptions::default();
        for position in 0..=MAX_LISTED_ENTRIES {
            zip.start_file(format!("pkg/file-{position}.py"), options).unwrap();
            zip.write_all(b"").unwrap();
        }
        zip.finish().unwrap();
    }

    assert!(matches!(
        list_members("pkg-1.0-py3-none-any.whl", &buf),
        Err(ArchiveError::TooManyEntries(limit)) if limit == MAX_LISTED_ENTRIES
    ));
}

#[test]
fn test_text_preview_rejects_binary_members_and_invalid_utf8() {
    let mut file = tempfile::NamedTempFile::new().unwrap();
    file.write_all(&small_zip()).unwrap();
    file.flush().unwrap();
    assert!(matches!(
        read_text_member_chunk_nested_path(
            "pkg-1.0-py3-none-any.whl",
            file.path(),
            &[],
            "big.bin",
            0,
            DEFAULT_MEMBER_CHUNK,
        ),
        Err(ArchiveError::BinaryMember(path)) if path == "big.bin"
    ));

    let invalid = zip_with_file("bad.txt", &[0xff], zip::CompressionMethod::Stored);
    let mut file = tempfile::NamedTempFile::new().unwrap();
    file.write_all(&invalid).unwrap();
    file.flush().unwrap();
    assert!(matches!(
        read_text_member_chunk_nested_path("pkg-1.0-py3-none-any.whl", file.path(), &[], "bad.txt", 0, 1),
        Err(ArchiveError::BinaryMember(path)) if path == "bad.txt"
    ));

    assert!(matches!(
        read_text_member_chunk_nested_path(
            "pkg-1.0-py3-none-any.whl",
            file.path(),
            &[],
            "../bad.txt",
            0,
            DEFAULT_MEMBER_CHUNK,
        ),
        Err(ArchiveError::UnsafeMember(path)) if path == "../bad.txt"
    ));

    assert!(matches!(
        read_text_member_chunk_nested_path(
            "pkg-1.0-py3-none-any.whl",
            file.path(),
            &["../inner.zip".to_owned()],
            "bad.txt",
            0,
            DEFAULT_MEMBER_CHUNK,
        ),
        Err(ArchiveError::UnsafeMember(path)) if path == "../inner.zip"
    ));
}

#[test]
fn test_text_preview_keeps_chunk_boundaries_on_utf8_chars() {
    let archive = zip_with_file("ok.txt", "aé".as_bytes(), zip::CompressionMethod::Stored);
    let mut file = tempfile::NamedTempFile::new().unwrap();
    file.write_all(&archive).unwrap();
    file.flush().unwrap();

    let chunk =
        read_text_member_chunk_nested_path("pkg-1.0-py3-none-any.whl", file.path(), &[], "ok.txt", 0, 2).unwrap();
    assert_eq!(chunk.bytes, b"a");
    assert_eq!(chunk.next_offset, Some(1));
}

#[test]
fn test_zip_listing_rejects_unsafe_member_names() {
    let mut buf = Vec::new();
    {
        let mut zip = zip::ZipWriter::new(std::io::Cursor::new(&mut buf));
        let options = zip::write::SimpleFileOptions::default();
        zip.start_file("../bad.py", options).unwrap();
        zip.write_all(b"bad = True\n").unwrap();
        zip.finish().unwrap();
    }
    assert!(matches!(
        list_members("pkg-1.0-py3-none-any.whl", &buf),
        Err(ArchiveError::UnsafeMember(path)) if path == "../bad.py"
    ));
}

#[test]
fn test_read_zip_member_chunk_reports_next_offset() {
    let chunk = read_member_chunk(
        "pkg-1.0-py3-none-any.whl",
        &small_zip(),
        "big.bin",
        0,
        DEFAULT_MEMBER_CHUNK,
    )
    .unwrap();
    assert_eq!(chunk.bytes.len(), usize::try_from(DEFAULT_MEMBER_CHUNK).unwrap());
    assert_eq!(chunk.size, DEFAULT_MEMBER_CHUNK + 1);
    assert_eq!(chunk.next_offset, Some(DEFAULT_MEMBER_CHUNK));
}

#[test]
fn test_read_stored_zip_member_chunk_at_offset() {
    let archive = zip_with_file("pkg/mod.py", b"abcdef", zip::CompressionMethod::Stored);
    let chunk = read_member_chunk("pkg-1.0-py3-none-any.whl", &archive, "pkg/mod.py", 2, 3).unwrap();
    assert_eq!(chunk.bytes, b"cde");
    assert_eq!(chunk.next_offset, Some(5));
    assert!(matches!(
        read_member_chunk("pkg-1.0-py3-none-any.whl", &archive, "pkg/mod.py", 7, 3),
        Err(ArchiveError::InvalidRange { .. })
    ));
    assert!(matches!(
        read_member_chunk("pkg-1.0-py3-none-any.whl", &archive, "pkg/missing.py", 1, 3),
        Err(ArchiveError::MemberNotFound)
    ));
}

#[test]
fn test_read_zip_member_offset_beyond_size_is_rejected() {
    assert!(matches!(
        read_member_chunk(
            "pkg-1.0-py3-none-any.whl",
            &small_zip(),
            "big.bin",
            DEFAULT_MEMBER_CHUNK + 2,
            DEFAULT_MEMBER_CHUNK,
        ),
        Err(ArchiveError::InvalidRange { .. })
    ));
    assert!(matches!(
        read_member_chunk(
            "pkg-1.0-py3-none-any.whl",
            &small_zip(),
            "pkg/missing.py",
            0,
            DEFAULT_MEMBER_CHUNK,
        ),
        Err(ArchiveError::MemberNotFound)
    ));
}

#[test]
fn test_read_zip_member_crc_mismatch_is_read_error() {
    let mut archive = zip_with_file(
        "pkg/mod.py",
        b"print('x')\nprint('y')\n",
        zip::CompressionMethod::Deflated,
    );
    let name_len = usize::from(u16::from_le_bytes([archive[26], archive[27]]));
    let extra_len = usize::from(u16::from_le_bytes([archive[28], archive[29]]));
    archive[30 + name_len + extra_len] ^= 0xff;

    assert!(matches!(
        read_member_chunk(
            "pkg-1.0-py3-none-any.whl",
            &archive,
            "pkg/mod.py",
            0,
            DEFAULT_MEMBER_CHUNK
        ),
        Err(ArchiveError::Read(_))
    ));
    assert!(matches!(
        read_member_chunk(
            "pkg-1.0-py3-none-any.whl",
            &archive,
            "pkg/mod.py",
            1,
            DEFAULT_MEMBER_CHUNK,
        ),
        Err(ArchiveError::Read(_))
    ));
}

#[test]
fn test_read_stored_zip_member_seek_error_is_read_error() {
    let mut archive = zip_with_file("pkg/mod.py", b"abcdef", zip::CompressionMethod::Stored);
    archive[0] = 0;

    assert!(matches!(
        read_member_chunk(
            "pkg-1.0-py3-none-any.whl",
            &archive,
            "pkg/mod.py",
            1,
            DEFAULT_MEMBER_CHUNK
        ),
        Err(ArchiveError::Read(_))
    ));
}

#[test]
fn test_tar_listing_skips_directories_and_missing_member() {
    let mut tarball = Vec::new();
    {
        let encoder = flate2::write::GzEncoder::new(&mut tarball, flate2::Compression::fast());
        let mut builder = tar::Builder::new(encoder);
        let mut dir = tar::Header::new_gnu();
        dir.set_entry_type(tar::EntryType::Directory);
        dir.set_size(0);
        dir.set_cksum();
        builder.append_data(&mut dir, "pkg-1.0/", std::io::empty()).unwrap();
        let content = b"x = 1\n";
        let mut file = tar::Header::new_gnu();
        file.set_size(content.len() as u64);
        file.set_cksum();
        builder.append_data(&mut file, "pkg-1.0/mod.py", &content[..]).unwrap();
        builder.into_inner().unwrap().finish().unwrap();
    }
    let members = list_members("pkg-1.0.tar.gz", &tarball).unwrap();
    assert_eq!(members.len(), 1, "the directory entry must be skipped");
    assert_eq!(members[0].path, "pkg-1.0/mod.py");
    assert!(matches!(
        read_member("pkg-1.0.tar.gz", &tarball, "pkg-1.0/nope.py"),
        Err(ArchiveError::MemberNotFound)
    ));
    let chunk = read_member_chunk("pkg-1.0.tar.gz", &tarball, "pkg-1.0/mod.py", 0, 5).unwrap();
    assert_eq!(chunk.bytes, b"x = 1");

    let mut file = tempfile::NamedTempFile::new().unwrap();
    file.write_all(&tarball).unwrap();
    file.flush().unwrap();
    assert_eq!(
        list_members_nested_path("pkg-1.0.TAR.GZ", file.path(), &[]).unwrap(),
        vec![Member {
            path: "pkg-1.0/mod.py".to_owned(),
            size: 6,
            kind: MemberKind::Text,
            previewable: true,
        }]
    );
    assert_eq!(
        read_text_member_chunk_nested_path(
            "pkg-1.0.TAR.GZ",
            file.path(),
            &[],
            "pkg-1.0/mod.py",
            0,
            DEFAULT_MEMBER_CHUNK,
        )
        .unwrap()
        .bytes,
        b"x = 1\n"
    );
}
