//! Distribution-archive introspection: list and read the members of a cached wheel or sdist, the
//! way pypi-browser does, but against velodex's own blob store.

use std::io::{Cursor, Read, Seek, SeekFrom, Write as _};
use std::path::{Path, PathBuf};

use serde::Serialize;

/// Default amount of one archive member returned by the inspect endpoint.
pub const DEFAULT_MEMBER_CHUNK: u64 = 256 * 1024;

/// Largest member chunk the inspect endpoint accepts in one response.
pub const MAX_MEMBER_CHUNK: u64 = 1024 * 1024;

/// Deepest nested archive stack the inspect endpoint will open.
pub const MAX_CONTAINER_DEPTH: usize = 8;

/// Largest archive member that can be treated as another archive.
pub const MAX_NESTED_ARCHIVE_SIZE: u64 = 128 * 1024 * 1024;

/// Largest number of file entries returned from one archive listing.
pub const MAX_LISTED_ENTRIES: usize = 10_000;

/// One entry of an archive listing.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Serialize)]
pub struct Member {
    pub path: String,
    pub size: u64,
    pub kind: MemberKind,
    pub previewable: bool,
}

/// The UI behavior available for an archive member.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum MemberKind {
    Archive,
    Text,
    Binary,
    Unknown,
}

impl MemberKind {
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Archive => "archive",
            Self::Text => "text",
            Self::Binary => "binary",
            Self::Unknown => "unknown",
        }
    }
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
    #[error("nested archive member {0:?} is not a supported archive")]
    UnsupportedNestedArchive(String),
    #[error("archive member not found")]
    MemberNotFound,
    #[error("archive member offset {offset} is beyond member size {size}")]
    InvalidRange { offset: u64, size: u64 },
    #[error("archive member path {0:?} is not a safe relative path")]
    UnsafeMember(String),
    #[error("archive nesting depth {depth} exceeds the configured limit of {limit}")]
    NestingTooDeep { depth: usize, limit: usize },
    #[error("nested archive member {member:?} is {size} bytes, above the configured limit of {limit} bytes")]
    NestedArchiveTooLarge { member: String, size: u64, limit: u64 },
    #[error("archive listing exceeds the configured limit of {0} file entries")]
    TooManyEntries(usize),
    #[error("archive member {0:?} is not a text member and cannot be previewed inline")]
    BinaryMember(String),
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

struct ResolvedArchive {
    filename: String,
    source: ArchiveSource,
    _temps: Vec<tempfile::TempPath>,
}

fn resolve_container_stack(
    filename: &str,
    path: &Path,
    containers: &[String],
) -> Result<ResolvedArchive, ArchiveError> {
    if containers.len() > MAX_CONTAINER_DEPTH {
        return Err(ArchiveError::NestingTooDeep {
            depth: containers.len(),
            limit: MAX_CONTAINER_DEPTH,
        });
    }
    let mut source = ArchiveSource::new(path.to_path_buf());
    let mut filename = filename.to_owned();
    let mut temps = Vec::new();
    for container in containers {
        let container = safe_member_name(container)?;
        if !is_supported_archive(&container) {
            return Err(ArchiveError::UnsupportedNestedArchive(container));
        }
        source = nested_archive_source(&filename, &source, &container, &mut temps)?;
        filename = container;
    }
    Ok(ResolvedArchive {
        filename,
        source,
        _temps: temps,
    })
}

fn nested_archive_source(
    filename: &str,
    source: &ArchiveSource,
    member: &str,
    temps: &mut Vec<tempfile::TempPath>,
) -> Result<ArchiveSource, ArchiveError> {
    if is_zip(filename) {
        nested_zip_source(source, member, temps)
    } else if is_tar_gz(filename) {
        nested_tar_source(source, member, temps)
    } else {
        Err(ArchiveError::Unsupported)
    }
}

fn nested_zip_source(
    source: &ArchiveSource,
    member: &str,
    temps: &mut Vec<tempfile::TempPath>,
) -> Result<ArchiveSource, ArchiveError> {
    let mut archive = zip::ZipArchive::new(source.open()?).map_err(read_error)?;
    let Ok(entry) = archive.by_name(member) else {
        return Err(ArchiveError::MemberNotFound);
    };
    safe_member_name(entry.name())?;
    if !entry.is_file() {
        return Err(ArchiveError::MemberNotFound);
    }
    reject_large_nested_archive(member, entry.size())?;
    if entry.compression() == zip::CompressionMethod::Stored
        && !entry.encrypted()
        && entry.compressed_size() == entry.size()
        && let Some(start) = entry.data_start()
    {
        return Ok(source.slice(start, entry.compressed_size()));
    }
    copy_nested_archive(entry, temps)
}

fn nested_tar_source(
    source: &ArchiveSource,
    member: &str,
    temps: &mut Vec<tempfile::TempPath>,
) -> Result<ArchiveSource, ArchiveError> {
    let mut archive = tar::Archive::new(flate2::read::GzDecoder::new(source.open()?));
    for entry in archive.entries().map_err(read_error)? {
        let entry = entry.map_err(read_error)?;
        if !entry.header().entry_type().is_file() {
            continue;
        }
        let path = entry.path().map_err(read_error)?.to_string_lossy().into_owned();
        let path = safe_member_name(&path)?;
        if path == member {
            reject_large_nested_archive(member, entry.size())?;
            return copy_nested_archive(entry, temps);
        }
    }
    Err(ArchiveError::MemberNotFound)
}

fn copy_nested_archive(reader: impl Read, temps: &mut Vec<tempfile::TempPath>) -> Result<ArchiveSource, ArchiveError> {
    let mut temp = tempfile::NamedTempFile::new().map_err(read_error)?;
    std::io::copy(&mut reader.take(MAX_NESTED_ARCHIVE_SIZE), temp.as_file_mut()).map_err(read_error)?;
    temp.as_file_mut().flush().map_err(read_error)?;
    let path = temp.path().to_path_buf();
    temps.push(temp.into_temp_path());
    Ok(ArchiveSource::new(path))
}

fn reject_large_nested_archive(member: &str, size: u64) -> Result<(), ArchiveError> {
    if size > MAX_NESTED_ARCHIVE_SIZE {
        Err(ArchiveError::NestedArchiveTooLarge {
            member: member.to_owned(),
            size,
            limit: MAX_NESTED_ARCHIVE_SIZE,
        })
    } else {
        Ok(())
    }
}

#[derive(Debug, Clone)]
struct ArchiveSource {
    path: PathBuf,
    start: u64,
    len: Option<u64>,
}

impl ArchiveSource {
    const fn new(path: PathBuf) -> Self {
        Self {
            path,
            start: 0,
            len: None,
        }
    }

    fn slice(&self, start: u64, len: u64) -> Self {
        Self {
            path: self.path.clone(),
            start: self.start.saturating_add(start),
            len: Some(len),
        }
    }

    fn open(&self) -> Result<FileRangeReader, ArchiveError> {
        FileRangeReader::new(self.path.clone(), self.start, self.len()?)
    }

    fn len(&self) -> Result<u64, ArchiveError> {
        match self.len {
            Some(len) => Ok(len),
            None => Ok(std::fs::metadata(&self.path)
                .map_err(read_error)?
                .len()
                .saturating_sub(self.start)),
        }
    }
}

struct FileRangeReader {
    file: std::fs::File,
    start: u64,
    len: u64,
    position: u64,
}

impl FileRangeReader {
    fn new(path: PathBuf, start: u64, len: u64) -> Result<Self, ArchiveError> {
        let mut file = std::fs::File::open(path).map_err(read_error)?;
        file.seek(SeekFrom::Start(start)).map_err(read_error)?;
        Ok(Self {
            file,
            start,
            len,
            position: 0,
        })
    }
}

impl Read for FileRangeReader {
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        let available = usize::try_from((self.len - self.position).min(u64::try_from(buf.len()).unwrap_or(u64::MAX)))
            .unwrap_or(buf.len());
        let read = self.file.read(&mut buf[..available])?;
        self.position += u64::try_from(read).unwrap_or_default();
        Ok(read)
    }
}

impl Seek for FileRangeReader {
    fn seek(&mut self, pos: SeekFrom) -> std::io::Result<u64> {
        let target = match pos {
            SeekFrom::Start(offset) => i128::from(offset),
            SeekFrom::Current(offset) => i128::from(self.position) + i128::from(offset),
            SeekFrom::End(offset) => i128::from(self.len) + i128::from(offset),
        };
        let position = u64::try_from(target.clamp(0, i128::from(self.len))).unwrap_or_default();
        self.file.seek(SeekFrom::Start(self.start + position))?;
        self.position = position;
        Ok(position)
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

fn is_supported_archive(filename: &str) -> bool {
    is_zip(filename) || is_tar_gz(filename)
}

fn list_members_source(filename: &str, source: &ArchiveSource) -> Result<Vec<Member>, ArchiveError> {
    if is_zip(filename) {
        list_zip(source.open()?)
    } else if is_tar_gz(filename) {
        list_tar(source.open()?)
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
    } else if is_tar_gz(filename) {
        read_tar_member(source.open()?, &member, offset, limit)
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
    let Ok(entry) = archive.by_name(&member) else {
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
            push_member(&mut members, path, entry.size())?;
        }
    }
    members.sort();
    Ok(members)
}

fn read_tar_member(reader: impl Read, member: &str, offset: u64, limit: u64) -> Result<MemberChunk, ArchiveError> {
    let member = safe_member_name(member)?;
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
