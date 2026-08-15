use std::io::Write;
use std::path::Path;

use anyhow::{Context as _, bail};
use peryx_driver::IndexDescription;
use peryx_driver::discovery::BaseUrl;

use crate::cli::IndexCommand;
use crate::config::Config;
use crate::server;

/// # Errors
/// Propagates the filesystem error when the directory cannot be created.
pub fn init_data_dir(data_dir: &Path) -> std::io::Result<bool> {
    if data_dir.exists() {
        return Ok(false);
    }
    std::fs::create_dir_all(data_dir)?;
    Ok(true)
}

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

/// # Errors
/// Returns an error if the base URL is invalid, the index route is unknown, or the requested
/// snippet needs uploads on a read-only index.
/// # Panics
///
/// Panics if the caller bypasses configuration validation.
pub fn config_snippet(config: &Config, route: &str, base_url: &str, format: &str) -> anyhow::Result<String> {
    config_snippet_with_plugins(config, &crate::compiled_plugins(), route, base_url, format)
}

/// # Errors
///
/// Returns an error for an invalid base URL, invalid index configuration, unknown route, unsupported
/// format, or read-only index.
///
/// # Panics
///
/// Panics if `config` contains an ecosystem identifier that passed validation but cannot be parsed.
pub fn config_snippet_with_plugins(
    config: &Config,
    plugins: &peryx_plugin_registry::PluginRegistry,
    route: &str,
    base_url: &str,
    format: &str,
) -> anyhow::Result<String> {
    let plugins = crate::server::activate_plugins(config, plugins)?;
    let base = BaseUrl::parse(base_url)?;
    let index = peryx_http::describe_indexes(&server::build_indexes_with_plugins(
        &config.indexes,
        &config.auth,
        config.offline,
        &plugins,
    )?)
    .into_iter()
    .find(|index| index.route == route)
    .with_context(|| format!("unknown index route {route:?}"))?;
    let Some(text) = plugins
        .snippet_text(
            &index.ecosystem.parse().expect("configured ecosystems were validated"),
            &base,
            &index.route,
            index.uploads,
            format,
        )
        .map_err(anyhow::Error::msg)?
    else {
        bail!("index route {route:?} does not accept uploads");
    };
    Ok(text)
}

/// # Errors
/// Returns an error if the configured indexes cannot be built, the index is unknown, or output
/// fails.
pub fn index(config: &Config, command: &IndexCommand, out: &mut dyn Write) -> anyhow::Result<()> {
    index_with_plugins(config, &crate::compiled_plugins(), command, out)
}

/// # Errors
///
/// Returns an error when index construction, lookup, or output fails.
pub fn index_with_plugins(
    config: &Config,
    plugins: &peryx_plugin_registry::PluginRegistry,
    command: &IndexCommand,
    out: &mut dyn Write,
) -> anyhow::Result<()> {
    let plugins = crate::server::activate_plugins(config, plugins)?;
    let indexes = peryx_http::describe_indexes(&server::build_indexes_with_plugins(
        &config.indexes,
        &config.auth,
        config.offline,
        &plugins,
    )?);
    match command {
        IndexCommand::List(args) => index_list(&indexes, args.ecosystem.as_deref(), out),
        IndexCommand::Show(args) => index_show(&indexes, &args.index, out),
    }
}

fn index_list(indexes: &[IndexDescription], ecosystem: Option<&str>, out: &mut dyn Write) -> anyhow::Result<()> {
    writeln!(out, "name\troute\tecosystem\tkind\tuploads")?;
    for index in indexes
        .iter()
        .filter(|index| ecosystem.is_none_or(|wanted| wanted == index.ecosystem))
    {
        writeln!(
            out,
            "{}\t{}\t{}\t{}\t{}",
            index.name, index.route, index.ecosystem, index.kind, index.uploads
        )?;
    }
    Ok(())
}

fn index_show(indexes: &[IndexDescription], selector: &str, out: &mut dyn Write) -> anyhow::Result<()> {
    let index = indexes
        .iter()
        .find(|index| index.name == selector || index.route == selector)
        .with_context(|| format!("unknown index {selector:?}"))?;
    writeln!(out, "name\t{}", index.name)?;
    writeln!(out, "route\t{}", index.route)?;
    writeln!(out, "ecosystem\t{}", index.ecosystem)?;
    writeln!(out, "kind\t{}", index.kind)?;
    writeln!(out, "uploads\t{}", index.uploads)?;
    if !index.layers.is_empty() {
        writeln!(out, "layers\t{}", index.layers.join(", "))?;
    }
    if let Some(upstream) = &index.upstream {
        writeln!(out, "upstream\t{}", upstream.url)?;
        writeln!(out, "offline\t{}", upstream.offline)?;
    }
    if let Some(upload_to) = &index.upload_to {
        writeln!(out, "upload_to\t{upload_to}")?;
    }
    Ok(())
}

#[cfg(test)]
#[path = "../../tests/unit/tests/app/indexes_tests.rs"]
mod tests;
