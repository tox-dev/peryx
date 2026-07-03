//! Distribution-archive introspection: list and read the members of a cached wheel or sdist, the
//! way pypi-browser does, but against velodex's own blob store.

use std::io::{Cursor, Read as _};

use serde::Serialize;

/// Members larger than this are not served inline; the artifact itself stays downloadable.
pub const MEMBER_LIMIT: u64 = 1024 * 1024;

/// One entry of an archive listing.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Serialize)]
pub struct Member {
    pub path: String,
    pub size: u64,
}

/// An error while reading an archive.
#[derive(Debug, thiserror::Error)]
pub enum ArchiveError {
    #[error("unsupported archive type")]
    Unsupported,
    #[error("archive member not found")]
    MemberNotFound,
    #[error("member exceeds the {MEMBER_LIMIT}-byte inline limit")]
    TooLarge,
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
        list_zip(bytes)
    } else if filename.ends_with(".tar.gz") {
        list_tar(bytes)
    } else {
        Err(ArchiveError::Unsupported)
    }
}

/// Read one member's bytes out of a distribution archive.
///
/// # Errors
/// Returns [`ArchiveError::MemberNotFound`] when `member` names no file in the archive,
/// [`ArchiveError::TooLarge`] past [`MEMBER_LIMIT`], and the listing errors otherwise.
pub fn read_member(filename: &str, bytes: &[u8], member: &str) -> Result<Vec<u8>, ArchiveError> {
    if is_zip(filename) {
        read_zip_member(bytes, member)
    } else if filename.ends_with(".tar.gz") {
        read_tar_member(bytes, member)
    } else {
        Err(ArchiveError::Unsupported)
    }
}

fn is_zip(filename: &str) -> bool {
    std::path::Path::new(filename)
        .extension()
        .is_some_and(|ext| ext.eq_ignore_ascii_case("whl") || ext.eq_ignore_ascii_case("zip"))
}

fn list_zip(bytes: &[u8]) -> Result<Vec<Member>, ArchiveError> {
    let mut archive = zip::ZipArchive::new(Cursor::new(bytes)).map_err(read_error)?;
    let mut members = Vec::with_capacity(archive.len());
    for position in 0..archive.len() {
        let entry = archive.by_index(position).map_err(read_error)?;
        if entry.is_file() {
            members.push(Member {
                path: entry.name().to_owned(),
                size: entry.size(),
            });
        }
    }
    members.sort();
    Ok(members)
}

fn read_zip_member(bytes: &[u8], member: &str) -> Result<Vec<u8>, ArchiveError> {
    let mut archive = zip::ZipArchive::new(Cursor::new(bytes)).map_err(read_error)?;
    let Ok(mut entry) = archive.by_name(member) else {
        return Err(ArchiveError::MemberNotFound);
    };
    if entry.size() > MEMBER_LIMIT {
        return Err(ArchiveError::TooLarge);
    }
    let mut out = Vec::with_capacity(usize::try_from(entry.size()).unwrap_or_default());
    entry.read_to_end(&mut out).map_err(read_error)?;
    Ok(out)
}

fn list_tar(bytes: &[u8]) -> Result<Vec<Member>, ArchiveError> {
    let mut archive = tar::Archive::new(flate2::read::GzDecoder::new(bytes));
    let mut members = Vec::new();
    for entry in archive.entries().map_err(read_error)? {
        let entry = entry.map_err(read_error)?;
        if entry.header().entry_type().is_file() {
            let path = entry.path().map_err(read_error)?.to_string_lossy().into_owned();
            members.push(Member {
                path,
                size: entry.size(),
            });
        }
    }
    members.sort();
    Ok(members)
}

fn read_tar_member(bytes: &[u8], member: &str) -> Result<Vec<u8>, ArchiveError> {
    let mut archive = tar::Archive::new(flate2::read::GzDecoder::new(bytes));
    for entry in archive.entries().map_err(read_error)? {
        let mut entry = entry.map_err(read_error)?;
        let path = entry.path().map_err(read_error)?.to_string_lossy().into_owned();
        if path == member {
            if entry.size() > MEMBER_LIMIT {
                return Err(ArchiveError::TooLarge);
            }
            let mut out = Vec::with_capacity(usize::try_from(entry.size()).unwrap_or_default());
            entry.read_to_end(&mut out).map_err(read_error)?;
            return Ok(out);
        }
    }
    Err(ArchiveError::MemberNotFound)
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
