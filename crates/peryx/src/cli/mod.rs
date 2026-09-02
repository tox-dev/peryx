mod cache;
mod config;
mod index;
mod job;
mod maintenance;
mod mirror;
mod quota;
mod retention;
mod snippet;

use std::path::PathBuf;

use clap::builder::styling::{AnsiColor, Effects, Styles};
use clap::{Args, Parser, Subcommand};

pub use cache::{
    CacheCommand, CacheListArgs, CachePurgeCommand, CachePurgeOrphanedBlobsArgs, CachePurgeResourceArgs,
    CacheRepairArgs, CacheRuntimeArgs,
};
pub use config::{ConfigCheckArgs, ConfigCommand};
pub use index::{IndexCommand, IndexListArgs, IndexShowArgs};
pub use job::{JobCommand, JobListArgs, JobShowArgs};
#[cfg(feature = "self-update")]
pub use maintenance::SelfCommand;
pub use maintenance::{
    AdministratorClientArgs, BackupCommand, BackupCreateArgs, BackupVerifyArgs, BootstrapAdministratorArgs,
    ImportDirArgs, InspectRevocationArgs, LiftRevocationArgs, ListRevocationsArgs, PolicyCommand, PolicyDryRunArgs,
    PutRevocationArgs, RestoreArgs, RevocationCommand, RevocationStatusArg, WriterCommand, WriterPromoteArgs,
};
pub use mirror::{PrefetchCommand, PrefetchOptions, PrefetchPlanArgs, PrefetchSyncArgs, PrefetchVerifyArgs};
pub use quota::{QuotaCommand, QuotaInspectArgs, QuotaListArgs};
pub use retention::{RetentionCommand, RetentionDryRunArgs, RetentionExportArgs};
pub use snippet::ConfigSnippetArgs;

use crate::config::{
    LogFormat, LogSink, PartialAuthConfig, PartialConfig, PartialJobsConfig, PartialLogConfig, PartialRateLimitConfig,
};

/// uv-style help colors: bold green section headers, cyan literals and placeholders.
const STYLES: Styles = Styles::styled()
    .header(AnsiColor::Green.on_default().effects(Effects::BOLD))
    .usage(AnsiColor::Green.on_default().effects(Effects::BOLD))
    .literal(AnsiColor::Cyan.on_default().effects(Effects::BOLD))
    .placeholder(AnsiColor::Cyan.on_default());

/// Cache, host, and combine artifact indexes across ecosystems.
#[derive(Debug, Parser)]
#[command(
    name = "peryx",
    version,
    about,
    styles = STYLES,
    after_help = concat!(
        "Examples:\n  peryx serve\n",
        "  peryx serve --port 8080 --data-dir /var/lib/peryx\n",
        "  peryx serve --config peryx.toml -v\n\n",
        "Documentation: https://peryx.readthedocs.io/",
    )
)]
pub struct Cli {
    #[command(subcommand)]
    pub command: Command,
}

#[derive(Debug, Clone, PartialEq, Eq, Subcommand)]
pub enum Command {
    /// Run the server.
    #[command(
        after_help = concat!(
            "Examples:\n  peryx serve\n",
            "  peryx serve --host 0.0.0.0 --port 4433\n",
            "  peryx serve --config peryx.toml --log-format json --log-sink file --log-file peryx.log",
        )
    )]
    Serve(RuntimeArgs),
    /// Initialize a data directory.
    Init(RuntimeArgs),
    /// Validate a resolved configuration without starting the server.
    #[command(subcommand)]
    Config(ConfigCommand),
    /// Create the first local administrator.
    BootstrapAdministrator(BootstrapAdministratorArgs),
    /// Create, inspect, list, and lift digest revocations.
    #[command(subcommand)]
    Revocation(RevocationCommand),
    /// Print client configuration for one index.
    ConfigSnippet(ConfigSnippetArgs),
    /// List and inspect the configured indexes.
    #[command(subcommand)]
    Index(IndexCommand),
    /// Inspect durable job-run history.
    #[command(subcommand)]
    Job(JobCommand),
    /// Inspect and maintain the on-disk cache.
    #[command(subcommand)]
    Cache(CacheCommand),
    /// Create and verify offline backups.
    #[command(subcommand)]
    Backup(BackupCommand),
    /// Restore an offline backup into a data directory.
    Restore(RestoreArgs),
    /// Import local artifacts into a hosted index.
    ImportDir(ImportDirArgs),
    /// Preview index policy decisions against cached records.
    #[command(subcommand)]
    Policy(PolicyCommand),
    /// Report configured limits and committed and reserved use per repository.
    #[command(subcommand)]
    Quota(QuotaCommand),
    /// Preview and export a repository's retention plan.
    #[command(subcommand)]
    Retention(RetentionCommand),
    /// Inspect and change the single-writer identity.
    #[command(subcommand)]
    Writer(WriterCommand),
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

#[derive(Debug, Clone, PartialEq, Eq, Default, Args)]
pub struct RuntimeArgs {
    /// Path to a TOML config file.
    #[arg(long, short = 'c')]
    pub config: Option<PathBuf>,

    /// Bind IP address or DNS hostname. IPv6 literals do not need brackets.
    #[arg(long)]
    pub host: Option<String>,

    /// Bind port.
    #[arg(long)]
    pub port: Option<u16>,

    /// Data directory.
    #[arg(long)]
    pub data_dir: Option<PathBuf>,

    /// Identity allowed to write this metadata store.
    #[arg(long)]
    pub writer_identity: Option<String>,

    /// This node's own identity in an `ha` consensus roster, naming its member entry.
    #[arg(long)]
    pub node_identity: Option<String>,

    /// Serve configured cached indexes without upstream access.
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
            writer_identity: self.writer_identity.clone(),
            node_identity: self.node_identity.clone(),
            offline: self.offline.then_some(true),
            read_only: self.read_only.then_some(true),
            cache_ttl_secs: None,
            hot_cache_bytes: None,
            netrc: None,
            max_stale_secs: None,
            usage_retention_days: None,
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
            availability: None,
            jobs: PartialJobsConfig::default(),
            blob: None,
        }
    }
}
