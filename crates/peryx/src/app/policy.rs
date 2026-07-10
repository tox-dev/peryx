//! Policy dry-run: preview allow/deny decisions over the cached and uploaded records.

use std::io::Write;

use super::CacheStores;
use crate::cli::{PolicyCommand, PolicyDryRunArgs};
use crate::config::Config;
use crate::server;

/// Run a policy inspection command.
///
/// # Errors
/// Returns an error if configured indexes cannot be built, the metadata store cannot be read, or
/// output fails.
pub fn policy(config: &Config, command: &PolicyCommand, out: &mut dyn Write) -> anyhow::Result<()> {
    let stores = CacheStores::open(config)?;
    let indexes = server::build_indexes(&config.indexes, config.offline)?;
    match command {
        PolicyCommand::DryRun(args) => policy_dry_run(&stores, &indexes, args, out),
    }
}

fn policy_dry_run(
    stores: &CacheStores,
    indexes: &[peryx_driver::Index],
    args: &PolicyDryRunArgs,
    out: &mut dyn Write,
) -> anyhow::Result<()> {
    writeln!(out, "action\tindex\tproject\tfilename\tversion\trule\tfield\treason")?;
    for driver in server::drivers().present() {
        driver
            .policy_dry_run(
                &stores.meta,
                indexes,
                args.index.as_deref(),
                args.project.as_deref(),
                out,
            )
            .map_err(|reason| anyhow::anyhow!("preview {} policy: {reason}", driver.ecosystem().as_str()))?;
    }
    Ok(())
}
