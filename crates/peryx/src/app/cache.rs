use std::io::Write;
use std::time::{SystemTime, UNIX_EPOCH};

use anyhow::Context as _;
use peryx_driver::DriverSet;
use peryx_plugin_registry::PluginRegistry;

use super::fsck::fsck_cache;
use super::purge::{purge_orphaned_blobs, purge_resource, validate_orphan_purge_mode};
use super::{CacheStores, index_names, reject_object_store_blob};
use crate::cli::{CacheCommand, CacheListArgs, CachePurgeCommand};
use crate::config::Config;

/// # Errors
/// Returns an error when storage or output fails or the configured blob backend is unsupported.
pub fn cache(config: &Config, command: &CacheCommand, out: &mut dyn Write) -> anyhow::Result<()> {
    cache_with_plugins(config, &crate::compiled_plugins(), command, out)
}

/// # Errors
/// Returns an error when storage, a plugin cache capability, or output fails.
pub fn cache_with_plugins(
    config: &Config,
    plugins: &PluginRegistry,
    command: &CacheCommand,
    out: &mut dyn Write,
) -> anyhow::Result<()> {
    let plugins = crate::server::activate_plugins(config, plugins)?;
    reject_object_store_blob(config, "cache maintenance")?;
    if matches!(command, CacheCommand::Purge(CachePurgeCommand::OrphanedBlobs(_))) {
        validate_orphan_purge_mode(config.availability.mode())?;
    }
    let writable = matches!(command, CacheCommand::Purge(_));
    let stores = CacheStores::open(config, &plugins, writable)?;
    let drivers = plugins.drivers();
    match command {
        CacheCommand::List(args) => list_cache(config, drivers, &stores, args, unix_now(), out),
        CacheCommand::Size(_) => size_cache(config, drivers, &stores, unix_now(), out),
        CacheCommand::Fsck(_) => fsck_cache(drivers, &stores, out),
        CacheCommand::Purge(CachePurgeCommand::Resource(args)) => purge_resource(config, drivers, &stores, args, out),
        CacheCommand::Purge(CachePurgeCommand::OrphanedBlobs(args)) => {
            purge_orphaned_blobs(drivers, &stores, args, unix_now(), out)
        }
    }
}

fn list_cache(
    config: &Config,
    drivers: &DriverSet,
    stores: &CacheStores,
    args: &CacheListArgs,
    now: i64,
    out: &mut dyn Write,
) -> anyhow::Result<()> {
    writeln!(
        out,
        "kind\tindex\tresource\tdigest\tage_secs\tfresh_secs\tstale\tsize_bytes\tkey"
    )?;
    if args.digest.is_none() {
        let names = index_names(config);
        let context = CacheListContext {
            config,
            stores,
            args,
            index_names: &names,
            now,
        };
        for ecosystem_driver in drivers.present() {
            let ecosystem = ecosystem_driver.ecosystem();
            let Some(driver) = drivers.get_cache(&ecosystem) else {
                continue;
            };
            list_driver_cache(
                &context,
                driver.as_ref(),
                drivers.get_name(&ecosystem).map(std::convert::AsRef::as_ref),
                out,
            )
            .context("scan cached index pages")?;
        }
    }
    if args.index.is_some() || args.resource.is_some() || args.stale || args.min_age_secs.is_some() {
        return Ok(());
    }
    stores
        .blobs
        .blocking()
        .visit(|entry| {
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

struct CacheListContext<'a> {
    config: &'a Config,
    stores: &'a CacheStores,
    args: &'a CacheListArgs,
    index_names: &'a [&'a str],
    now: i64,
}

fn list_driver_cache(
    context: &CacheListContext<'_>,
    driver: &dyn peryx_driver::serving::CacheDriver,
    names: Option<&dyn peryx_driver::serving::NameDriver>,
    out: &mut dyn Write,
) -> anyhow::Result<()> {
    let resource_filter = match (context.args.resource.as_deref(), names) {
        (Some(resource), Some(driver)) => Some(driver.normalize_name(resource)),
        (Some(resource), None) => Some(resource.to_owned()),
        (None, _) => None,
    };
    for page in driver
        .cache_pages(&context.stores.meta, context.index_names)
        .map_err(anyhow::Error::msg)?
    {
        let age = age_secs(context.now, page.fetched_at_unix);
        let stale = is_stale(age, page.fresh_secs.unwrap_or(context.config.cache_ttl_secs));
        if context
            .args
            .index
            .as_deref()
            .is_some_and(|filter| filter != page.index.as_str())
            || resource_filter
                .as_deref()
                .is_some_and(|filter| filter != page.resource.as_str())
            || context.args.stale && !stale
            || context.args.min_age_secs.is_some_and(|min| age < min)
            || context.args.min_size_bytes.is_some_and(|min| page.body_bytes < min)
        {
            continue;
        }
        writeln!(
            out,
            "index\t{}\t{}\t\t{age}\t{}\t{stale}\t{}\t{}",
            page.index,
            page.resource,
            page.fresh_secs.map_or_else(|| "-".to_owned(), |secs| secs.to_string()),
            page.body_bytes,
            page.key,
        )?;
    }
    Ok(())
}

fn size_cache(
    config: &Config,
    drivers: &DriverSet,
    stores: &CacheStores,
    now: i64,
    out: &mut dyn Write,
) -> anyhow::Result<()> {
    let mut index_pages = 0_u64;
    let mut index_bytes = 0_u64;
    let mut stale_index_pages = 0_u64;
    let mut record_counts: Vec<(String, u64)> = Vec::new();
    let names = index_names(config);
    for driver in drivers.cache_drivers() {
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
        record_counts.extend(driver.cache_record_counts(&stores.meta).map_err(anyhow::Error::msg)?);
    }

    let mut blob_files = 0_u64;
    let mut blob_bytes = 0_u64;
    let mut invalid_blob_paths = 0_u64;
    stores
        .blobs
        .blocking()
        .visit(|entry| {
            blob_files += 1;
            blob_bytes += entry.bytes;
            invalid_blob_paths += u64::from(entry.digest.is_none());
            Ok::<(), std::io::Error>(())
        })
        .context("scan blob files")?;
    let stages = stores.blobs.blocking().stage_usage().context("scan blob stages")?;

    writeln!(out, "index_pages\t{index_pages}")?;
    writeln!(out, "stale_index_pages\t{stale_index_pages}")?;
    writeln!(out, "index_bytes\t{index_bytes}")?;
    writeln!(out, "blob_files\t{blob_files}")?;
    writeln!(out, "blob_bytes\t{blob_bytes}")?;
    writeln!(out, "invalid_blob_paths\t{invalid_blob_paths}")?;
    writeln!(out, "stage_files\t{}", stages.files)?;
    writeln!(out, "stage_bytes\t{}", stages.bytes)?;
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

#[cfg(test)]
#[path = "../../tests/unit/tests/app/cache_tests.rs"]
mod tests;
