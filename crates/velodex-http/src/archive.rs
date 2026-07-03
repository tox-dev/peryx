//! Distribution-archive introspection: list and read the members of a cached wheel or sdist, the
//! way pypi-browser does, but against velodex's own blob store.

use std::io::{Cursor, Read, Seek};
use std::path::Path;

use serde::Serialize;

/// Default amount of one archive member returned by the inspect endpoint.
pub const DEFAULT_MEMBER_CHUNK: u64 = 256 * 1024;

/// Largest member chunk the inspect endpoint accepts in one response.
pub const MAX_MEMBER_CHUNK: u64 = 1024 * 1024;

/// One entry of an archive listing.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Serialize)]
pub struct Member {
    pub path: String,
    pub size: u64,
}

/// A bounded slice of one archive member.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MemberChunk {
    pub bytes: Vec<u8>,
    pub size: u64,
    pub offset: u64,
    pub next_offset: Option<u64>,
}

/// An error while reading an archive.
#[derive(Debug, thiserror::Error)]
pub enum ArchiveError {
    #[error("unsupported archive type; accepted formats are .whl, .zip, and .tar.gz")]
    Unsupported,
    #[error("archive member not found")]
    MemberNotFound,
    #[error("archive member offset {offset} is beyond member size {size}")]
    InvalidRange { offset: u64, size: u64 },
    #[error("archive member path {0:?} is not a safe relative path")]
    UnsafeMember(String),
    #[error("archive read failed: {0}")]
    Read(String),
}

/// List the file members of a distribution archive: a wheel or zip (`.whl`, `.zip`) or a gzipped
/// tarball (`.tar.gz`).
///
/// # Errors
/// Returns [`ArchiveError::Unsupported`] for other filename extensions and
/// [`ArchiveError::Read`] on a corrupt archive.
pub fn list_members(filename: &str, bytes: &[u8]) -> Result<Vec<Member>, ArchiveError> {
    if is_zip(filename) {
        list_zip(Cursor::new(bytes))
    } else if is_tar_gz(filename) {
        list_tar(Cursor::new(bytes))
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
    let file = std::fs::File::open(path).map_err(read_error)?;
    if is_zip(filename) {
        list_zip(file)
    } else if is_tar_gz(filename) {
        list_tar(file)
    } else {
        Err(ArchiveError::Unsupported)
    }
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
    } else if is_tar_gz(filename) {
        read_tar_member(Cursor::new(bytes), member, offset, limit)
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
    let file = std::fs::File::open(path).map_err(read_error)?;
    if is_zip(filename) {
        read_zip_member(file, member, offset, limit)
    } else if is_tar_gz(filename) {
        read_tar_member(file, member, offset, limit)
    } else {
        Err(ArchiveError::Unsupported)
    }
}

fn is_zip(filename: &str) -> bool {
    std::path::Path::new(filename)
        .extension()
        .is_some_and(|ext| ext.eq_ignore_ascii_case("whl") || ext.eq_ignore_ascii_case("zip"))
}

fn is_tar_gz(filename: &str) -> bool {
    filename
        .get(filename.len().saturating_sub(7)..)
        .is_some_and(|suffix| suffix.eq_ignore_ascii_case(".tar.gz"))
}

fn list_zip(reader: impl Read + Seek) -> Result<Vec<Member>, ArchiveError> {
    let mut archive = zip::ZipArchive::new(reader).map_err(read_error)?;
    let mut members = Vec::with_capacity(archive.len());
    for position in 0..archive.len() {
        let entry = archive.by_index(position).map_err(read_error)?;
        if entry.is_file() {
            let name = safe_member_name(entry.name())?;
            members.push(Member {
                path: name,
                size: entry.size(),
            });
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
    let mut archive = zip::ZipArchive::new(reader).map_err(read_error)?;
    let Ok(entry) = archive.by_name(member) else {
        return Err(ArchiveError::MemberNotFound);
    };
    safe_member_name(entry.name())?;
    let size = entry.size();
    read_slice(entry, size, offset, limit)
}

fn list_tar(reader: impl Read) -> Result<Vec<Member>, ArchiveError> {
    let mut archive = tar::Archive::new(flate2::read::GzDecoder::new(reader));
    let mut members = Vec::new();
    for entry in archive.entries().map_err(read_error)? {
        let entry = entry.map_err(read_error)?;
        if entry.header().entry_type().is_file() {
            let path = entry.path().map_err(read_error)?.to_string_lossy().into_owned();
            let path = safe_member_name(&path)?;
            members.push(Member {
                path,
                size: entry.size(),
            });
        }
    }
    members.sort();
    Ok(members)
}

fn read_tar_member(reader: impl Read, member: &str, offset: u64, limit: u64) -> Result<MemberChunk, ArchiveError> {
    let mut archive = tar::Archive::new(flate2::read::GzDecoder::new(reader));
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

fn read_slice(mut reader: impl Read, size: u64, offset: u64, limit: u64) -> Result<MemberChunk, ArchiveError> {
    if offset > size {
        return Err(ArchiveError::InvalidRange { offset, size });
    }
    std::io::copy(&mut reader.by_ref().take(offset), &mut std::io::sink()).map_err(read_error)?;
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

fn safe_member_name(path: &str) -> Result<String, ArchiveError> {
    let safe = !path.is_empty()
        && !path.starts_with('/')
        && !path.starts_with('\\')
        && !path.contains('\\')
        && !path.contains('\0')
        && path
            .split('/')
            .all(|part| !part.is_empty() && part != "." && part != "..");
    if safe {
        Ok(path.to_owned())
    } else {
        Err(ArchiveError::UnsafeMember(path.to_owned()))
    }
}

fn read_error(err: impl std::fmt::Display) -> ArchiveError {
    ArchiveError::Read(err.to_string())
}

/// Extract a wheel's `*.dist-info/METADATA` document, the file pypi.org serves as the PEP 658
/// sibling of an upload. Returns `None` for non-wheels or wheels without one.
#[must_use]
pub fn wheel_metadata(filename: &str, bytes: &[u8]) -> Option<Vec<u8>> {
    if !std::path::Path::new(filename)
        .extension()
        .is_some_and(|ext| ext.eq_ignore_ascii_case("whl"))
    {
        return None;
    }
    let member = list_members(filename, bytes)
        .ok()?
        .into_iter()
        .find(|member| member.path.ends_with(".dist-info/METADATA"))?;
    read_member(filename, bytes, &member.path).ok()
}
