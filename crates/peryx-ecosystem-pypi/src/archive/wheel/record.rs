//! `RECORD`: every member of the wheel listed with its digest and size, and each one verified.
//!
//! The hash comparison is constant-time. A wheel is attacker-supplied, and leaking where two digests
//! first differ would let a caller search for a collision one byte at a time.

use std::collections::BTreeMap;
use std::io::{Read, Seek};

use base64::Engine as _;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use sha2::{Sha256, Sha384, Sha512};

use super::{ArchiveError, WheelMember, invalid_wheel, read_error, safe_member_name};

struct RecordEntry {
    hash: String,
    size: String,
}

pub(super) fn validate_record<R: Read + Seek>(
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
