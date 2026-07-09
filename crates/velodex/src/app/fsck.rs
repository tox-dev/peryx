//! Cache consistency checks: metadata records and content-addressed blobs.

use std::io::Write;

use anyhow::Context as _;
use velodex_ecosystem_pypi::parse_detail;
use velodex_storage::blob::{BlobEntry, Digest};
use velodex_storage::meta::CachedIndex;

use super::{CacheStores, split_pair, split_triple, upload_digests};

pub(super) fn fsck_cache(stores: &CacheStores, out: &mut dyn Write) -> anyhow::Result<()> {
    let mut problems = 0_u64;
    stores
        .meta
        .scan_index_records(|key, bytes| {
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
        .context("scan index metadata")?;
    stores
        .meta
        .scan_file_urls(|digest, value| {
            if Digest::from_hex(digest).is_none() || split_pair(value).is_none() {
                problems += 1;
                writeln!(out, "metadata\tfile-url\t{digest}\tinvalid record")?;
            }
            Ok::<(), std::io::Error>(())
        })
        .context("scan file URL metadata")?;
    stores
        .meta
        .scan_metadata_records(|digest, value| {
            let valid = Digest::from_hex(digest).is_some()
                && split_triple(value)
                    .is_some_and(|(_url, metadata_digest, _source)| Digest::from_hex(metadata_digest).is_some());
            if !valid {
                problems += 1;
                writeln!(out, "metadata\tpep658\t{digest}\tinvalid record")?;
            }
            Ok::<(), std::io::Error>(())
        })
        .context("scan PEP 658 metadata")?;
    stores
        .meta
        .scan_project_records(|key, display| {
            if !valid_project_key(key) || display.is_empty() {
                problems += 1;
                writeln!(out, "metadata\tproject\t{key}\tinvalid record")?;
            }
            Ok::<(), std::io::Error>(())
        })
        .context("scan project metadata")?;
    stores
        .meta
        .scan_upload_records(|key, bytes| {
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
                if !stores.blobs.exists(&digest) {
                    problems += 1;
                    writeln!(out, "metadata\tupload\t{key}\tmissing blob {}", digest.as_str())?;
                }
            }
            Ok::<(), std::io::Error>(())
        })
        .context("scan upload metadata")?;
    stores
        .meta
        .scan_override_records(|key, kind| {
            if !valid_upload_key(key) || !matches!(kind, "hidden" | "yanked") {
                problems += 1;
                writeln!(out, "metadata\toverride\t{key}\tinvalid record")?;
            }
            Ok::<(), std::io::Error>(())
        })
        .context("scan override metadata")?;
    stores
        .blobs
        .scan(|entry| {
            problems += check_blob(stores, &entry, out)?;
            Ok::<(), std::io::Error>(())
        })
        .context("scan blob files")?;
    if problems == 0 {
        writeln!(out, "ok")?;
    } else {
        writeln!(out, "problems\t{problems}")?;
    }
    Ok(())
}

fn check_blob(stores: &CacheStores, entry: &BlobEntry, out: &mut dyn Write) -> std::io::Result<u64> {
    let Some(digest) = &entry.digest else {
        writeln!(
            out,
            "blob\tpath\t{}\tinvalid content-addressed path",
            entry.path.display()
        )?;
        return Ok(1);
    };
    match stores.blobs.verify(digest) {
        Ok(true) => Ok(0),
        Ok(false) => {
            writeln!(out, "blob\thash\t{}\tdigest mismatch", digest.as_str())?;
            Ok(1)
        }
        Err(err) => {
            writeln!(out, "blob\tread\t{}\t{err}", digest.as_str())?;
            Ok(1)
        }
    }
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
