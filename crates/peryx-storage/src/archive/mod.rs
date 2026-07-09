//! Ecosystem-neutral archive introspection: list and read the members of a cached zip- or tar-family
//! archive on disk, the way pypi-browser does, but against peryx's own blob store.
//!
//! A wheel, an sdist, and an OCI image layer are all one of these container formats, so every
//! ecosystem's file browser drives this one engine; the format-specific validation and metadata
//! extraction live in each ecosystem crate.

mod engine;
mod model;
mod source;

pub use engine::{
    list_members, list_members_nested_path, list_members_path, read_member, read_member_chunk, read_member_chunk_path,
    read_text_member_chunk_nested_path,
};
pub use model::{ArchiveError, Member, MemberChunk, MemberKind};

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
/// gzip-tar (a container layer or an sdist) decompresses every entry it skips over, so an unbounded
/// read lets a small gzip bomb inflate to gigabytes of CPU; the cap bounds that per request.
const MAX_DECOMPRESSED_INSPECT_BYTES: u64 = 512 * 1024 * 1024;

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

/// Whether `filename` names a gzip-compressed tar (`.tar.gz` or `.tgz`), the layer and sdist form.
#[must_use]
pub fn is_tar_gz(filename: &str) -> bool {
    filename
        .get(filename.len().saturating_sub(7)..)
        .is_some_and(|suffix| suffix.eq_ignore_ascii_case(".tar.gz"))
        || filename
            .get(filename.len().saturating_sub(4)..)
            .is_some_and(|suffix| suffix.eq_ignore_ascii_case(".tgz"))
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

fn is_supported_archive(filename: &str) -> bool {
    is_zip(filename) || is_tar(filename) || is_tar_gz(filename)
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
