//! Cache inspection: list, size, and the dispatch into fsck and purge.

use std::io::Write;
use std::time::{SystemTime, UNIX_EPOCH};

use anyhow::Context as _;

use super::fsck::fsck_cache;
use super::purge::{purge_orphaned_blobs, purge_project};
use super::{CacheStores, index_names};
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
    writeln!(
        out,
        "kind\tindex\tproject\tdigest\tage_secs\tfresh_secs\tstale\tsize_bytes\tkey"
    )?;
    if args.digest.is_none() {
        let names = index_names(config);
        for driver in crate::server::drivers().present() {
            // Each ecosystem produces its own cached pages, split into its own terms; the filtering
            // and row shape stay neutral. The write shares the scan's context so a broken pipe here
            // surfaces the same way an unreadable store would.
            let mut render = || -> anyhow::Result<()> {
                let project_filter = args.project.as_deref().map(|project| driver.normalize_name(project));
                let pages = driver.cache_pages(&stores.meta, &names).map_err(anyhow::Error::msg)?;
                for page in pages {
                    let age = age_secs(now, page.fetched_at_unix);
                    let ttl = page.fresh_secs.unwrap_or(config.cache_ttl_secs);
                    let stale = is_stale(age, ttl);
                    if args
                        .index
                        .as_deref()
                        .is_some_and(|filter| filter != page.index.as_str())
                        || project_filter
                            .as_deref()
                            .is_some_and(|filter| filter != page.project.as_str())
                        || args.stale && !stale
                        || args.min_age_secs.is_some_and(|min| age < min)
                        || args.min_size_bytes.is_some_and(|min| page.body_bytes < min)
                    {
                        continue;
                    }
                    writeln!(
                        out,
                        "index\t{}\t{}\t\t{age}\t{}\t{stale}\t{}\t{}",
                        page.index,
                        page.project,
                        page.fresh_secs.map_or_else(|| "-".to_owned(), |secs| secs.to_string()),
                        page.body_bytes,
                        page.key,
                    )?;
                }
                Ok(())
            };
            render().context("scan cached index pages")?;
        }
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
    let mut record_counts: Vec<(String, u64)> = Vec::new();
    let names = index_names(config);
    for driver in crate::server::drivers().present() {
        let pages = driver
            .cache_pages(&stores.meta, &names)
            .map_err(anyhow::Error::msg)
            .context("scan cached index pages")?;
        for page in pages {
            index_pages += 1;
            index_bytes += page.record_bytes;
            let age = age_secs(now, page.fetched_at_unix);
            let ttl = page.fresh_secs.unwrap_or(config.cache_ttl_secs);
            stale_index_pages += u64::from(is_stale(age, ttl));
        }
        // Each driver labels its own record kinds, so the labels across drivers never collide; the
        // counts append rather than merge.
        record_counts.extend(driver.cache_record_counts(&stores.meta).map_err(anyhow::Error::msg)?);
    }

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

    writeln!(out, "index_pages\t{index_pages}")?;
    writeln!(out, "stale_index_pages\t{stale_index_pages}")?;
    writeln!(out, "index_bytes\t{index_bytes}")?;
    writeln!(out, "blob_files\t{blob_files}")?;
    writeln!(out, "blob_bytes\t{blob_bytes}")?;
    writeln!(out, "invalid_blob_paths\t{invalid_blob_paths}")?;
    for (label, count) in record_counts {
        writeln!(out, "{label}\t{count}")?;
    }
    Ok(())
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
