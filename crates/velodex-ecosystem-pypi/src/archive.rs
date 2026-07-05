//! Distribution-archive introspection: list and read the members of a cached wheel or sdist, the
//! way pypi-browser does, but against velodex's own blob store.

use std::collections::{BTreeMap, BTreeSet};
use std::io::{Cursor, Read, Seek, SeekFrom, Write as _};
use std::path::{Path, PathBuf};

use crate::{DistributionKind, parse_distribution_filename};
use base64::Engine as _;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use serde::Serialize;
use sha2::{Sha256, Sha384, Sha512};
use zip::read::HasZipMetadata;

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

pub const MAX_WHEEL_METADATA_BYTES: u64 = 16 * 1024 * 1024;
const MAX_WHEEL_RECORD_BYTES: u64 = 64 * 1024 * 1024;
const MAX_WHEEL_ENTRY_POINTS_BYTES: u64 = 1024 * 1024;
const MAX_SDIST_METADATA_BYTES: u64 = 16 * 1024 * 1024;
const MAX_SDIST_ENTRIES: usize = 100_000;
const SUPPORTED_WHEEL_MAJOR_VERSION: u64 = 1;

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
    #[error("unsupported archive type; accepted formats are .whl, .zip, .egg, .tar, .tar.gz, and .tgz")]
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
    #[error("invalid wheel: {0}")]
    InvalidWheel(String),
    #[error("invalid sdist: {0}")]
    InvalidSdist(String),
    #[error("archive read failed: {0}")]
    Read(String),
}

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
    } else if is_tar(filename) {
        nested_tar_source(source.open()?, member, temps)
    } else if is_tar_gz(filename) {
        nested_tar_source(flate2::read::GzDecoder::new(source.open()?), member, temps)
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
    reader: impl Read,
    member: &str,
    temps: &mut Vec<tempfile::TempPath>,
) -> Result<ArchiveSource, ArchiveError> {
    let mut archive = tar::Archive::new(reader);
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
    std::path::Path::new(filename).extension().is_some_and(|ext| {
        ext.eq_ignore_ascii_case("whl") || ext.eq_ignore_ascii_case("zip") || ext.eq_ignore_ascii_case("egg")
    })
}

fn is_tar(filename: &str) -> bool {
    std::path::Path::new(filename)
        .extension()
        .is_some_and(|ext| ext.eq_ignore_ascii_case("tar"))
}

fn is_tar_gz(filename: &str) -> bool {
    filename
        .get(filename.len().saturating_sub(7)..)
        .is_some_and(|suffix| suffix.eq_ignore_ascii_case(".tar.gz"))
        || filename
            .get(filename.len().saturating_sub(4)..)
            .is_some_and(|suffix| suffix.eq_ignore_ascii_case(".tgz"))
}

fn strip_ascii_suffix_ignore_case<'a>(value: &'a str, suffix: &str) -> Option<&'a str> {
    let split = value.len().checked_sub(suffix.len())?;
    value.as_bytes()[split..]
        .eq_ignore_ascii_case(suffix.as_bytes())
        .then_some(&value[..split])
}

fn is_supported_archive(filename: &str) -> bool {
    is_zip(filename) || is_tar(filename) || is_tar_gz(filename)
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
    let mut archive = tar::Archive::new(reader);
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
    let mut archive = tar::Archive::new(reader);
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

fn invalid_wheel(message: impl Into<String>) -> ArchiveError {
    ArchiveError::InvalidWheel(message.into())
}

fn invalid_sdist(message: impl Into<String>) -> ArchiveError {
    ArchiveError::InvalidSdist(message.into())
}

/// Validate a wheel's required structure and return its exact `METADATA` bytes.
///
/// # Errors
/// Returns [`ArchiveError::InvalidWheel`] when required wheel metadata is missing or inconsistent,
/// and [`ArchiveError::Read`] when the staged file or ZIP cannot be read.
pub fn validate_wheel_path(filename: &str, path: &Path) -> Result<Vec<u8>, ArchiveError> {
    let file = std::fs::File::open(path).map_err(read_error)?;
    validate_wheel_reader(filename, file)
}

fn validate_wheel_reader(filename: &str, reader: impl Read + Seek) -> Result<Vec<u8>, ArchiveError> {
    let expected_dist_info = expected_wheel_dist_info_dir(filename)?;
    let metadata_path = format!("{expected_dist_info}/METADATA");
    let wheel_path = format!("{expected_dist_info}/WHEEL");
    let record_path = format!("{expected_dist_info}/RECORD");
    let entry_points_path = format!("{expected_dist_info}/entry_points.txt");

    let mut archive = zip::ZipArchive::new(reader).map_err(read_error)?;
    let members = wheel_members(&mut archive, &expected_dist_info)?;
    for path in [&metadata_path, &wheel_path, &record_path] {
        if !members.files.contains_key(path) {
            return Err(invalid_wheel(format!("missing required {path}")));
        }
    }

    let metadata = read_zip_member_limited(&mut archive, &metadata_path, MAX_WHEEL_METADATA_BYTES)?;
    let wheel = read_zip_member_limited(&mut archive, &wheel_path, MAX_WHEEL_METADATA_BYTES)?;
    validate_wheel_file(filename, &wheel)?;

    let record = read_zip_member_limited(&mut archive, &record_path, MAX_WHEEL_RECORD_BYTES)?;
    validate_record(&mut archive, &members.files, &record, &record_path, &expected_dist_info)?;

    if members.files.contains_key(&entry_points_path) {
        let entry_points = read_zip_member_limited(&mut archive, &entry_points_path, MAX_WHEEL_ENTRY_POINTS_BYTES)?;
        validate_entry_points(&entry_points)?;
    }

    Ok(metadata)
}

#[derive(Debug)]
struct WheelMembers {
    files: BTreeMap<String, WheelMember>,
}

#[derive(Debug, Clone, Copy)]
struct WheelMember {
    index: usize,
    size: u64,
}

fn wheel_members<R: Read + Seek>(
    archive: &mut zip::ZipArchive<R>,
    expected_dist_info: &str,
) -> Result<WheelMembers, ArchiveError> {
    let mut dist_info_dirs = BTreeSet::new();
    let mut files = BTreeMap::new();
    for index in 0..archive.len() {
        let entry = archive.by_index(index).map_err(read_error)?;
        let raw_name = entry.name();
        let name = if entry.is_dir() {
            raw_name.strip_suffix('/').unwrap_or(raw_name)
        } else {
            raw_name
        };
        let name = safe_member_name(name)?;
        if let Some(dist_info_dir) = top_level_dist_info_dir(&name) {
            dist_info_dirs.insert(dist_info_dir.to_owned());
        }
        if entry.is_file() {
            files.insert(
                name.clone(),
                WheelMember {
                    index,
                    size: entry.size(),
                },
            );
        }
    }

    match dist_info_dirs.len() {
        0 => Err(invalid_wheel("missing .dist-info directory")),
        1 if dist_info_dirs.contains(expected_dist_info) => Ok(WheelMembers { files }),
        1 => Err(invalid_wheel(format!(
            ".dist-info directory {} does not match expected {expected_dist_info}",
            dist_info_dirs.iter().next().expect("one dist-info directory")
        ))),
        _ => Err(invalid_wheel(format!(
            "multiple .dist-info directories found: {}",
            dist_info_dirs.into_iter().collect::<Vec<_>>().join(", ")
        ))),
    }
}

fn top_level_dist_info_dir(path: &str) -> Option<&str> {
    let first = path.split('/').next()?;
    first.ends_with(".dist-info").then_some(first)
}

fn expected_wheel_dist_info_dir(filename: &str) -> Result<String, ArchiveError> {
    let parsed = parse_distribution_filename(filename)
        .map_err(|err| invalid_wheel(format!("invalid wheel filename {filename:?}: {err:?}")))?;
    if parsed.kind != DistributionKind::Wheel {
        return Err(invalid_wheel(format!("{filename:?} is not a wheel filename")));
    }
    let name = parsed.normalized_name.replace('-', "_");
    let version = parsed.version.to_string().replace('-', "_");
    Ok(format!("{name}-{version}.dist-info"))
}

fn read_zip_member_limited<R: Read + Seek>(
    archive: &mut zip::ZipArchive<R>,
    path: &str,
    limit: u64,
) -> Result<Vec<u8>, ArchiveError> {
    let mut entry = archive.by_name(path).map_err(read_error)?;
    if entry.size() > limit {
        return Err(invalid_wheel(format!(
            "{path} is {} bytes, above the upload validation limit of {limit} bytes",
            entry.size()
        )));
    }
    let capacity = usize::try_from(entry.size()).expect("wheel validation limit fits usize");
    let mut bytes = Vec::with_capacity(capacity);
    entry.read_to_end(&mut bytes).map_err(read_error)?;
    Ok(bytes)
}

fn validate_wheel_file(filename: &str, bytes: &[u8]) -> Result<(), ArchiveError> {
    let text = std::str::from_utf8(bytes).map_err(|_| invalid_wheel("WHEEL is not valid UTF-8"))?;
    let versions = header_values(text, "Wheel-Version");
    let [version] = versions.as_slice() else {
        return Err(invalid_wheel("WHEEL must contain exactly one Wheel-Version field"));
    };
    let version = parse_wheel_version(version)?;
    if version[0] > SUPPORTED_WHEEL_MAJOR_VERSION {
        return Err(invalid_wheel(format!(
            "Wheel-Version {} is newer than supported major version {SUPPORTED_WHEEL_MAJOR_VERSION}",
            version.iter().map(u64::to_string).collect::<Vec<_>>().join(".")
        )));
    }

    let purelib = header_values(text, "Root-Is-Purelib");
    let [purelib] = purelib.as_slice() else {
        return Err(invalid_wheel("WHEEL must contain exactly one Root-Is-Purelib field"));
    };
    if !matches!(purelib.to_ascii_lowercase().as_str(), "true" | "false") {
        return Err(invalid_wheel(format!("Root-Is-Purelib has invalid value {purelib:?}")));
    }

    validate_wheel_build(filename, &header_values(text, "Build"))?;

    let tags = header_values(text, "Tag");
    if tags.is_empty() {
        return Err(invalid_wheel("WHEEL must contain at least one Tag field"));
    }
    let actual = tags
        .into_iter()
        .map(validate_wheel_tag)
        .collect::<Result<BTreeSet<_>, _>>()?;
    let expected = expected_wheel_tags(filename);
    if actual != expected {
        return Err(invalid_wheel(format!(
            "WHEEL Tag fields do not match filename tags; expected {}, got {}",
            expected.into_iter().collect::<Vec<_>>().join(", "),
            actual.into_iter().collect::<Vec<_>>().join(", ")
        )));
    }
    Ok(())
}

fn validate_wheel_build(filename: &str, actual: &[&str]) -> Result<(), ArchiveError> {
    match (expected_wheel_build(filename), actual) {
        (None, []) => Ok(()),
        (None, [_]) => Err(invalid_wheel(
            "WHEEL contains a Build field, but the filename has no build tag",
        )),
        (Some(expected), [actual]) if *actual == expected => Ok(()),
        (Some(expected), []) => Err(invalid_wheel(format!(
            "WHEEL is missing Build field for filename build tag {expected:?}"
        ))),
        (Some(expected), [actual]) => Err(invalid_wheel(format!(
            "WHEEL Build field {actual:?} does not match filename build tag {expected:?}"
        ))),
        (None | Some(_), _) => Err(invalid_wheel("WHEEL must contain at most one Build field")),
    }
}

fn header_values<'a>(text: &'a str, key: &str) -> Vec<&'a str> {
    text.lines()
        .filter_map(|line| {
            let (name, value) = line.split_once(':')?;
            name.eq_ignore_ascii_case(key).then(|| value.trim())
        })
        .collect()
}

fn parse_wheel_version(value: &str) -> Result<Vec<u64>, ArchiveError> {
    let parts = value
        .split('.')
        .map(|part| {
            if part.is_empty() || !part.bytes().all(|byte| byte.is_ascii_digit()) {
                return Err(invalid_wheel(format!("invalid Wheel-Version {value:?}")));
            }
            part.parse::<u64>()
                .map_err(|_| invalid_wheel(format!("invalid Wheel-Version {value:?}")))
        })
        .collect::<Result<Vec<_>, _>>()?;
    if parts.len() < 2 {
        return Err(invalid_wheel(format!("invalid Wheel-Version {value:?}")));
    }
    Ok(parts)
}

fn validate_wheel_tag(value: &str) -> Result<String, ArchiveError> {
    let parts = value.split('-').collect::<Vec<_>>();
    let [python, abi, platform] = parts.as_slice() else {
        return Err(invalid_wheel(format!("invalid WHEEL Tag {value:?}")));
    };
    if [python, abi, platform]
        .into_iter()
        .any(|part| part.is_empty() || !part.bytes().all(|byte| byte.is_ascii_alphanumeric() || byte == b'_'))
    {
        return Err(invalid_wheel(format!("invalid WHEEL Tag {value:?}")));
    }
    Ok(value.to_owned())
}

fn expected_wheel_tags(filename: &str) -> BTreeSet<String> {
    let parts = wheel_filename_parts(filename);
    let python_tags = parts[parts.len() - 3].split('.');
    let abi_tags = parts[parts.len() - 2].split('.');
    let platform_tags = parts[parts.len() - 1].split('.');
    let mut tags = BTreeSet::new();
    for python in python_tags {
        for abi in abi_tags.clone() {
            for platform in platform_tags.clone() {
                tags.insert(format!("{python}-{abi}-{platform}"));
            }
        }
    }
    tags
}

fn expected_wheel_build(filename: &str) -> Option<&str> {
    let parts = wheel_filename_parts(filename);
    (parts.len() == 6).then_some(parts[2])
}

fn wheel_filename_parts(filename: &str) -> Vec<&str> {
    let stem = &filename[..filename.len() - 4];
    let parts = stem.split('-').collect::<Vec<_>>();
    debug_assert!(matches!(parts.len(), 5 | 6));
    parts
}

#[derive(Debug)]
struct RecordEntry {
    hash: String,
    size: String,
}

fn validate_record<R: Read + Seek>(
    archive: &mut zip::ZipArchive<R>,
    members: &BTreeMap<String, WheelMember>,
    bytes: &[u8],
    record_path: &str,
    dist_info_dir: &str,
) -> Result<(), ArchiveError> {
    let records = record_entries(bytes)?;
    validate_record_rows(members, &records, record_path, dist_info_dir)?;
    for (path, member) in members {
        if path == record_path || is_record_signature(path, dist_info_dir) {
            continue;
        }
        let record = records
            .get(path)
            .ok_or_else(|| invalid_wheel(format!("RECORD is missing entry for {path}")))?;
        validate_record_size(path, &record.size, member.size)?;
        validate_record_hash(archive, path, *member, &record.hash)?;
    }
    Ok(())
}

fn record_entries(bytes: &[u8]) -> Result<BTreeMap<String, RecordEntry>, ArchiveError> {
    let mut reader = csv::ReaderBuilder::new()
        .has_headers(false)
        .flexible(false)
        .from_reader(bytes);
    let mut records = BTreeMap::new();
    for result in reader.records() {
        let row = result.map_err(|err| invalid_wheel(format!("invalid RECORD CSV: {err}")))?;
        if row.len() != 3 {
            return Err(invalid_wheel("RECORD rows must contain path, hash, and size"));
        }
        let path = safe_member_name(&row[0])?;
        if records
            .insert(
                path.clone(),
                RecordEntry {
                    hash: row[1].to_owned(),
                    size: row[2].to_owned(),
                },
            )
            .is_some()
        {
            return Err(invalid_wheel(format!("RECORD contains duplicate entry for {path}")));
        }
    }
    if records.is_empty() {
        return Err(invalid_wheel("RECORD is empty"));
    }
    Ok(records)
}

fn validate_record_rows(
    members: &BTreeMap<String, WheelMember>,
    records: &BTreeMap<String, RecordEntry>,
    record_path: &str,
    dist_info_dir: &str,
) -> Result<(), ArchiveError> {
    for (path, record) in records {
        if is_record_signature(path, dist_info_dir) {
            return Err(invalid_wheel(format!(
                "deprecated signature file {path} must not be listed in RECORD"
            )));
        }
        let Some(member) = members.get(path) else {
            return Err(invalid_wheel(format!(
                "RECORD entry {path} is not present in the archive"
            )));
        };
        if path == record_path {
            if !record.hash.is_empty() {
                return Err(invalid_wheel("RECORD must not contain a hash for itself"));
            }
            if !record.size.is_empty() {
                validate_record_size(path, &record.size, member.size)?;
            }
        }
    }
    if !records.contains_key(record_path) {
        return Err(invalid_wheel(format!("RECORD is missing entry for {record_path}")));
    }
    Ok(())
}

fn is_record_signature(path: &str, dist_info_dir: &str) -> bool {
    path.strip_prefix(dist_info_dir)
        .is_some_and(|suffix| matches!(suffix, "/RECORD.jws" | "/RECORD.p7s"))
}

fn validate_record_size(path: &str, value: &str, actual: u64) -> Result<(), ArchiveError> {
    if value.is_empty() {
        return Ok(());
    }
    let expected = value
        .parse::<u64>()
        .map_err(|_| invalid_wheel(format!("RECORD entry {path} has invalid size {value:?}")))?;
    if expected != actual {
        return Err(invalid_wheel(format!(
            "RECORD entry {path} has size {expected}, but archive member is {actual} bytes"
        )));
    }
    Ok(())
}

fn validate_record_hash<R: Read + Seek>(
    archive: &mut zip::ZipArchive<R>,
    path: &str,
    member: WheelMember,
    value: &str,
) -> Result<(), ArchiveError> {
    let (algorithm, expected) = value
        .split_once('=')
        .ok_or_else(|| invalid_wheel(format!("RECORD entry {path} is missing hash algorithm")))?;
    if expected.is_empty() {
        return Err(invalid_wheel(format!("RECORD entry {path} is missing hash value")));
    }
    let expected = URL_SAFE_NO_PAD
        .decode(expected)
        .map_err(|err| invalid_wheel(format!("RECORD entry {path} has invalid base64 hash: {err}")))?;
    let mut entry = archive.by_index(member.index).map_err(read_error)?;
    let actual = match algorithm {
        "sha256" => digest_reader::<Sha256>(&mut entry)?,
        "sha384" => digest_reader::<Sha384>(&mut entry)?,
        "sha512" => digest_reader::<Sha512>(&mut entry)?,
        _ => {
            return Err(invalid_wheel(format!(
                "RECORD entry {path} uses unsupported hash algorithm {algorithm:?}; expected sha256, sha384, or sha512"
            )));
        }
    };
    if !constant_time_bytes_eq(&actual, &expected) {
        return Err(invalid_wheel(format!("RECORD hash mismatch for {path}")));
    }
    Ok(())
}

fn digest_reader<D: sha2::Digest>(mut reader: impl Read) -> Result<Vec<u8>, ArchiveError> {
    let mut hasher = D::new();
    let mut buffer = [0; 16 * 1024];
    loop {
        let read = reader.read(&mut buffer).map_err(read_error)?;
        if read == 0 {
            break;
        }
        hasher.update(&buffer[..read]);
    }
    Ok(hasher.finalize().to_vec())
}

fn constant_time_bytes_eq(left: &[u8], right: &[u8]) -> bool {
    let mut diff = left.len() ^ right.len();
    for index in 0..left.len().max(right.len()) {
        diff |=
            usize::from(left.get(index).copied().unwrap_or_default() ^ right.get(index).copied().unwrap_or_default());
    }
    diff == 0
}

fn validate_entry_points(bytes: &[u8]) -> Result<(), ArchiveError> {
    let text = std::str::from_utf8(bytes).map_err(|_| invalid_wheel("entry_points.txt is not valid UTF-8"))?;
    let mut section = None;
    for (line_no, line) in text.lines().enumerate() {
        let trimmed = line.trim();
        if trimmed.is_empty() || trimmed.starts_with('#') || trimmed.starts_with(';') {
            continue;
        }
        if line.starts_with(' ') || line.starts_with('\t') {
            if section.is_none() {
                return Err(invalid_wheel(format!(
                    "entry_points.txt continuation on line {} has no section",
                    line_no + 1
                )));
            }
            continue;
        }
        if trimmed.starts_with('[') && trimmed.ends_with(']') {
            let name = trimmed[1..trimmed.len() - 1].trim();
            if name.is_empty() {
                return Err(invalid_wheel(format!(
                    "entry_points.txt has an empty section on line {}",
                    line_no + 1
                )));
            }
            section = Some(name.to_owned());
            continue;
        }
        let Some((name, _value)) = trimmed.split_once('=') else {
            return Err(invalid_wheel(format!(
                "entry_points.txt line {} is not a key=value entry",
                line_no + 1
            )));
        };
        let name = name.trim();
        if name.is_empty() {
            return Err(invalid_wheel(format!(
                "entry_points.txt line {} has an empty entry point name",
                line_no + 1
            )));
        }
        let Some(section) = section.as_deref() else {
            return Err(invalid_wheel(format!(
                "entry_points.txt entry on line {} has no section",
                line_no + 1
            )));
        };
        if matches!(section, "console_scripts" | "gui_scripts") && !is_valid_entry_point_name(name) {
            return Err(invalid_wheel(format!(
                "entry_points.txt has invalid entry point name {name:?} in section {section:?}"
            )));
        }
    }
    Ok(())
}

fn is_valid_entry_point_name(value: &str) -> bool {
    !value.is_empty()
        && !value.contains('/')
        && !value.contains('\\')
        && value
            .chars()
            .all(|ch| ch.is_alphanumeric() || matches!(ch, '_' | '.' | '-'))
}

/// Extract a wheel's `*.dist-info/METADATA` document, the file pypi.org serves as the PEP 658
/// sibling of an upload. Returns `None` for non-wheels or wheels without one.
#[must_use]
pub fn wheel_metadata(filename: &str, bytes: &[u8]) -> Option<Vec<u8>> {
    wheel_metadata_reader(filename, Cursor::new(bytes)).ok().flatten()
}

/// The exact wheel metadata member implied by a wheel filename.
///
/// # Errors
/// Returns [`ArchiveError::InvalidWheel`] when `filename` ends with `.whl` but is not a valid
/// wheel filename.
pub fn wheel_metadata_member_path(filename: &str) -> Result<Option<String>, ArchiveError> {
    if !is_wheel(filename) {
        return Ok(None);
    }
    Ok(Some(format!("{}/METADATA", expected_wheel_dist_info_dir(filename)?)))
}

/// Extract a wheel's `*.dist-info/METADATA` document from a staged file without buffering the wheel.
///
/// # Errors
/// Returns [`ArchiveError::Read`] when the staged file or ZIP cannot be read.
pub fn wheel_metadata_path(filename: &str, path: &Path) -> Result<Option<Vec<u8>>, ArchiveError> {
    let file = std::fs::File::open(path).map_err(read_error)?;
    wheel_metadata_reader(filename, file)
}

/// Validate a PEP 625 `.tar.gz` sdist and return its exact `PKG-INFO` bytes.
///
/// # Errors
/// Returns [`ArchiveError::InvalidSdist`] when required sdist structure or metadata is missing or
/// inconsistent, and [`ArchiveError::Read`] when the staged file or tarball cannot be read.
pub fn validate_sdist_path(filename: &str, path: &Path) -> Result<Vec<u8>, ArchiveError> {
    let file = std::fs::File::open(path).map_err(read_error)?;
    validate_sdist_reader(filename, file)
}

/// Extract an sdist's `PKG-INFO` document from a staged file without buffering the sdist.
///
/// # Errors
/// Returns [`ArchiveError::InvalidSdist`] for invalid `.tar.gz` sdists and [`ArchiveError::Read`]
/// when the staged file or tarball cannot be read.
pub fn sdist_metadata_path(filename: &str, path: &Path) -> Result<Option<Vec<u8>>, ArchiveError> {
    if !is_tar_gz(filename) {
        return Ok(None);
    }
    validate_sdist_path(filename, path).map(Some)
}

fn validate_sdist_reader(filename: &str, reader: impl Read) -> Result<Vec<u8>, ArchiveError> {
    let root = expected_sdist_root(filename)?;
    let mut archive = tar::Archive::new(flate2::read::GzDecoder::new(reader));
    let mut members = SdistMembers::new(root);
    for entry in archive.entries().map_err(read_error)? {
        let mut entry = entry.map_err(read_error)?;
        let entry_type = entry.header().entry_type();
        let path = entry.path().map_err(read_error)?.to_string_lossy().into_owned();
        let path = if entry_type.is_dir() {
            path.strip_suffix('/').unwrap_or(&path).to_owned()
        } else {
            path
        };
        let path = safe_tar_member_name(&path)?;
        validate_sdist_member_path(&members.root, &path, entry_type)?;
        validate_sdist_member_type(&path, entry_type)?;
        if entry_type.is_symlink() || entry_type.is_hard_link() {
            let target = entry
                .link_name()
                .map_err(read_error)?
                .ok_or_else(|| invalid_sdist(format!("link entry {path:?} is missing its target")))?
                .to_string_lossy()
                .into_owned();
            validate_sdist_link(&members.root, &path, &target, entry_type)?;
        }
        if entry_type.is_file() {
            let size = entry.size();
            if path == members.pkg_info_path() {
                let metadata = read_sdist_member_limited(&mut entry, &path, size, MAX_SDIST_METADATA_BYTES)?;
                members.set_metadata(metadata)?;
            }
        }
        members.record(path, entry_type)?;
    }
    members.finish()
}

#[derive(Debug)]
struct SdistMembers {
    root: String,
    pyproject: bool,
    metadata: Option<Vec<u8>>,
    entries: usize,
    paths: BTreeSet<String>,
}

impl SdistMembers {
    const fn new(root: String) -> Self {
        Self {
            root,
            pyproject: false,
            metadata: None,
            entries: 0,
            paths: BTreeSet::new(),
        }
    }

    fn pkg_info_path(&self) -> String {
        format!("{}/PKG-INFO", self.root)
    }

    fn pyproject_path(&self) -> String {
        format!("{}/pyproject.toml", self.root)
    }

    fn set_metadata(&mut self, metadata: Vec<u8>) -> Result<(), ArchiveError> {
        if self.metadata.replace(metadata).is_some() {
            return Err(invalid_sdist(format!(
                "multiple {} entries found",
                self.pkg_info_path()
            )));
        }
        Ok(())
    }

    fn record(&mut self, path: String, entry_type: tar::EntryType) -> Result<(), ArchiveError> {
        self.entries += 1;
        if self.entries > MAX_SDIST_ENTRIES {
            return Err(invalid_sdist(format!(
                "archive has more than {MAX_SDIST_ENTRIES} entries"
            )));
        }
        if entry_type.is_file() {
            self.pyproject |= path == self.pyproject_path();
        }
        if entry_type.is_file() || entry_type.is_hard_link() || entry_type.is_symlink() {
            self.paths.insert(path);
        }
        Ok(())
    }

    fn finish(self) -> Result<Vec<u8>, ArchiveError> {
        if !self.pyproject {
            return Err(invalid_sdist(format!("missing required {}/pyproject.toml", self.root)));
        }
        let metadata = self
            .metadata
            .ok_or_else(|| invalid_sdist(format!("missing required {}/PKG-INFO", self.root)))?;
        let text = std::str::from_utf8(&metadata).map_err(|_| invalid_sdist("PKG-INFO is not valid UTF-8"))?;
        let doc = crate::parse_metadata(text);
        let metadata_version_text = doc
            .metadata_version
            .as_deref()
            .ok_or_else(|| invalid_sdist("PKG-INFO is missing Metadata-Version"))?;
        let metadata_version = parse_metadata_version(metadata_version_text)?;
        if !metadata_version_at_least(metadata_version, (2, 2)) {
            return Err(invalid_sdist(format!(
                "PKG-INFO Metadata-Version {metadata_version_text} is older than the required 2.2"
            )));
        }
        if metadata_version_at_least(metadata_version, (2, 4)) {
            for license_file in doc.license_files {
                let license_file = safe_tar_member_name(&license_file)?;
                let path = format!("{}/{}", self.root, license_file);
                if !self.paths.contains(&path) {
                    return Err(invalid_sdist(format!(
                        "License-File {license_file:?} is missing from the sdist"
                    )));
                }
            }
        }
        Ok(metadata)
    }
}

fn expected_sdist_root(filename: &str) -> Result<String, ArchiveError> {
    let parsed = parse_distribution_filename(filename)
        .map_err(|err| invalid_sdist(format!("invalid sdist filename {filename:?}: {err:?}")))?;
    if parsed.kind != DistributionKind::SdistTarGz {
        return Err(invalid_sdist(format!("{filename:?} is not an sdist filename")));
    }
    let root = strip_ascii_suffix_ignore_case(filename, ".tar.gz")
        .expect("parse_distribution_filename accepted only .tar.gz sdists");
    safe_member_name(root)?;
    Ok(root.to_owned())
}

fn validate_sdist_member_path(root: &str, path: &str, entry_type: tar::EntryType) -> Result<(), ArchiveError> {
    if path == root && entry_type.is_dir() {
        return Ok(());
    }
    let Some((top_level, _rest)) = path.split_once('/') else {
        return Err(invalid_sdist(format!(
            "archive entry {path:?} is outside required top-level directory {root:?}"
        )));
    };
    if top_level != root {
        return Err(invalid_sdist(format!(
            "archive entry {path:?} is outside required top-level directory {root:?}"
        )));
    }
    Ok(())
}

fn validate_sdist_member_type(path: &str, entry_type: tar::EntryType) -> Result<(), ArchiveError> {
    if entry_type.is_file() || entry_type.is_dir() || entry_type.is_hard_link() || entry_type.is_symlink() {
        return Ok(());
    }
    Err(invalid_sdist(format!(
        "unsupported tar entry {path:?} of type {:?}",
        entry_type.as_byte() as char
    )))
}

fn validate_sdist_link(root: &str, path: &str, target: &str, entry_type: tar::EntryType) -> Result<(), ArchiveError> {
    let target = safe_tar_member_name(target)?;
    let resolved = if entry_type.is_symlink() {
        let (parent, _) = path
            .rsplit_once('/')
            .expect("sdist link paths are validated before link targets");
        format!("{parent}/{target}")
    } else {
        target
    };
    if resolved == root || resolved.strip_prefix(root).is_some_and(|rest| rest.starts_with('/')) {
        return Ok(());
    }
    Err(invalid_sdist(format!(
        "tar link {path:?} points outside required top-level directory {root:?}"
    )))
}

fn read_sdist_member_limited(
    reader: &mut impl Read,
    path: &str,
    size: u64,
    limit: u64,
) -> Result<Vec<u8>, ArchiveError> {
    if size > limit {
        return Err(invalid_sdist(format!(
            "{path} is {size} bytes, above the upload validation limit of {limit} bytes"
        )));
    }
    let capacity = usize::try_from(size).expect("sdist validation limit fits usize");
    let mut bytes = Vec::with_capacity(capacity);
    reader.read_to_end(&mut bytes).map_err(read_error)?;
    Ok(bytes)
}

fn safe_tar_member_name(path: &str) -> Result<String, ArchiveError> {
    let path = safe_member_name(path)?;
    if is_windows_absolute(&path) {
        Err(ArchiveError::UnsafeMember(path))
    } else {
        Ok(path)
    }
}

fn is_windows_absolute(path: &str) -> bool {
    path.as_bytes()
        .get(..2)
        .is_some_and(|prefix| prefix[0].is_ascii_alphabetic() && prefix[1] == b':')
}

fn parse_metadata_version(value: &str) -> Result<(u64, u64), ArchiveError> {
    let Some((major, minor)) = value.split_once('.') else {
        return Err(invalid_sdist(format!("invalid Metadata-Version {value:?}")));
    };
    let parse = |part: &str| {
        if part.is_empty() || !part.bytes().all(|byte| byte.is_ascii_digit()) {
            return Err(invalid_sdist(format!("invalid Metadata-Version {value:?}")));
        }
        part.parse::<u64>()
            .map_err(|_| invalid_sdist(format!("invalid Metadata-Version {value:?}")))
    };
    Ok((parse(major)?, parse(minor)?))
}

const fn metadata_version_at_least(actual: (u64, u64), required: (u64, u64)) -> bool {
    actual.0 > required.0 || (actual.0 == required.0 && actual.1 >= required.1)
}

fn wheel_metadata_reader(filename: &str, reader: impl Read + Seek) -> Result<Option<Vec<u8>>, ArchiveError> {
    let Some(metadata_path) = wheel_metadata_member_path(filename)? else {
        return Ok(None);
    };
    let mut archive = zip::ZipArchive::new(reader).map_err(read_error)?;
    let mut entry = match archive.by_name(&metadata_path) {
        Ok(entry) => entry,
        Err(zip::result::ZipError::FileNotFound) => return Ok(None),
        Err(err) => return Err(read_error(err)),
    };
    if !entry.is_file() {
        return Ok(None);
    }
    if entry.size() > MAX_WHEEL_METADATA_BYTES {
        return Err(invalid_wheel(format!(
            "{metadata_path} is {} bytes, above the upload validation limit of {MAX_WHEEL_METADATA_BYTES} bytes",
            entry.size()
        )));
    }
    let mut bytes = Vec::with_capacity(entry.size().min(256 * 1024) as usize);
    entry.read_to_end(&mut bytes).map_err(read_error)?;
    Ok(Some(bytes))
}

fn is_wheel(filename: &str) -> bool {
    std::path::Path::new(filename)
        .extension()
        .is_some_and(|ext| ext.eq_ignore_ascii_case("whl"))
}
