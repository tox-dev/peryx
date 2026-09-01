use std::io::Write;
use std::time::{SystemTime, UNIX_EPOCH};

use anyhow::Context as _;
use peryx_driver::DriverSet;
use peryx_driver::cache_inspection::{
    CacheListFilter, CachePageSource, resource_filter, write_cache_list, write_cache_size,
};
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
    let writable = matches!(command, CacheCommand::Purge(purge) if purge.confirmed());
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
    let mut sources = Vec::new();
    if args.digest.is_none() {
        let names = index_names(config);
        for ecosystem_driver in drivers.present() {
            let ecosystem = ecosystem_driver.ecosystem();
            let Some(driver) = drivers.get_cache(&ecosystem) else {
                continue;
            };
            sources.push(CachePageSource {
                resource_filter: resource_filter(
                    args.resource.as_deref(),
                    drivers.get_name(&ecosystem).map(std::convert::AsRef::as_ref),
                ),
                pages: driver
                    .cache_pages(&stores.meta, &names)
                    .map_err(anyhow::Error::msg)
                    .context("scan cached index pages")?,
            });
        }
    }
    write_cache_list(
        sources,
        &stores.blobs,
        &CacheListFilter {
            index: args.index.as_deref(),
            resource_filtered: args.resource.is_some(),
            digest: args.digest.as_deref(),
            stale: args.stale,
            min_age_secs: args.min_age_secs,
            min_size_bytes: args.min_size_bytes,
        },
        config.cache_ttl_secs,
        now,
        out,
    )
    .map_err(anyhow::Error::new)
}

fn size_cache(
    config: &Config,
    drivers: &DriverSet,
    stores: &CacheStores,
    now: i64,
    out: &mut dyn Write,
) -> anyhow::Result<()> {
    let mut pages = Vec::new();
    let mut record_counts = Vec::new();
    let names = index_names(config);
    for driver in drivers.cache_drivers() {
        pages.extend(
            driver
                .cache_pages(&stores.meta, &names)
                .map_err(anyhow::Error::msg)
                .context("scan cached index pages")?,
        );
        record_counts.extend(driver.cache_record_counts(&stores.meta).map_err(anyhow::Error::msg)?);
    }
    write_cache_size(&pages, record_counts, &stores.blobs, config.cache_ttl_secs, now, out).map_err(anyhow::Error::new)
}

fn unix_now() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |duration| duration.as_secs().try_into().unwrap_or(i64::MAX))
}

#[cfg(test)]
#[path = "../../tests/unit/tests/app/cache_tests.rs"]
mod tests;
