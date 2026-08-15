use clap::{Args, Subcommand};

use super::RuntimeArgs;

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

#[derive(Debug, Clone, PartialEq, Eq, Args)]
pub struct IndexListArgs {
    #[command(flatten)]
    pub runtime: RuntimeArgs,

    /// Filter by ecosystem.
    #[arg(long)]
    pub ecosystem: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Args)]
pub struct IndexShowArgs {
    #[command(flatten)]
    pub runtime: RuntimeArgs,

    /// Configured index name or route.
    pub index: String,
}
