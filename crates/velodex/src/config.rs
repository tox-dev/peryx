//! Runtime configuration.
//!
//! Values resolve with the precedence `defaults < file < env < CLI`. Each source below the
//! defaults is a [`PartialConfig`] (every field optional) that overlays the ones before it, so the
//! merge is a pure function and the precedence is unit-testable without touching the environment.

use std::path::PathBuf;

use serde::Deserialize;
use velodex_format::Ecosystem;
use velodex_http::rate_limit::{DEFAULT_UPSTREAM_CONCURRENCY, RateLimitConfig, RouteLimit};
use velodex_policy::PolicyConfig;

/// A fully resolved configuration.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Config {
    pub host: String,
    pub port: u16,
    pub data_dir: PathBuf,
    /// Disable upstream network access and serve only cached mirror data.
    pub offline: bool,
    /// Fallback freshness for cached simple pages, in seconds. Upstream `Cache-Control` lifetimes
    /// take precedence; this applies only when the server granted none.
    pub cache_ttl_secs: i64,
    /// The configured indexes: caches, hosted stores, and virtual indexes that compose them.
    pub indexes: Vec<IndexConfig>,
    pub log: LogConfig,
    pub rate_limit: RateLimitConfig,
}

/// One configured index, addressed at `route`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct IndexConfig {
    /// Identifier other indexes reference in their `layers`.
    pub name: String,
    /// URL prefix the index is served under, for example `root/pypi`.
    pub route: String,
    /// The package ecosystem this index serves. Immutable once created.
    pub ecosystem: Ecosystem,
    pub kind: IndexKind,
    pub policy: PolicyConfig,
    pub webhooks: Vec<WebhookConfig>,
}

/// The three composable index roles: a read-through proxy, a writable hosted store, or a virtual
/// index that aggregates other indexes under one route.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum IndexKind {
    /// Cache an upstream simple index, fetching on demand.
    Cached {
        upstream: String,
        username: Option<String>,
        password: Option<String>,
        /// Bearer token; takes precedence over username/password.
        token: Option<String>,
        /// Concurrent upstream fetches allowed for this cached index in this process; `0` disables the cap.
        upstream_concurrency: usize,
        /// Serve only cached data for this index.
        offline: bool,
        /// Optional package set and artifact filters for `velodex mirror`.
        prefetch: Box<MirrorPrefetchConfig>,
    },
    /// A hosted store that accepts uploads. `upload_token` is the Basic-auth password an upload must
    /// present (`None` disables uploads); `volatile` allows delete and overwrite.
    Hosted {
        upload_token: Option<String>,
        volatile: bool,
    },
    /// An ordered aggregation of other indexes (its members, by name, in `layers`). Resolution merges
    /// members first-match; a file in an earlier member shadows a later one. Uploads target `upload`.
    Virtual {
        layers: Vec<String>,
        upload: Option<String>,
    },
}

/// Mirror prefetch behavior configured under `[index.prefetch]`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MirrorPrefetchConfig {
    pub mode: MirrorPrefetchMode,
    pub packages: Vec<String>,
    pub requirements: Vec<PathBuf>,
    pub include_wheels: bool,
    pub include_sdists: bool,
    pub python_tags: Vec<String>,
    pub abi_tags: Vec<String>,
    pub platform_tags: Vec<String>,
    pub max_file_size_bytes: Option<u64>,
    pub metadata_only: bool,
}

impl Default for MirrorPrefetchConfig {
    fn default() -> Self {
        Self {
            mode: MirrorPrefetchMode::Selected,
            packages: Vec::new(),
            requirements: Vec::new(),
            include_wheels: true,
            include_sdists: true,
            python_tags: Vec::new(),
            abi_tags: Vec::new(),
            platform_tags: Vec::new(),
            max_file_size_bytes: None,
            metadata_only: false,
        }
    }
}

/// Which projects `velodex mirror` selects before artifact filters apply.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, clap::ValueEnum)]
#[serde(rename_all = "kebab-case")]
pub enum MirrorPrefetchMode {
    All,
    Selected,
    MetadataOnly,
}

#[derive(Debug, Default, Clone, PartialEq, Eq, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct RawMirrorPrefetch {
    pub mode: Option<MirrorPrefetchMode>,
    pub packages: Option<Vec<String>>,
    pub requirements: Option<Vec<PathBuf>>,
    pub include_wheels: Option<bool>,
    pub include_sdists: Option<bool>,
    pub python_tags: Option<Vec<String>>,
    pub abi_tags: Option<Vec<String>>,
    pub platform_tags: Option<Vec<String>>,
    pub max_file_size_bytes: Option<u64>,
    pub metadata_only: Option<bool>,
}

impl RawMirrorPrefetch {
    #[must_use]
    pub fn resolve(self) -> MirrorPrefetchConfig {
        let mode = self.mode.unwrap_or(MirrorPrefetchMode::Selected);
        MirrorPrefetchConfig {
            mode,
            packages: self.packages.unwrap_or_default(),
            requirements: self.requirements.unwrap_or_default(),
            include_wheels: self.include_wheels.unwrap_or(true),
            include_sdists: self.include_sdists.unwrap_or(true),
            python_tags: self.python_tags.unwrap_or_default(),
            abi_tags: self.abi_tags.unwrap_or_default(),
            platform_tags: self.platform_tags.unwrap_or_default(),
            max_file_size_bytes: self.max_file_size_bytes,
            metadata_only: self
                .metadata_only
                .unwrap_or(matches!(mode, MirrorPrefetchMode::MetadataOnly)),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WebhookConfig {
    pub name: String,
    pub url: String,
    pub secret: WebhookSecret,
    pub events: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum WebhookSecret {
    Literal(String),
    Env(String),
}

impl Default for Config {
    fn default() -> Self {
        Self {
            host: "127.0.0.1".to_owned(),
            port: 4433,
            data_dir: PathBuf::from("velodex-data"),
            offline: false,
            cache_ttl_secs: 300,
            indexes: default_indexes(),
            log: LogConfig::default(),
            rate_limit: RateLimitConfig::default(),
        }
    }
}

/// The out-of-the-box topology: a pypi.org mirror with a local store overlaid in front of it, served
/// together at `root/pypi`. Uploads to `root/pypi` land in the local layer once a token is set.
fn default_indexes() -> Vec<IndexConfig> {
    vec![
        IndexConfig {
            name: "pypi".to_owned(),
            route: "pypi".to_owned(),
            ecosystem: Ecosystem::Pypi,
            policy: PolicyConfig::default(),
            webhooks: Vec::new(),
            kind: IndexKind::Cached {
                upstream: "https://pypi.org/simple/".to_owned(),
                username: None,
                password: None,
                token: None,
                upstream_concurrency: DEFAULT_UPSTREAM_CONCURRENCY,
                offline: false,
                prefetch: Box::default(),
            },
        },
        IndexConfig {
            name: "local".to_owned(),
            route: "local".to_owned(),
            ecosystem: Ecosystem::Pypi,
            policy: PolicyConfig::default(),
            webhooks: Vec::new(),
            kind: IndexKind::Hosted {
                upload_token: None,
                volatile: true,
            },
        },
        IndexConfig {
            name: "root/pypi".to_owned(),
            route: "root/pypi".to_owned(),
            ecosystem: Ecosystem::Pypi,
            policy: PolicyConfig::default(),
            webhooks: Vec::new(),
            kind: IndexKind::Virtual {
                layers: vec!["local".to_owned(), "pypi".to_owned()],
                upload: Some("local".to_owned()),
            },
        },
    ]
}

impl Config {
    /// Overlay a partial source on top of these values, returning the merged config.
    ///
    /// # Errors
    /// Returns [`ConfigError::Index`] if the partial defines indexes but one is not classifiable as a
    /// mirror, local, or overlay.
    pub fn apply(mut self, partial: PartialConfig) -> Result<Self, ConfigError> {
        if let Some(host) = partial.host {
            self.host = host;
        }
        if let Some(port) = partial.port {
            self.port = port;
        }
        if let Some(data_dir) = partial.data_dir {
            self.data_dir = data_dir;
        }
        if let Some(offline) = partial.offline {
            self.offline = offline;
        }
        if let Some(cache_ttl_secs) = partial.cache_ttl_secs {
            self.cache_ttl_secs = cache_ttl_secs;
        }
        if let Some(raw) = partial.indexes {
            self.indexes = raw.into_iter().map(classify_index).collect::<Result<_, _>>()?;
        }
        self.log = self.log.apply(partial.log);
        self.rate_limit = apply_rate_limit(self.rate_limit, partial.rate_limit);
        Ok(self)
    }
}

/// Turn a raw `[[index]]` table into a classified [`IndexConfig`]: `layers` makes an overlay, else
/// `mirror` makes a mirror, else `local`/`upload_token` makes a local store.
fn classify_index(raw: RawIndex) -> Result<IndexConfig, ConfigError> {
    let route = raw.route.clone().unwrap_or_else(|| raw.name.clone());
    let ecosystem = match &raw.ecosystem {
        Some(value) => value.parse().map_err(|_| ConfigError::Index {
            name: raw.name.clone(),
            reason: "unknown ecosystem",
        })?,
        None => Ecosystem::default(),
    };
    let kind = if let Some(layers) = raw.layers {
        IndexKind::Virtual {
            layers,
            upload: raw.upload,
        }
    } else if let Some(upstream) = raw.cached {
        IndexKind::Cached {
            upstream,
            username: raw.username,
            password: raw.password,
            token: raw.token,
            upstream_concurrency: raw.upstream_concurrency.unwrap_or(DEFAULT_UPSTREAM_CONCURRENCY),
            offline: raw.offline.unwrap_or(false),
            prefetch: Box::new(raw.prefetch.unwrap_or_default().resolve()),
        }
    } else if raw.hosted == Some(true) || raw.upload_token.is_some() {
        IndexKind::Hosted {
            upload_token: raw.upload_token,
            volatile: raw.volatile.unwrap_or(true),
        }
    } else {
        return Err(ConfigError::Index {
            name: raw.name,
            reason: "index needs one of `cached`, `hosted`, or `layers`",
        });
    };
    Ok(IndexConfig {
        name: raw.name,
        route,
        ecosystem,
        kind,
        policy: raw.policy,
        webhooks: raw
            .webhooks
            .into_iter()
            .map(classify_webhook)
            .collect::<Result<_, _>>()?,
    })
}

fn classify_webhook(raw: RawWebhook) -> Result<WebhookConfig, ConfigError> {
    if raw.name.is_empty() {
        return Err(ConfigError::Webhook {
            name: raw.name,
            reason: "webhook name is required",
        });
    }
    if raw.url.is_empty() {
        return Err(ConfigError::Webhook {
            name: raw.name,
            reason: "webhook url is required",
        });
    }
    let secret = match (raw.secret, raw.secret_env) {
        (Some(secret), None) if !secret.is_empty() => WebhookSecret::Literal(secret),
        (None, Some(secret_env)) if !secret_env.is_empty() => WebhookSecret::Env(secret_env),
        _ => {
            return Err(ConfigError::Webhook {
                name: raw.name,
                reason: "webhook needs exactly one of `secret` or `secret_env`",
            });
        }
    };
    Ok(WebhookConfig {
        name: raw.name,
        url: raw.url,
        secret,
        events: raw.events,
    })
}

/// Logging configuration: level filter, output format, and sink.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LogConfig {
    /// A `tracing` `EnvFilter` directive, for example `info` or `velodex_upstream=debug`.
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

/// A configuration source with every field optional, used for the file and CLI overlays.
#[derive(Debug, Default, Clone, PartialEq, Eq, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct PartialConfig {
    pub host: Option<String>,
    pub port: Option<u16>,
    pub data_dir: Option<PathBuf>,
    pub offline: Option<bool>,
    pub cache_ttl_secs: Option<i64>,
    /// The `[[index]]` array from the TOML file. When present it replaces the default topology.
    #[serde(rename = "index")]
    pub indexes: Option<Vec<RawIndex>>,
    pub log: PartialLogConfig,
    pub rate_limit: PartialRateLimitConfig,
}

/// A raw `[[index]]` table before classification. Exactly one of `cached`, `hosted`, or `layers`
/// selects the kind; [`classify_index`] enforces that.
#[derive(Debug, Default, Clone, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RawIndex {
    pub name: String,
    pub route: Option<String>,
    pub ecosystem: Option<String>,
    #[serde(default)]
    pub policy: PolicyConfig,
    pub cached: Option<String>,
    pub username: Option<String>,
    pub password: Option<String>,
    pub token: Option<String>,
    pub upstream_concurrency: Option<usize>,
    pub offline: Option<bool>,
    pub prefetch: Option<RawMirrorPrefetch>,
    pub hosted: Option<bool>,
    pub upload_token: Option<String>,
    pub volatile: Option<bool>,
    pub layers: Option<Vec<String>>,
    pub upload: Option<String>,
    #[serde(default, rename = "webhook")]
    pub webhooks: Vec<RawWebhook>,
}

#[derive(Debug, Default, Clone, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RawWebhook {
    pub name: String,
    pub url: String,
    pub secret: Option<String>,
    pub secret_env: Option<String>,
    #[serde(default)]
    pub events: Vec<String>,
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

/// The rate-limit half of [`PartialConfig`].
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct PartialRateLimitConfig {
    pub enabled: Option<bool>,
    pub max_clients: Option<u64>,
    pub simple: PartialRouteLimit,
    pub metadata: PartialRouteLimit,
    pub artifact: PartialRouteLimit,
    pub upload: PartialRouteLimit,
    pub admin: PartialRouteLimit,
}

#[derive(Debug, Default, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct PartialRouteLimit {
    pub requests: Option<u64>,
    pub window_secs: Option<u64>,
}

const fn apply_rate_limit(mut base: RateLimitConfig, partial: PartialRateLimitConfig) -> RateLimitConfig {
    if let Some(enabled) = partial.enabled {
        base.enabled = enabled;
    }
    if let Some(max_clients) = partial.max_clients {
        base.max_clients = max_clients;
    }
    base.simple = apply_route_limit(base.simple, partial.simple);
    base.metadata = apply_route_limit(base.metadata, partial.metadata);
    base.artifact = apply_route_limit(base.artifact, partial.artifact);
    base.upload = apply_route_limit(base.upload, partial.upload);
    base.admin = apply_route_limit(base.admin, partial.admin);
    base
}

const fn apply_route_limit(mut base: RouteLimit, partial: PartialRouteLimit) -> RouteLimit {
    if let Some(requests) = partial.requests {
        base.requests = requests;
    }
    if let Some(window_secs) = partial.window_secs {
        base.window_secs = window_secs;
    }
    base
}

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
}

/// Parse a TOML document into a [`PartialConfig`].
///
/// # Errors
/// Returns [`ConfigError::Parse`] when `text` is not valid TOML for the schema. `path` is used only
/// for the error message.
pub fn from_toml(path: PathBuf, text: &str) -> Result<PartialConfig, ConfigError> {
    toml::from_str(text).map_err(|source| ConfigError::Parse { path, source })
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
