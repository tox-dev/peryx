use std::io::Write;

use anyhow::{Context as _, anyhow};

use crate::config::Config;

/// # Errors
/// Returns an error when no expected writer is configured, the metadata store cannot be opened, the
/// active identity changed, the replacement is invalid, or output fails.
pub fn promote_writer(config: &Config, replacement: &str, out: &mut dyn Write) -> anyhow::Result<()> {
    promote_writer_with_plugins(config, &crate::compiled_plugins(), replacement, out)
}

/// # Errors
/// Returns an error when metadata access, identity replacement, or output fails.
pub fn promote_writer_with_plugins(
    config: &Config,
    plugins: &peryx_plugin_registry::PluginRegistry,
    replacement: &str,
    out: &mut dyn Write,
) -> anyhow::Result<()> {
    let expected = config
        .writer_identity
        .as_deref()
        .ok_or_else(|| anyhow!("writer identity is not configured; set `writer_identity` to the active writer"))?;
    let path = config.data_dir.join("peryx.redb");
    let meta = crate::metadata::open_existing(&path, plugins)?;
    meta.promote_writer_identity(expected, replacement)
        .context(format!("promote writer from {expected:?} to {replacement:?}"))?;
    writeln!(out, "writer\t{expected}\t{replacement}")?;
    Ok(())
}

/// Claim the configured writer identity in the local metadata store.
///
/// This seeds a replica offline so it can verify the writer it follows when it starts read-only. The
/// store is created if absent, a repeat claim of the same identity is a no-op, and a store another
/// writer already owns is rejected.
///
/// # Errors
/// Returns an error when no writer identity is configured, the process cannot create the data directory
/// or metadata store, another writer already owns the store, the identity is invalid, or output fails.
pub fn claim_writer(config: &Config, out: &mut dyn Write) -> anyhow::Result<()> {
    claim_writer_with_plugins(config, &crate::compiled_plugins(), out)
}

/// # Errors
/// Returns an error when the process cannot create the data directory, metadata access or identity
/// claim fails, or output fails.
pub fn claim_writer_with_plugins(
    config: &Config,
    plugins: &peryx_plugin_registry::PluginRegistry,
    out: &mut dyn Write,
) -> anyhow::Result<()> {
    let identity = config.writer_identity.as_deref().ok_or_else(|| {
        anyhow!("writer identity is not configured; set `writer_identity` to the writer this replica follows")
    })?;
    std::fs::create_dir_all(&config.data_dir)
        .context(format!("create data directory {}", config.data_dir.display()))?;
    let path = config.data_dir.join("peryx.redb");
    let meta = crate::metadata::open(&path, plugins)?;
    meta.claim_writer_identity(identity)
        .context(format!("claim writer identity {identity:?}"))?;
    writeln!(out, "writer\t{identity}")?;
    Ok(())
}
