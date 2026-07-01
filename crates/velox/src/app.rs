//! Command dispatch and the pieces of startup that do not touch global state.

use std::path::Path;

use crate::cli::Command;
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

/// Run a subcommand against a resolved config.
///
/// # Errors
/// Propagates errors from the subcommand, for example a failure to create the data directory.
pub fn dispatch(command: Command, config: &Config) -> anyhow::Result<()> {
    match command {
        Command::Init => {
            if init_data_dir(&config.data_dir)? {
                tracing::info!(path = %config.data_dir.display(), "initialized data directory");
            } else {
                tracing::info!(path = %config.data_dir.display(), "data directory already exists");
            }
            Ok(())
        }
        Command::Serve => {
            tracing::info!(
                host = %config.host,
                port = config.port,
                upstream = %config.upstream_url,
                "serve is not yet implemented (arrives in Phase 1)"
            );
            Ok(())
        }
    }
}
