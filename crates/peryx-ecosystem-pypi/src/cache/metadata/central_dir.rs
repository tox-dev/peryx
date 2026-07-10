//! A minimal ZIP central-directory reader: locate one member (`METADATA`) and its byte span so the
//! resolver can range-read it out of a wheel without downloading the archive. Pure byte parsing.

pub(super) const ZIP_TAIL_BYTES: u64 = 66_000;
pub(super) const ZIP_EOCD_LEN: usize = 22;
pub(super) const ZIP_EOCD_SIGNATURE: [u8; 4] = [0x50, 0x4b, 0x05, 0x06];
pub(super) const ZIP_CENTRAL_SIGNATURE: [u8; 4] = [0x50, 0x4b, 0x01, 0x02];
pub(super) const ZIP_LOCAL_SIGNATURE: [u8; 4] = [0x50, 0x4b, 0x03, 0x04];
pub(super) const ZIP_COMPRESSION_STORED: u16 = 0;
pub(super) const ZIP_COMPRESSION_DEFLATED: u16 = 8;
pub(super) struct CentralDirectory {
    pub(super) offset: u64,
    pub(super) len: u64,
}
pub(super) struct CentralDirectoryEntry {
    pub(super) compression_method: u16,
    pub(super) compressed_size: u64,
    pub(super) uncompressed_size: u64,
    pub(super) local_header_offset: u64,
}
pub(super) enum DirectoryEntrySearch {
    Found(CentralDirectoryEntry),
    Missing,
    Invalid,
}
pub(super) fn central_directory(tail: &[u8]) -> Option<CentralDirectory> {
    let eocd = (0..=tail.len().checked_sub(ZIP_EOCD_LEN)?)
        .rev()
        .find(|&position| tail[position..].starts_with(&ZIP_EOCD_SIGNATURE))?;
    let comment_len = usize::from(read_u16(tail, eocd + 20)?);
    if eocd + ZIP_EOCD_LEN + comment_len != tail.len() {
        return None;
    }
    let len = u64::from(read_u32(tail, eocd + 12)?);
    let offset = u64::from(read_u32(tail, eocd + 16)?);
    if len == u64::from(u32::MAX) || offset == u64::from(u32::MAX) {
        return None;
    }
    Some(CentralDirectory { offset, len })
}
pub(super) fn find_central_directory_entry(directory: &[u8], metadata_path: &str) -> DirectoryEntrySearch {
    let mut position = 0;
    while position + 46 <= directory.len() {
        if !directory[position..].starts_with(&ZIP_CENTRAL_SIGNATURE) {
            return DirectoryEntrySearch::Invalid;
        }
        let flags = read_u16(directory, position + 8).expect("central directory fixed header is in bounds");
        let compression_method =
            read_u16(directory, position + 10).expect("central directory fixed header is in bounds");
        let compressed_size =
            u64::from(read_u32(directory, position + 20).expect("central directory fixed header is in bounds"));
        let uncompressed_size =
            u64::from(read_u32(directory, position + 24).expect("central directory fixed header is in bounds"));
        let name_len =
            usize::from(read_u16(directory, position + 28).expect("central directory fixed header is in bounds"));
        let extra_len =
            usize::from(read_u16(directory, position + 30).expect("central directory fixed header is in bounds"));
        let comment_len =
            usize::from(read_u16(directory, position + 32).expect("central directory fixed header is in bounds"));
        let local_header_offset =
            u64::from(read_u32(directory, position + 42).expect("central directory fixed header is in bounds"));
        let name_start = position + 46;
        let name_end = name_start + name_len;
        let next = name_end + extra_len + comment_len;
        if next > directory.len() {
            return DirectoryEntrySearch::Invalid;
        }
        if flags & 1 == 0
            && compressed_size != u64::from(u32::MAX)
            && uncompressed_size != u64::from(u32::MAX)
            && local_header_offset != u64::from(u32::MAX)
            && &directory[name_start..name_end] == metadata_path.as_bytes()
        {
            return DirectoryEntrySearch::Found(CentralDirectoryEntry {
                compression_method,
                compressed_size,
                uncompressed_size,
                local_header_offset,
            });
        }
        position = next;
    }
    DirectoryEntrySearch::Missing
}
pub(super) fn read_u16(bytes: &[u8], offset: usize) -> Option<u16> {
    Some(u16::from_le_bytes(bytes.get(offset..offset + 2)?.try_into().ok()?))
}
pub(super) fn read_u32(bytes: &[u8], offset: usize) -> Option<u32> {
    Some(u32::from_le_bytes(bytes.get(offset..offset + 4)?.try_into().ok()?))
}

#[cfg(test)]
mod tests {
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
}
