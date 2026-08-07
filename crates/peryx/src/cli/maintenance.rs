//! Bootstrap, backup, restore, import, policy, and self-management commands.

use std::path::PathBuf;

use clap::{ArgGroup, Args, Subcommand};

use super::RuntimeArgs;

/// Options for creating the first local administrator.
#[derive(Debug, Clone, PartialEq, Eq, Args)]
#[command(group(
    ArgGroup::new("password_source")
        .required(true)
        .multiple(false)
        .args(["password_stdin", "password_file"])
))]
pub struct BootstrapAdministratorArgs {
    #[command(flatten)]
    pub runtime: RuntimeArgs,

    /// Display name for the administrator.
    pub display_name: String,

    /// Read the password from standard input.
    #[arg(long)]
    pub password_stdin: bool,

    /// Read the password from a secret file.
    #[arg(long, value_name = "PATH")]
    pub password_file: Option<PathBuf>,
}

/// Digest revocation management over the administrator HTTP API.
#[derive(Debug, Clone, PartialEq, Eq, Subcommand)]
pub enum RevocationCommand {
    /// Create, retry, or reopen a digest revocation.
    Put(PutRevocationArgs),
    /// Inspect one digest revocation.
    Inspect(InspectRevocationArgs),
    /// List current digest revocation records.
    List(ListRevocationsArgs),
    /// Lift one digest revocation without changing package visibility state.
    Lift(LiftRevocationArgs),
}

impl RevocationCommand {
    #[must_use]
    pub const fn client(&self) -> &AdministratorClientArgs {
        match self {
            Self::Put(args) => &args.client,
            Self::Inspect(args) => &args.client,
            Self::List(args) => &args.client,
            Self::Lift(args) => &args.client,
        }
    }
}

/// Authentication and server options shared by administrator HTTP commands.
#[derive(Debug, Clone, PartialEq, Eq, Args)]
#[command(group(
    ArgGroup::new("administrator_password_source")
        .required(true)
        .multiple(false)
        .args(["password_stdin", "password_file"])
))]
pub struct AdministratorClientArgs {
    /// peryx server URL. HTTP is accepted only for a loopback server.
    #[arg(long, value_name = "URL")]
    pub server: String,

    /// Local administrator display name.
    #[arg(long)]
    pub user: String,

    /// Read the administrator password from standard input.
    #[arg(long)]
    pub password_stdin: bool,

    /// Read the administrator password from a secret file.
    #[arg(long, value_name = "PATH")]
    pub password_file: Option<PathBuf>,
}

/// Options for putting an active revocation.
#[derive(Debug, Clone, PartialEq, Eq, Args)]
pub struct PutRevocationArgs {
    #[command(flatten)]
    pub client: AdministratorClientArgs,
    /// Canonical `sha256:<hex>` artifact digest.
    pub digest: String,
    /// Incident reason stored with the revocation.
    #[arg(long)]
    pub reason: String,
}

/// Options for inspecting one revocation.
#[derive(Debug, Clone, PartialEq, Eq, Args)]
pub struct InspectRevocationArgs {
    #[command(flatten)]
    pub client: AdministratorClientArgs,
    /// Canonical `sha256:<hex>` artifact digest.
    pub digest: String,
}

/// Options for listing revocations.
#[derive(Debug, Clone, PartialEq, Eq, Args)]
pub struct ListRevocationsArgs {
    #[command(flatten)]
    pub client: AdministratorClientArgs,
    /// Include only active or lifted records.
    #[arg(long, value_enum)]
    pub status: Option<RevocationStatusArg>,
    /// Resume after this canonical digest.
    #[arg(long)]
    pub cursor: Option<String>,
    /// Number of records to return, from 1 through 100.
    #[arg(long)]
    pub limit: Option<usize>,
}

/// Options for lifting one revocation.
#[derive(Debug, Clone, PartialEq, Eq, Args)]
pub struct LiftRevocationArgs {
    #[command(flatten)]
    pub client: AdministratorClientArgs,
    /// Canonical `sha256:<hex>` artifact digest.
    pub digest: String,
}

/// A revocation lifecycle filter.
#[derive(Debug, Clone, Copy, PartialEq, Eq, clap::ValueEnum)]
#[value(rename_all = "lowercase")]
pub enum RevocationStatusArg {
    Active,
    Lifted,
}

impl RevocationStatusArg {
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Active => "active",
            Self::Lifted => "lifted",
        }
    }
}

/// Single-writer failover commands.
#[derive(Debug, Clone, PartialEq, Eq, Subcommand)]
pub enum WriterCommand {
    /// Replace the configured writer identity after stopping the active writer.
    Promote(WriterPromoteArgs),
    /// Claim the configured writer identity offline, seeding a replica's store before it starts.
    Claim(WriterClaimArgs),
}

impl WriterCommand {
    #[must_use]
    pub const fn runtime_args(&self) -> &RuntimeArgs {
        match self {
            Self::Promote(args) => &args.runtime,
            Self::Claim(args) => &args.runtime,
        }
    }
}

/// Options for manual writer promotion.
#[derive(Debug, Clone, PartialEq, Eq, Args)]
pub struct WriterPromoteArgs {
    #[command(flatten)]
    pub runtime: RuntimeArgs,

    /// Identity that will become the writer.
    pub replacement: String,
}

/// Options for seeding a replica with the writer identity it follows.
#[derive(Debug, Clone, PartialEq, Eq, Args)]
pub struct WriterClaimArgs {
    #[command(flatten)]
    pub runtime: RuntimeArgs,
}

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

    /// Directory containing artifacts accepted by the index's ecosystem.
    pub dir: PathBuf,
}
