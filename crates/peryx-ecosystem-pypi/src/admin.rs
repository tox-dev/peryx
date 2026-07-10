//! The `PyPI` half of peryx's cache-maintenance commands: which stored blobs its metadata tables
//! reference, and whether those tables are internally consistent. The neutral binary drives the
//! blob store itself (content-addressed, so ecosystem-agnostic) and dispatches the metadata half
//! here through the ecosystem driver.

use std::collections::BTreeSet;
use std::io::Write;

use peryx_storage::blob::{BlobStore, Digest};
use peryx_storage::meta::{CachedIndex, MetaStore};

use crate::upload::Uploaded;
use crate::{CoreMetadata, parse_detail};

/// The blob digests every `PyPI` metadata table references: cached file URLs, PEP 658 metadata
/// siblings, and hosted upload records. The neutral orphan-blob collector keeps these and reclaims
/// the rest.
///
/// # Errors
/// Returns a message when a metadata record is malformed, since a purge must not run against a store
/// it cannot fully account for.
pub fn referenced_blob_digests(meta: &MetaStore) -> Result<BTreeSet<String>, String> {
    let mut digests = BTreeSet::new();
    meta.scan_file_urls(|digest, value| {
        if Digest::from_hex(digest).is_none() || split_pair(value).is_none() {
            return Err(format!("invalid file URL record for digest {digest:?}"));
        }
        digests.insert(digest.to_owned());
        Ok(())
    })
    .map_err(|err| err.to_string())?;
    meta.scan_metadata_records(|digest, value| {
        let Some((_url, metadata_digest, _source)) = split_triple(value) else {
            return Err(format!("invalid PEP 658 metadata record for digest {digest:?}"));
        };
        if Digest::from_hex(digest).is_none() {
            return Err(format!("invalid PEP 658 wheel digest {digest:?}"));
        }
        if Digest::from_hex(metadata_digest).is_none() {
            return Err(format!("invalid PEP 658 metadata digest {metadata_digest:?}"));
        }
        digests.insert(digest.to_owned());
        digests.insert(metadata_digest.to_owned());
        Ok(())
    })
    .map_err(|err| err.to_string())?;
    meta.scan_upload_records(|key, bytes| {
        for digest in upload_digests(bytes).ok_or_else(|| format!("invalid upload record {key}"))? {
            digests.insert(digest.as_str().to_owned());
        }
        Ok::<(), String>(())
    })
    .map_err(|err| err.to_string())?;
    Ok(digests)
}

/// Validate every `PyPI` metadata record in `meta`, writing one tab-separated line per problem to
/// `out` and returning the count. Blob contents are the neutral caller's to verify.
///
/// # Errors
/// Returns a message when the store cannot be read or `out` cannot be written.
pub fn fsck_metadata(meta: &MetaStore, blobs: &BlobStore, out: &mut dyn Write) -> Result<u64, String> {
    let mut problems = 0_u64;
    meta.scan_index_records(|key, bytes| {
        match CachedIndex::decode(bytes) {
            Ok(record) if parse_detail(&record.body).is_ok() => {}
            Ok(_) => {
                problems += 1;
                writeln!(out, "metadata\tindex\t{key}\tinvalid project detail")?;
            }
            Err(err) => {
                problems += 1;
                writeln!(out, "metadata\tindex\t{key}\t{err}")?;
            }
        }
        Ok::<(), std::io::Error>(())
    })
    .map_err(|err| err.to_string())?;
    meta.scan_file_urls(|digest, value| {
        if Digest::from_hex(digest).is_none() || split_pair(value).is_none() {
            problems += 1;
            writeln!(out, "metadata\tfile-url\t{digest}\tinvalid record")?;
        }
        Ok::<(), std::io::Error>(())
    })
    .map_err(|err| err.to_string())?;
    meta.scan_metadata_records(|digest, value| {
        let valid = Digest::from_hex(digest).is_some()
            && split_triple(value)
                .is_some_and(|(_url, metadata_digest, _source)| Digest::from_hex(metadata_digest).is_some());
        if !valid {
            problems += 1;
            writeln!(out, "metadata\tpep658\t{digest}\tinvalid record")?;
        }
        Ok::<(), std::io::Error>(())
    })
    .map_err(|err| err.to_string())?;
    meta.scan_project_records(|key, display| {
        if !valid_project_key(key) || display.is_empty() {
            problems += 1;
            writeln!(out, "metadata\tproject\t{key}\tinvalid record")?;
        }
        Ok::<(), std::io::Error>(())
    })
    .map_err(|err| err.to_string())?;
    meta.scan_upload_records(|key, bytes| {
        let Some(digests) = upload_digests(bytes) else {
            problems += 1;
            writeln!(out, "metadata\tupload\t{key}\tinvalid record")?;
            return Ok(());
        };
        if !valid_upload_key(key) {
            problems += 1;
            writeln!(out, "metadata\tupload\t{key}\tinvalid key")?;
            return Ok(());
        }
        for digest in digests {
            if !blobs.exists(&digest) {
                problems += 1;
                writeln!(out, "metadata\tupload\t{key}\tmissing blob {}", digest.as_str())?;
            }
        }
        Ok::<(), std::io::Error>(())
    })
    .map_err(|err| err.to_string())?;
    meta.scan_override_records(|key, kind| {
        if !valid_upload_key(key) || !matches!(kind, "hidden" | "yanked") {
            problems += 1;
            writeln!(out, "metadata\toverride\t{key}\tinvalid record")?;
        }
        Ok::<(), std::io::Error>(())
    })
    .map_err(|err| err.to_string())?;
    Ok(problems)
}

/// The stored-blob digests one upload record names: its distribution file, and the PEP 658 metadata
/// sibling when the upload carried one. `None` when the record does not deserialize.
fn upload_digests(bytes: &[u8]) -> Option<Vec<Digest>> {
    let upload: Uploaded = serde_json::from_slice(bytes).ok()?;
    let mut digests = vec![Digest::from_hex(upload.file.hashes.get("sha256")?)?];
    if let CoreMetadata::Hashes(hashes) = upload.file.core_metadata
        && let Some(metadata_digest) = hashes.get("sha256")
    {
        digests.push(Digest::from_hex(metadata_digest)?);
    }
    Some(digests)
}

fn split_pair(value: &str) -> Option<(&str, &str)> {
    value.split_once('\n')
}

fn split_triple(value: &str) -> Option<(&str, &str, &str)> {
    let mut parts = value.splitn(3, '\n');
    Some((parts.next()?, parts.next()?, parts.next()?))
}

fn valid_project_key(key: &str) -> bool {
    key.split_once('/')
        .is_some_and(|(index, project)| !index.is_empty() && !project.is_empty())
}

fn valid_upload_key(key: &str) -> bool {
    let mut parts = key.splitn(3, '/');
    parts.next().is_some_and(|part| !part.is_empty())
        && parts.next().is_some_and(|part| !part.is_empty())
        && parts.next().is_some_and(|part| !part.is_empty())
}
