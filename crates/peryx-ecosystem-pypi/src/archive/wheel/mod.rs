//! Wheel validation: the structure PEP 427 requires, and the members the other units check.
//!
//! A wheel is a zip whose `*.dist-info` directory must match its filename and must carry `METADATA`,
//! `WHEEL` and `RECORD`. This unit finds those members and orders the checks; each check with a
//! grammar of its own lives beside it.

use std::collections::{BTreeMap, BTreeSet};
use std::io::{Read, Seek};
use std::path::Path;

use super::{ArchiveError, read_error, safe_member_name};
use crate::{DistributionKind, parse_distribution_filename};

mod entry_points;
mod metadata;
mod record;
mod wheel_file;

pub use metadata::{wheel_metadata, wheel_metadata_member_path, wheel_metadata_path};

use entry_points::validate_entry_points;
use record::validate_record;
use wheel_file::validate_wheel_file;

/// Largest wheel `METADATA` document the extractor buffers.
pub const MAX_WHEEL_METADATA_BYTES: u64 = 16 * 1024 * 1024;
const MAX_WHEEL_RECORD_BYTES: u64 = 64 * 1024 * 1024;
const MAX_WHEEL_ENTRY_POINTS_BYTES: u64 = 1024 * 1024;
const SUPPORTED_WHEEL_MAJOR_VERSION: u64 = 1;

/// Wrap a wheel-validation failure as [`ArchiveError::Invalid`] with the `invalid wheel:` prefix the
/// upload API surfaces.
fn invalid_wheel(message: impl std::fmt::Display) -> ArchiveError {
    ArchiveError::Invalid(format!("invalid wheel: {message}"))
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
