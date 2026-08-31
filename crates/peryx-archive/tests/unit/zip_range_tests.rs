use rstest::rstest;

use super::*;

const NAME: &str = "pkg-1.0.dist-info/METADATA";
const LOCAL_OFFSET: u64 = 7;

fn crc32(bytes: &[u8]) -> u32 {
    let mut crc = flate2::Crc::new();
    crc.update(bytes);
    crc.sum()
}

fn eocd(directory_len: u32, directory_offset: u32, comment_len: u16) -> Vec<u8> {
    let mut bytes = vec![0_u8; EOCD_LEN];
    bytes[..4].copy_from_slice(&EOCD_SIGNATURE);
    bytes[12..16].copy_from_slice(&directory_len.to_le_bytes());
    bytes[16..20].copy_from_slice(&directory_offset.to_le_bytes());
    bytes[20..22].copy_from_slice(&comment_len.to_le_bytes());
    bytes
}

#[rstest]
#[case::comment_length_mismatch(0, 0, 1, u64::MAX)]
#[case::zip64_length(u32::MAX, 0, 0, u64::MAX)]
#[case::zip64_offset(0, u32::MAX, 0, u64::MAX)]
#[case::over_budget(u32::try_from(MAX_ZIP_CENTRAL_DIRECTORY_BYTES).unwrap() + 1, 0, 0, u64::MAX)]
#[case::past_end_of_artifact(10, 5, 0, 14)]
fn test_zip_central_directory_rejects_unusable_spans(
    #[case] directory_len: u32,
    #[case] directory_offset: u32,
    #[case] comment_len: u16,
    #[case] artifact_len: u64,
) {
    let tail = eocd(directory_len, directory_offset, comment_len);

    assert_eq!(zip_central_directory(&tail, artifact_len), None);
}

#[test]
fn test_zip_central_directory_rejects_a_tail_shorter_than_the_record() {
    assert_eq!(zip_central_directory(&[0; EOCD_LEN - 1], u64::MAX), None);
}

#[test]
fn test_zip_central_directory_accepts_a_span_within_the_artifact() {
    let tail = eocd(10, 5, 0);

    assert_eq!(
        zip_central_directory(&tail, 15),
        Some(ZipCentralDirectory { offset: 5, len: 10 })
    );
}

/// A stored central-directory entry for `name`, whose fields a case can then bend.
fn central_entry(name: &str, payload: &[u8]) -> Vec<u8> {
    let size = u32::try_from(payload.len()).unwrap();
    let mut bytes = vec![0_u8; CENTRAL_ENTRY_LEN];
    bytes[..4].copy_from_slice(&CENTRAL_SIGNATURE);
    bytes[16..20].copy_from_slice(&crc32(payload).to_le_bytes());
    bytes[20..24].copy_from_slice(&size.to_le_bytes());
    bytes[24..28].copy_from_slice(&size.to_le_bytes());
    bytes[28..30].copy_from_slice(&u16::try_from(name.len()).unwrap().to_le_bytes());
    bytes[42..46].copy_from_slice(&u32::try_from(LOCAL_OFFSET).unwrap().to_le_bytes());
    bytes.extend_from_slice(name.as_bytes());
    bytes
}

fn stored_entry(payload: &[u8]) -> ZipEntry {
    ZipEntry {
        name: NAME.to_owned(),
        flags: 0,
        compression_method: COMPRESSION_STORED,
        crc32: crc32(payload),
        compressed_size: payload.len() as u64,
        uncompressed_size: payload.len() as u64,
        local_header_offset: LOCAL_OFFSET,
    }
}

fn deflated_entry(payload: &[u8]) -> ZipEntry {
    ZipEntry {
        compression_method: COMPRESSION_DEFLATED,
        compressed_size: deflate(payload).len() as u64,
        ..stored_entry(payload)
    }
}

#[test]
fn test_find_zip_entry_reads_every_field_it_cross_checks() {
    let mut directory = central_entry(NAME, b"body");
    directory[8..10].copy_from_slice(&FLAG_DATA_DESCRIPTOR.to_le_bytes());
    directory[10..12].copy_from_slice(&COMPRESSION_DEFLATED.to_le_bytes());

    assert_eq!(
        find_zip_entry(&directory, NAME),
        ZipEntrySearch::Found(ZipEntry {
            flags: FLAG_DATA_DESCRIPTOR,
            compression_method: COMPRESSION_DEFLATED,
            ..stored_entry(b"body")
        })
    );
}

#[test]
fn test_find_zip_entry_walks_past_the_members_it_was_not_asked_for() {
    let mut directory = central_entry("pkg-1.0.dist-info/WHEEL", b"other");
    directory.extend_from_slice(&central_entry(NAME, b"body"));

    assert_eq!(
        find_zip_entry(&directory, NAME),
        ZipEntrySearch::Found(stored_entry(b"body"))
    );
}

#[rstest]
#[case::encrypted(1)]
#[case::strong_encryption(1 << 6)]
#[case::masked_local_values(1 << 13)]
fn test_find_zip_entry_refuses_flags_it_cannot_honour(#[case] flags: u16) {
    let mut directory = central_entry(NAME, b"body");
    directory[8..10].copy_from_slice(&flags.to_le_bytes());

    assert_eq!(find_zip_entry(&directory, NAME), ZipEntrySearch::Unsupported);
}

#[rstest]
#[case::compressed_size(20)]
#[case::uncompressed_size(24)]
#[case::local_header_offset(42)]
fn test_find_zip_entry_refuses_a_zip64_field(#[case] offset: usize) {
    let mut directory = central_entry(NAME, b"body");
    directory[offset..offset + 4].copy_from_slice(&u32::MAX.to_le_bytes());

    assert_eq!(find_zip_entry(&directory, NAME), ZipEntrySearch::Unsupported);
}

#[test]
fn test_find_zip_entry_rejects_an_unsigned_entry() {
    assert_eq!(find_zip_entry(&[0; CENTRAL_ENTRY_LEN], NAME), ZipEntrySearch::Invalid);
}

#[test]
fn test_find_zip_entry_rejects_an_entry_running_past_the_directory() {
    let directory = central_entry(NAME, b"body");

    assert_eq!(
        find_zip_entry(&directory[..=CENTRAL_ENTRY_LEN], NAME),
        ZipEntrySearch::Invalid
    );
}

#[test]
fn test_find_zip_entry_reports_a_directory_without_the_member() {
    assert_eq!(find_zip_entry(&[], NAME), ZipEntrySearch::Missing);
}

/// A local file header agreeing with `entry` in every cross-checked field.
fn local_header(entry: &ZipEntry) -> Vec<u8> {
    let mut bytes = vec![0_u8; LOCAL_HEADER_LEN];
    bytes[..4].copy_from_slice(&LOCAL_SIGNATURE);
    bytes[6..8].copy_from_slice(&entry.flags.to_le_bytes());
    bytes[8..10].copy_from_slice(&entry.compression_method.to_le_bytes());
    bytes[14..18].copy_from_slice(&entry.crc32.to_le_bytes());
    bytes[18..22].copy_from_slice(&u32::try_from(entry.compressed_size).unwrap().to_le_bytes());
    bytes[22..26].copy_from_slice(&u32::try_from(entry.uncompressed_size).unwrap().to_le_bytes());
    bytes[26..28].copy_from_slice(&u16::try_from(entry.name.len()).unwrap().to_le_bytes());
    bytes.extend_from_slice(entry.name.as_bytes());
    bytes
}

#[test]
fn test_data_start_clears_a_local_header_that_agrees() {
    let entry = stored_entry(b"body");
    let mut header = local_header(&entry);
    header[28..30].copy_from_slice(&11_u16.to_le_bytes());

    assert_eq!(
        entry.data_start(&header),
        Ok(LOCAL_OFFSET + entry.local_header_len() + 11)
    );
}

#[test]
fn test_data_start_reads_the_central_values_for_a_streamed_member() {
    let entry = ZipEntry {
        flags: FLAG_DATA_DESCRIPTOR,
        ..stored_entry(b"body")
    };
    let mut header = local_header(&entry);
    header[14..26].fill(0);

    assert_eq!(entry.data_start(&header), Ok(LOCAL_OFFSET + entry.local_header_len()));
}

#[test]
fn test_data_start_rejects_a_short_header() {
    let entry = stored_entry(b"body");
    let header = local_header(&entry);
    let truncated = &header[..header.len() - 1];

    assert_eq!(
        entry.data_start(truncated).unwrap_err().to_string(),
        format!(
            "local header of {NAME:?} is {} bytes, short of the {} it declares",
            truncated.len(),
            header.len()
        )
    );
}

#[test]
fn test_data_start_rejects_an_unsigned_header() {
    let entry = stored_entry(b"body");
    let mut header = local_header(&entry);
    header[..4].fill(0);

    assert_eq!(
        entry.data_start(&header).unwrap_err().to_string(),
        format!("local header of {NAME:?} carries no local file signature")
    );
}

#[rstest]
#[case::flags("general-purpose flags", 6, 4, 0)]
#[case::compression_method("compression method", 8, 8, 0)]
#[case::file_name_length("file name length", 26, 3, NAME.len() as u64)]
fn test_data_start_rejects_a_local_u16_the_directory_contradicts(
    #[case] field: &str,
    #[case] offset: usize,
    #[case] value: u16,
    #[case] expected: u64,
) {
    let entry = stored_entry(b"body");
    let mut header = local_header(&entry);
    header[offset..offset + 2].copy_from_slice(&value.to_le_bytes());

    assert_eq!(
        entry.data_start(&header).unwrap_err().to_string(),
        format!("{field} is {value} in the local header and {expected} in the central directory")
    );
}

#[rstest]
#[case::crc32("CRC-32", 14, 1, u64::from(crc32(b"body")))]
#[case::compressed_size("compressed size", 18, 9, 4)]
#[case::uncompressed_size("uncompressed size", 22, 9, 4)]
fn test_data_start_rejects_a_local_u32_the_directory_contradicts(
    #[case] field: &str,
    #[case] offset: usize,
    #[case] value: u32,
    #[case] expected: u64,
) {
    let entry = stored_entry(b"body");
    let mut header = local_header(&entry);
    header[offset..offset + 4].copy_from_slice(&value.to_le_bytes());

    assert_eq!(
        entry.data_start(&header).unwrap_err().to_string(),
        format!("{field} is {value} in the local header and {expected} in the central directory")
    );
}

#[test]
fn test_data_start_rejects_a_local_header_naming_another_member() {
    let entry = stored_entry(b"body");
    let mut header = local_header(&entry);
    header[LOCAL_HEADER_LEN] = b'X';
    let renamed = format!("X{}", &NAME[1..]);

    assert_eq!(
        entry.data_start(&header).unwrap_err().to_string(),
        format!("local header names {renamed:?} where the central directory names {NAME:?}")
    );
}

fn deflate(bytes: &[u8]) -> Vec<u8> {
    let mut encoder = flate2::write::DeflateEncoder::new(Vec::new(), flate2::Compression::default());
    std::io::Write::write_all(&mut encoder, bytes).unwrap();
    encoder.finish().unwrap()
}

#[test]
fn test_decode_returns_a_stored_member_that_matches_every_declaration() {
    assert_eq!(stored_entry(b"body").decode(b"body"), Ok(b"body".to_vec()));
}

#[test]
fn test_decode_returns_a_deflated_member_that_matches_every_declaration() {
    assert_eq!(deflated_entry(b"body").decode(&deflate(b"body")), Ok(b"body".to_vec()));
}

#[test]
fn test_decode_refuses_a_compression_method_it_does_not_implement() {
    let entry = ZipEntry {
        compression_method: 99,
        ..stored_entry(b"body")
    };

    assert_eq!(
        entry.decode(b"body").unwrap_err().to_string(),
        "compression method 99 is not supported"
    );
}

#[test]
fn test_decode_refuses_a_stored_member_whose_declared_sizes_differ() {
    let entry = ZipEntry {
        uncompressed_size: 5,
        ..stored_entry(b"body")
    };

    assert_eq!(
        entry.decode(b"body").unwrap_err().to_string(),
        "stored member declares 4 compressed and 5 uncompressed bytes"
    );
}

#[test]
fn test_decode_refuses_a_stream_that_ends_early() {
    let compressed = deflate(b"body");
    let truncated = &compressed[..compressed.len() - 2];

    assert_eq!(
        deflated_entry(b"body").decode(truncated).unwrap_err().to_string(),
        "member decoded to 3 bytes where the central directory declares 4"
    );
}

#[test]
fn test_decode_refuses_a_stream_that_does_not_deflate() {
    let payload = b"Metadata-Version: 2.1\nName: peryxpkg\nVersion: 1.0\n";
    let mut compressed = deflate(payload);
    compressed[0] ^= 0x07;

    assert_eq!(
        deflated_entry(payload).decode(&compressed).unwrap_err().to_string(),
        "member does not decode: corrupt deflate stream"
    );
}

#[test]
fn test_decode_refuses_a_member_shorter_than_it_declares() {
    assert_eq!(
        stored_entry(b"body").decode(b"bod").unwrap_err().to_string(),
        "member decoded to 3 bytes where the central directory declares 4"
    );
}

#[test]
fn test_decode_refuses_a_member_that_expands_past_its_declaration() {
    assert_eq!(
        deflated_entry(b"body")
            .decode(&deflate(b"body!"))
            .unwrap_err()
            .to_string(),
        "member decoded to 5 bytes where the central directory declares 4"
    );
}

#[test]
fn test_decode_refuses_bytes_after_the_compression_stream() {
    let mut compressed = deflate(b"body");
    compressed.extend_from_slice(b"trailer");

    assert_eq!(
        deflated_entry(b"body").decode(&compressed).unwrap_err().to_string(),
        "7 bytes follow the member's compression stream"
    );
}

#[test]
fn test_decode_refuses_bytes_that_do_not_hash_to_the_declared_crc() {
    assert_eq!(
        stored_entry(b"body").decode(b"BODY").unwrap_err().to_string(),
        format!(
            "member decoded to CRC-32 {:#010x} where the central directory declares {:#010x}",
            crc32(b"BODY"),
            crc32(b"body")
        )
    );
}
