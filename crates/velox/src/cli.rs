//! Command-line interface.

use std::path::PathBuf;

use clap::{Parser, Subcommand};

use crate::config::{LogFormat, LogSink, PartialConfig, PartialLogConfig};

/// velox: a PyPI-compatible read-through cache and private-index overlay.
#[derive(Debug, Parser)]
#[command(name = "velox", version, about)]
pub struct Cli {
    /// Path to a TOML config file.
    #[arg(long, short = 'c', global = true)]
    pub config: Option<PathBuf>,

    /// Log level filter, e.g. `info` or `velox_upstream=debug`.
    #[arg(long, global = true)]
    pub log_level: Option<String>,

    /// Increase log verbosity: `-v` for debug, `-vv` for trace.
    #[arg(long, short = 'v', global = true, action = clap::ArgAction::Count)]
    pub verbose: u8,

    /// Bind host.
    #[arg(long, global = true)]
    pub host: Option<String>,

    /// Bind port.
    #[arg(long, global = true)]
    pub port: Option<u16>,

    /// Data directory.
    #[arg(long, global = true)]
    pub data_dir: Option<PathBuf>,

    /// Log output format.
    #[arg(long, global = true, value_enum)]
    pub log_format: Option<LogFormat>,

    /// Log sink.
    #[arg(long, global = true, value_enum)]
    pub log_sink: Option<LogSink>,

    /// Log file path, used when `--log-sink file`.
    #[arg(long, global = true)]
    pub log_file: Option<PathBuf>,

    #[command(subcommand)]
    pub command: Command,
}

/// A velox subcommand.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Subcommand)]
pub enum Command {
    /// Run the server.
    Serve,
    /// Initialize a data directory.
    Init,
}

impl Cli {
    /// Project the CLI flags into a [`PartialConfig`] overlay, the highest-precedence source.
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
            upstream_url: None,
            log: PartialLogConfig {
                level,
                format: self.log_format,
                sink: self.log_sink,
                file: self.log_file.clone(),
            },
        }
    }
}
