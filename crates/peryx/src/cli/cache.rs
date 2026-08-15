use clap::{Args, Subcommand};

use super::RuntimeArgs;

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
            Self::Purge(CachePurgeCommand::Resource(args)) => &args.runtime,
            Self::Purge(CachePurgeCommand::OrphanedBlobs(args)) => &args.runtime,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Args)]
pub struct CacheRuntimeArgs {
    #[command(flatten)]
    pub runtime: RuntimeArgs,
}

#[derive(Debug, Clone, PartialEq, Eq, Args)]
pub struct CacheListArgs {
    #[command(flatten)]
    pub runtime: RuntimeArgs,

    /// Filter by configured index name.
    #[arg(long)]
    pub index: Option<String>,

    /// Filter by resource.
    #[arg(long)]
    pub resource: Option<String>,

    /// Filter by blob digest.
    #[arg(long)]
    pub digest: Option<String>,

    /// Filter for stale cached index pages.
    #[arg(long)]
    pub stale: bool,

    /// Minimum entry age in seconds.
    #[arg(long)]
    pub min_age_secs: Option<u64>,

    /// Minimum entry size in bytes.
    #[arg(long)]
    pub min_size_bytes: Option<u64>,
}

#[derive(Debug, Clone, PartialEq, Eq, Subcommand)]
pub enum CachePurgeCommand {
    /// Remove cached metadata for one resource.
    Resource(CachePurgeResourceArgs),
    /// Remove blob files that no metadata record references.
    OrphanedBlobs(CachePurgeOrphanedBlobsArgs),
}

#[derive(Debug, Clone, PartialEq, Eq, Args)]
pub struct CachePurgeResourceArgs {
    #[command(flatten)]
    pub runtime: RuntimeArgs,

    /// Cached index name.
    #[arg(long)]
    pub index: String,

    /// Resource name to purge.
    #[arg(long)]
    pub resource: String,

    /// Delete the planned records; omission previews the plan.
    #[arg(long)]
    pub yes: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Args)]
pub struct CachePurgeOrphanedBlobsArgs {
    #[command(flatten)]
    pub runtime: RuntimeArgs,

    /// Delete the planned blob files; omission previews the plan.
    #[arg(long)]
    pub yes: bool,
}
