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
    AcmeConfig, Config, IndexConfig, IndexKind, LogConfig, LogFormat, LogSink, PrefetchConfig, PrefetchMode, TlsConfig,
    WebhookConfig, WebhookSecret,
};
pub use raw::{
    PartialConfig, PartialLogConfig, PartialRateLimitConfig, PartialRouteLimit, RawAcme, RawIndex, RawPolicy,
    RawPrefetchConfig, RawTls, RawWebhook,
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
    #[error("webhook {name}: {reason}")]
    Webhook { name: String, reason: &'static str },
    #[error("tls: {reason}")]
    Tls { reason: &'static str },
    #[error("invalid environment variable {var}: {reason}")]
    Env { var: &'static str, reason: String },
}
