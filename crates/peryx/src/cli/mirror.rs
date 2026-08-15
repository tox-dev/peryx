use clap::{Args, Subcommand};

use super::RuntimeArgs;

#[derive(Debug, Clone, PartialEq, Eq, Subcommand)]
pub enum PrefetchCommand {
    /// Print the selected subjects and artifacts without writing cache entries.
    Plan(PrefetchPlanArgs),
    /// Fetch selected listings, metadata, and artifacts.
    Sync(PrefetchSyncArgs),
    /// Check cached listings, metadata, and artifacts for a prefetch set.
    Verify(PrefetchVerifyArgs),
}

impl PrefetchCommand {
    #[must_use]
    pub const fn runtime_args(&self) -> &RuntimeArgs {
        &self.options().runtime
    }

    #[must_use]
    pub const fn options(&self) -> &PrefetchOptions {
        match self {
            Self::Plan(args) => &args.options,
            Self::Sync(args) => &args.options,
            Self::Verify(args) => &args.options,
        }
    }
}

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

#[derive(Debug, Clone, PartialEq, Eq, Args)]
pub struct PrefetchPlanArgs {
    #[command(flatten)]
    pub options: PrefetchOptions,
}

#[derive(Debug, Clone, PartialEq, Eq, Args)]
pub struct PrefetchSyncArgs {
    #[command(flatten)]
    pub options: PrefetchOptions,
}

#[derive(Debug, Clone, PartialEq, Eq, Args)]
pub struct PrefetchVerifyArgs {
    #[command(flatten)]
    pub options: PrefetchOptions,
}
