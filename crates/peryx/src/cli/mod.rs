//! Command-line interface.

mod cache;
mod index;
mod maintenance;
mod mirror;
mod snippet;

use std::path::PathBuf;

use clap::builder::styling::{AnsiColor, Effects, Styles};
use clap::{Args, Parser, Subcommand, ValueEnum};

pub use cache::{
    CacheCommand, CacheListArgs, CachePurgeCommand, CachePurgeOrphanedBlobsArgs, CachePurgeProjectArgs,
    CacheRuntimeArgs,
};
pub use index::{IndexCommand, IndexListArgs, IndexShowArgs};
#[cfg(feature = "self-update")]
pub use maintenance::SelfCommand;
pub use maintenance::{
    BackupCommand, BackupCreateArgs, BackupVerifyArgs, ImportDirArgs, PolicyCommand, PolicyDryRunArgs, RestoreArgs,
};
pub use mirror::{PrefetchCommand, PrefetchOptions, PrefetchPlanArgs, PrefetchSyncArgs, PrefetchVerifyArgs};
pub use snippet::{ConfigSnippetArgs, SnippetFormat};

use crate::config::{LogFormat, LogSink, PartialAuthConfig, PartialConfig, PartialLogConfig, PartialRateLimitConfig};

/// uv-style help colors: bold green section headers, cyan literals and placeholders.
const STYLES: Styles = Styles::styled()
    .header(AnsiColor::Green.on_default().effects(Effects::BOLD))
    .usage(AnsiColor::Green.on_default().effects(Effects::BOLD))
    .literal(AnsiColor::Cyan.on_default().effects(Effects::BOLD))
    .placeholder(AnsiColor::Cyan.on_default());

/// peryx: one blazing-fast vault for a wide range of ecosystems — caching proxy, hosted store, and virtual index.
#[derive(Debug, Parser)]
#[command(
    name = "peryx",
    version,
    about,
    styles = STYLES,
    after_help = "Examples:\n  peryx serve\n  peryx serve --port 8080 --data-dir /var/lib/peryx\n  peryx serve --config peryx.toml -v\n\nDocumentation: https://peryx.readthedocs.io/"
)]
pub struct Cli {
    #[command(subcommand)]
    pub command: Command,
}

/// A peryx subcommand.
#[derive(Debug, Clone, PartialEq, Eq, Subcommand)]
pub enum Command {
    /// Run the server.
    #[command(
        after_help = "Examples:\n  peryx serve\n  peryx serve --host 0.0.0.0 --port 4433\n  peryx serve --config peryx.toml --log-format json --log-sink file --log-file peryx.log"
    )]
    Serve(RuntimeArgs),
    /// Initialize a data directory.
    Init(RuntimeArgs),
    /// Print client configuration for one index.
    #[command(
        after_help = "Examples:\n  peryx config-snippet --base-url https://packages.example --index root/pypi pip.conf\n  peryx config-snippet --base-url https://packages.example --index root/pypi uv.toml\n  peryx config-snippet --base-url https://packages.example --index root/pypi .pypirc"
    )]
    ConfigSnippet(ConfigSnippetArgs),
    /// List and inspect the configured indexes.
    #[command(subcommand)]
    Index(IndexCommand),
    /// Inspect and maintain the on-disk cache.
    #[command(subcommand)]
    Cache(CacheCommand),
    /// Create and verify offline backups.
    #[command(subcommand)]
    Backup(BackupCommand),
    /// Restore an offline backup into a data directory.
    Restore(RestoreArgs),
    /// Import local wheels and sdists into a hosted index.
    ImportDir(ImportDirArgs),
    /// Preview index policy decisions against cached records.
    #[command(subcommand)]
    Policy(PolicyCommand),
    /// Plan, sync, and verify a cached index's mirror working set.
    #[command(subcommand, name = "mirror")]
    Prefetch(PrefetchCommand),
    /// Print the `OpenAPI` description of the HTTP API as JSON.
    Openapi,
    /// Manage this peryx installation.
    #[cfg(feature = "self-update")]
    #[command(subcommand, name = "self")]
    SelfManage(SelfCommand),
}

/// The ecosystem a command targets. One variant today; the axis is reserved so `OCI`, npm, and more
/// slot in without reshaping the CLI.
#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
#[value(rename_all = "lowercase")]
pub enum EcosystemArg {
    Pypi,
}

impl EcosystemArg {
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Pypi => "pypi",
        }
    }
}

/// Configuration flags shared by the commands that read the runtime configuration.
#[derive(Debug, Clone, PartialEq, Eq, Default, Args)]
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

    /// Serve configured cached indexes from cache only.
    #[arg(long)]
    pub offline: bool,

    /// Run as a read replica, rejecting mutations and upstream cache fills.
    #[arg(long)]
    pub read_only: bool,

    /// Log level filter, e.g. `info` or `peryx_upstream=debug`.
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
            offline: self.offline.then_some(true),
            read_only: self.read_only.then_some(true),
            cache_ttl_secs: None,
            hot_cache_bytes: None,
            max_stale_secs: None,
            indexes: None,
            tls: None,
            acme: None,
            log: PartialLogConfig {
                level,
                format: self.log_format,
                sink: self.log_sink,
                file: self.log_file.clone(),
            },
            rate_limit: PartialRateLimitConfig::default(),
            auth: PartialAuthConfig::default(),
            replication: None,
        }
    }
}
