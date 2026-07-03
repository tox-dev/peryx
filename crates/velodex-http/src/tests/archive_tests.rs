use std::io::Write as _;

use crate::archive::{
    ArchiveError, DEFAULT_MEMBER_CHUNK, Member, list_members, list_members_path, read_member, read_member_chunk,
    read_member_chunk_path,
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
}

#[test]
fn test_list_zip_members() {
    let members = list_members("pkg-1.0-py3-none-any.whl", &small_zip()).unwrap();
    assert_eq!(
        members,
        vec![Member {
            path: "big.bin".to_owned(),
            size: DEFAULT_MEMBER_CHUNK + 1,
        }]
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
}
