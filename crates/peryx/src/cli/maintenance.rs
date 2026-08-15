use std::path::PathBuf;

use clap::{ArgGroup, Args, Subcommand};

use super::RuntimeArgs;

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

#[derive(Debug, Clone, PartialEq, Eq, Subcommand)]
pub enum RevocationCommand {
    /// Create, retry, or reopen a digest revocation.
    Put(PutRevocationArgs),
    /// Inspect one digest revocation.
    Inspect(InspectRevocationArgs),
    /// List current digest revocation records.
    List(ListRevocationsArgs),
    /// Lift one digest revocation without changing resource visibility.
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

#[derive(Debug, Clone, PartialEq, Eq, Args)]
#[command(group(
    ArgGroup::new("administrator_password_source")
        .required(true)
        .multiple(false)
        .args(["password_stdin", "password_file"])
))]
pub struct AdministratorClientArgs {
    /// peryx server URL. HTTP requires a loopback server.
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

#[derive(Debug, Clone, PartialEq, Eq, Args)]
pub struct InspectRevocationArgs {
    #[command(flatten)]
    pub client: AdministratorClientArgs,
    /// Canonical `sha256:<hex>` artifact digest.
    pub digest: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Args)]
pub struct ListRevocationsArgs {
    #[command(flatten)]
    pub client: AdministratorClientArgs,
    /// Filter by active or lifted status.
    #[arg(long, value_enum)]
    pub status: Option<RevocationStatusArg>,
    /// Resume after this canonical digest.
    #[arg(long)]
    pub cursor: Option<String>,
    /// Number of records to return, from 1 through 100.
    #[arg(long)]
    pub limit: Option<usize>,
}

#[derive(Debug, Clone, PartialEq, Eq, Args)]
pub struct LiftRevocationArgs {
    #[command(flatten)]
    pub client: AdministratorClientArgs,
    /// Canonical `sha256:<hex>` artifact digest.
    pub digest: String,
}

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

#[derive(Debug, Clone, PartialEq, Eq, Args)]
pub struct WriterPromoteArgs {
    #[command(flatten)]
    pub runtime: RuntimeArgs,

    /// Identity that will become the writer.
    pub replacement: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Args)]
pub struct WriterClaimArgs {
    #[command(flatten)]
    pub runtime: RuntimeArgs,
}

#[derive(Debug, Clone, PartialEq, Eq, Subcommand)]
pub enum PolicyCommand {
    /// Report cached resources and artifacts the configured policy would block.
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

#[derive(Debug, Clone, PartialEq, Eq, Args)]
pub struct PolicyDryRunArgs {
    #[command(flatten)]
    pub runtime: RuntimeArgs,

    /// Filter by index name or route.
    #[arg(long)]
    pub index: Option<String>,

    /// Filter by resource.
    #[arg(long)]
    pub resource: Option<String>,
}

#[cfg(feature = "self-update")]
#[derive(Debug, Clone, Copy, PartialEq, Eq, Subcommand)]
pub enum SelfCommand {
    /// Update peryx to the latest release.
    Update,
}

#[derive(Debug, Clone, PartialEq, Eq, Subcommand)]
pub enum BackupCommand {
    /// Create a full backup directory.
    Create(BackupCreateArgs),
    /// Verify a backup directory.
    Verify(BackupVerifyArgs),
}

#[derive(Debug, Clone, PartialEq, Eq, Args)]
pub struct BackupCreateArgs {
    #[command(flatten)]
    pub runtime: RuntimeArgs,

    /// Backup directory to create.
    pub path: PathBuf,
}

#[derive(Debug, Clone, PartialEq, Eq, Args)]
pub struct BackupVerifyArgs {
    /// Backup directory to verify.
    pub path: PathBuf,
}

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

#[derive(Debug, Clone, PartialEq, Eq, Args)]
pub struct ImportDirArgs {
    #[command(flatten)]
    pub runtime: RuntimeArgs,

    /// Hosted index name or route.
    pub index: String,

    /// Directory containing artifacts accepted by the index's ecosystem.
    pub dir: PathBuf,
}
