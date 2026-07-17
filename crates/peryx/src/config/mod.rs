//! Runtime configuration.
//!
//! Values resolve with the precedence `defaults < file < env < CLI`. Each source below the
//! defaults is a [`PartialConfig`] (every field optional) that overlays the ones before it, so the
//! merge is a pure function and the precedence is unit-testable without touching the environment.

mod load;
mod merge;
mod model;
mod raw;

use std::path::PathBuf;

#[cfg(test)]
pub(crate) use load::from_env_source;
pub use load::{from_env, from_file, from_toml};
#[cfg(test)]
pub(crate) use merge::classify_tls;
pub use model::{
    AcmeConfig, AuthConfig, Config, DEFAULT_REPLICA_PAGE_SIZE, DEFAULT_REPLICA_POLL_INTERVAL_SECS, IndexConfig,
    IndexKind, JobsConfig, JobsMode, LogConfig, LogFormat, LogSink, PrefetchConfig, PrefetchMode, ReplicationConfig,
    SecretSource, TlsConfig, TokenConfig, TrustedPublisherConfig, UpstreamConfig, UpstreamRoutingConfig,
    UpstreamTlsConfig, WebhookConfig, WebhookSecret,
};
pub use raw::{
    PartialAuthConfig, PartialConfig, PartialJobsConfig, PartialLogConfig, PartialRateLimitConfig, PartialRouteLimit,
    RawAcme, RawIndex, RawPolicy, RawPrefetchConfig, RawReplication, RawTls, RawToken, RawTrustedPublisher,
    RawUpstream, RawWebhook,
};

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
    #[error("replication: {reason}")]
    Replication { reason: &'static str },
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
