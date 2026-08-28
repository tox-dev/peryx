use std::io::Write;

use crate::config::{AvailabilityConfig, Config, ReplicationConfig, TlsConfig};
use crate::server;

/// # Errors
/// Returns the configuration error the server would hit while assembling its state, or an output
/// error while writing the summary.
pub fn config_check(config: &Config, out: &mut dyn Write) -> anyhow::Result<()> {
    config_check_with_plugins(config, &crate::compiled_plugins(), out)
}

/// # Errors
///
/// Returns an error when validation, state assembly, or output fails.
pub fn config_check_with_plugins(
    config: &Config,
    plugins: &peryx_plugin_registry::PluginRegistry,
    out: &mut dyn Write,
) -> anyhow::Result<()> {
    let plugins = server::activate_plugins(config, plugins)?;
    config_check_with_active_plugins(config, &plugins, out)
}

pub fn config_check_with_active_plugins(
    config: &Config,
    plugins: &peryx_plugin_registry::PluginRegistry,
    out: &mut dyn Write,
) -> anyhow::Result<()> {
    let listen_address = server::check_config_with_active_plugins(config, plugins)?;
    writeln!(out, "configuration is valid")?;
    let scheme = match &config.tls {
        None => "http",
        Some(TlsConfig::Manual { .. }) => "https",
        Some(TlsConfig::Acme(_)) => "https+acme",
    };
    writeln!(out, "  listen: {scheme}://{listen_address}")?;
    let count = config.indexes.len();
    let plural = if count == 1 { "" } else { "es" };
    writeln!(out, "  indexes: {count} configured index{plural}")?;
    writeln!(out, "  availability: {}", availability_summary(config))?;
    Ok(())
}

/// The effective availability mode and its resolved topology, for `config check` to echo. It names the
/// role and its non-secret address and any datacenter group, but never the replication credential: the
/// effective-config output is safe to paste into a ticket.
fn availability_summary(config: &Config) -> String {
    let topology = |role: &ReplicationConfig| match role {
        ReplicationConfig::Primary { source, .. } => format!("primary, source {source:?}"),
        ReplicationConfig::Replica { upstream, .. } => format!("replica, upstream {upstream:?}"),
    };
    let mode = match &config.availability {
        AvailabilityConfig::None => "none (single node)".to_owned(),
        AvailabilityConfig::Dc(role) => format!("dc ({})", topology(role)),
        AvailabilityConfig::Ha(role) => format!("ha ({})", topology(role)),
    };
    match &config.dc_membership {
        None => mode,
        Some(membership) => {
            let count = membership.members.len();
            let plural = if count == 1 { "" } else { "s" };
            format!("{mode}, group {:?} ({count} member{plural})", membership.group)
        }
    }
}

#[cfg(test)]
#[path = "../../tests/unit/tests/app/config_tests.rs"]
mod tests;
