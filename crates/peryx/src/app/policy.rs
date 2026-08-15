use std::io::Write;

use anyhow::Context as _;

use super::CacheStores;
use crate::cli::{PolicyCommand, PolicyDryRunArgs};
use crate::config::Config;
use crate::server;

/// # Errors
/// Returns an error if configured indexes cannot be built, the metadata store cannot be read, or
/// output fails.
pub fn policy(config: &Config, command: &PolicyCommand, out: &mut dyn Write) -> anyhow::Result<()> {
    policy_with_plugins(config, &crate::compiled_plugins(), command, out)
}

/// # Errors
///
/// Returns an error when store access, index construction, policy evaluation, or output fails.
pub fn policy_with_plugins(
    config: &Config,
    plugins: &peryx_plugin_registry::PluginRegistry,
    command: &PolicyCommand,
    out: &mut dyn Write,
) -> anyhow::Result<()> {
    let plugins = crate::server::activate_plugins(config, plugins)?;
    match command {
        PolicyCommand::DryRun(args) => policy_dry_run(config, &plugins, args, out),
    }
}

fn policy_dry_run(
    config: &Config,
    plugins: &peryx_plugin_registry::PluginRegistry,
    args: &PolicyDryRunArgs,
    out: &mut dyn Write,
) -> anyhow::Result<()> {
    let ecosystems = if let Some(selector) = args.index.as_deref() {
        vec![
            config
                .indexes
                .iter()
                .find(|index| index.name == selector || index.route == selector)
                .ok_or_else(|| anyhow::anyhow!("unknown index {selector:?}"))?
                .ecosystem
                .clone(),
        ]
    } else {
        config.indexes.iter().fold(Vec::new(), |mut ecosystems, index| {
            if !ecosystems.contains(&index.ecosystem) {
                ecosystems.push(index.ecosystem.clone());
            }
            ecosystems
        })
    };
    let mut drivers = Vec::new();
    for ecosystem in ecosystems {
        let Some(driver) = plugins.drivers().get_policy_dry_run(&ecosystem) else {
            if args.index.is_some() {
                anyhow::bail!("index ecosystem {ecosystem} does not support policy dry-run");
            }
            continue;
        };
        drivers.push((ecosystem, driver));
    }
    anyhow::ensure!(!drivers.is_empty(), "no configured ecosystem supports policy dry-run");
    let stores = CacheStores::open(config, plugins, false)?;
    writeln!(out, "action\tindex\tresource\tartifact\tgroup\trule\tfield\treason")?;
    for (ecosystem, driver) in drivers {
        let index_configs = config
            .indexes
            .iter()
            .filter(|index| index.ecosystem == ecosystem)
            .cloned()
            .collect::<Vec<_>>();
        let indexes = server::build_indexes_with_plugins(&index_configs, &config.auth, config.offline, plugins)?;
        driver
            .policy_dry_run(
                &stores.meta,
                &indexes,
                args.index.as_deref(),
                args.resource.as_deref(),
                out,
            )
            .map_err(anyhow::Error::msg)
            .context(format!("preview {ecosystem} policy"))?;
    }
    Ok(())
}

#[cfg(test)]
#[path = "../../tests/unit/tests/app/policy_tests.rs"]
mod tests;
