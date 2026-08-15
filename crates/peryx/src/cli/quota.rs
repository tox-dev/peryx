use clap::{Args, Subcommand};

use super::RuntimeArgs;

#[derive(Debug, Clone, PartialEq, Eq, Subcommand)]
pub enum QuotaCommand {
    /// List every repository's quota as a table.
    List(QuotaListArgs),
    /// Inspect one repository's quota as JSON.
    Inspect(QuotaInspectArgs),
}

impl QuotaCommand {
    #[must_use]
    pub const fn runtime_args(&self) -> &RuntimeArgs {
        match self {
            Self::List(args) => &args.runtime,
            Self::Inspect(args) => &args.runtime,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Args)]
pub struct QuotaListArgs {
    #[command(flatten)]
    pub runtime: RuntimeArgs,
}

#[derive(Debug, Clone, PartialEq, Eq, Args)]
pub struct QuotaInspectArgs {
    #[command(flatten)]
    pub runtime: RuntimeArgs,

    /// Index name to inspect.
    #[arg(long)]
    pub index: String,
}
