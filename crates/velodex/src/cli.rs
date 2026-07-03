//! Command-line interface.

use std::path::PathBuf;

use clap::builder::styling::{AnsiColor, Effects, Styles};
use clap::{Args, Parser, Subcommand, ValueEnum};

use crate::config::{LogFormat, LogSink, PartialConfig, PartialLogConfig};

/// uv-style help colors: bold green section headers, cyan literals and placeholders.
const STYLES: Styles = Styles::styled()
    .header(AnsiColor::Green.on_default().effects(Effects::BOLD))
    .usage(AnsiColor::Green.on_default().effects(Effects::BOLD))
    .literal(AnsiColor::Cyan.on_default().effects(Effects::BOLD))
    .placeholder(AnsiColor::Cyan.on_default());

/// velodex: a PyPI-compatible read-through cache and private-index overlay.
#[derive(Debug, Parser)]
#[command(
    name = "velodex",
    version,
    about,
    styles = STYLES,
    after_help = "Examples:\n  velodex serve\n  velodex serve --port 8080 --data-dir /var/lib/velodex\n  velodex serve --config velodex.toml -v\n\nDocumentation: https://velodex.readthedocs.io/"
)]
pub struct Cli {
    #[command(subcommand)]
    pub command: Command,
}

/// A velodex subcommand.
#[derive(Debug, Clone, PartialEq, Eq, Subcommand)]
pub enum Command {
    /// Run the server.
    #[command(
        after_help = "Examples:\n  velodex serve\n  velodex serve --host 0.0.0.0 --port 4433\n  velodex serve --config velodex.toml --log-format json --log-sink file --log-file velodex.log"
    )]
    Serve(RuntimeArgs),
    /// Initialize a data directory.
    Init(RuntimeArgs),
    /// Print client configuration for one index.
    #[command(
        after_help = "Examples:\n  velodex config-snippet --base-url https://packages.example --index root/pypi pip.conf\n  velodex config-snippet --base-url https://packages.example --index root/pypi uv.toml\n  velodex config-snippet --base-url https://packages.example --index root/pypi .pypirc"
    )]
    ConfigSnippet(ConfigSnippetArgs),
    /// Inspect and maintain the on-disk cache.
    #[command(subcommand)]
    Cache(CacheCommand),
    /// Print the `OpenAPI` description of the HTTP API as JSON.
    Openapi,
    /// Manage this velodex installation.
    #[cfg(feature = "self-update")]
    #[command(subcommand, name = "self")]
    SelfManage(SelfCommand),
}

/// Actions on the velodex installation itself.
#[cfg(feature = "self-update")]
#[derive(Debug, Clone, Copy, PartialEq, Eq, Subcommand)]
pub enum SelfCommand {
    /// Update velodex to the latest release.
    Update,
}

/// Cache inspection and maintenance commands.
#[derive(Debug, Clone, PartialEq, Eq, Subcommand)]
pub enum CacheCommand {
    /// List cached index pages and blobs.
    List(CacheListArgs),
    /// Report cache record and blob sizes.
    Size(CacheRuntimeArgs),
    /// Validate metadata records and blob hashes.
    Fsck(CacheRuntimeArgs),
    /// Plan or run cache cleanup.
    #[command(subcommand)]
    Purge(CachePurgeCommand),
}

impl CacheCommand {
    #[must_use]
    pub const fn runtime_args(&self) -> &RuntimeArgs {
        match self {
            Self::List(args) => &args.runtime,
            Self::Size(args) | Self::Fsck(args) => &args.runtime,
            Self::Purge(command) => command.runtime_args(),
        }
    }
}

/// Runtime configuration flags for cache commands.
#[derive(Debug, Clone, PartialEq, Eq, Args)]
pub struct CacheRuntimeArgs {
    #[command(flatten)]
    pub runtime: RuntimeArgs,
}

/// Filters for `velodex cache list`.
#[derive(Debug, Clone, PartialEq, Eq, Args)]
pub struct CacheListArgs {
    #[command(flatten)]
    pub runtime: RuntimeArgs,

    /// Only show cached pages for this configured index name.
    #[arg(long)]
    pub index: Option<String>,

    /// Only show cached pages for this project.
    #[arg(long)]
    pub project: Option<String>,

    /// Only show this blob digest.
    #[arg(long)]
    pub digest: Option<String>,

    /// Only show stale cached index pages.
    #[arg(long)]
    pub stale: bool,

    /// Only show entries at least this many seconds old.
    #[arg(long)]
    pub min_age_secs: Option<u64>,

    /// Only show entries at least this many bytes.
    #[arg(long)]
    pub min_size_bytes: Option<u64>,
}

/// Cache cleanup commands.
#[derive(Debug, Clone, PartialEq, Eq, Subcommand)]
pub enum CachePurgeCommand {
    /// Remove cached metadata for one project.
    Project(CachePurgeProjectArgs),
    /// Remove blob files that no metadata record references.
    OrphanedBlobs(CachePurgeOrphanedBlobsArgs),
}

impl CachePurgeCommand {
    #[must_use]
    pub const fn runtime_args(&self) -> &RuntimeArgs {
        match self {
            Self::Project(args) => &args.runtime,
            Self::OrphanedBlobs(args) => &args.runtime,
        }
    }
}

/// Options for project cache cleanup.
#[derive(Debug, Clone, PartialEq, Eq, Args)]
pub struct CachePurgeProjectArgs {
    #[command(flatten)]
    pub runtime: RuntimeArgs,

    /// Cached index name.
    #[arg(long)]
    pub index: String,

    /// Project name to purge.
    #[arg(long)]
    pub project: String,

    /// Delete the planned records. Without this flag, the command only prints the plan.
    #[arg(long)]
    pub yes: bool,
}

/// Options for orphaned blob cleanup.
#[derive(Debug, Clone, PartialEq, Eq, Args)]
pub struct CachePurgeOrphanedBlobsArgs {
    #[command(flatten)]
    pub runtime: RuntimeArgs,

    /// Delete the planned blob files. Without this flag, the command only prints the plan.
    #[arg(long)]
    pub yes: bool,
}

/// Configuration flags for the client snippet command.
#[derive(Debug, Clone, PartialEq, Eq, Args)]
pub struct ConfigSnippetArgs {
    /// Path to a TOML config file.
    #[arg(long, short = 'c')]
    pub config: Option<PathBuf>,

    /// Public base URL clients use to reach velodex, without the index route.
    #[arg(long)]
    pub base_url: String,

    /// Index route to configure.
    #[arg(long, default_value = "root/pypi")]
    pub index: String,

    /// Client configuration file to print.
    #[arg(value_enum)]
    pub format: SnippetFormat,
}

/// A client configuration snippet format.
#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
pub enum SnippetFormat {
    #[value(name = "pip.conf")]
    PipConf,
    #[value(name = "uv.toml")]
    UvToml,
    #[value(name = ".pypirc")]
    Pypirc,
}

impl From<SnippetFormat> for velodex_http::discovery::SnippetKind {
    fn from(value: SnippetFormat) -> Self {
        match value {
            SnippetFormat::PipConf => Self::PipConf,
            SnippetFormat::UvToml => Self::UvToml,
            SnippetFormat::Pypirc => Self::Pypirc,
        }
    }
}

/// Configuration flags shared by the commands that read the runtime configuration.
#[derive(Debug, Clone, PartialEq, Eq, Args)]
pub struct RuntimeArgs {
    /// Path to a TOML config file.
    #[arg(long, short = 'c')]
    pub config: Option<PathBuf>,

    /// Bind host.
    #[arg(long)]
    pub host: Option<String>,

    /// Bind port.
    #[arg(long)]
    pub port: Option<u16>,

    /// Data directory.
    #[arg(long)]
    pub data_dir: Option<PathBuf>,

    /// Log level filter, e.g. `info` or `velodex_upstream=debug`.
    #[arg(long, help_heading = "Logging")]
    pub log_level: Option<String>,

    /// Increase log verbosity: `-v` for debug, `-vv` for trace.
    #[arg(long, short = 'v', action = clap::ArgAction::Count, help_heading = "Logging")]
    pub verbose: u8,

    /// Log output format.
    #[arg(long, value_enum, help_heading = "Logging")]
    pub log_format: Option<LogFormat>,

    /// Log sink.
    #[arg(long, value_enum, help_heading = "Logging")]
    pub log_sink: Option<LogSink>,

    /// Log file path, used when `--log-sink file`.
    #[arg(long, help_heading = "Logging")]
    pub log_file: Option<PathBuf>,
}

impl RuntimeArgs {
    /// Project the flags into a [`PartialConfig`] overlay, the highest-precedence source.
    #[must_use]
    pub fn overlay(&self) -> PartialConfig {
        let level = self.log_level.clone().or_else(|| match self.verbose {
            0 => None,
            1 => Some("debug".to_owned()),
            _ => Some("trace".to_owned()),
        });
        PartialConfig {
            host: self.host.clone(),
            port: self.port,
            data_dir: self.data_dir.clone(),
            cache_ttl_secs: None,
            indexes: None,
            log: PartialLogConfig {
                level,
                format: self.log_format,
                sink: self.log_sink,
                file: self.log_file.clone(),
            },
        }
    }
}
