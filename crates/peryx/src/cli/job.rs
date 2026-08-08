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
    /// Refresh a remote project catalog and bounded project metadata set.
    Run {
        #[command(flatten)]
        runtime: RuntimeArgs,
        /// Configured cached repository name.
        #[arg(long)]
        repository: String,
        /// Named upstream source; omit to use repository routing.
        #[arg(long)]
        source: Option<String>,
        /// Maximum catalog projects to refresh.
        #[arg(long, default_value_t = peryx_driver::jobs::DEFAULT_CATALOG_PROJECTS)]
        max_projects: usize,
        /// Maximum project-metadata requests in flight.
        #[arg(long, default_value_t = peryx_driver::jobs::DEFAULT_CATALOG_CONCURRENCY)]
        concurrency: usize,
        /// Overall wall-time budget in seconds.
        #[arg(long, default_value_t = peryx_driver::jobs::DEFAULT_CATALOG_TIMEOUT.as_secs())]
        timeout_secs: u64,
    },
    /// Rebuild the derived package search index from authoritative metadata.
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
