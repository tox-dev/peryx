//! Loading configuration from a TOML file and from `PERYX_*` environment variables.

use std::path::PathBuf;

use super::ConfigError;
use super::raw::{PartialAuthConfig, PartialConfig, PartialJobsConfig, PartialLogConfig, PartialRateLimitConfig};

/// Parse a TOML document into a [`PartialConfig`].
///
/// # Errors
/// Returns [`ConfigError::Parse`] when `text` is not valid TOML for the schema. `path` is used only
/// for the error message.
pub fn from_toml(path: PathBuf, text: &str) -> Result<PartialConfig, ConfigError> {
    toml::from_str(text).map_err(|source| ConfigError::Parse { path, source })
}

/// The overlay sourced from `PERYX_*` environment variables. This tier sits between file and CLI.
///
/// Only scalar settings are environment-configurable; the `[[index]]` topology and rate limits stay
/// file- and CLI-configured, since neither maps cleanly to flat variables.
///
/// # Errors
/// Returns [`ConfigError::Env`] when a variable holds a value its target type rejects (a `PORT` that
/// is not a `u16`, a `LOG_FORMAT` that names no known format, and so on).
pub fn from_env() -> Result<PartialConfig, ConfigError> {
    from_env_source(|var| std::env::var(var).ok())
}

pub fn from_env_source(get: impl Fn(&str) -> Option<String>) -> Result<PartialConfig, ConfigError> {
    let get = |var: &str| get(var).filter(|value| !value.is_empty());
    Ok(PartialConfig {
        host: get("PERYX_HOST"),
        port: parse_env(&get, "PERYX_PORT")?,
        data_dir: get("PERYX_DATA_DIR").map(PathBuf::from),
        writer_identity: get("PERYX_WRITER_IDENTITY"),
        offline: parse_env(&get, "PERYX_OFFLINE")?,
        read_only: parse_env(&get, "PERYX_READ_ONLY")?,
        cache_ttl_secs: parse_env(&get, "PERYX_CACHE_TTL_SECS")?,
        hot_cache_bytes: parse_env(&get, "PERYX_HOT_CACHE_BYTES")?,
        netrc: get("PERYX_NETRC").map(PathBuf::from),
        max_stale_secs: parse_env(&get, "PERYX_MAX_STALE_SECS")?,
        indexes: None,
        tls: None,
        acme: None,
        log: PartialLogConfig {
            level: get("PERYX_LOG_LEVEL"),
            format: parse_env_enum(&get, "PERYX_LOG_FORMAT")?,
            sink: parse_env_enum(&get, "PERYX_LOG_SINK")?,
            file: get("PERYX_LOG_FILE").map(PathBuf::from),
        },
        rate_limit: PartialRateLimitConfig::default(),
        auth: PartialAuthConfig::default(),
        replication: None,
        jobs: PartialJobsConfig::default(),
    })
}

fn parse_env<T>(get: &impl Fn(&str) -> Option<String>, var: &'static str) -> Result<Option<T>, ConfigError>
where
    T: std::str::FromStr,
    T::Err: std::fmt::Display,
{
    get(var)
        .map(|value| {
            value.parse::<T>().map_err(|err| ConfigError::Env {
                var,
                reason: err.to_string(),
            })
        })
        .transpose()
}

fn parse_env_enum<T: clap::ValueEnum>(
    get: &impl Fn(&str) -> Option<String>,
    var: &'static str,
) -> Result<Option<T>, ConfigError> {
    get(var)
        .map(|value| T::from_str(&value, true).map_err(|reason| ConfigError::Env { var, reason }))
        .transpose()
}

/// Read a config file from disk into a [`PartialConfig`].
///
/// # Errors
/// Returns [`ConfigError::Read`] if the file cannot be read and [`ConfigError::Parse`] if it is not
/// valid TOML.
pub fn from_file(path: PathBuf) -> Result<PartialConfig, ConfigError> {
    let text = std::fs::read_to_string(&path).map_err(|source| ConfigError::Read {
        path: path.clone(),
        source,
    })?;
    from_toml(path, &text)
}
