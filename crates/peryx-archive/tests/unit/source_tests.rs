use std::io::{Cursor, Read as _, Seek as _, SeekFrom};

use rstest::rstest;

use super::{ArchiveSource, resolve_container_stack};
use crate::ArchiveError;
use crate::tests::{BODY, PROFILE, write_archive, zip};

const CONTAINER: &str = "inner.zip";

fn inner() -> Vec<u8> {
    zip(&[("text.txt", BODY)], zip::CompressionMethod::Stored)
}

/// Offset of the outer archive's local header for its first member.
fn local_header(outer: &[u8]) -> usize {
    outer.windows(4).position(|window| window == b"PK\x03\x04").unwrap()
}

/// Offset of the outer archive's central-directory entry; a stored inner zip carries its own
/// entry earlier in the bytes, so the outer's is the last one.
fn central_entry(outer: &[u8]) -> usize {
    outer.windows(4).rposition(|window| window == b"PK\x01\x02").unwrap()
}

#[test]
fn test_stored_container_is_read_in_place() {
    let inner = inner();
    let outer = zip(&[(CONTAINER, &inner)], zip::CompressionMethod::Stored);
    let (_dir, path) = write_archive(&outer);
    let data_start = zip::ZipArchive::new(Cursor::new(&outer))
        .unwrap()
        .by_index(0)
        .unwrap()
        .data_start();

    let resolved = resolve_container_stack(&PROFILE, "outer.zip", &path, &[CONTAINER.to_owned()]).unwrap();

    assert_eq!(
        (resolved.source.path, resolved.source.start, resolved.source.len),
        (path, data_start.unwrap(), Some(inner.len() as u64))
    );
}

#[test]
fn test_compressed_container_is_spilled_to_its_own_file() {
    let outer = zip(&[(CONTAINER, &inner())], zip::CompressionMethod::Deflated);
    let (_dir, path) = write_archive(&outer);

    let resolved = resolve_container_stack(&PROFILE, "outer.zip", &path, &[CONTAINER.to_owned()]).unwrap();

    assert_ne!(resolved.source.path, path);
    assert_eq!((resolved.source.start, resolved.source.len), (0, None));
}

#[test]
fn test_stored_container_declaring_another_size_is_rejected_as_truncated() {
    let inner = inner();
    let mut outer = zip(&[(CONTAINER, &inner)], zip::CompressionMethod::Stored);
    let declared = (u32::try_from(inner.len()).unwrap() + 1).to_le_bytes();
    let local = local_header(&outer);
    outer[local + 22..local + 26].copy_from_slice(&declared);
    let central = central_entry(&outer);
    outer[central + 24..central + 28].copy_from_slice(&declared);
    let (_dir, path) = write_archive(&outer);

    assert!(matches!(
        resolve_container_stack(&PROFILE, "outer.zip", &path, &[CONTAINER.to_owned()]),
        Err(ArchiveError::TruncatedMember { expected, actual })
            if expected == inner.len() as u64 + 1 && actual == inner.len() as u64
    ));
}

#[test]
fn test_encrypted_container_is_rejected_before_it_is_read() {
    let mut outer = zip(&[(CONTAINER, &inner())], zip::CompressionMethod::Stored);
    let local = local_header(&outer);
    outer[local + 6] |= 1;
    let central = central_entry(&outer);
    outer[central + 8] |= 1;
    let (_dir, path) = write_archive(&outer);

    assert!(matches!(
        resolve_container_stack(&PROFILE, "outer.zip", &path, &[CONTAINER.to_owned()]),
        Err(ArchiveError::Read(message)) if message.contains("assword")
    ));
}

#[test]
fn test_slice_reads_stop_at_the_slice_end() {
    let (_dir, path) = write_archive(b"0123456789");
    let mut reader = ArchiveSource::new(path).slice(2, 3).open().unwrap();
    let mut bytes = Vec::new();

    reader.read_to_end(&mut bytes).unwrap();

    assert_eq!(bytes, b"234");
}

#[rstest]
#[case::start_within(SeekFrom::Start(1), SeekFrom::Start(3), 3, b"56")]
#[case::start_past_the_end(SeekFrom::Start(1), SeekFrom::Start(9), 5, b"")]
#[case::current_forward(SeekFrom::Start(1), SeekFrom::Current(1), 2, b"456")]
#[case::current_in_place(SeekFrom::Start(1), SeekFrom::Current(0), 1, b"3456")]
#[case::current_backward(SeekFrom::Start(3), SeekFrom::Current(-1), 2, b"456")]
#[case::current_before_the_start(SeekFrom::Start(1), SeekFrom::Current(-5), 0, b"23456")]
#[case::current_past_the_end(SeekFrom::Start(1), SeekFrom::Current(10), 5, b"")]
#[case::end_backward(SeekFrom::Start(0), SeekFrom::End(-2), 3, b"56")]
#[case::end_before_the_start(SeekFrom::Start(0), SeekFrom::End(-9), 0, b"23456")]
#[case::end_in_place(SeekFrom::Start(0), SeekFrom::End(0), 5, b"")]
#[case::end_forward(SeekFrom::Start(0), SeekFrom::End(3), 5, b"")]
fn test_slice_seeks_stay_within_the_slice(
    #[case] first: SeekFrom,
    #[case] second: SeekFrom,
    #[case] position: u64,
    #[case] rest: &[u8],
) {
    let (_dir, path) = write_archive(b"0123456789");
    let mut reader = ArchiveSource::new(path).slice(2, 5).open().unwrap();
    reader.seek(first).unwrap();

    assert_eq!(reader.seek(second).unwrap(), position);

    let mut bytes = Vec::new();
    reader.read_to_end(&mut bytes).unwrap();
    assert_eq!(bytes, rest);
}
