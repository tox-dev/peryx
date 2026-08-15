use clap::{Args, Subcommand};

use super::RuntimeArgs;

#[derive(Debug, Clone, PartialEq, Eq, Subcommand)]
pub enum ConfigCommand {
    /// Resolve every configuration source and validate the result before opening storage or network access.
    Check(ConfigCheckArgs),
}

impl ConfigCommand {
    #[must_use]
    pub const fn runtime_args(&self) -> &RuntimeArgs {
        match self {
            Self::Check(args) => &args.runtime,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Args)]
pub struct ConfigCheckArgs {
    #[command(flatten)]
    pub runtime: RuntimeArgs,
}
