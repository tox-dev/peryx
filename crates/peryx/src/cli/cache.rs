//! The `cache` command group: inspect and maintain the on-disk cache.

use clap::{Args, Subcommand};

use super::RuntimeArgs;

/// Cache inspection and maintenance commands.
#[derive(Debug, Clone, PartialEq, Eq, Subcommand)]
pub enum CacheCommand {
    /// List cached index pages and blobs.
    List(CacheListArgs),
    /// Report cache record and blob sizes.
    Size(CacheRuntimeArgs),
    /// Validate metadata records and blob hashes.
    Fsck(CacheRuntimeArgs),
    /// Plan or run cache cleanup.
    #[command(subcommand)]
    Purge(CachePurgeCommand),
}

impl CacheCommand {
    #[must_use]
    pub const fn runtime_args(&self) -> &RuntimeArgs {
        match self {
            Self::List(args) => &args.runtime,
            Self::Size(args) | Self::Fsck(args) => &args.runtime,
            Self::Purge(command) => command.runtime_args(),
        }
    }
}

/// Runtime configuration flags for cache commands.
#[derive(Debug, Clone, PartialEq, Eq, Args)]
pub struct CacheRuntimeArgs {
    #[command(flatten)]
    pub runtime: RuntimeArgs,
}

/// Filters for `peryx cache list`.
#[derive(Debug, Clone, PartialEq, Eq, Args)]
pub struct CacheListArgs {
    #[command(flatten)]
    pub runtime: RuntimeArgs,

    /// Only show cached pages for this configured index name.
    #[arg(long)]
    pub index: Option<String>,

    /// Only show cached pages for this project.
    #[arg(long)]
    pub project: Option<String>,

    /// Only show this blob digest.
    #[arg(long)]
    pub digest: Option<String>,

    /// Only show stale cached index pages.
    #[arg(long)]
    pub stale: bool,

    /// Only show entries at least this many seconds old.
    #[arg(long)]
    pub min_age_secs: Option<u64>,

    /// Only show entries at least this many bytes.
    #[arg(long)]
    pub min_size_bytes: Option<u64>,
}

/// Cache cleanup commands.
#[derive(Debug, Clone, PartialEq, Eq, Subcommand)]
pub enum CachePurgeCommand {
    /// Remove cached metadata for one project.
    Project(CachePurgeProjectArgs),
    /// Remove blob files that no metadata record references.
    OrphanedBlobs(CachePurgeOrphanedBlobsArgs),
}

impl CachePurgeCommand {
    #[must_use]
    pub const fn runtime_args(&self) -> &RuntimeArgs {
        match self {
            Self::Project(args) => &args.runtime,
            Self::OrphanedBlobs(args) => &args.runtime,
        }
    }
}

/// Options for project cache cleanup.
#[derive(Debug, Clone, PartialEq, Eq, Args)]
pub struct CachePurgeProjectArgs {
    #[command(flatten)]
    pub runtime: RuntimeArgs,

    /// Cached index name.
    #[arg(long)]
    pub index: String,

    /// Project name to purge.
    #[arg(long)]
    pub project: String,

    /// Delete the planned records. Without this flag, the command only prints the plan.
    #[arg(long)]
    pub yes: bool,
}

/// Options for orphaned blob cleanup.
#[derive(Debug, Clone, PartialEq, Eq, Args)]
pub struct CachePurgeOrphanedBlobsArgs {
    #[command(flatten)]
    pub runtime: RuntimeArgs,

    /// Delete the planned blob files. Without this flag, the command only prints the plan.
    #[arg(long)]
    pub yes: bool,
}
