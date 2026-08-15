use std::path::PathBuf;

use clap::{Args, Subcommand};

use super::RuntimeArgs;

#[derive(Debug, Clone, PartialEq, Eq, Subcommand)]
pub enum RetentionCommand {
    /// Preview the ordered removal candidates one page at a time.
    DryRun(RetentionDryRunArgs),
    /// Stream the whole plan as JSON Lines for machine consumption.
    Export(RetentionExportArgs),
}

impl RetentionCommand {
    #[must_use]
    pub const fn runtime_args(&self) -> &RuntimeArgs {
        match self {
            Self::DryRun(args) => &args.runtime,
            Self::Export(args) => &args.runtime,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Args)]
pub struct RetentionDryRunArgs {
    #[command(flatten)]
    pub runtime: RuntimeArgs,

    /// Hosted index name to plan.
    #[arg(long)]
    pub index: String,

    /// TOML file of `keep` and `expire` retention rules; without it the policy retains everything.
    #[arg(long, value_name = "PATH")]
    pub rules: Option<PathBuf>,

    /// Page size, from 1 through 1000; omit to list every candidate.
    #[arg(long)]
    pub limit: Option<usize>,

    /// Resume token printed by a prior page.
    #[arg(long)]
    pub cursor: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Args)]
pub struct RetentionExportArgs {
    #[command(flatten)]
    pub runtime: RuntimeArgs,

    /// Hosted index name to plan.
    #[arg(long)]
    pub index: String,

    /// TOML file of `keep` and `expire` retention rules; without it the policy retains everything.
    #[arg(long, value_name = "PATH")]
    pub rules: Option<PathBuf>,

    /// Resume token from an interrupted export; the export restarts at that boundary.
    #[arg(long)]
    pub cursor: Option<String>,
}
