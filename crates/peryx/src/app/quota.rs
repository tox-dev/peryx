//! Both resolve the configured indexes so a repository's limits come from its policy, and read the
//! committed and reserved counters the store maintains rather than scanning artifacts. `list` prints a
//! tab-separated row per repository; `inspect` prints one repository's full status as JSON. Neither
//! writes metadata.

use std::io::Write;

use anyhow::Context as _;
use peryx_driver::quota::repository_quota;

use super::CacheStores;
use crate::cli::{QuotaCommand, QuotaInspectArgs};
use crate::config::Config;
use crate::server;

/// # Errors
/// Returns an error if the configured indexes cannot be built, the metadata store cannot be read, the
/// named index is unknown, or output fails.
pub fn quota(config: &Config, command: &QuotaCommand, out: &mut dyn Write) -> anyhow::Result<()> {
    quota_with_plugins(config, &crate::compiled_plugins(), command, out)
}

/// # Errors
///
/// Returns an error when store access, index construction or lookup, serialization, or output fails.
pub fn quota_with_plugins(
    config: &Config,
    plugins: &peryx_plugin_registry::PluginRegistry,
    command: &QuotaCommand,
    out: &mut dyn Write,
) -> anyhow::Result<()> {
    let plugins = crate::server::activate_plugins(config, plugins)?;
    let stores = CacheStores::open(config, &plugins, false)?;
    let indexes = server::build_indexes_with_plugins(&config.indexes, &config.auth, config.offline, &plugins)?;
    match command {
        QuotaCommand::List(_) => list(&stores, &indexes, out),
        QuotaCommand::Inspect(args) => inspect(&stores, &indexes, args, out),
    }
}

fn list(stores: &CacheStores, indexes: &[peryx_driver::Index], out: &mut dyn Write) -> anyhow::Result<()> {
    writeln!(
        out,
        "repository\tecosystem\tused_bytes\treserved_bytes\tbyte_limit\tremaining_bytes\tresources\tresource_limit\taudit"
    )?;
    for index in indexes {
        let usage = read_usage(stores, &index.name)?;
        let status = repository_quota(index, &usage);
        writeln!(
            out,
            "{}\t{}\t{}\t{}\t{}\t{}\t{}\t{}\t{}",
            status.repository,
            status.ecosystem,
            status.accounted_bytes.committed,
            status.accounted_bytes.reserved,
            optional(status.accounted_bytes.limit),
            optional(status.accounted_bytes.remaining),
            status.resources.committed,
            optional(status.resources.limit),
            status.limits.audit,
        )?;
    }
    Ok(())
}

fn inspect(
    stores: &CacheStores,
    indexes: &[peryx_driver::Index],
    args: &QuotaInspectArgs,
    out: &mut dyn Write,
) -> anyhow::Result<()> {
    let index = indexes
        .iter()
        .find(|index| index.name == args.index)
        .with_context(|| format!("unknown index {:?}", args.index))?;
    let usage = read_usage(stores, &index.name)?;
    let status = repository_quota(index, &usage);
    writeln!(out, "{}", serde_json::to_string_pretty(&status)?)?;
    Ok(())
}

fn read_usage(stores: &CacheStores, name: &str) -> anyhow::Result<peryx_storage::meta::QuotaUsage> {
    stores
        .meta
        .quota_usage(name)
        .with_context(|| format!("read quota counters for {name:?}"))
}

fn optional(value: Option<u64>) -> String {
    value.map_or_else(|| "-".to_owned(), |value| value.to_string())
}

#[cfg(test)]
#[path = "../../tests/unit/tests/app/quota_tests.rs"]
mod tests;
