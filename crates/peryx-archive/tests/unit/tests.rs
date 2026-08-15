use std::io::Write as _;

use flate2::{Compression, write::GzEncoder};
use rstest::rstest;

use crate::{
    ArchiveError, ArchiveFormat, ArchiveProfile, Member, MemberKind, generic_format, generic_member_kind, list_members,
    list_members_nested_path, list_members_path, read_error, read_member, read_member_chunk, read_member_chunk_path,
    read_text_member_chunk_nested_path, safe_member_name, strip_ascii_suffix_ignore_case,
};

const BODY: &[u8] = b"body\n";

struct TestProfile;

impl ArchiveProfile for TestProfile {
    fn format(&self, name: &str) -> Option<ArchiveFormat> {
        generic_format(name)
    }

    fn member_kind(&self, path: &str) -> MemberKind {
        generic_member_kind(path)
    }
}

const PROFILE: TestProfile = TestProfile;

fn zip(entries: &[(&str, &[u8])], method: zip::CompressionMethod) -> Vec<u8> {
    let mut buf = Vec::new();
    {
        let mut archive = zip::ZipWriter::new(std::io::Cursor::new(&mut buf));
        for (path, body) in entries {
            archive
                .start_file(
                    path,
                    zip::write::SimpleFileOptions::default().compression_method(method),
                )
                .unwrap();
            archive.write_all(body).unwrap();
        }
        archive.finish().unwrap();
    }
    buf
}

fn zip_with(path: &str) -> Vec<u8> {
    zip(&[(path, BODY)], zip::CompressionMethod::Stored)
}

fn zip_with_symlink(path: &str) -> Vec<u8> {
    let mut bytes = Vec::new();
    {
        let mut archive = zip::ZipWriter::new(std::io::Cursor::new(&mut bytes));
        archive
            .add_symlink(path, "target", zip::write::SimpleFileOptions::default())
            .unwrap();
        archive.finish().unwrap();
    }
    bytes
}

fn tar(entries: &[(&str, &[u8])]) -> Vec<u8> {
    let mut archive = tar::Builder::new(Vec::new());
    for (path, body) in entries {
        let mut header = tar::Header::new_gnu();
        header.set_mode(0o644);
        header.set_size(body.len() as u64);
        header.set_cksum();
        archive.append_data(&mut header, path, *body).unwrap();
    }
    archive.into_inner().unwrap()
}

fn tar_gz(entries: &[(&str, &[u8])]) -> Vec<u8> {
    let mut encoder = GzEncoder::new(Vec::new(), Compression::default());
    encoder.write_all(&tar(entries)).unwrap();
    encoder.finish().unwrap()
}

fn tar_with_directory_entry(path: &str, body: &[u8]) -> Vec<u8> {
    let mut archive = tar::Builder::new(Vec::new());
    let mut directory = tar::Header::new_gnu();
    directory.set_entry_type(tar::EntryType::Directory);
    directory.set_mode(0o755);
    directory.set_size(0);
    directory.set_cksum();
    archive.append_data(&mut directory, "dir", std::io::empty()).unwrap();
    let mut file = tar::Header::new_gnu();
    file.set_mode(0o644);
    file.set_size(body.len() as u64);
    file.set_cksum();
    archive.append_data(&mut file, path, body).unwrap();
    archive.into_inner().unwrap()
}

fn tar_with_directory() -> Vec<u8> {
    tar_with_directory_entry("file.txt", BODY)
}

fn oversized_nested_tar() -> Vec<u8> {
    let mut header = tar::Header::new_gnu();
    header.set_path("inner.zip").unwrap();
    header.set_mode(0o644);
    header.set_size((128 << 20) + 1);
    header.set_cksum();
    let mut bytes = header.as_bytes().to_vec();
    bytes.extend_from_slice(&[0; 1024]);
    bytes
}

fn zip_with_declared_size(size: u32) -> Vec<u8> {
    let mut bytes = zip(&[("file.txt", BODY)], zip::CompressionMethod::Deflated);
    let central = bytes.windows(4).position(|window| window == b"PK\x01\x02").unwrap();
    bytes[central + 24..central + 28].copy_from_slice(&size.to_le_bytes());
    bytes
}

fn write_archive(bytes: &[u8]) -> (tempfile::TempDir, std::path::PathBuf) {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("archive");
    std::fs::write(&path, bytes).unwrap();
    (dir, path)
}

#[rstest]
#[case::readme("README", MemberKind::Text)]
#[case::license("LICENSE", MemberKind::Text)]
#[case::copying("COPYING", MemberKind::Text)]
#[case::authors("AUTHORS", MemberKind::Text)]
#[case::changelog("CHANGELOG", MemberKind::Text)]
#[case::makefile("Makefile", MemberKind::Text)]
#[case::nested_license("bundle/LICENSE", MemberKind::Text)]
#[case::nested_archive("bundle/source.tar.gz", MemberKind::Archive)]
#[case::extension_form_still_text("README.md", MemberKind::Text)]
#[case::exact_match_not_prefix("NOTICES", MemberKind::Unknown)]
#[case::other_extensionless_stays_unknown("notes", MemberKind::Unknown)]
#[case::binary_extension_stays_binary("logo.png", MemberKind::Binary)]
#[case::shared_library_stays_binary("module.so", MemberKind::Binary)]
#[case::wasm_stays_binary("module.wasm", MemberKind::Binary)]
#[case::webp_stays_binary("image.webp", MemberKind::Binary)]
#[case::unknown_extension_stays_unknown("payload.xyz", MemberKind::Unknown)]
fn test_conventional_extensionless_names_classify_as_text(#[case] path: &str, #[case] expected: MemberKind) {
    assert_eq!(
        list_members(&PROFILE, "bundle.zip", &zip_with(path)).unwrap(),
        vec![Member {
            path: path.to_owned(),
            size: BODY.len() as u64,
            kind: expected,
            previewable: expected == MemberKind::Text,
        }],
    );
}

#[rstest]
#[case("bundle.ZIP", Some(ArchiveFormat::Zip))]
#[case("bundle.tar", Some(ArchiveFormat::Tar))]
#[case("bundle.TAR.GZ", Some(ArchiveFormat::TarGz))]
#[case("bundle.tgz", Some(ArchiveFormat::TarGz))]
#[case("bundle.bin", None)]
fn test_generic_format_classifies_supported_suffixes(#[case] name: &str, #[case] expected: Option<ArchiveFormat>) {
    assert_eq!(generic_format(name), expected);
}

#[rstest]
#[case("NAME.TXT", ".txt", Some("NAME"))]
#[case("name", ".txt", None)]
#[case("x", "long", None)]
fn test_ascii_suffix_matching_is_case_insensitive(
    #[case] value: &str,
    #[case] suffix: &str,
    #[case] expected: Option<&str>,
) {
    assert_eq!(strip_ascii_suffix_ignore_case(value, suffix), expected);
}

#[test]
fn test_member_kind_names_match_serialized_values() {
    assert_eq!(
        [
            MemberKind::Archive.as_str(),
            MemberKind::Text.as_str(),
            MemberKind::Binary.as_str(),
            MemberKind::Unknown.as_str(),
        ],
        ["archive", "text", "binary", "unknown"]
    );
}

#[rstest]
#[case("dir/file.txt", true)]
#[case("", false)]
#[case("/absolute", false)]
#[case("../parent", false)]
#[case("dir\\file", false)]
fn test_safe_member_name_rejects_paths_that_escape_the_archive(#[case] path: &str, #[case] valid: bool) {
    assert_eq!(safe_member_name(path).is_ok(), valid);
}

#[test]
fn test_read_error_preserves_the_source_message() {
    assert!(matches!(
        read_error(std::io::Error::other("broken archive")),
        ArchiveError::Read(message) if message == "broken archive"
    ));
}

#[test]
fn test_archive_readers_share_listing_and_range_behavior() {
    for (name, bytes) in [
        (
            "bundle.zip",
            zip(&[("dir/file.txt", b"package")], zip::CompressionMethod::Deflated),
        ),
        ("bundle.tar", tar(&[("dir/file.txt", b"package")])),
        ("bundle.tar.gz", tar_gz(&[("dir/file.txt", b"package")])),
    ] {
        assert_eq!(
            list_members(&PROFILE, name, &bytes).unwrap(),
            vec![Member {
                path: "dir/file.txt".to_owned(),
                size: 7,
                kind: MemberKind::Text,
                previewable: true,
            }]
        );
        assert_eq!(read_member(&PROFILE, name, &bytes, "dir/file.txt").unwrap(), b"package");
        assert_eq!(
            read_member_chunk(&PROFILE, name, &bytes, "dir/file.txt", 1, 4)
                .unwrap()
                .bytes,
            b"acka"
        );
    }
}

#[test]
fn test_archive_listings_skip_directories() {
    let mut zip_bytes = Vec::new();
    {
        let mut archive = zip::ZipWriter::new(std::io::Cursor::new(&mut zip_bytes));
        archive
            .add_directory("dir/", zip::write::SimpleFileOptions::default())
            .unwrap();
        archive
            .start_file("file.txt", zip::write::SimpleFileOptions::default())
            .unwrap();
        archive.write_all(BODY).unwrap();
        archive.finish().unwrap();
    }
    assert_eq!(
        list_members(&PROFILE, "bundle.zip", &zip_bytes).unwrap()[0].path,
        "file.txt"
    );
    assert_eq!(
        list_members(&PROFILE, "bundle.tar", &tar_with_directory()).unwrap()[0].path,
        "file.txt"
    );
    assert_eq!(
        read_member(&PROFILE, "bundle.tar", &tar_with_directory(), "file.txt").unwrap(),
        BODY
    );
}

#[test]
fn test_file_backed_archive_readers_share_listing_and_range_behavior() {
    for (name, bytes) in [
        (
            "bundle.zip",
            zip(&[("file.txt", b"package")], zip::CompressionMethod::Stored),
        ),
        ("bundle.tar", tar(&[("file.txt", b"package")])),
        ("bundle.tar.gz", tar_gz(&[("file.txt", b"package")])),
    ] {
        let (_dir, path) = write_archive(&bytes);
        assert_eq!(list_members_path(&PROFILE, name, &path).unwrap()[0].path, "file.txt");
        let chunk = read_member_chunk_path(&PROFILE, name, &path, "file.txt", 2, 3).unwrap();
        assert_eq!(
            (chunk.bytes, chunk.size, chunk.offset, chunk.next_offset),
            (b"cka".to_vec(), 7, 2, Some(5))
        );
    }
}

#[rstest]
#[case(zip::CompressionMethod::Stored)]
#[case(zip::CompressionMethod::Deflated)]
fn test_nested_zip_members_are_listed_and_read(#[case] method: zip::CompressionMethod) {
    let inner = zip(&[("text.txt", "aébc".as_bytes())], zip::CompressionMethod::Deflated);
    let outer = zip(&[("inner.zip", &inner)], method);
    let (_dir, path) = write_archive(&outer);
    let containers = ["inner.zip".to_owned()];

    assert_eq!(
        list_members_nested_path(&PROFILE, "outer.zip", &path, &containers).unwrap()[0].path,
        "text.txt"
    );
    let chunk =
        read_text_member_chunk_nested_path(&PROFILE, "outer.zip", &path, &containers, "text.txt", 2, 2).unwrap();
    assert_eq!(
        (chunk.bytes, chunk.offset, chunk.next_offset),
        (b"b".to_vec(), 3, Some(4))
    );
}

#[test]
fn test_text_chunks_trim_an_incomplete_trailing_character() {
    let bytes = zip(&[("text.txt", "abé".as_bytes())], zip::CompressionMethod::Deflated);
    let (_dir, path) = write_archive(&bytes);
    let chunk = read_text_member_chunk_nested_path(&PROFILE, "bundle.zip", &path, &[], "text.txt", 0, 3).unwrap();
    assert_eq!((chunk.bytes, chunk.next_offset), (b"ab".to_vec(), Some(2)));
}

#[test]
fn test_text_chunks_reject_binary_content_and_binary_members() {
    for (member, body) in [("text.txt", &[0xff][..]), ("image.png", BODY)] {
        let bytes = zip(&[(member, body)], zip::CompressionMethod::Deflated);
        let (_dir, path) = write_archive(&bytes);
        assert!(matches!(
            read_text_member_chunk_nested_path(&PROFILE, "bundle.zip", &path, &[], member, 0, 8),
            Err(ArchiveError::BinaryMember(name)) if name == member
        ));
    }
}

#[rstest]
#[case("inner.tar", tar(&[("text.txt", b"package")]))]
#[case("inner.tar.gz", tar_gz(&[("text.txt", b"package")]))]
fn test_nested_tar_members_are_read(#[case] inner_name: &str, #[case] inner: Vec<u8>) {
    let outer = zip(&[(inner_name, &inner)], zip::CompressionMethod::Stored);
    let (_dir, path) = write_archive(&outer);
    assert_eq!(
        read_text_member_chunk_nested_path(&PROFILE, "outer.zip", &path, &[inner_name.to_owned()], "text.txt", 0, 4,)
            .unwrap()
            .bytes,
        b"pack"
    );
}

#[rstest]
#[case("outer.tar", tar(&[("inner.zip", zip_with("text.txt").as_slice())]))]
#[case("outer.tar.gz", tar_gz(&[("inner.zip", zip_with("text.txt").as_slice())]))]
#[case("outer.tar", tar_with_directory_entry("inner.zip", &zip_with("text.txt")))]
fn test_nested_members_are_extracted_from_tar_parents(#[case] outer_name: &str, #[case] outer: Vec<u8>) {
    let (_dir, path) = write_archive(&outer);
    assert_eq!(
        list_members_nested_path(&PROFILE, outer_name, &path, &["inner.zip".to_owned()]).unwrap()[0].path,
        "text.txt"
    );
}

#[test]
fn test_archive_readers_report_invalid_inputs() {
    let bytes = zip_with("file.txt");
    assert!(matches!(
        read_member_chunk(&PROFILE, "bundle.zip", &bytes, "file.txt", 6, 1),
        Err(ArchiveError::InvalidRange { offset: 6, size: 5 })
    ));
    assert!(matches!(
        read_member(&PROFILE, "bundle.zip", &bytes, "missing.txt"),
        Err(ArchiveError::MemberNotFound)
    ));
    assert!(matches!(
        list_members(&PROFILE, "bundle.bin", &bytes),
        Err(ArchiveError::Unsupported)
    ));
    assert!(matches!(
        read_member_chunk(&PROFILE, "bundle.bin", &bytes, "file.txt", 0, 1),
        Err(ArchiveError::Unsupported)
    ));
    let tar = tar(&[("other.txt", BODY)]);
    assert!(matches!(
        read_member_chunk(&PROFILE, "bundle.tar", &tar, "missing.txt", 0, 1),
        Err(ArchiveError::MemberNotFound)
    ));
    assert!(matches!(
        read_member_chunk(&PROFILE, "bundle.tar", &tar, "other.txt", 6, 1),
        Err(ArchiveError::InvalidRange { offset: 6, size: 5 })
    ));
    let mut corrupt = zip_with("file.txt");
    let central = corrupt.windows(4).position(|window| window == b"PK\x01\x02").unwrap();
    corrupt[central + 10..central + 12].copy_from_slice(&99_u16.to_le_bytes());
    for offset in [0, 1] {
        assert!(matches!(
            read_member_chunk(&PROFILE, "bundle.zip", &corrupt, "file.txt", offset, 1),
            Err(ArchiveError::Read(_))
        ));
    }
    let mut malformed_local_header = zip_with("file.txt");
    malformed_local_header[..4].copy_from_slice(b"nope");
    assert!(matches!(
        read_member_chunk(&PROFILE, "bundle.zip", &malformed_local_header, "file.txt", 0, 1,),
        Err(ArchiveError::Read(_))
    ));
    assert!(matches!(
        read_member(
            &PROFILE,
            "bundle.zip",
            &zip(&[("file.txt", BODY)], zip::CompressionMethod::Deflated),
            "missing.txt",
        ),
        Err(ArchiveError::MemberNotFound)
    ));
    assert!(matches!(
        read_member_chunk(
            &PROFILE,
            "bundle.zip",
            &zip_with_declared_size((512 << 20) + 1),
            "file.txt",
            (512 << 20) + 1,
            1,
        ),
        Err(ArchiveError::InvalidRange {
            offset: 536_870_913,
            size: 536_870_912,
        })
    ));
}

#[rstest]
#[case::streaming(0)]
#[case::seeked(1)]
fn test_zip_member_reads_reject_symlinks(#[case] offset: u64) {
    assert!(matches!(
        read_member_chunk(
            &PROFILE,
            "bundle.zip",
            &zip_with_symlink("link.txt"),
            "link.txt",
            offset,
            1
        ),
        Err(ArchiveError::MemberNotFound)
    ));
}

#[test]
fn test_nested_zip_rejects_a_symlink_container() {
    let (_dir, path) = write_archive(&zip_with_symlink("link.zip"));
    assert!(matches!(
        list_members_nested_path(&PROFILE, "outer.zip", &path, &["link.zip".to_owned()]),
        Err(ArchiveError::MemberNotFound)
    ));
}

#[test]
fn test_archive_listing_rejects_more_than_the_entry_limit() {
    let mut bytes = Vec::new();
    {
        let mut archive = zip::ZipWriter::new(std::io::Cursor::new(&mut bytes));
        for index in 0..=10_000 {
            archive
                .start_file(format!("{index:05}.txt"), zip::write::SimpleFileOptions::default())
                .unwrap();
        }
        archive.finish().unwrap();
    }
    assert!(matches!(
        list_members(&PROFILE, "bundle.zip", &bytes),
        Err(ArchiveError::TooManyEntries(10_000))
    ));
}

#[test]
fn test_nested_archive_reader_rejects_unsafe_and_excessive_paths() {
    let outer = zip_with("text.txt");
    let (_dir, path) = write_archive(&outer);
    assert!(matches!(
        read_text_member_chunk_nested_path(&PROFILE, "outer.zip", &path, &[], "../text.txt", 0, 1),
        Err(ArchiveError::UnsafeMember(_))
    ));
    assert!(matches!(
        list_members_nested_path(&PROFILE, "outer.zip", &path, &vec!["inner.zip".to_owned(); 9]),
        Err(ArchiveError::NestingTooDeep { .. })
    ));
    assert!(matches!(
        list_members_nested_path(&PROFILE, "outer.zip", &path, &["inner.bin".to_owned()]),
        Err(ArchiveError::UnsupportedNestedArchive(name)) if name == "inner.bin"
    ));
    assert!(matches!(
        list_members_nested_path(&PROFILE, "outer.zip", &path, &["inner.zip".to_owned()]),
        Err(ArchiveError::MemberNotFound)
    ));
    let (_dir, path) = write_archive(&tar(&[("other.zip", &zip_with("text.txt"))]));
    assert!(matches!(
        list_members_nested_path(&PROFILE, "outer.tar", &path, &["inner.zip".to_owned()]),
        Err(ArchiveError::MemberNotFound)
    ));
    let (_dir, path) = write_archive(&oversized_nested_tar());
    assert!(matches!(
        list_members_nested_path(&PROFILE, "outer.tar", &path, &["inner.zip".to_owned()]),
        Err(ArchiveError::NestedArchiveTooLarge { size, .. }) if size == (128 << 20) + 1
    ));
}
