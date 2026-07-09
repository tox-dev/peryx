//! The `index` command group: list and inspect configured indexes.

use clap::{Args, Subcommand};

use super::{EcosystemArg, RuntimeArgs};

/// Inspect the configured indexes.
#[derive(Debug, Clone, PartialEq, Eq, Subcommand)]
pub enum IndexCommand {
    /// List the configured indexes.
    List(IndexListArgs),
    /// Show one index in detail.
    Show(IndexShowArgs),
}

impl IndexCommand {
    #[must_use]
    pub const fn runtime_args(&self) -> &RuntimeArgs {
        match self {
            Self::List(args) => &args.runtime,
            Self::Show(args) => &args.runtime,
        }
    }
}

/// Options for `velodex index list`.
#[derive(Debug, Clone, PartialEq, Eq, Args)]
pub struct IndexListArgs {
    #[command(flatten)]
    pub runtime: RuntimeArgs,

    /// Show only indexes of this ecosystem.
    #[arg(long, value_enum)]
    pub ecosystem: Option<EcosystemArg>,
}

/// Options for `velodex index show`.
#[derive(Debug, Clone, PartialEq, Eq, Args)]
pub struct IndexShowArgs {
    #[command(flatten)]
    pub runtime: RuntimeArgs,

    /// Configured index name or route.
    pub index: String,
}
