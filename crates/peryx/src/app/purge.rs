//! Cache purging: per-project removal, orphaned-blob collection, and the reference scan.

use std::collections::BTreeSet;
use std::io::Write;

use anyhow::{Context as _, bail};
use peryx_ecosystem_pypi::{CoreMetadata, normalize_name, parse_detail};
use peryx_storage::blob::Digest;
use peryx_storage::meta::{CachedIndex, MetaStore, ProjectCachePurgeCounts};

use super::{CacheStores, upload_digests};
use crate::cli::{CachePurgeOrphanedBlobsArgs, CachePurgeProjectArgs};

pub(super) fn purge_project(
    stores: &CacheStores,
    args: &CachePurgeProjectArgs,
    out: &mut dyn Write,
) -> anyhow::Result<()> {
    let normalized = normalize_name(&args.project);
    let target_key = format!("{}/{}", args.index, normalized);
    let target_refs = project_refs(stores, &target_key)?;
    let preserved_refs = preserved_refs(stores, &target_key)?;
    let file_digests = target_refs
        .files
        .difference(&preserved_refs.files)
        .cloned()
        .collect::<Vec<_>>();
    let metadata_digests = target_refs
        .metadata_wheels
        .difference(&preserved_refs.files)
        .cloned()
        .collect::<Vec<_>>();
    let counts = if args.yes {
        stores
            .meta
            .delete_project_cache(&args.index, &normalized, &file_digests, &metadata_digests)
            .context("delete project cache metadata")?
    } else {
        stores
            .meta
            .count_project_cache_purge(&args.index, &normalized, &file_digests, &metadata_digests)
            .context("count project cache metadata")?
    };
    let header = b"action\ttarget\tindex\tproject\tindex_pages\tproject_records\tfile_url_records\tmetadata_records\n";
    out.write_all(header)?;
    write_project_purge_counts(
        out,
        if args.yes { "removed" } else { "dry-run" },
        &args.index,
        &normalized,
        counts,
    )
}

pub(super) fn purge_orphaned_blobs(
    stores: &CacheStores,
    args: &CachePurgeOrphanedBlobsArgs,
    out: &mut dyn Write,
) -> anyhow::Result<()> {
    let references = referenced_blob_digests(&stores.meta)?;
    let mut count = 0_u64;
    let mut bytes = 0_u64;
    writeln!(out, "action\ttarget\tdigest\tsize_bytes\tpath")?;
    stores
        .blobs
        .scan(|entry| {
            let Some(digest) = &entry.digest else {
                return Ok::<(), anyhow::Error>(());
            };
            if references.contains(digest.as_str()) {
                return Ok(());
            }
            count += 1;
            bytes += entry.bytes;
            if args.yes {
                stores.blobs.remove(digest)?;
            }
            writeln!(
                out,
                "{}\torphaned-blob\t{}\t{}\t{}",
                if args.yes { "removed" } else { "dry-run" },
                digest.as_str(),
                entry.bytes,
                entry.path.display()
            )?;
            Ok(())
        })
        .map_err(|err| anyhow::anyhow!("{err}"))
        .context("scan orphaned blob files")?;
    writeln!(
        out,
        "summary\t{}\torphaned-blobs\t{count}\t{bytes}",
        if args.yes { "removed" } else { "dry-run" }
    )?;
    Ok(())
}

#[derive(Default)]
struct CacheRefs {
    files: BTreeSet<String>,
    metadata_wheels: BTreeSet<String>,
}

fn project_refs(stores: &CacheStores, target_key: &str) -> anyhow::Result<CacheRefs> {
    let Some(record) = stores
        .meta
        .get_index(target_key)
        .with_context(|| format!("read cached project {target_key}"))?
    else {
        return Ok(CacheRefs::default());
    };
    let mut refs = CacheRefs::default();
    add_index_refs(&mut refs, &record).with_context(|| format!("read cached project digests for {target_key}"))?;
    Ok(refs)
}

fn preserved_refs(stores: &CacheStores, target_key: &str) -> anyhow::Result<CacheRefs> {
    let mut refs = CacheRefs::default();
    stores
        .meta
        .scan_index_records(|key, bytes| {
            if key == target_key {
                return Ok::<(), anyhow::Error>(());
            }
            let record = CachedIndex::decode(bytes).map_err(anyhow::Error::from)?;
            add_index_refs(&mut refs, &record)?;
            Ok(())
        })
        .map_err(|err| anyhow::anyhow!("{err}"))
        .context("scan cached pages for shared digests")?;
    stores
        .meta
        .scan_upload_records(|key, bytes| {
            for digest in upload_digests(bytes).with_context(|| format!("read upload record {key}"))? {
                refs.files.insert(digest.as_str().to_owned());
            }
            Ok::<(), anyhow::Error>(())
        })
        .map_err(|err| anyhow::anyhow!("{err}"))
        .context("scan upload records for shared digests")?;
    Ok(refs)
}

/// Every blob digest any installed ecosystem's metadata references, unioned across drivers. Blobs are
/// content-addressed and shared, so a blob is orphaned only when no ecosystem names it; the collector
/// walks this whole set before reclaiming anything.
pub fn referenced_blob_digests(meta: &MetaStore) -> anyhow::Result<BTreeSet<String>> {
    let mut digests = BTreeSet::new();
    for driver in crate::server::drivers().present() {
        digests.extend(
            driver
                .referenced_blob_digests(meta)
                .map_err(|reason| anyhow::anyhow!("scan {} blob references: {reason}", driver.ecosystem().as_str()))?,
        );
    }
    Ok(digests)
}

fn add_index_refs(refs: &mut CacheRefs, record: &CachedIndex) -> anyhow::Result<()> {
    for file in parse_detail(&record.body)?.files {
        let Some(sha256) = file.hashes.get("sha256") else {
            continue;
        };
        if Digest::from_hex(sha256).is_none() {
            bail!("cached page contains invalid sha256 digest {sha256:?}");
        }
        refs.files.insert(sha256.to_owned());
        if let CoreMetadata::Hashes(hashes) = file.core_metadata
            && let Some(metadata_digest) = hashes.get("sha256")
        {
            if Digest::from_hex(metadata_digest).is_none() {
                bail!("cached page contains invalid metadata digest {metadata_digest:?}");
            }
            refs.metadata_wheels.insert(sha256.to_owned());
        }
    }
    Ok(())
}

fn write_project_purge_counts(
    out: &mut dyn Write,
    action: &str,
    index: &str,
    project: &str,
    counts: ProjectCachePurgeCounts,
) -> anyhow::Result<()> {
    writeln!(
        out,
        "{action}\tproject\t{index}\t{project}\t{}\t{}\t{}\t{}",
        counts.index_pages, counts.project_records, counts.file_url_records, counts.metadata_records
    )?;
    Ok(())
}
