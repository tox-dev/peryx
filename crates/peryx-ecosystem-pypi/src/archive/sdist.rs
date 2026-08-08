//! Sdist validation and `PKG-INFO` sidecar extraction.

use std::collections::BTreeMap;
use std::collections::btree_map::Entry;
use std::io::{Read, Seek};
use std::path::Path;

use super::{
    ArchiveError, EntryKind, ValidatedArchive, duplicate_member_message, is_tar_gz, read_error,
    reject_duplicate_zip_members, safe_member_name, strip_ascii_suffix_ignore_case,
};
use crate::{DistributionKind, parse_distribution_filename};

const MAX_SDIST_METADATA_BYTES: u64 = 16 * 1024 * 1024;
const MAX_SDIST_ENTRIES: usize = 100_000;

/// Most bytes validation will inflate across an sdist's members. Walking a gzip tar inflates each
/// member's body to reach the next header, so the expanded total, not the compressed upload, bounds
/// that work; a member's declared size is folded into this budget before its body is consumed.
const MAX_SDIST_EXPANDED_BYTES: u64 = 8 * 1024 * 1024 * 1024;

/// Largest expansion a single zip sdist member may declare, uncompressed over compressed. DEFLATE
/// tops out near 1032:1, so a member claiming more stored a far smaller stream than it declares and
/// is a decompression bomb rather than ordinary content. A gzip tar carries no per-member compressed
/// size, so only the zip path can enforce this before summing declared sizes into the budget.
const MAX_SDIST_COMPRESSION_RATIO: u64 = 1000;

/// Wrap an sdist-validation failure as [`ArchiveError::Invalid`] with the `invalid sdist:` prefix.
fn invalid_sdist(message: impl std::fmt::Display) -> ArchiveError {
    ArchiveError::Invalid(format!("invalid sdist: {message}"))
}

/// Validate a PEP 625 `.tar.gz` sdist and return its exact `PKG-INFO` bytes with the license files
/// it declares but does not carry.
///
/// # Errors
/// Returns [`ArchiveError::InvalidSdist`] when required sdist structure or metadata is missing or
/// inconsistent, and [`ArchiveError::Read`] when the staged file or tarball cannot be read.
pub fn validate_sdist_path(filename: &str, path: &Path) -> Result<ValidatedArchive, ArchiveError> {
    let file = std::fs::File::open(path).map_err(read_error)?;
    validate_sdist_reader(filename, file)
}

/// Validate a PEP 527 `.zip` sdist and return its exact `PKG-INFO` bytes with the license files it
/// declares but does not carry.
///
/// # Errors
/// Returns [`ArchiveError::InvalidSdist`] when required sdist structure or metadata is missing or
/// inconsistent, and [`ArchiveError::Read`] when the staged file or ZIP cannot be read.
pub fn validate_zip_sdist_path(filename: &str, path: &Path) -> Result<ValidatedArchive, ArchiveError> {
    let file = std::fs::File::open(path).map_err(read_error)?;
    validate_zip_sdist_reader(filename, file)
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
    validate_sdist_path(filename, path).map(|archive| Some(archive.metadata))
}

fn validate_sdist_reader(filename: &str, reader: impl Read) -> Result<ValidatedArchive, ArchiveError> {
    let root = expected_sdist_root(filename, DistributionKind::SdistTarGz, ".tar.gz")?;
    let mut archive = tar::Archive::new(flate2::read::GzDecoder::new(reader));
    let mut members = SdistMembers::new(root);
    let mut expanded = 0;
    for entry in archive.entries().map_err(read_error)? {
        let mut entry = entry.map_err(read_error)?;
        let entry_type = entry.header().entry_type();
        let path = entry.path().map_err(read_error)?.to_string_lossy().into_owned();
        let path = if entry_type.is_dir() {
            path.strip_suffix('/').unwrap_or(&path).to_owned()
        } else {
            path
        };
        let path = safe_sdist_member_name(&path)?;
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
            expanded = account_sdist_expansion(expanded, size)?;
            if path == members.pkg_info_path() {
                let metadata = read_sdist_member_limited(&mut entry, &path, size, MAX_SDIST_METADATA_BYTES)?;
                members.set_metadata(metadata)?;
            }
        }
        members.record(path, entry_type)?;
    }
    members.finish()
}

fn validate_zip_sdist_reader(filename: &str, reader: impl Read + Seek) -> Result<ValidatedArchive, ArchiveError> {
    let root = expected_sdist_root(filename, DistributionKind::SdistZip, ".zip")?;
    let archive = zip::ZipArchive::new(reader).map_err(read_error)?;
    let directory_start = archive.central_directory_start();
    let mut reader = archive.into_inner();
    reject_duplicate_zip_members(&mut reader, directory_start, invalid_sdist)?;
    let mut archive = zip::ZipArchive::new(reader).map_err(read_error)?;
    let mut members = SdistMembers::new(root);
    let pkg_info_path = members.pkg_info_path();
    let mut expanded = 0;
    for index in 0..archive.len() {
        let mut entry = archive.by_index(index).map_err(read_error)?;
        let is_dir = entry.is_dir();
        let raw_name = entry.name();
        let name = if is_dir {
            raw_name.strip_suffix('/').unwrap_or(raw_name)
        } else {
            raw_name
        };
        let path = safe_sdist_member_name(name)?;
        // A zip sdist has only files and directories, so it reuses the tar member vocabulary the
        // layout checks below are written against; a zip can carry no link to point out of the root.
        let entry_type = if is_dir {
            tar::EntryType::Directory
        } else {
            tar::EntryType::Regular
        };
        validate_sdist_member_path(&members.root, &path, entry_type)?;
        if entry.is_file() {
            let size = entry.size();
            expanded = account_zip_sdist_expansion(expanded, &path, size, entry.compressed_size())?;
            if path == pkg_info_path {
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
    paths: BTreeMap<String, EntryKind>,
}

impl SdistMembers {
    const fn new(root: String) -> Self {
        Self {
            root,
            pyproject: false,
            metadata: None,
            entries: 0,
            paths: BTreeMap::new(),
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
        let kind = if entry_type.is_dir() {
            EntryKind::Directory
        } else {
            EntryKind::File
        };
        match self.paths.entry(path) {
            Entry::Vacant(slot) => {
                slot.insert(kind);
            }
            Entry::Occupied(slot) => {
                return Err(invalid_sdist(duplicate_member_message(slot.key(), *slot.get(), kind)));
            }
        }
        Ok(())
    }

    fn finish(self) -> Result<ValidatedArchive, ArchiveError> {
        if !self.pyproject {
            return Err(invalid_sdist(format!("missing required {}/pyproject.toml", self.root)));
        }
        let metadata = self
            .metadata
            .ok_or_else(|| invalid_sdist(format!("missing required {}/PKG-INFO", self.root)))?;
        let text = std::str::from_utf8(&metadata).map_err(|_| invalid_sdist("PKG-INFO is not valid UTF-8"))?;
        let doc = crate::parse_metadata(text).map_err(|err| invalid_sdist(format!("malformed PKG-INFO: {err}")))?;
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
        // PEP 639 declares an sdist's license files relative to its project root, so one without a
        // member there names a file the sdist does not ship. Upload validation rejects a malformed
        // declared path on its own, so here one merely reads as missing.
        Ok(ValidatedArchive {
            missing_license_files: doc
                .license_files
                .into_iter()
                .filter(|value| !self.paths.contains_key(&format!("{}/{value}", self.root)))
                .collect(),
            metadata,
        })
    }
}

fn expected_sdist_root(filename: &str, kind: DistributionKind, suffix: &str) -> Result<String, ArchiveError> {
    let parsed = parse_distribution_filename(filename)
        .map_err(|err| invalid_sdist(format!("invalid sdist filename {filename:?}: {err:?}")))?;
    if parsed.kind != kind {
        return Err(invalid_sdist(format!("{filename:?} is not an sdist filename")));
    }
    let root = strip_ascii_suffix_ignore_case(filename, suffix)
        .expect("parse_distribution_filename accepted the sdist suffix");
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
    let target = safe_sdist_member_name(target)?;
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

/// Fold one member's declared uncompressed size into the running expanded total, rejecting an archive
/// whose members sum past [`MAX_SDIST_EXPANDED_BYTES`]. The sum is checked, so a member declaring a
/// size near [`u64::MAX`] fails the budget rather than wrapping it, and a member declaring more than
/// the whole budget on its own is refused before the tar walk inflates its body to reach the next
/// header.
fn account_sdist_expansion(running: u64, size: u64) -> Result<u64, ArchiveError> {
    running
        .checked_add(size)
        .filter(|total| *total <= MAX_SDIST_EXPANDED_BYTES)
        .ok_or_else(|| {
            invalid_sdist(format!(
                "archive members expand to more than the upload validation limit of {MAX_SDIST_EXPANDED_BYTES} bytes"
            ))
        })
}

/// Fold a zip sdist member into the expanded budget, first rejecting one that declares more than
/// [`MAX_SDIST_COMPRESSION_RATIO`] uncompressed bytes per compressed byte. A zip records both sizes,
/// so a decompression bomb is caught from central-directory metadata before its body is read.
fn account_zip_sdist_expansion(running: u64, path: &str, size: u64, compressed: u64) -> Result<u64, ArchiveError> {
    if size > compressed.saturating_mul(MAX_SDIST_COMPRESSION_RATIO) {
        return Err(invalid_sdist(format!(
            "member {path} declares {size} bytes from {compressed} compressed, above the {MAX_SDIST_COMPRESSION_RATIO}:1 expansion limit"
        )));
    }
    account_sdist_expansion(running, size)
}

fn safe_sdist_member_name(path: &str) -> Result<String, ArchiveError> {
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

#[cfg(test)]
#[path = "../../tests/unit/archive/sdist/tests.rs"]
mod tests;
