//! Wheel validation and PEP 658 `METADATA` sidecar extraction.

use std::collections::{BTreeMap, BTreeSet};
use std::io::{Cursor, Read, Seek};
use std::path::Path;

use base64::Engine as _;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use sha2::{Sha256, Sha384, Sha512};

use super::{ArchiveError, read_error, safe_member_name};
use crate::{DistributionKind, parse_distribution_filename};

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
