use rstest::rstest;

use super::*;

fn eocd(directory_len: u32, directory_offset: u32, comment_len: u16) -> Vec<u8> {
    let mut bytes = vec![0_u8; ZIP_EOCD_LEN];
    bytes[..4].copy_from_slice(&ZIP_EOCD_SIGNATURE);
    bytes[12..16].copy_from_slice(&directory_len.to_le_bytes());
    bytes[16..20].copy_from_slice(&directory_offset.to_le_bytes());
    bytes[20..22].copy_from_slice(&comment_len.to_le_bytes());
    bytes
}

#[rstest]
#[case::comment_length_mismatch(0, 0, 1, u64::MAX)]
#[case::zip64_length(u32::MAX, 0, 0, u64::MAX)]
#[case::zip64_offset(0, u32::MAX, 0, u64::MAX)]
#[case::over_budget(u32::try_from(MAX_CENTRAL_DIRECTORY_BYTES).unwrap() + 1, 0, 0, u64::MAX)]
#[case::past_end_of_artifact(10, 5, 0, 14)]
fn test_central_directory_rejects_unusable_spans(
    #[case] directory_len: u32,
    #[case] directory_offset: u32,
    #[case] comment_len: u16,
    #[case] artifact_len: u64,
) {
    let tail = eocd(directory_len, directory_offset, comment_len);

    assert!(central_directory(&tail, artifact_len).is_none());
}

#[test]
fn test_central_directory_accepts_a_span_within_the_artifact() {
    let tail = eocd(10, 5, 0);

    let directory = central_directory(&tail, 15).expect("the span ends at the artifact length");

    assert_eq!((directory.offset, directory.len), (5, 10));
}

#[test]
fn test_find_central_directory_entry_rejects_malformed_and_missing_entries() {
    assert!(matches!(
        find_central_directory_entry(&[0; 46], "pkg-1.0.dist-info/METADATA"),
        DirectoryEntrySearch::Invalid
    ));

    let mut truncated = [0_u8; 46];
    truncated[..4].copy_from_slice(&ZIP_CENTRAL_SIGNATURE);
    truncated[28..30].copy_from_slice(&10_u16.to_le_bytes());
    assert!(matches!(
        find_central_directory_entry(&truncated, "pkg-1.0.dist-info/METADATA"),
        DirectoryEntrySearch::Invalid
    ));

    assert!(matches!(
        find_central_directory_entry(&[], "pkg-1.0.dist-info/METADATA"),
        DirectoryEntrySearch::Missing
    ));
}
