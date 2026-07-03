use std::io::Write as _;

use crate::archive::{ArchiveError, MEMBER_LIMIT, list_members, read_member};

fn small_zip() -> Vec<u8> {
    let mut buf = Vec::new();
    {
        let mut zip = zip::ZipWriter::new(std::io::Cursor::new(&mut buf));
        let options = zip::write::SimpleFileOptions::default();
        zip.start_file("big.bin", options).unwrap();
        zip.write_all(&vec![0_u8; usize::try_from(MEMBER_LIMIT + 1).unwrap()])
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
fn test_read_zip_member_over_limit() {
    assert!(matches!(
        read_member("pkg-1.0-py3-none-any.whl", &small_zip(), "big.bin"),
        Err(ArchiveError::TooLarge)
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
}
