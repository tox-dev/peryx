//! The fully resolved configuration types and their defaults.

use std::path::PathBuf;

use peryx_ecosystem_pypi::policy::PypiPolicyConfig;
use peryx_format::Ecosystem;
use peryx_http::rate_limit::{DEFAULT_UPSTREAM_CONCURRENCY, RateLimitConfig};
use peryx_policy::PolicyConfig;
use serde::Deserialize;

/// A fully resolved configuration.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Config {
    pub host: String,
    pub port: u16,
    pub data_dir: PathBuf,
    /// Disable upstream network access and serve only cached data.
    pub offline: bool,
    /// Fallback freshness for cached simple pages, in seconds. Upstream `Cache-Control` lifetimes
    /// take precedence; this applies only when the server granted none.
    pub cache_ttl_secs: i64,
    /// The configured indexes: caches, hosted stores, and virtual indexes that compose them.
    pub indexes: Vec<IndexConfig>,
    /// How the server terminates TLS, or `None` for plain HTTP (the zero-config default, which
    /// docker/podman accept over loopback). Serving it costs nothing until set.
    pub tls: Option<TlsConfig>,
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
    /// The `PyPI`-specific policy keys, compiled into format rules for a `PyPI` index and ignored for
    /// any other ecosystem. Empty for a non-`PyPI` index.
    pub pypi_policy: PypiPolicyConfig,
    pub webhooks: Vec<WebhookConfig>,
}

/// The three composable index roles: a read-through cache, a writable hosted store, or a virtual
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
        /// Optional package set and artifact filters for `peryx prefetch`.
        prefetch: Box<PrefetchConfig>,
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

/// Prefetch behavior configured under `[index.prefetch]`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PrefetchConfig {
    pub mode: PrefetchMode,
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

impl Default for PrefetchConfig {
    fn default() -> Self {
        Self {
            mode: PrefetchMode::Selected,
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

/// Which projects `peryx prefetch` selects before artifact filters apply.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, clap::ValueEnum)]
#[serde(rename_all = "kebab-case")]
pub enum PrefetchMode {
    All,
    Selected,
    MetadataOnly,
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
            data_dir: PathBuf::from("peryx-data"),
            offline: false,
            cache_ttl_secs: 300,
            indexes: default_indexes(),
            tls: None,
            log: LogConfig::default(),
            rate_limit: RateLimitConfig::default(),
        }
    }
}

/// How the server obtains and serves its TLS certificate.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TlsConfig {
    /// Serve HTTPS from a PEM certificate chain and private key on disk.
    Manual { cert: PathBuf, key: PathBuf },
    /// Obtain and renew a certificate automatically from an ACME provider (Let's Encrypt), so a
    /// publicly reachable deployment serves trusted HTTPS with no manual certificate handling.
    Acme(AcmeConfig),
}

/// Automatic-certificate settings for an ACME (Let's Encrypt) deployment.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AcmeConfig {
    /// The domains to request a certificate for; the server must be reachable at these on port 443.
    pub domains: Vec<String>,
    /// The contact email the ACME account registers, for expiry notices.
    pub contact: String,
    /// Where issued certificates and the account key are cached between restarts.
    pub cache_dir: PathBuf,
    /// Use the provider's staging environment (higher rate limits, untrusted certs) while testing.
    pub staging: bool,
}

/// The out-of-the-box topology: one trio per ecosystem. For pypi, a pypi.org cache and a hosted
/// store combined by a virtual index at `root/pypi`; for oci, a Docker Hub cache and a hosted store
/// combined by a virtual index at `root/oci`. Uploads to a virtual index land in its hosted layer
/// once a token is set.
fn default_indexes() -> Vec<IndexConfig> {
    vec![
        IndexConfig {
            name: "pypi".to_owned(),
            route: "pypi".to_owned(),
            ecosystem: Ecosystem::Pypi,
            policy: PolicyConfig::default(),
            pypi_policy: PypiPolicyConfig::default(),
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
            name: "hosted".to_owned(),
            route: "hosted".to_owned(),
            ecosystem: Ecosystem::Pypi,
            policy: PolicyConfig::default(),
            pypi_policy: PypiPolicyConfig::default(),
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
            pypi_policy: PypiPolicyConfig::default(),
            webhooks: Vec::new(),
            kind: IndexKind::Virtual {
                layers: vec!["hosted".to_owned(), "pypi".to_owned()],
                upload: Some("hosted".to_owned()),
            },
        },
        IndexConfig {
            name: "dockerhub".to_owned(),
            route: "dockerhub".to_owned(),
            ecosystem: Ecosystem::Oci,
            policy: PolicyConfig::default(),
            pypi_policy: PypiPolicyConfig::default(),
            webhooks: Vec::new(),
            kind: IndexKind::Cached {
                upstream: "https://registry-1.docker.io".to_owned(),
                username: None,
                password: None,
                token: None,
                upstream_concurrency: DEFAULT_UPSTREAM_CONCURRENCY,
                offline: false,
                prefetch: Box::default(),
            },
        },
        IndexConfig {
            name: "images".to_owned(),
            route: "images".to_owned(),
            ecosystem: Ecosystem::Oci,
            policy: PolicyConfig::default(),
            pypi_policy: PypiPolicyConfig::default(),
            webhooks: Vec::new(),
            kind: IndexKind::Hosted {
                upload_token: None,
                volatile: true,
            },
        },
        IndexConfig {
            name: "root/oci".to_owned(),
            route: "root/oci".to_owned(),
            ecosystem: Ecosystem::Oci,
            policy: PolicyConfig::default(),
            pypi_policy: PypiPolicyConfig::default(),
            webhooks: Vec::new(),
            kind: IndexKind::Virtual {
                layers: vec!["images".to_owned(), "dockerhub".to_owned()],
                upload: Some("images".to_owned()),
            },
        },
    ]
}

/// Logging configuration: level filter, output format, and sink.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LogConfig {
    /// A `tracing` `EnvFilter` directive, for example `info` or `peryx_upstream=debug`.
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
