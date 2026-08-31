//! Byte-level ZIP record parsing for ranged reads.
//!
//! A reader that pulls byte ranges never holds the whole archive, so it cannot lean on a full ZIP
//! reader to notice that the archive's own records disagree about a member. These functions parse
//! the records such a reader does hold and cross-check them: a member's bytes are trusted only once
//! the end-of-central-directory record, the central-directory entry, the local file header, and the
//! member's own CRC-32 all describe the same file. Pure byte parsing, no I/O.

use std::io::Read as _;

/// Bytes of an archive's tail a ranged reader pulls to find the end-of-central-directory record:
/// the fixed 22-byte record plus the largest comment a ZIP32 archive can carry.
pub const ZIP_TAIL_BYTES: u64 = 66_000;

/// Largest central directory a ranged reader will request.
///
/// ZIP32 can declare a directory close to 4 GiB and the reader buffers whatever it requests, so the
/// declaration alone must never size the allocation. An archive that declares more is left to a
/// full-archive reader.
pub const MAX_ZIP_CENTRAL_DIRECTORY_BYTES: u64 = 16 * 1024 * 1024;

const EOCD_LEN: usize = 22;
const EOCD_SIGNATURE: [u8; 4] = [0x50, 0x4b, 0x05, 0x06];
const CENTRAL_ENTRY_LEN: usize = 46;
const CENTRAL_SIGNATURE: [u8; 4] = [0x50, 0x4b, 0x01, 0x02];
const LOCAL_HEADER_LEN: usize = 30;
const LOCAL_SIGNATURE: [u8; 4] = [0x50, 0x4b, 0x03, 0x04];
const COMPRESSION_STORED: u16 = 0;
const COMPRESSION_DEFLATED: u16 = 8;

/// APPNOTE 4.4.4 general-purpose bit 3: a streaming writer that could not seek back left placeholder
/// CRC and size fields in the local header and wrote the real values after the data, so those three
/// fields carry no claim to cross-check.
const FLAG_DATA_DESCRIPTOR: u16 = 1 << 3;

/// General-purpose bits this reader can honour. Bits 1 and 2 only hint at the deflate level, bit 3
/// defers the CRC and sizes to a data descriptor, and bit 11 declares the name UTF-8. Bits 0 and 6
/// encrypt the member, bit 13 masks the very fields cross-checked here, and the rest are reserved.
const READABLE_FLAGS: u16 = (1 << 1) | (1 << 2) | FLAG_DATA_DESCRIPTOR | (1 << 11);

/// The span the central directory occupies inside the archive.
#[derive(Debug, PartialEq, Eq)]
pub struct ZipCentralDirectory {
    pub offset: u64,
    pub len: u64,
}

/// One central-directory entry, carrying every field the local header and the member's own bytes
/// are held to.
#[derive(Debug, PartialEq, Eq)]
pub struct ZipEntry {
    pub name: String,
    pub flags: u16,
    pub compression_method: u16,
    pub crc32: u32,
    pub compressed_size: u64,
    pub uncompressed_size: u64,
    pub local_header_offset: u64,
}

#[derive(Debug, PartialEq, Eq)]
pub enum ZipEntrySearch {
    Found(ZipEntry),
    /// The directory is well formed and holds no such member.
    Missing,
    /// The member exists but is encrypted, ZIP64, or otherwise beyond this reader.
    Unsupported,
    /// The directory itself does not parse.
    Invalid,
}

#[derive(Debug, PartialEq, Eq, thiserror::Error)]
pub enum ZipRecordError {
    #[error("local header of {name:?} is {actual} bytes, short of the {expected} it declares")]
    ShortLocalHeader { name: String, expected: u64, actual: u64 },
    #[error("local header of {0:?} carries no local file signature")]
    LocalSignature(String),
    #[error("local header names {actual:?} where the central directory names {expected:?}")]
    NameMismatch { expected: String, actual: String },
    #[error("{field} is {actual} in the local header and {expected} in the central directory")]
    HeaderMismatch {
        field: &'static str,
        expected: u64,
        actual: u64,
    },
    #[error("compression method {0} is not supported")]
    UnsupportedCompression(u16),
    #[error("stored member declares {compressed} compressed and {uncompressed} uncompressed bytes")]
    StoredSizeMismatch { compressed: u64, uncompressed: u64 },
    #[error("member does not decode: {0}")]
    Decode(String),
    #[error("member decoded to {actual} bytes where the central directory declares {expected}")]
    LengthMismatch { expected: u64, actual: u64 },
    #[error("{0} bytes follow the member's compression stream")]
    TrailingBytes(u64),
    #[error("member decoded to CRC-32 {actual:#010x} where the central directory declares {expected:#010x}")]
    CrcMismatch { expected: u32, actual: u32 },
}

/// The central directory an archive's tail points at, or `None` when the tail carries no
/// end-of-central-directory record this reader can act on.
///
/// `tail` is the last bytes of an `artifact_len`-byte archive, at most [`ZIP_TAIL_BYTES`] of them.
#[must_use]
pub fn zip_central_directory(tail: &[u8], artifact_len: u64) -> Option<ZipCentralDirectory> {
    let eocd = (0..=tail.len().checked_sub(EOCD_LEN)?)
        .rev()
        .find(|&position| tail[position..].starts_with(&EOCD_SIGNATURE))?;
    let record = &tail[eocd..];
    if EOCD_LEN + usize::from(u16_at(record, 20)) != record.len() {
        return None;
    }
    let len = u64::from(u32_at(record, 12));
    let offset = u64::from(u32_at(record, 16));
    if len == u64::from(u32::MAX) || offset == u64::from(u32::MAX) {
        return None;
    }
    if len > MAX_ZIP_CENTRAL_DIRECTORY_BYTES || offset + len > artifact_len {
        return None;
    }
    Some(ZipCentralDirectory { offset, len })
}

/// Search `directory`, the archive's whole central directory, for the member named `name`.
#[must_use]
pub fn find_zip_entry(directory: &[u8], name: &str) -> ZipEntrySearch {
    let mut position = 0;
    while position + CENTRAL_ENTRY_LEN <= directory.len() {
        let entry = &directory[position..];
        if !entry.starts_with(&CENTRAL_SIGNATURE) {
            return ZipEntrySearch::Invalid;
        }
        let name_end = position + CENTRAL_ENTRY_LEN + usize::from(u16_at(entry, 28));
        let next = name_end + usize::from(u16_at(entry, 30)) + usize::from(u16_at(entry, 32));
        if next > directory.len() {
            return ZipEntrySearch::Invalid;
        }
        if &directory[position + CENTRAL_ENTRY_LEN..name_end] != name.as_bytes() {
            position = next;
            continue;
        }
        let flags = u16_at(entry, 8);
        let compressed_size = u64::from(u32_at(entry, 20));
        let uncompressed_size = u64::from(u32_at(entry, 24));
        let local_header_offset = u64::from(u32_at(entry, 42));
        if flags & !READABLE_FLAGS != 0
            || compressed_size == u64::from(u32::MAX)
            || uncompressed_size == u64::from(u32::MAX)
            || local_header_offset == u64::from(u32::MAX)
        {
            return ZipEntrySearch::Unsupported;
        }
        return ZipEntrySearch::Found(ZipEntry {
            name: name.to_owned(),
            flags,
            compression_method: u16_at(entry, 10),
            crc32: u32_at(entry, 16),
            compressed_size,
            uncompressed_size,
            local_header_offset,
        });
    }
    ZipEntrySearch::Missing
}

impl ZipEntry {
    /// Bytes of local header [`Self::data_start`] needs: the fixed header plus the name the central
    /// directory declares for the member.
    #[must_use]
    pub const fn local_header_len(&self) -> u64 {
        (LOCAL_HEADER_LEN + self.name.len()) as u64
    }

    /// Where the member's compressed bytes begin, once the local header agrees with this entry.
    ///
    /// `header` is the [`Self::local_header_len`] bytes at [`Self::local_header_offset`]. A member
    /// written by a streaming writer (general-purpose bit 3) leaves its local CRC and sizes as
    /// placeholders, so those three fields are read from the central directory instead of compared
    /// against it.
    ///
    /// # Errors
    /// Returns [`ZipRecordError`] when `header` is short or unsigned, or when it names, compresses,
    /// flags, or sizes the member differently from the central directory.
    pub fn data_start(&self, header: &[u8]) -> Result<u64, ZipRecordError> {
        if (header.len() as u64) < self.local_header_len() {
            return Err(ZipRecordError::ShortLocalHeader {
                name: self.name.clone(),
                expected: self.local_header_len(),
                actual: header.len() as u64,
            });
        }
        if !header.starts_with(&LOCAL_SIGNATURE) {
            return Err(ZipRecordError::LocalSignature(self.name.clone()));
        }
        agree("general-purpose flags", self.flags.into(), u16_at(header, 6).into())?;
        agree(
            "compression method",
            self.compression_method.into(),
            u16_at(header, 8).into(),
        )?;
        let name_len = usize::from(u16_at(header, 26));
        agree("file name length", self.name.len() as u64, name_len as u64)?;
        let name = &header[LOCAL_HEADER_LEN..][..name_len];
        if name != self.name.as_bytes() {
            return Err(ZipRecordError::NameMismatch {
                expected: self.name.clone(),
                actual: String::from_utf8_lossy(name).into_owned(),
            });
        }
        if self.flags & FLAG_DATA_DESCRIPTOR == 0 {
            agree("CRC-32", self.crc32.into(), u32_at(header, 14).into())?;
            agree("compressed size", self.compressed_size, u32_at(header, 18).into())?;
            agree("uncompressed size", self.uncompressed_size, u32_at(header, 22).into())?;
        }
        Ok(self.local_header_offset + self.local_header_len() + u64::from(u16_at(header, 28)))
    }

    /// The member's bytes, decoded from `compressed` and held to everything this entry declares.
    ///
    /// `compressed` is the [`Self::compressed_size`] bytes at [`Self::data_start`]. Decoding stops
    /// one byte past the declared uncompressed size, so a member that expands further is rejected on
    /// that byte rather than after its whole expansion is buffered.
    ///
    /// # Errors
    /// Returns [`ZipRecordError`] when the member uses a compression method this reader does not
    /// implement, does not decode, declares stored sizes that disagree, leaves bytes after its
    /// compression stream, or decodes to a length or CRC-32 the central directory does not declare.
    pub fn decode(&self, compressed: &[u8]) -> Result<Vec<u8>, ZipRecordError> {
        let (decoded, consumed) = match self.compression_method {
            COMPRESSION_STORED => self.stored(compressed)?,
            COMPRESSION_DEFLATED => self.inflated(compressed)?,
            method => return Err(ZipRecordError::UnsupportedCompression(method)),
        };
        if decoded.len() as u64 != self.uncompressed_size {
            return Err(ZipRecordError::LengthMismatch {
                expected: self.uncompressed_size,
                actual: decoded.len() as u64,
            });
        }
        if consumed != compressed.len() as u64 {
            return Err(ZipRecordError::TrailingBytes(compressed.len() as u64 - consumed));
        }
        let mut crc = flate2::Crc::new();
        crc.update(&decoded);
        if crc.sum() != self.crc32 {
            return Err(ZipRecordError::CrcMismatch {
                expected: self.crc32,
                actual: crc.sum(),
            });
        }
        Ok(decoded)
    }

    fn stored(&self, compressed: &[u8]) -> Result<(Vec<u8>, u64), ZipRecordError> {
        if self.compressed_size != self.uncompressed_size {
            return Err(ZipRecordError::StoredSizeMismatch {
                compressed: self.compressed_size,
                uncompressed: self.uncompressed_size,
            });
        }
        Ok((compressed.to_vec(), compressed.len() as u64))
    }

    /// Reports the bytes the decompressor consumed rather than the bytes it was handed, so a
    /// declared span holding a valid stream plus anything else is caught by the caller. Matching
    /// output length does not prove the span holds exactly one stream.
    fn inflated(&self, compressed: &[u8]) -> Result<(Vec<u8>, u64), ZipRecordError> {
        let mut stream = flate2::read::DeflateDecoder::new(compressed);
        let mut decoded = Vec::new();
        stream
            .by_ref()
            .take(self.uncompressed_size.saturating_add(1))
            .read_to_end(&mut decoded)
            .map_err(|err| ZipRecordError::Decode(err.to_string()))?;
        Ok((decoded, stream.total_in()))
    }
}

const fn agree(field: &'static str, expected: u64, actual: u64) -> Result<(), ZipRecordError> {
    if expected == actual {
        Ok(())
    } else {
        Err(ZipRecordError::HeaderMismatch {
            field,
            expected,
            actual,
        })
    }
}

fn u16_at(bytes: &[u8], offset: usize) -> u16 {
    u16::from_le_bytes(
        bytes[offset..offset + 2]
            .try_into()
            .expect("the caller proved the field is in bounds"),
    )
}

fn u32_at(bytes: &[u8], offset: usize) -> u32 {
    u32::from_le_bytes(
        bytes[offset..offset + 4]
            .try_into()
            .expect("the caller proved the field is in bounds"),
    )
}

#[cfg(test)]
#[path = "../tests/unit/zip_range_tests.rs"]
mod tests;
