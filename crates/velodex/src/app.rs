//! Command actions that do not touch global state.

use std::path::Path;

use anyhow::{Context as _, bail};
use velodex_http::discovery::{BaseUrl, SnippetKind, snippet_text};

use crate::config::Config;
use crate::server;

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

/// Render one client configuration snippet from the configured index topology.
///
/// # Errors
/// Returns an error if the base URL is invalid, the index route is unknown, or the requested
/// snippet needs uploads on a read-only index.
pub fn config_snippet(config: &Config, route: &str, base_url: &str, kind: SnippetKind) -> anyhow::Result<String> {
    let base = BaseUrl::parse(base_url)?;
    let index = velodex_http::describe_indexes(&server::build_indexes(&config.indexes)?)
        .into_iter()
        .find(|index| index.route == route)
        .with_context(|| format!("unknown index route {route:?}"))?;
    let Some(text) = snippet_text(&base, &index.route, index.uploads, kind) else {
        bail!("index route {route:?} does not accept uploads");
    };
    Ok(text)
}
