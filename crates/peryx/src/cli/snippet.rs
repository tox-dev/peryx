//! The `config-snippet` command: print client configuration for one index.

use std::path::PathBuf;

use clap::Args;

/// Configuration flags for the client snippet command.
#[derive(Debug, Clone, PartialEq, Eq, Args)]
pub struct ConfigSnippetArgs {
    /// Path to a TOML config file.
    #[arg(long, short = 'c')]
    pub config: Option<PathBuf>,

    /// Public base URL clients use to reach peryx, without the index route.
    #[arg(long)]
    pub base_url: String,

    /// Index route to configure.
    #[arg(long)]
    pub index: String,

    /// Client configuration file to print.
    pub format: String,
}
