//! The `job` command group: inspect durable job-run history.

use clap::{Args, Subcommand};

use super::RuntimeArgs;

/// Inspect durable job-run history.
#[derive(Debug, Clone, PartialEq, Eq, Subcommand)]
pub enum JobCommand {
    /// List job runs, newest first.
    List(JobListArgs),
    /// Show one job run in detail.
    Show(JobShowArgs),
}

impl JobCommand {
    #[must_use]
    pub const fn runtime_args(&self) -> &RuntimeArgs {
        match self {
            Self::List(args) => &args.runtime,
            Self::Show(args) => &args.runtime,
        }
    }
}

/// Options for `peryx job list`.
#[derive(Debug, Clone, PartialEq, Eq, Args)]
pub struct JobListArgs {
    #[command(flatten)]
    pub runtime: RuntimeArgs,
}

/// Options for `peryx job show`.
#[derive(Debug, Clone, PartialEq, Eq, Args)]
pub struct JobShowArgs {
    #[command(flatten)]
    pub runtime: RuntimeArgs,

    /// Job-run ID.
    pub id: String,
}
