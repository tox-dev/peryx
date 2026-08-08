use super::*;

#[test]
fn test_central_directory_rejects_comment_mismatch_and_zip64() {
    let mut eocd = [0_u8; ZIP_EOCD_LEN];
    eocd[..4].copy_from_slice(&ZIP_EOCD_SIGNATURE);
    eocd[20] = 1;
    assert!(central_directory(&eocd).is_none());

    let mut eocd = [0_u8; ZIP_EOCD_LEN];
    eocd[..4].copy_from_slice(&ZIP_EOCD_SIGNATURE);
    eocd[12..16].copy_from_slice(&u32::MAX.to_le_bytes());
    assert!(central_directory(&eocd).is_none());
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
