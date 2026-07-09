//! Backup, restore, import, policy, and self-management commands.

use std::path::PathBuf;

use clap::{Args, Subcommand};

use super::RuntimeArgs;

/// Index policy commands.
#[derive(Debug, Clone, PartialEq, Eq, Subcommand)]
pub enum PolicyCommand {
    /// Report cached projects and files the configured policy would block.
    DryRun(PolicyDryRunArgs),
}

impl PolicyCommand {
    #[must_use]
    pub const fn runtime_args(&self) -> &RuntimeArgs {
        match self {
            Self::DryRun(args) => &args.runtime,
        }
    }
}

/// Options for policy dry-run output.
#[derive(Debug, Clone, PartialEq, Eq, Args)]
pub struct PolicyDryRunArgs {
    #[command(flatten)]
    pub runtime: RuntimeArgs,

    /// Only report this index name or route.
    #[arg(long)]
    pub index: Option<String>,

    /// Only report this project.
    #[arg(long)]
    pub project: Option<String>,
}

/// Actions on the peryx installation itself.
#[cfg(feature = "self-update")]
#[derive(Debug, Clone, Copy, PartialEq, Eq, Subcommand)]
pub enum SelfCommand {
    /// Update peryx to the latest release.
    Update,
}

/// Offline backup commands.
#[derive(Debug, Clone, PartialEq, Eq, Subcommand)]
pub enum BackupCommand {
    /// Create a full backup directory.
    Create(BackupCreateArgs),
    /// Verify a backup directory.
    Verify(BackupVerifyArgs),
}

impl BackupCommand {
    #[must_use]
    pub const fn runtime_args(&self) -> Option<&RuntimeArgs> {
        match self {
            Self::Create(args) => Some(&args.runtime),
            Self::Verify(_) => None,
        }
    }
}

/// Options for backup creation.
#[derive(Debug, Clone, PartialEq, Eq, Args)]
pub struct BackupCreateArgs {
    #[command(flatten)]
    pub runtime: RuntimeArgs,

    /// Backup directory to create.
    pub path: PathBuf,
}

/// Options for backup verification.
#[derive(Debug, Clone, PartialEq, Eq, Args)]
pub struct BackupVerifyArgs {
    /// Backup directory to verify.
    pub path: PathBuf,
}

/// Options for restore.
#[derive(Debug, Clone, PartialEq, Eq, Args)]
pub struct RestoreArgs {
    /// Backup directory to restore from.
    pub path: PathBuf,

    /// Data directory to write.
    #[arg(long)]
    pub data_dir: PathBuf,

    /// Replace a non-empty target data directory.
    #[arg(long)]
    pub force: bool,
}

/// Options for directory import.
#[derive(Debug, Clone, PartialEq, Eq, Args)]
pub struct ImportDirArgs {
    #[command(flatten)]
    pub runtime: RuntimeArgs,

    /// Hosted index name or route.
    pub index: String,

    /// Directory containing wheel or sdist files.
    pub dir: PathBuf,
}
