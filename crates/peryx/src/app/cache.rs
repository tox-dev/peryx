//! Cache inspection: list, size, and the dispatch into fsck and purge.

use std::io::Write;
use std::time::{SystemTime, UNIX_EPOCH};

use anyhow::Context as _;
use peryx_ecosystem_pypi::normalize_name;
use peryx_storage::meta::MetaStore;

use super::fsck::fsck_cache;
use super::purge::{purge_orphaned_blobs, purge_project};
use super::{CacheStores, index_names, split_page_key};
use crate::cli::{CacheCommand, CacheListArgs, CachePurgeCommand};
use crate::config::Config;

/// Run a cache inspection or maintenance command.
///
/// # Errors
/// Returns an error if the metadata store or blob store cannot be read, or if output fails.
pub fn cache(config: &Config, command: &CacheCommand, out: &mut dyn Write) -> anyhow::Result<()> {
    cache_at(config, command, unix_now(), out)
}

fn cache_at(config: &Config, command: &CacheCommand, now: i64, out: &mut dyn Write) -> anyhow::Result<()> {
    let stores = CacheStores::open(config)?;
    match command {
        CacheCommand::List(args) => list_cache(config, &stores, args, now, out),
        CacheCommand::Size(_) => size_cache(config, &stores, now, out),
        CacheCommand::Fsck(_) => fsck_cache(&stores, out),
        CacheCommand::Purge(CachePurgeCommand::Project(args)) => purge_project(config, &stores, args, out),
        CacheCommand::Purge(CachePurgeCommand::OrphanedBlobs(args)) => purge_orphaned_blobs(&stores, args, out),
    }
}

fn list_cache(
    config: &Config,
    stores: &CacheStores,
    args: &CacheListArgs,
    now: i64,
    out: &mut dyn Write,
) -> anyhow::Result<()> {
    let project = args.project.as_deref().map(normalize_name);
    let index_names = index_names(config);
    writeln!(
        out,
        "kind\tindex\tproject\tdigest\tage_secs\tfresh_secs\tstale\tsize_bytes\tkey"
    )?;
    if args.digest.is_none() {
        stores
            .meta
            .scan_index_pages(|page| {
                let (index, project_name) = split_page_key(&page.key, &index_names);
                let age = age_secs(now, page.summary.fetched_at_unix);
                let ttl = page.summary.fresh_secs.unwrap_or(config.cache_ttl_secs);
                let stale = is_stale(age, ttl);
                if args.index.as_deref().is_some_and(|filter| filter != index.as_str())
                    || project.as_deref().is_some_and(|filter| filter != project_name.as_str())
                    || args.stale && !stale
                    || args.min_age_secs.is_some_and(|min| age < min)
                    || args.min_size_bytes.is_some_and(|min| page.summary.body_bytes < min)
                {
                    return Ok(());
                }
                writeln!(
                    out,
                    "index\t{index}\t{project_name}\t\t{age}\t{}\t{stale}\t{}\t{}",
                    page.summary
                        .fresh_secs
                        .map_or_else(|| "-".to_owned(), |secs| secs.to_string()),
                    page.summary.body_bytes,
                    page.key,
                )
            })
            .context("scan cached index pages")?;
    }
    if args.index.is_some() || args.project.is_some() || args.stale || args.min_age_secs.is_some() {
        return Ok(());
    }
    stores
        .blobs
        .scan(|entry| {
            let Some(digest) = &entry.digest else {
                return Ok(());
            };
            if args.digest.as_deref().is_some_and(|filter| filter != digest.as_str())
                || args.min_size_bytes.is_some_and(|min| entry.bytes < min)
            {
                return Ok(());
            }
            writeln!(
                out,
                "blob\t\t\t{}\t-\t-\t-\t{}\t{}",
                digest.as_str(),
                entry.bytes,
                entry.path.display()
            )
        })
        .context("scan blob files")?;
    Ok(())
}

fn size_cache(config: &Config, stores: &CacheStores, now: i64, out: &mut dyn Write) -> anyhow::Result<()> {
    let mut index_pages = 0_u64;
    let mut index_bytes = 0_u64;
    let mut stale_index_pages = 0_u64;
    stores
        .meta
        .scan_index_pages(|page| {
            index_pages += 1;
            index_bytes += page.summary.record_bytes;
            let age = age_secs(now, page.summary.fetched_at_unix);
            let ttl = page.summary.fresh_secs.unwrap_or(config.cache_ttl_secs);
            stale_index_pages += u64::from(is_stale(age, ttl));
            Ok::<(), std::io::Error>(())
        })
        .context("scan cached index pages")?;

    let mut blob_files = 0_u64;
    let mut blob_bytes = 0_u64;
    let mut invalid_blob_paths = 0_u64;
    stores
        .blobs
        .scan(|entry| {
            blob_files += 1;
            blob_bytes += entry.bytes;
            invalid_blob_paths += u64::from(entry.digest.is_none());
            Ok::<(), std::io::Error>(())
        })
        .context("scan blob files")?;

    let counts = metadata_counts(&stores.meta)?;
    writeln!(out, "index_pages\t{index_pages}")?;
    writeln!(out, "stale_index_pages\t{stale_index_pages}")?;
    writeln!(out, "index_bytes\t{index_bytes}")?;
    writeln!(out, "blob_files\t{blob_files}")?;
    writeln!(out, "blob_bytes\t{blob_bytes}")?;
    writeln!(out, "invalid_blob_paths\t{invalid_blob_paths}")?;
    writeln!(out, "file_url_records\t{}", counts.file_urls)?;
    writeln!(out, "metadata_records\t{}", counts.metadata)?;
    writeln!(out, "project_records\t{}", counts.projects)?;
    writeln!(out, "upload_records\t{}", counts.uploads)?;
    writeln!(out, "override_records\t{}", counts.overrides)?;
    Ok(())
}

#[derive(Default)]
struct MetadataCounts {
    file_urls: u64,
    metadata: u64,
    projects: u64,
    uploads: u64,
    overrides: u64,
}

fn metadata_counts(meta: &MetaStore) -> anyhow::Result<MetadataCounts> {
    let mut counts = MetadataCounts::default();
    meta.scan_file_urls(|_digest, _value| {
        counts.file_urls += 1;
        Ok::<(), std::io::Error>(())
    })
    .context("scan file URL metadata")?;
    meta.scan_metadata_records(|_digest, _value| {
        counts.metadata += 1;
        Ok::<(), std::io::Error>(())
    })
    .context("scan PEP 658 metadata")?;
    meta.scan_project_records(|_key, _display| {
        counts.projects += 1;
        Ok::<(), std::io::Error>(())
    })
    .context("scan project metadata")?;
    meta.scan_upload_records(|_key, _bytes| {
        counts.uploads += 1;
        Ok::<(), std::io::Error>(())
    })
    .context("scan upload metadata")?;
    meta.scan_override_records(|_key, _kind| {
        counts.overrides += 1;
        Ok::<(), std::io::Error>(())
    })
    .context("scan override metadata")?;
    Ok(counts)
}

fn age_secs(now: i64, fetched_at: i64) -> u64 {
    now.saturating_sub(fetched_at).try_into().unwrap_or_default()
}

const fn is_stale(age: u64, ttl: i64) -> bool {
    ttl <= 0 || age >= ttl.cast_unsigned()
}

fn unix_now() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |duration| duration.as_secs().try_into().unwrap_or(i64::MAX))
}
