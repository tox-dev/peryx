use std::io::{Read, Seek, SeekFrom, Write as _};
use std::path::{Path, PathBuf};

use super::model::ArchiveError;
use super::{
    MAX_CONTAINER_DEPTH, MAX_DECOMPRESSED_INSPECT_BYTES, MAX_NESTED_ARCHIVE_SIZE, is_supported_archive, is_tar,
    is_tar_gz, is_zip, read_error, safe_member_name,
};

pub(super) struct ResolvedArchive {
    pub(super) filename: String,
    pub(super) source: ArchiveSource,
    _temps: Vec<tempfile::TempPath>,
}

pub(super) fn resolve_container_stack(
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
    // Cap the decompressed bytes walked to find the member, exactly as the top-level tar readers do; a
    // raw `GzDecoder` here would otherwise let a small crafted `.tar.gz` inflate without bound.
    let mut archive = tar::Archive::new(reader.take(MAX_DECOMPRESSED_INSPECT_BYTES));
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
pub(super) struct ArchiveSource {
    path: PathBuf,
    start: u64,
    len: Option<u64>,
}

impl ArchiveSource {
    pub(super) const fn new(path: PathBuf) -> Self {
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

    pub(super) fn open(&self) -> Result<FileRangeReader, ArchiveError> {
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

pub(super) struct FileRangeReader {
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
