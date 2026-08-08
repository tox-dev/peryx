//! Safe ZIP and TAR introspection for ecosystem-owned artifact formats.

mod engine;
mod model;
mod source;

pub use engine::{
    list_members, list_members_nested_path, list_members_path, read_member, read_member_chunk, read_member_chunk_path,
    read_text_member_chunk_nested_path,
};
pub use model::{ArchiveError, ArchiveFormat, ArchiveProfile, Member, MemberChunk, MemberKind};

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

/// Most decompressed bytes an archive listing or member read pulls from a top-level archive. Walking a
/// A gzip-compressed tar decompresses every entry it skips over, so an unbounded
/// read lets a small gzip bomb inflate to gigabytes of CPU; the cap bounds that per request.
const MAX_DECOMPRESSED_INSPECT_BYTES: u64 = 512 * 1024 * 1024;

/// Classify conventional ZIP and TAR names.
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

/// Strip a case-insensitive ASCII `suffix` from `value`, returning the remainder, or `None` when it
/// does not end with the suffix.
#[must_use]
pub fn strip_ascii_suffix_ignore_case<'a>(value: &'a str, suffix: &str) -> Option<&'a str> {
    let split = value.len().checked_sub(suffix.len())?;
    value.as_bytes()[split..]
        .eq_ignore_ascii_case(suffix.as_bytes())
        .then_some(&value[..split])
}

/// Classify ecosystem-neutral archive members.
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

/// Validate that `path` is a safe relative archive member name (no absolute prefix, `..`, `\`, or
/// NUL), returning it owned, or [`ArchiveError::UnsafeMember`].
///
/// # Errors
/// Returns [`ArchiveError::UnsafeMember`] when the name could escape a storage key or URL path.
pub fn safe_member_name(path: &str) -> Result<String, ArchiveError> {
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

/// Wrap any displayable I/O or decode failure as [`ArchiveError::Read`].
pub fn read_error(err: impl std::fmt::Display) -> ArchiveError {
    ArchiveError::Read(err.to_string())
}
