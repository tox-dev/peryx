//! The `mirror` command group: plan, sync, and verify a cached index's prefetch working set.

use clap::{Args, Subcommand};

use super::RuntimeArgs;

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

    /// Override one ecosystem mirror setting as TOML `KEY=VALUE`.
    #[arg(long = "option", value_name = "KEY=VALUE")]
    pub overrides: Vec<String>,
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
