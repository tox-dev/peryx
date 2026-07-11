//! Index topology commands: data-directory init, client snippets, and index listing.

use std::io::Write;
use std::path::Path;

use anyhow::{Context as _, bail};
use peryx_driver::IndexDescription;
use peryx_driver::discovery::BaseUrl;
use peryx_ecosystem_pypi::discovery::{SnippetKind, snippet_text};

use crate::cli::{EcosystemArg, IndexCommand};
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

/// Run `peryx init`: ensure the data directory exists.
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
    let index = peryx_http::describe_indexes(&server::build_indexes(&config.indexes, config.offline)?)
        .into_iter()
        .find(|index| index.route == route)
        .with_context(|| format!("unknown index route {route:?}"))?;
    let Some(text) = snippet_text(&base, &index.route, index.uploads, kind) else {
        bail!("index route {route:?} does not accept uploads");
    };
    Ok(text)
}

/// List or show the configured indexes.
///
/// # Errors
/// Returns an error if the configured indexes cannot be built, the index is unknown, or output
/// fails.
pub fn index(config: &Config, command: &IndexCommand, out: &mut dyn Write) -> anyhow::Result<()> {
    let indexes = peryx_http::describe_indexes(&server::build_indexes(&config.indexes, config.offline)?);
    match command {
        IndexCommand::List(args) => index_list(&indexes, args.ecosystem, out),
        IndexCommand::Show(args) => index_show(&indexes, &args.index, out),
    }
}

fn index_list(
    indexes: &[IndexDescription],
    ecosystem: Option<EcosystemArg>,
    out: &mut dyn Write,
) -> anyhow::Result<()> {
    writeln!(out, "name\troute\tecosystem\tkind\tuploads")?;
    for index in indexes
        .iter()
        .filter(|index| ecosystem.is_none_or(|wanted| wanted.as_str() == index.ecosystem))
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
