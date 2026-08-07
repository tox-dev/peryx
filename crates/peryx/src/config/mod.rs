//! Runtime configuration.
//!
//! Values resolve with the precedence `defaults < file < env < CLI`. Each source below the
//! defaults is a [`PartialConfig`] (every field optional) that overlays the ones before it, so the
//! merge is a pure function and the precedence is unit-testable without touching the environment.

mod load;
mod merge;
mod model;
mod raw;
mod repository_migration;

use std::path::PathBuf;

#[cfg(test)]
pub(crate) use load::from_env_source;
pub use load::{from_env, from_file, from_toml};
#[cfg(test)]
pub(crate) use merge::classify_tls;
pub use model::{
    AcmeConfig, AuthConfig, AvailabilityConfig, AvailabilityListenerConfig, AvailabilityListenerTls, AvailabilityMode,
    BlobStorageConfig, Config, CredentialFailureMode, CredentialRefreshConfig, DEFAULT_REPLICA_PAGE_SIZE,
    DEFAULT_REPLICA_POLL_INTERVAL_SECS, DEFAULT_WRITE_ACK_DEADLINE_SECS, DcMember, DcMembership, DcRole, IndexConfig,
    IndexKind, JobsConfig, JobsMode, LdapBindConfig, LdapProviderConfig, LogConfig, LogFormat, LogSink,
    OidcProviderConfig, PrefetchConfig, ReplicationConfig, S3StorageConfig, SecretSource, TlsConfig, TokenConfig,
    TrustedPublisherConfig, UpstreamConfig, UpstreamRoutingConfig, UpstreamTlsConfig, WebhookConfig, WebhookSecret,
    WriteAckConfig,
};
pub use raw::{
    PartialAuthConfig, PartialConfig, PartialJobsConfig, PartialLogConfig, PartialRateLimitConfig, PartialRouteLimit,
    RawAcme, RawAvailability, RawBlobStorage, RawCredentialExec, RawDcMember, RawExternalGroupGrant, RawIndex,
    RawJobSchedule, RawLdapMode, RawLdapProvider, RawOidcProvider, RawPolicy, RawPrefetchConfig, RawReplication,
    RawTls, RawToken, RawTrustedPublisher, RawUpstream, RawWebhook,
};
pub use repository_migration::reconcile_configured_repositories;

/// An error while assembling configuration.
#[derive(Debug, thiserror::Error)]
pub enum ConfigError {
    #[error("failed to read config file {path}: {source}")]
    Read { path: PathBuf, source: std::io::Error },
    #[error("failed to parse config file {path}: {source}")]
    Parse { path: PathBuf, source: toml::de::Error },
    #[error("index {name}: {reason}")]
    Index { name: String, reason: &'static str },
    #[error("index {index}: token {name}: {reason}")]
    Token {
        index: String,
        name: String,
        reason: &'static str,
    },
    #[error("auth: {reason}")]
    Auth { reason: &'static str },
    #[error("trusted publisher {id}: {reason}")]
    TrustedPublisher { id: String, reason: &'static str },
    #[error("LDAP provider {id}: {reason}")]
    LdapProvider { id: String, reason: &'static str },
    #[error("OIDC provider {id}: {reason}")]
    OidcProvider { id: String, reason: &'static str },
    #[error("availability: {reason}")]
    Availability { reason: &'static str },
    #[error("datacenter membership: {reason}")]
    DcMembership { reason: String },
    #[error("replication: {reason}")]
    Replication { reason: &'static str },
    #[error("jobs schedule [{index}]: {reason}")]
    Jobs { index: usize, reason: &'static str },
    #[error("blob storage: {reason}")]
    Blob { reason: String },
    #[error(
        "blob storage durability: {mode} availability requires {shortfall}, which the configured backend cannot prove"
    )]
    Durability {
        mode: &'static str,
        shortfall: &'static str,
    },
    #[error("writer identity: {reason}")]
    WriterIdentity { reason: &'static str },
    #[error("secret file {path} holds no secret")]
    EmptySecret { path: PathBuf },
    #[error("secret file {path} exceeds the {limit}-byte limit")]
    OversizeSecret { path: PathBuf, limit: u64 },
    #[error("credential environment variable {var} is unset, empty, or not valid UTF-8")]
    EnvSecret { var: String },
    #[error("webhook {name}: {reason}")]
    Webhook { name: String, reason: &'static str },
    #[error("tls: {reason}")]
    Tls { reason: &'static str },
    #[error("invalid environment variable {var}: {reason}")]
    Env { var: &'static str, reason: String },
}
