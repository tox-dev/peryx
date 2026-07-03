//! Command actions that do not touch global state.

use std::path::Path;

use crate::config::Config;

/// Create the data directory if it is missing. Returns whether it was created.
///
/// # Errors
/// Propagates the filesystem error when the directory cannot be created.
pub fn init_data_dir(data_dir: &Path) -> std::io::Result<bool> {
    if data_dir.exists() {
        return Ok(false);
    }
    std::fs::create_dir_all(data_dir)?;
    Ok(true)
}

/// Run `velodex init`: ensure the data directory exists.
///
/// # Errors
/// Propagates the filesystem error when the directory cannot be created.
pub fn init(config: &Config) -> anyhow::Result<()> {
    if init_data_dir(&config.data_dir)? {
        tracing::info!(path = %config.data_dir.display(), "initialized data directory");
    } else {
        tracing::info!(path = %config.data_dir.display(), "data directory already exists");
    }
    Ok(())
}
