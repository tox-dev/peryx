//! The `mirror` command group: plan, sync, and verify a cached index's prefetch working set.

use std::path::PathBuf;

use clap::{Args, Subcommand};

use super::RuntimeArgs;
use crate::config::PrefetchMode;

/// Prefetch synchronization commands.
#[derive(Debug, Clone, PartialEq, Eq, Subcommand)]
pub enum PrefetchCommand {
    /// Print the selected projects and files without writing cache entries.
    Plan(PrefetchPlanArgs),
    /// Fetch selected project pages, metadata siblings, and artifacts.
    Sync(PrefetchSyncArgs),
    /// Check cached pages, metadata siblings, and artifacts for a prefetch set.
    Verify(PrefetchVerifyArgs),
}

impl PrefetchCommand {
    #[must_use]
    pub const fn runtime_args(&self) -> &RuntimeArgs {
        &self.options().runtime
    }

    /// The options every prefetch subcommand carries, regardless of the verb.
    #[must_use]
    pub const fn options(&self) -> &PrefetchOptions {
        match self {
            Self::Plan(args) => &args.options,
            Self::Sync(args) => &args.options,
            Self::Verify(args) => &args.options,
        }
    }
}

/// Options shared by prefetch commands.
#[derive(Debug, Clone, PartialEq, Eq, Args)]
pub struct PrefetchOptions {
    #[command(flatten)]
    pub runtime: RuntimeArgs,

    /// Configured index name or route to sync.
    pub index: String,

    /// Add a package selector such as `requests>=2,<3`.
    #[arg(long = "package", short = 'p')]
    pub packages: Vec<String>,

    /// Read package selectors from a requirements or constraints file.
    #[arg(long = "requirements", short = 'r')]
    pub requirements: Vec<PathBuf>,

    /// Override the configured prefetch mode.
    #[arg(long, value_enum)]
    pub mode: Option<PrefetchMode>,

    /// Fetch Simple pages and PEP 658 metadata, but skip artifacts.
    #[arg(long)]
    pub metadata_only: bool,

    /// Exclude wheel artifacts.
    #[arg(long)]
    pub no_wheels: bool,

    /// Exclude source distributions.
    #[arg(long)]
    pub no_sdists: bool,

    /// Keep only wheels with this Python tag; repeat for more tags.
    #[arg(long = "python-tag")]
    pub python_tags: Vec<String>,

    /// Keep only wheels with this ABI tag; repeat for more tags.
    #[arg(long = "abi-tag")]
    pub abi_tags: Vec<String>,

    /// Keep only wheels with this platform tag; repeat for more tags.
    #[arg(long = "platform-tag")]
    pub platform_tags: Vec<String>,

    /// Skip files larger than this many bytes when upstream reports a size.
    #[arg(long)]
    pub max_file_size_bytes: Option<u64>,

    /// Add an OCI image reference to mirror, such as `library/alpine:latest`; repeat for more. Used
    /// when the index is an OCI index instead of the `PyPI` package selectors above.
    #[arg(long = "image")]
    pub images: Vec<String>,
}

/// Options for `peryx prefetch plan`.
#[derive(Debug, Clone, PartialEq, Eq, Args)]
pub struct PrefetchPlanArgs {
    #[command(flatten)]
    pub options: PrefetchOptions,
}

/// Options for `peryx prefetch sync`.
#[derive(Debug, Clone, PartialEq, Eq, Args)]
pub struct PrefetchSyncArgs {
    #[command(flatten)]
    pub options: PrefetchOptions,
}

/// Options for `peryx prefetch verify`.
#[derive(Debug, Clone, PartialEq, Eq, Args)]
pub struct PrefetchVerifyArgs {
    #[command(flatten)]
    pub options: PrefetchOptions,
}
