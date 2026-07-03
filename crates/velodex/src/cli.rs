//! Command-line interface.

use std::path::PathBuf;

use clap::builder::styling::{AnsiColor, Effects, Styles};
use clap::{Args, Parser, Subcommand};

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
