use std::io::Write;

use anyhow::{Context as _, anyhow};
use peryx_storage::meta::MetaStore;

use crate::config::Config;

/// Replace the expected writer identity in the configured metadata store.
///
/// # Errors
/// Returns an error when no expected writer is configured, the metadata store cannot be opened, the
/// active identity changed, the replacement is invalid, or output fails.
pub fn promote_writer(config: &Config, replacement: &str, out: &mut dyn Write) -> anyhow::Result<()> {
    let expected = config
        .writer_identity
        .as_deref()
        .ok_or_else(|| anyhow!("writer identity is not configured; set `writer_identity` to the active writer"))?;
    let path = config.data_dir.join("peryx.redb");
    let meta = MetaStore::open_existing(&path).with_context(|| format!("open metadata store {}", path.display()))?;
    meta.promote_writer_identity(expected, replacement)
        .with_context(|| format!("promote writer from {expected:?} to {replacement:?}"))?;
    writeln!(out, "writer\t{expected}\t{replacement}")?;
    Ok(())
}
