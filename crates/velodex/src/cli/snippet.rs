//! The `config-snippet` command: print client configuration for one index.

use std::path::PathBuf;

use clap::{Args, ValueEnum};

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

impl From<SnippetFormat> for velodex_ecosystem_pypi::discovery::SnippetKind {
    fn from(value: SnippetFormat) -> Self {
        match value {
            SnippetFormat::PipConf => Self::PipConf,
            SnippetFormat::UvToml => Self::UvToml,
            SnippetFormat::Pypirc => Self::Pypirc,
        }
    }
}
