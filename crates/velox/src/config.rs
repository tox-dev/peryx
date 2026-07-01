//! Runtime configuration.
//!
//! Values resolve with the precedence `defaults < file < env < CLI`. Each source below the
//! defaults is a [`PartialConfig`] (every field optional) that overlays the ones before it, so the
//! merge is a pure function and the precedence is unit-testable without touching the environment.

use std::path::PathBuf;

use serde::Deserialize;

/// A fully resolved configuration.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Config {
    pub host: String,
    pub port: u16,
    pub data_dir: PathBuf,
    pub upstream_url: String,
    /// Username for the upstream mirror (Basic auth), for example `__token__` for pypi.org tokens.
    pub upstream_username: Option<String>,
    pub upstream_password: Option<String>,
    /// Bearer token for the upstream mirror (Artifactory/GitLab access tokens); takes precedence
    /// over username/password.
    pub upstream_token: Option<String>,
    /// The route prefix of the mirror index, for example `root/pypi`.
    pub index: String,
    /// How long a cached simple page is served before revalidating, in seconds.
    pub cache_ttl_secs: i64,
    pub log: LogConfig,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            host: "127.0.0.1".to_owned(),
            port: 4433,
            data_dir: PathBuf::from("velox-data"),
            upstream_url: "https://pypi.org/simple/".to_owned(),
            upstream_username: None,
            upstream_password: None,
            upstream_token: None,
            index: "root/pypi".to_owned(),
            cache_ttl_secs: 1800,
            log: LogConfig::default(),
        }
    }
}

impl Config {
    /// Overlay a partial source on top of these values, returning the merged config.
    #[must_use]
    pub fn apply(mut self, partial: PartialConfig) -> Self {
        if let Some(host) = partial.host {
            self.host = host;
        }
        if let Some(port) = partial.port {
            self.port = port;
        }
        if let Some(data_dir) = partial.data_dir {
            self.data_dir = data_dir;
        }
        if let Some(upstream_url) = partial.upstream_url {
            self.upstream_url = upstream_url;
        }
        if partial.upstream_username.is_some() {
            self.upstream_username = partial.upstream_username;
        }
        if partial.upstream_password.is_some() {
            self.upstream_password = partial.upstream_password;
        }
        if partial.upstream_token.is_some() {
            self.upstream_token = partial.upstream_token;
        }
        if let Some(index) = partial.index {
            self.index = index;
        }
        if let Some(cache_ttl_secs) = partial.cache_ttl_secs {
            self.cache_ttl_secs = cache_ttl_secs;
        }
        self.log = self.log.apply(partial.log);
        self
    }
}

/// Logging configuration: level filter, output format, and sink.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LogConfig {
    /// A `tracing` `EnvFilter` directive, for example `info` or `velox_upstream=debug`.
    pub level: String,
    pub format: LogFormat,
    pub sink: LogSink,
    /// Target path when `sink` is [`LogSink::File`].
    pub file: Option<PathBuf>,
}

impl Default for LogConfig {
    fn default() -> Self {
        Self {
            level: "info".to_owned(),
            format: LogFormat::Pretty,
            sink: LogSink::Stdout,
            file: None,
        }
    }
}

impl LogConfig {
    #[must_use]
    pub fn apply(mut self, partial: PartialLogConfig) -> Self {
        if let Some(level) = partial.level {
            self.level = level;
        }
        if let Some(format) = partial.format {
            self.format = format;
        }
        if let Some(sink) = partial.sink {
            self.sink = sink;
        }
        if partial.file.is_some() {
            self.file = partial.file;
        }
        self
    }
}

/// How log lines are rendered.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, clap::ValueEnum)]
#[serde(rename_all = "lowercase")]
pub enum LogFormat {
    /// Human-readable, for a terminal.
    Pretty,
    /// One JSON object per line, for log aggregation.
    Json,
}

/// Where log lines go.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, clap::ValueEnum)]
#[serde(rename_all = "lowercase")]
pub enum LogSink {
    Stdout,
    File,
    Journald,
    Syslog,
}

/// A configuration source with every field optional, used for file, env, and CLI overlays.
#[derive(Debug, Default, Clone, PartialEq, Eq, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct PartialConfig {
    pub host: Option<String>,
    pub port: Option<u16>,
    pub data_dir: Option<PathBuf>,
    pub upstream_url: Option<String>,
    pub upstream_username: Option<String>,
    pub upstream_password: Option<String>,
    pub upstream_token: Option<String>,
    pub index: Option<String>,
    pub cache_ttl_secs: Option<i64>,
    pub log: PartialLogConfig,
}

/// The logging half of [`PartialConfig`].
#[derive(Debug, Default, Clone, PartialEq, Eq, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct PartialLogConfig {
    pub level: Option<String>,
    pub format: Option<LogFormat>,
    pub sink: Option<LogSink>,
    pub file: Option<PathBuf>,
}

/// An error while assembling configuration.
#[derive(Debug, thiserror::Error)]
pub enum ConfigError {
    #[error("failed to read config file {path}: {source}")]
    Read { path: PathBuf, source: std::io::Error },
    #[error("failed to parse config file {path}: {source}")]
    Parse { path: PathBuf, source: toml::de::Error },
    #[error("environment variable {key} has an invalid value {value:?}: {reason}")]
    Env { key: String, value: String, reason: String },
}

/// Parse a TOML document into a [`PartialConfig`].
///
/// # Errors
/// Returns [`ConfigError::Parse`] when `text` is not valid TOML for the schema. `path` is used only
/// for the error message.
pub fn from_toml(path: PathBuf, text: &str) -> Result<PartialConfig, ConfigError> {
    toml::from_str(text).map_err(|source| ConfigError::Parse { path, source })
}

/// Build a [`PartialConfig`] from environment variables given as `(key, value)` pairs.
///
/// Callers inject them in tests; recognized keys are prefixed `VELOX_`. Taking a concrete `Vec`
/// keeps this a single instantiation, which callers (including `main`) share.
///
/// # Errors
/// Returns [`ConfigError::Env`] when a value fails to parse, for example a non-numeric `VELOX_PORT`.
pub fn from_env(vars: Vec<(String, String)>) -> Result<PartialConfig, ConfigError> {
    let mut partial = PartialConfig::default();
    for (key, value) in vars {
        match key.as_str() {
            "VELOX_HOST" => partial.host = Some(value),
            "VELOX_PORT" => {
                let port = value.parse().map_err(|err: std::num::ParseIntError| ConfigError::Env {
                    key,
                    value,
                    reason: err.to_string(),
                })?;
                partial.port = Some(port);
            }
            "VELOX_DATA_DIR" => partial.data_dir = Some(PathBuf::from(value)),
            "VELOX_UPSTREAM_URL" => partial.upstream_url = Some(value),
            "VELOX_UPSTREAM_USERNAME" => partial.upstream_username = Some(value),
            "VELOX_UPSTREAM_PASSWORD" => partial.upstream_password = Some(value),
            "VELOX_UPSTREAM_TOKEN" => partial.upstream_token = Some(value),
            "VELOX_INDEX" => partial.index = Some(value),
            "VELOX_CACHE_TTL_SECS" => {
                let ttl = value.parse().map_err(|err: std::num::ParseIntError| ConfigError::Env {
                    key,
                    value,
                    reason: err.to_string(),
                })?;
                partial.cache_ttl_secs = Some(ttl);
            }
            "VELOX_LOG_LEVEL" => partial.log.level = Some(value),
            _ => {}
        }
    }
    Ok(partial)
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
