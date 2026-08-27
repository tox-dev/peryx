use clap::{Args, Subcommand};

use super::RuntimeArgs;

#[derive(Debug, Clone, PartialEq, Eq, Subcommand)]
pub enum JobCommand {
    /// List job runs, newest first.
    List(JobListArgs),
    /// Show one job run in detail.
    Show(JobShowArgs),
    /// Run a registered ecosystem job.
    Run {
        #[command(flatten)]
        runtime: RuntimeArgs,
        /// Registered ecosystem job command.
        command: Option<String>,
        /// Ecosystem-owned target.
        #[arg(long)]
        target: String,
        /// Ecosystem-owned source override.
        #[arg(long)]
        source: Option<String>,
        /// Maximum items to process; omit to use the ecosystem default.
        #[arg(long)]
        item_limit: Option<usize>,
        /// Maximum requests in flight; omit to use the ecosystem default.
        #[arg(long)]
        concurrency: Option<usize>,
        /// Overall wall-time budget in seconds.
        #[arg(long)]
        timeout_secs: Option<u64>,
    },
    /// Rebuild the derived resource search index from authoritative metadata.
    Reindex {
        #[command(flatten)]
        runtime: RuntimeArgs,
        /// Documents committed per chunk while rebuilding.
        #[arg(long, default_value_t = peryx_driver::jobs::DEFAULT_SEARCH_REBUILD_CHUNK)]
        chunk_size: usize,
    },
    /// Finalize an authority's retained ingress intents at its new home after a failover transfer.
    Drain {
        #[command(flatten)]
        runtime: RuntimeArgs,
        /// The authority whose retained intents to drain into local metadata.
        #[arg(long)]
        authority: String,
    },
}

impl JobCommand {
    #[must_use]
    pub const fn runtime_args(&self) -> &RuntimeArgs {
        match self {
            Self::List(args) => &args.runtime,
            Self::Show(args) => &args.runtime,
            Self::Run { runtime, .. } | Self::Reindex { runtime, .. } | Self::Drain { runtime, .. } => runtime,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Args)]
pub struct JobListArgs {
    #[command(flatten)]
    pub runtime: RuntimeArgs,
}

#[derive(Debug, Clone, PartialEq, Eq, Args)]
pub struct JobShowArgs {
    #[command(flatten)]
    pub runtime: RuntimeArgs,

    /// Job-run ID.
    pub id: String,
}
