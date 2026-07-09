use std::io::{Cursor, Read, Seek, SeekFrom};
use std::path::Path;

use zip::read::HasZipMetadata;

use super::model::{ArchiveError, Member, MemberChunk, MemberKind};
use super::source::{ArchiveSource, resolve_container_stack};
use super::{
    MAX_DECOMPRESSED_INSPECT_BYTES, MAX_LISTED_ENTRIES, is_supported_archive, is_tar, is_tar_gz, is_zip, read_error,
    safe_member_name,
};

/// List the file members of a distribution archive: a wheel, zip, zipped egg, or tar archive.
///
/// # Errors
/// Returns [`ArchiveError::Unsupported`] for other filename extensions and
/// [`ArchiveError::Read`] on a corrupt archive.
pub fn list_members(filename: &str, bytes: &[u8]) -> Result<Vec<Member>, ArchiveError> {
    if is_zip(filename) {
        list_zip(Cursor::new(bytes))
    } else if is_tar(filename) {
        list_tar(Cursor::new(bytes))
    } else if is_tar_gz(filename) {
        list_tar(flate2::read::GzDecoder::new(Cursor::new(bytes)))
    } else {
        Err(ArchiveError::Unsupported)
    }
}

/// List members from a cached blob on disk without reading the whole archive into memory.
///
/// # Errors
/// Returns [`ArchiveError::Unsupported`] for other filename extensions and
/// [`ArchiveError::Read`] on a corrupt or unreadable archive.
pub fn list_members_path(filename: &str, path: &Path) -> Result<Vec<Member>, ArchiveError> {
    list_members_nested_path(filename, path, &[])
}

/// List members from an archive inside a cached archive on disk.
///
/// # Errors
/// Returns the same errors as [`list_members_path`], plus container-stack validation errors.
pub fn list_members_nested_path(
    filename: &str,
    path: &Path,
    containers: &[String],
) -> Result<Vec<Member>, ArchiveError> {
    let resolved = resolve_container_stack(filename, path, containers)?;
    list_members_source(&resolved.filename, &resolved.source)
}

/// Read one member's bytes out of a distribution archive.
///
/// # Errors
/// Returns [`ArchiveError::MemberNotFound`] when `member` names no file in the archive and the
/// listing errors otherwise.
pub fn read_member(filename: &str, bytes: &[u8], member: &str) -> Result<Vec<u8>, ArchiveError> {
    Ok(read_member_chunk(filename, bytes, member, 0, u64::MAX)?.bytes)
}

/// Read a bounded slice of one member out of a distribution archive.
///
/// # Errors
/// Returns [`ArchiveError::MemberNotFound`] when `member` names no file in the archive,
/// [`ArchiveError::InvalidRange`] when `offset` is beyond the member, and the listing errors
/// otherwise.
pub fn read_member_chunk(
    filename: &str,
    bytes: &[u8],
    member: &str,
    offset: u64,
    limit: u64,
) -> Result<MemberChunk, ArchiveError> {
    if is_zip(filename) {
        read_zip_member(Cursor::new(bytes), member, offset, limit)
    } else if is_tar(filename) {
        read_tar_member(Cursor::new(bytes), member, offset, limit)
    } else if is_tar_gz(filename) {
        read_tar_member(flate2::read::GzDecoder::new(Cursor::new(bytes)), member, offset, limit)
    } else {
        Err(ArchiveError::Unsupported)
    }
}

/// Read a bounded slice of one member from a cached blob on disk.
///
/// # Errors
/// Returns [`ArchiveError::MemberNotFound`] when `member` names no file in the archive,
/// [`ArchiveError::InvalidRange`] when `offset` is beyond the member, and the listing errors
/// otherwise.
pub fn read_member_chunk_path(
    filename: &str,
    path: &Path,
    member: &str,
    offset: u64,
    limit: u64,
) -> Result<MemberChunk, ArchiveError> {
    let source = ArchiveSource::new(path.to_path_buf());
    read_member_chunk_source(filename, &source, member, offset, limit)
}

/// Read one text member chunk from an archive inside a cached archive on disk.
///
/// # Errors
/// Returns [`ArchiveError::BinaryMember`] when `member` is not classified as text or the selected
/// chunk is not valid UTF-8. Other errors match [`read_member_chunk_path`].
pub fn read_text_member_chunk_nested_path(
    filename: &str,
    path: &Path,
    containers: &[String],
    member: &str,
    offset: u64,
    limit: u64,
) -> Result<MemberChunk, ArchiveError> {
    let member = safe_member_name(member)?;
    if !is_previewable_member(&member) {
        return Err(ArchiveError::BinaryMember(member));
    }
    let resolved = resolve_container_stack(filename, path, containers)?;
    text_chunk(
        &member,
        read_member_chunk_source(&resolved.filename, &resolved.source, &member, offset, limit)?,
    )
}

fn list_members_source(filename: &str, source: &ArchiveSource) -> Result<Vec<Member>, ArchiveError> {
    if is_zip(filename) {
        list_zip(source.open()?)
    } else if is_tar(filename) {
        list_tar(source.open()?)
    } else if is_tar_gz(filename) {
        list_tar(flate2::read::GzDecoder::new(source.open()?))
    } else {
        Err(ArchiveError::Unsupported)
    }
}

fn read_member_chunk_source(
    filename: &str,
    source: &ArchiveSource,
    member: &str,
    offset: u64,
    limit: u64,
) -> Result<MemberChunk, ArchiveError> {
    let member = safe_member_name(member)?;
    if is_zip(filename) {
        read_zip_member(source.open()?, &member, offset, limit)
    } else if is_tar(filename) {
        read_tar_member(source.open()?, &member, offset, limit)
    } else if is_tar_gz(filename) {
        read_tar_member(flate2::read::GzDecoder::new(source.open()?), &member, offset, limit)
    } else {
        Err(ArchiveError::Unsupported)
    }
}

fn text_chunk(member: &str, mut chunk: MemberChunk) -> Result<MemberChunk, ArchiveError> {
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

fn list_zip(reader: impl Read + Seek) -> Result<Vec<Member>, ArchiveError> {
    let mut archive = zip::ZipArchive::new(reader).map_err(read_error)?;
    let mut members = Vec::with_capacity(archive.len().min(MAX_LISTED_ENTRIES));
    for position in 0..archive.len() {
        let entry = archive.by_index(position).map_err(read_error)?;
        if entry.is_file() {
            let name = safe_member_name(entry.name())?;
            push_member(&mut members, name, entry.size())?;
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
    if offset > 0 {
        match archive.by_name_seek(&member) {
            Ok(mut entry) => {
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
            Err(zip::result::ZipError::UnsupportedArchive("Seekable compressed files are not yet supported")) => {}
            Err(zip::result::ZipError::FileNotFound) => return Err(ArchiveError::MemberNotFound),
            Err(err) => return Err(read_error(err)),
        }
    }
    let Ok(entry) = archive.by_name(&member) else {
        return Err(ArchiveError::MemberNotFound);
    };
    safe_member_name(entry.name())?;
    let size = entry.size();
    read_slice(entry, size, offset, limit)
}

fn list_tar(reader: impl Read) -> Result<Vec<Member>, ArchiveError> {
    let mut archive = tar::Archive::new(reader.take(MAX_DECOMPRESSED_INSPECT_BYTES));
    let mut members = Vec::new();
    for entry in archive.entries().map_err(read_error)? {
        let entry = entry.map_err(read_error)?;
        if entry.header().entry_type().is_file() {
            let path = entry.path().map_err(read_error)?.to_string_lossy().into_owned();
            let path = safe_member_name(&path)?;
            push_member(&mut members, path, entry.size())?;
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
        if !entry.header().entry_type().is_file() {
            continue;
        }
        let path = entry.path().map_err(read_error)?.to_string_lossy().into_owned();
        let path = safe_member_name(&path)?;
        if path == member {
            let size = entry.size();
            return read_slice(entry, size, offset, limit);
        }
    }
    Err(ArchiveError::MemberNotFound)
}

fn push_member(members: &mut Vec<Member>, path: String, size: u64) -> Result<(), ArchiveError> {
    if members.len() == MAX_LISTED_ENTRIES {
        return Err(ArchiveError::TooManyEntries(MAX_LISTED_ENTRIES));
    }
    let kind = member_kind(&path);
    members.push(Member {
        path,
        size,
        kind,
        previewable: kind == MemberKind::Text,
    });
    Ok(())
}

fn member_kind(path: &str) -> MemberKind {
    if is_supported_archive(path) {
        MemberKind::Archive
    } else if is_text_member(path) {
        MemberKind::Text
    } else if is_binary_member(path) {
        MemberKind::Binary
    } else {
        MemberKind::Unknown
    }
}

fn is_previewable_member(path: &str) -> bool {
    member_kind(path) == MemberKind::Text
}

fn is_text_member(path: &str) -> bool {
    let filename = path.rsplit('/').next().unwrap_or(path);
    if matches!(
        filename,
        "METADATA"
            | "PKG-INFO"
            | "WHEEL"
            | "RECORD"
            | "INSTALLER"
            | "REQUESTED"
            | "entry_points.txt"
            | "top_level.txt"
            | "namespace_packages.txt"
            | "SOURCES.txt"
    ) {
        return true;
    }
    std::path::Path::new(filename)
        .extension()
        .and_then(|extension| extension.to_str())
        .is_some_and(|extension| {
            matches!(
                extension.to_ascii_lowercase().as_str(),
                "asc"
                    | "cfg"
                    | "cjs"
                    | "conf"
                    | "css"
                    | "csv"
                    | "h"
                    | "hpp"
                    | "html"
                    | "ini"
                    | "js"
                    | "json"
                    | "lock"
                    | "md"
                    | "mjs"
                    | "py"
                    | "pyi"
                    | "rst"
                    | "svg"
                    | "toml"
                    | "tsv"
                    | "txt"
                    | "xml"
                    | "yaml"
                    | "yml"
            )
        })
}

fn is_binary_member(path: &str) -> bool {
    std::path::Path::new(path)
        .extension()
        .and_then(|extension| extension.to_str())
        .is_some_and(|extension| {
            matches!(
                extension.to_ascii_lowercase().as_str(),
                "a" | "bmp"
                    | "bin"
                    | "dll"
                    | "dylib"
                    | "exe"
                    | "gif"
                    | "ico"
                    | "jpg"
                    | "jpeg"
                    | "o"
                    | "pyd"
                    | "png"
                    | "pyc"
                    | "so"
                    | "wasm"
                    | "webp"
            )
        })
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
