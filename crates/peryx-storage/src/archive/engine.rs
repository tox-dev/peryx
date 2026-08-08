use std::io::{Cursor, Read, Seek, SeekFrom};
use std::path::Path;

use zip::read::HasZipMetadata;

use super::model::{ArchiveError, ArchiveFormat, ArchiveProfile, Member, MemberChunk, MemberKind};
use super::source::{ArchiveSource, resolve_container_stack};
use super::{MAX_DECOMPRESSED_INSPECT_BYTES, MAX_LISTED_ENTRIES, read_error, safe_member_name};

/// List the file members of a zip or tar archive.
///
/// # Errors
/// Returns [`ArchiveError::Unsupported`] for other filename extensions and
/// [`ArchiveError::Read`] on a corrupt archive.
pub fn list_members(profile: &dyn ArchiveProfile, filename: &str, bytes: &[u8]) -> Result<Vec<Member>, ArchiveError> {
    match profile.format(filename) {
        Some(ArchiveFormat::Zip) => list_zip(profile, Cursor::new(bytes)),
        Some(ArchiveFormat::Tar) => list_tar(profile, Cursor::new(bytes)),
        Some(ArchiveFormat::TarGz) => list_tar(profile, flate2::read::GzDecoder::new(Cursor::new(bytes))),
        None => Err(ArchiveError::Unsupported),
    }
}

/// List members from a cached blob on disk without reading the whole archive into memory.
///
/// # Errors
/// Returns [`ArchiveError::Unsupported`] for other filename extensions and
/// [`ArchiveError::Read`] on a corrupt or unreadable archive.
pub fn list_members_path(
    profile: &dyn ArchiveProfile,
    filename: &str,
    path: &Path,
) -> Result<Vec<Member>, ArchiveError> {
    list_members_nested_path(profile, filename, path, &[])
}

/// List members from an archive inside a cached archive on disk.
///
/// # Errors
/// Returns the same errors as [`list_members_path`], plus container-stack validation errors.
pub fn list_members_nested_path(
    profile: &dyn ArchiveProfile,
    filename: &str,
    path: &Path,
    containers: &[String],
) -> Result<Vec<Member>, ArchiveError> {
    let resolved = resolve_container_stack(profile, filename, path, containers)?;
    list_members_source(profile, resolved.format, &resolved.source)
}

/// Read one member's bytes out of a distribution archive.
///
/// # Errors
/// Returns [`ArchiveError::MemberNotFound`] when `member` names no file in the archive and the
/// listing errors otherwise.
pub fn read_member(
    profile: &dyn ArchiveProfile,
    filename: &str,
    bytes: &[u8],
    member: &str,
) -> Result<Vec<u8>, ArchiveError> {
    Ok(read_member_chunk(profile, filename, bytes, member, 0, u64::MAX)?.bytes)
}

/// Read a bounded slice of one member out of a distribution archive.
///
/// # Errors
/// Returns [`ArchiveError::MemberNotFound`] when `member` names no file in the archive,
/// [`ArchiveError::InvalidRange`] when `offset` is beyond the member, and the listing errors
/// otherwise.
pub fn read_member_chunk(
    profile: &dyn ArchiveProfile,
    filename: &str,
    bytes: &[u8],
    member: &str,
    offset: u64,
    limit: u64,
) -> Result<MemberChunk, ArchiveError> {
    match profile.format(filename) {
        Some(ArchiveFormat::Zip) => read_zip_member(Cursor::new(bytes), member, offset, limit),
        Some(ArchiveFormat::Tar) => read_tar_member(Cursor::new(bytes), member, offset, limit),
        Some(ArchiveFormat::TarGz) => {
            read_tar_member(flate2::read::GzDecoder::new(Cursor::new(bytes)), member, offset, limit)
        }
        None => Err(ArchiveError::Unsupported),
    }
}

/// Read a bounded slice of one member from a cached blob on disk.
///
/// # Errors
/// Returns [`ArchiveError::MemberNotFound`] when `member` names no file in the archive,
/// [`ArchiveError::InvalidRange`] when `offset` is beyond the member, and the listing errors
/// otherwise.
pub fn read_member_chunk_path(
    profile: &dyn ArchiveProfile,
    filename: &str,
    path: &Path,
    member: &str,
    offset: u64,
    limit: u64,
) -> Result<MemberChunk, ArchiveError> {
    let source = ArchiveSource::new(path.to_path_buf());
    let format = profile.format(filename).ok_or(ArchiveError::Unsupported)?;
    read_member_chunk_source(format, &source, member, offset, limit)
}

/// Read one text member chunk from an archive inside a cached archive on disk.
///
/// # Errors
/// Returns [`ArchiveError::BinaryMember`] when `member` is not classified as text or the selected
/// chunk is not valid UTF-8. Other errors match [`read_member_chunk_path`].
pub fn read_text_member_chunk_nested_path(
    profile: &dyn ArchiveProfile,
    filename: &str,
    path: &Path,
    containers: &[String],
    member: &str,
    offset: u64,
    limit: u64,
) -> Result<MemberChunk, ArchiveError> {
    let member = safe_member_name(member)?;
    if profile.member_kind(&member) != MemberKind::Text {
        return Err(ArchiveError::BinaryMember(member));
    }
    let resolved = resolve_container_stack(profile, filename, path, containers)?;
    text_chunk(
        &member,
        read_member_chunk_source(resolved.format, &resolved.source, &member, offset, limit)?,
    )
}

fn list_members_source(
    profile: &dyn ArchiveProfile,
    format: ArchiveFormat,
    source: &ArchiveSource,
) -> Result<Vec<Member>, ArchiveError> {
    match format {
        ArchiveFormat::Zip => list_zip(profile, source.open()?),
        ArchiveFormat::Tar => list_tar(profile, source.open()?),
        ArchiveFormat::TarGz => list_tar(profile, flate2::read::GzDecoder::new(source.open()?)),
    }
}

fn read_member_chunk_source(
    format: ArchiveFormat,
    source: &ArchiveSource,
    member: &str,
    offset: u64,
    limit: u64,
) -> Result<MemberChunk, ArchiveError> {
    let member = safe_member_name(member)?;
    match format {
        ArchiveFormat::Zip => read_zip_member(source.open()?, &member, offset, limit),
        ArchiveFormat::Tar => read_tar_member(source.open()?, &member, offset, limit),
        ArchiveFormat::TarGz => read_tar_member(flate2::read::GzDecoder::new(source.open()?), &member, offset, limit),
    }
}

fn text_chunk(member: &str, mut chunk: MemberChunk) -> Result<MemberChunk, ArchiveError> {
    realign_to_char_boundary(&mut chunk);
    match std::str::from_utf8(&chunk.bytes) {
        Ok(_) => Ok(chunk),
        Err(err) if err.error_len().is_none() && chunk.next_offset.is_some() && err.valid_up_to() > 0 => {
            chunk.bytes.truncate(err.valid_up_to());
            let next = chunk.offset + u64::try_from(chunk.bytes.len()).unwrap_or_default();
            chunk.next_offset = (next < chunk.size).then_some(next);
            Ok(chunk)
        }
        Err(_) => Err(ArchiveError::BinaryMember(member.to_owned())),
    }
}

/// Advance a chunk that starts mid-character past its stray continuation bytes.
///
/// A non-zero read offset can land between a character's bytes, so its tail leads the chunk and a
/// genuinely-textual member would otherwise read as binary. A UTF-8 character carries at most three
/// continuation bytes, so a longer leading run is malformed and the caller rejects it as binary.
fn realign_to_char_boundary(chunk: &mut MemberChunk) {
    if chunk.offset == 0 {
        return;
    }
    let lead = chunk
        .bytes
        .iter()
        .take_while(|&&byte| byte & 0b1100_0000 == 0b1000_0000)
        .count();
    if (1..=3).contains(&lead) {
        chunk.bytes.drain(..lead);
        chunk.offset += lead as u64;
    }
}

fn list_zip(profile: &dyn ArchiveProfile, reader: impl Read + Seek) -> Result<Vec<Member>, ArchiveError> {
    let mut archive = zip::ZipArchive::new(reader).map_err(read_error)?;
    let mut members = Vec::with_capacity(archive.len().min(MAX_LISTED_ENTRIES));
    for position in 0..archive.len() {
        let entry = archive.by_index(position).map_err(read_error)?;
        if entry.is_file() {
            let name = safe_member_name(entry.name())?;
            push_member(profile, &mut members, name, entry.size())?;
        }
    }
    members.sort();
    Ok(members)
}

fn read_zip_member(
    reader: impl Read + Seek,
    member: &str,
    offset: u64,
    limit: u64,
) -> Result<MemberChunk, ArchiveError> {
    let member = safe_member_name(member)?;
    let mut archive = zip::ZipArchive::new(reader).map_err(read_error)?;
    if offset > 0
        && let Ok(mut entry) = archive.by_name_seek(&member)
    {
        let size = {
            let metadata = entry.get_metadata();
            safe_member_name(&metadata.file_name)?;
            metadata.uncompressed_size
        };
        if offset > size {
            return Err(ArchiveError::InvalidRange { offset, size });
        }
        entry.seek(SeekFrom::Start(offset)).map_err(read_error)?;
        return read_from_current(entry, size, offset, limit);
    }
    let entry = match archive.by_name(&member) {
        Ok(entry) => entry,
        Err(zip::result::ZipError::FileNotFound) => return Err(ArchiveError::MemberNotFound),
        Err(error) => return Err(read_error(error)),
    };
    safe_member_name(entry.name())?;
    let size = entry.size();
    // The skip in `read_slice` decompresses `offset` bytes for a compressed member, so a crafted
    // member with a huge declared size could force unbounded work. Cap the reachable range at the
    // decompression budget, matching the `.take(MAX_DECOMPRESSED_INSPECT_BYTES)` on the tar readers.
    let inspectable = size.min(MAX_DECOMPRESSED_INSPECT_BYTES);
    if offset > inspectable {
        return Err(ArchiveError::InvalidRange {
            offset,
            size: inspectable,
        });
    }
    read_slice(entry, size, offset, limit)
}

fn list_tar(profile: &dyn ArchiveProfile, reader: impl Read) -> Result<Vec<Member>, ArchiveError> {
    let mut archive = tar::Archive::new(reader.take(MAX_DECOMPRESSED_INSPECT_BYTES));
    let mut members = Vec::new();
    for entry in archive.entries().map_err(read_error)? {
        let entry = entry.map_err(read_error)?;
        if entry.header().entry_type().is_file() {
            let path = entry.path().map_err(read_error)?.to_string_lossy().into_owned();
            let path = safe_member_name(&path)?;
            push_member(profile, &mut members, path, entry.size())?;
        }
    }
    members.sort();
    Ok(members)
}

fn read_tar_member(reader: impl Read, member: &str, offset: u64, limit: u64) -> Result<MemberChunk, ArchiveError> {
    let member = safe_member_name(member)?;
    let mut archive = tar::Archive::new(reader.take(MAX_DECOMPRESSED_INSPECT_BYTES));
    for entry in archive.entries().map_err(read_error)? {
        let entry = entry.map_err(read_error)?;
        if entry.header().entry_type().is_file() {
            let path = entry.path().map_err(read_error)?.to_string_lossy().into_owned();
            let path = safe_member_name(&path)?;
            if path == member {
                let size = entry.size();
                return read_slice(entry, size, offset, limit);
            }
        }
    }
    Err(ArchiveError::MemberNotFound)
}

fn push_member(
    profile: &dyn ArchiveProfile,
    members: &mut Vec<Member>,
    path: String,
    size: u64,
) -> Result<(), ArchiveError> {
    if members.len() == MAX_LISTED_ENTRIES {
        return Err(ArchiveError::TooManyEntries(MAX_LISTED_ENTRIES));
    }
    let kind = profile.member_kind(&path);
    members.push(Member {
        path,
        size,
        kind,
        previewable: kind == MemberKind::Text,
    });
    Ok(())
}

fn read_slice(mut reader: impl Read, size: u64, offset: u64, limit: u64) -> Result<MemberChunk, ArchiveError> {
    if offset > size {
        return Err(ArchiveError::InvalidRange { offset, size });
    }
    std::io::copy(&mut reader.by_ref().take(offset), &mut std::io::sink()).map_err(read_error)?;
    read_from_current(reader, size, offset, limit)
}

fn read_from_current(reader: impl Read, size: u64, offset: u64, limit: u64) -> Result<MemberChunk, ArchiveError> {
    let remaining = size - offset;
    let count = remaining.min(limit);
    let mut bytes = Vec::with_capacity(usize::try_from(count).unwrap_or_default());
    reader.take(count).read_to_end(&mut bytes).map_err(read_error)?;
    let next = offset + bytes.len() as u64;
    Ok(MemberChunk {
        bytes,
        size,
        offset,
        next_offset: (next < size).then_some(next),
    })
}
