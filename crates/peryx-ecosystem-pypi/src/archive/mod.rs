//! The wheel and sdist correctness checks run at upload time, and the PEP 658 `METADATA`/`PKG-INFO`
//! sidecar extraction use the bounded archive engine in `peryx-archive`.

use std::collections::BTreeMap;
use std::io::{Read, Seek, SeekFrom};
use std::path::Path;

mod sdist;
mod wheel;

pub use peryx_archive::{
    ArchiveError, DEFAULT_MEMBER_CHUNK, MAX_CONTAINER_DEPTH, MAX_LISTED_ENTRIES, MAX_MEMBER_CHUNK,
    MAX_NESTED_ARCHIVE_SIZE, Member, MemberChunk, MemberKind, read_error, safe_member_name,
    strip_ascii_suffix_ignore_case,
};
use peryx_archive::{ArchiveFormat, ArchiveProfile, generic_format, generic_member_kind};

pub use sdist::{sdist_metadata_path, validate_sdist_path, validate_zip_sdist_path};
pub use wheel::{
    MAX_WHEEL_METADATA_BYTES, validate_wheel_path, wheel_metadata, wheel_metadata_member_path, wheel_metadata_path,
};

struct PypiArchiveProfile;

impl ArchiveProfile for PypiArchiveProfile {
    fn format(&self, name: &str) -> Option<ArchiveFormat> {
        let extension = Path::new(name).extension()?;
        if extension.eq_ignore_ascii_case("whl") || extension.eq_ignore_ascii_case("egg") {
            Some(ArchiveFormat::Zip)
        } else {
            generic_format(name)
        }
    }

    fn member_kind(&self, path: &str) -> MemberKind {
        if self.format(path).is_some() {
            return MemberKind::Archive;
        }
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
                | "MANIFEST.in"
        ) {
            return MemberKind::Text;
        }
        match Path::new(filename)
            .extension()
            .and_then(|extension| extension.to_str())
            .map(str::to_ascii_lowercase)
            .as_deref()
        {
            Some("py" | "pyi") => MemberKind::Text,
            Some("pyc" | "pyd") => MemberKind::Binary,
            _ => generic_member_kind(path),
        }
    }
}

const PROFILE: PypiArchiveProfile = PypiArchiveProfile;

#[must_use]
pub fn is_supported_archive(filename: &str) -> bool {
    PROFILE.format(filename).is_some()
}

#[must_use]
pub fn is_tar_gz(filename: &str) -> bool {
    filename
        .get(filename.len().saturating_sub(7)..)
        .is_some_and(|suffix| suffix.eq_ignore_ascii_case(".tar.gz"))
        || filename
            .get(filename.len().saturating_sub(4)..)
            .is_some_and(|suffix| suffix.eq_ignore_ascii_case(".tgz"))
}

/// # Errors
/// Returns an error when the archive format or contents are invalid.
pub fn list_members(filename: &str, bytes: &[u8]) -> Result<Vec<Member>, ArchiveError> {
    peryx_archive::list_members(&PROFILE, filename, bytes)
}

/// # Errors
/// Returns an error when the archive cannot be read or parsed.
pub fn list_members_path(filename: &str, path: &Path) -> Result<Vec<Member>, ArchiveError> {
    peryx_archive::list_members_path(&PROFILE, filename, path)
}

/// # Errors
/// Returns an error when a container or nested archive is invalid.
pub fn list_members_nested_path(
    filename: &str,
    path: &Path,
    containers: &[String],
) -> Result<Vec<Member>, ArchiveError> {
    peryx_archive::list_members_nested_path(&PROFILE, filename, path, containers)
}

/// # Errors
/// Returns an error when the archive or requested member is invalid.
pub fn read_member(filename: &str, bytes: &[u8], member: &str) -> Result<Vec<u8>, ArchiveError> {
    peryx_archive::read_member(&PROFILE, filename, bytes, member)
}

/// # Errors
/// Returns an error when the member range cannot be read.
pub fn read_member_chunk(
    filename: &str,
    bytes: &[u8],
    member: &str,
    offset: u64,
    limit: u64,
) -> Result<MemberChunk, ArchiveError> {
    peryx_archive::read_member_chunk(&PROFILE, filename, bytes, member, offset, limit)
}

/// # Errors
/// Returns an error when the archive or member range cannot be read.
pub fn read_member_chunk_path(
    filename: &str,
    path: &Path,
    member: &str,
    offset: u64,
    limit: u64,
) -> Result<MemberChunk, ArchiveError> {
    peryx_archive::read_member_chunk_path(&PROFILE, filename, path, member, offset, limit)
}

/// # Errors
/// Returns an error when a nested member is missing, binary, or invalid.
pub fn read_text_member_chunk_nested_path(
    filename: &str,
    path: &Path,
    containers: &[String],
    member: &str,
    offset: u64,
    limit: u64,
) -> Result<MemberChunk, ArchiveError> {
    peryx_archive::read_text_member_chunk_nested_path(&PROFILE, filename, path, containers, member, offset, limit)
}

/// What one validation pass over a distribution archive yields.
///
/// Upload validation owns the `License-File` rejection, so the walk that already lists the members
/// reports the declared paths it did not find rather than failing on them.
#[derive(Debug)]
pub struct ValidatedArchive {
    pub metadata: Vec<u8>,
    pub missing_license_files: Vec<String>,
}

/// Whether an archive member is a file or a directory, kept per normalized name so a path spelled as
/// both can be rejected. Links share the file slot; they carry a distinct path and cannot alias a
/// directory here.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum EntryKind {
    File,
    Directory,
}

/// Explain why a normalized member path repeats. A ZIP permits duplicate names and validation and
/// consumption can then pick different bytes for one path, so any second occurrence is rejected: a
/// duplicated file, a duplicated directory, or a name spelled as both a file and a directory.
fn duplicate_member_message(name: &str, first: EntryKind, second: EntryKind) -> String {
    match (first, second) {
        (EntryKind::File, EntryKind::File) => format!("duplicate file member {name:?}"),
        (EntryKind::Directory, EntryKind::Directory) => format!("duplicate directory member {name:?}"),
        _ => format!("member {name:?} is both a file and a directory"),
    }
}

const CENTRAL_DIRECTORY_SIGNATURE: [u8; 4] = [0x50, 0x4b, 0x01, 0x02];
const CENTRAL_DIRECTORY_HEADER_LEN: usize = 46;

/// Reject a ZIP whose central directory names any normalized path twice.
///
/// [`zip::ZipArchive`] folds the central directory into a name-keyed map, so its `by_index` walk
/// never reveals a duplicate: the last record silently wins. A ZIP reader in an installer may pick a
/// different record for the same name, so validation and consumption could hash and extract
/// different bytes. This walks the raw records `zip` already located to catch the ambiguity the
/// deduplicated view hides. `invalid` wraps the failure with the wheel or sdist prefix.
fn reject_duplicate_zip_members<R: Read + Seek>(
    reader: &mut R,
    directory_start: u64,
    invalid: impl Fn(String) -> ArchiveError,
) -> Result<(), ArchiveError> {
    reader.seek(SeekFrom::Start(directory_start)).map_err(read_error)?;
    let mut header = [0_u8; CENTRAL_DIRECTORY_HEADER_LEN];
    let mut seen: BTreeMap<Vec<u8>, EntryKind> = BTreeMap::new();
    loop {
        reader.read_exact(&mut header[..4]).map_err(read_error)?;
        if header[..4] != CENTRAL_DIRECTORY_SIGNATURE {
            break;
        }
        reader.read_exact(&mut header[4..]).map_err(read_error)?;
        let name_len = usize::from(u16::from_le_bytes([header[28], header[29]]));
        let extra_len = i64::from(u16::from_le_bytes([header[30], header[31]]));
        let comment_len = i64::from(u16::from_le_bytes([header[32], header[33]]));
        let mut name = vec![0_u8; name_len];
        reader.read_exact(&mut name).map_err(read_error)?;
        reader
            .seek(SeekFrom::Current(extra_len + comment_len))
            .map_err(read_error)?;
        let is_dir = name.last() == Some(&b'/');
        if is_dir {
            name.pop();
        }
        let kind = if is_dir { EntryKind::Directory } else { EntryKind::File };
        if let Some(first) = seen.insert(name.clone(), kind) {
            return Err(invalid(duplicate_member_message(
                &String::from_utf8_lossy(&name),
                first,
                kind,
            )));
        }
    }
    Ok(())
}
