mod engine;
mod model;
mod source;
mod zip_range;

pub use engine::{
    list_members, list_members_nested_path, list_members_path, read_member, read_member_chunk, read_member_chunk_path,
    read_text_member_chunk_nested_path,
};
pub use model::{ArchiveError, ArchiveFormat, ArchiveProfile, Member, MemberChunk, MemberKind};
pub use zip_range::{
    MAX_ZIP_CENTRAL_DIRECTORY_BYTES, ZIP_TAIL_BYTES, ZipCentralDirectory, ZipEntry, ZipEntrySearch, ZipRecordError,
    find_zip_entry, zip_central_directory,
};

pub const DEFAULT_MEMBER_CHUNK: u64 = 256 * 1024;

pub const MAX_MEMBER_CHUNK: u64 = 1024 * 1024;

pub const MAX_CONTAINER_DEPTH: usize = 8;

pub const MAX_NESTED_ARCHIVE_SIZE: u64 = 128 * 1024 * 1024;

pub const MAX_LISTED_ENTRIES: usize = 10_000;

/// Gzip-compressed TAR files require decoding skipped entries, so this cap bounds decompression work.
const MAX_DECOMPRESSED_INSPECT_BYTES: u64 = 512 * 1024 * 1024;

#[must_use]
pub fn generic_format(name: &str) -> Option<ArchiveFormat> {
    let extension = std::path::Path::new(name).extension()?;
    if extension.eq_ignore_ascii_case("zip") {
        Some(ArchiveFormat::Zip)
    } else if extension.eq_ignore_ascii_case("tar") {
        Some(ArchiveFormat::Tar)
    } else if name
        .get(name.len().saturating_sub(7)..)
        .is_some_and(|suffix| suffix.eq_ignore_ascii_case(".tar.gz"))
        || name
            .get(name.len().saturating_sub(4)..)
            .is_some_and(|suffix| suffix.eq_ignore_ascii_case(".tgz"))
    {
        Some(ArchiveFormat::TarGz)
    } else {
        None
    }
}

#[must_use]
pub fn strip_ascii_suffix_ignore_case<'a>(value: &'a str, suffix: &str) -> Option<&'a str> {
    let split = value.len().checked_sub(suffix.len())?;
    value.as_bytes()[split..]
        .eq_ignore_ascii_case(suffix.as_bytes())
        .then_some(&value[..split])
}

#[must_use]
pub fn generic_member_kind(path: &str) -> MemberKind {
    if generic_format(path).is_some() {
        return MemberKind::Archive;
    }
    let filename = path.rsplit('/').next().unwrap_or(path);
    if matches!(
        filename,
        "README"
            | "LICENSE"
            | "LICENCE"
            | "COPYING"
            | "COPYRIGHT"
            | "AUTHORS"
            | "CONTRIBUTORS"
            | "CHANGELOG"
            | "CHANGES"
            | "NEWS"
            | "NOTICE"
            | "INSTALL"
            | "THANKS"
            | "TODO"
            | "MANIFEST"
            | "Makefile"
    ) {
        return MemberKind::Text;
    }
    let Some(extension) = std::path::Path::new(filename)
        .extension()
        .and_then(|value| value.to_str())
    else {
        return MemberKind::Unknown;
    };
    if matches!(
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
            | "rst"
            | "svg"
            | "toml"
            | "tsv"
            | "txt"
            | "xml"
            | "yaml"
            | "yml"
    ) {
        MemberKind::Text
    } else if matches!(
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
            | "png"
            | "so"
            | "wasm"
            | "webp"
    ) {
        MemberKind::Binary
    } else {
        MemberKind::Unknown
    }
}

/// Returns a storage-safe relative member name with `.` components removed.
///
/// # Errors
/// Returns [`ArchiveError::UnsafeMember`] when the name could escape a storage key or URL path.
pub fn safe_member_name(path: &str) -> Result<String, ArchiveError> {
    if path.is_empty() || path.starts_with('/') || path.starts_with('\\') || path.contains('\\') || path.contains('\0')
    {
        return Err(ArchiveError::UnsafeMember(path.to_owned()));
    }
    let mut components = Vec::new();
    for component in path.split('/') {
        match component {
            "." => {}
            "" | ".." => return Err(ArchiveError::UnsafeMember(path.to_owned())),
            _ => components.push(component),
        }
    }
    if components.is_empty() {
        Err(ArchiveError::UnsafeMember(path.to_owned()))
    } else {
        Ok(components.join("/"))
    }
}

fn zip_member_position(
    archive: &mut zip::ZipArchive<impl std::io::Read + std::io::Seek>,
    member: &str,
) -> Result<Option<usize>, ArchiveError> {
    for position in 0..archive.len() {
        let entry = archive.by_index(position).map_err(read_error)?;
        if entry.is_file() && safe_member_name(entry.name()).is_ok_and(|name| name == member) {
            return Ok(Some(position));
        }
    }
    Ok(None)
}

pub fn read_error(err: impl std::fmt::Display) -> ArchiveError {
    ArchiveError::Read(err.to_string())
}

const fn ensure_inspection_range(start: u64, offset: u64, count: u64) -> Result<(), ArchiveError> {
    if start.saturating_add(offset).saturating_add(count) > MAX_DECOMPRESSED_INSPECT_BYTES {
        Err(ArchiveError::InspectionLimitExceeded {
            limit: MAX_DECOMPRESSED_INSPECT_BYTES,
        })
    } else {
        Ok(())
    }
}

#[cfg(test)]
#[path = "../tests/unit/tests.rs"]
mod tests;
