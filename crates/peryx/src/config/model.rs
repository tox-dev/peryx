//! The fully resolved configuration types and their defaults.

use std::collections::BTreeSet;
use std::num::NonZeroUsize;
use std::path::PathBuf;
use std::time::Duration;

use peryx_core::Ecosystem;
use peryx_driver::rate_limit::{DEFAULT_UPSTREAM_CONCURRENCY, RateLimitConfig};
use peryx_http::{DEFAULT_HOT_CACHE_BYTES, DEFAULT_MAX_STALE_SECS};
use peryx_identity::{Action, Glob, Grant, IndexAcl, NamedToken};
use peryx_policy::PolicyConfig;
use serde::Deserialize;
use toml::Table;

use super::ConfigError;

/// A fully resolved configuration.
#[derive(Debug, Clone, PartialEq)]
pub struct Config {
    pub host: String,
    pub port: u16,
    pub data_dir: PathBuf,
    pub writer_identity: Option<String>,
    /// Disable upstream network access and serve only cached data.
    pub offline: bool,
    /// Reject client mutations and disable upstream cache fills on a read replica.
    pub read_only: bool,
    /// Fallback freshness for cached simple pages, in seconds. Upstream `Cache-Control` lifetimes
    /// take precedence; this applies only when the server granted none.
    pub cache_ttl_secs: i64,
    /// Byte budget for the transformed-page cache: memory traded against warm-serve speed. Pages in
    /// it are re-derivable from the cached raw page, so a smaller budget only lowers the warm-hit
    /// rate; `0` turns the cache off and every warm page pays its transform again.
    pub hot_cache_bytes: u64,
    /// Bound on stale-on-error serving, in seconds; `0` serves stale without limit.
    pub max_stale_secs: i64,
    /// The configured indexes: caches, hosted stores, and virtual indexes that compose them.
    pub indexes: Vec<IndexConfig>,
    /// How the server terminates TLS, or `None` for plain HTTP (the zero-config default, which
    /// docker/podman accept over loopback). Serving it costs nothing until set.
    pub tls: Option<TlsConfig>,
    pub log: LogConfig,
    pub rate_limit: RateLimitConfig,
    pub auth: AuthConfig,
    pub replication: Option<ReplicationConfig>,
}

pub const DEFAULT_REPLICA_PAGE_SIZE: usize = 100;
pub const DEFAULT_REPLICA_POLL_INTERVAL_SECS: u64 = 1;

/// The process role for replication.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ReplicationConfig {
    Primary {
        source: String,
        token: SecretSource,
    },
    Replica {
        upstream: String,
        token: SecretSource,
        poll_interval: Duration,
        page_size: NonZeroUsize,
    },
}

/// The `[auth]` table: the settings every index's access rules share.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AuthConfig {
    /// The key peryx signs its own tokens with. Unset leaves the token realm without a key.
    pub signing_key: Option<SecretSource>,
    /// How long a minted token stays valid, in seconds.
    pub token_ttl_secs: i64,
    /// What an index's `anonymous_read` defaults to. Set it to `false` to close a whole server's reads
    /// with one key instead of one per index.
    pub default_anonymous_read: bool,
}

impl Default for AuthConfig {
    fn default() -> Self {
        Self {
            signing_key: None,
            token_ttl_secs: 300,
            default_anonymous_read: true,
        }
    }
}

/// Where a secret's value comes from. A `*_file` sibling keeps the value out of the config file, so a
/// mounted Docker or Kubernetes secret, a systemd credential, or a Vault-rendered file can hold it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SecretSource {
    Literal(String),
    File(PathBuf),
}

impl SecretSource {
    /// The secret's value, reading the file when that is where it lives. Surrounding whitespace goes:
    /// a secret file written by `echo` or a Kubernetes mount ends in a newline that is not part of it.
    ///
    /// # Errors
    /// Returns [`ConfigError::Read`] when the file cannot be read and [`ConfigError::EmptySecret`] when
    /// it holds nothing but whitespace.
    pub fn read(&self) -> Result<String, ConfigError> {
        match self {
            Self::Literal(secret) => Ok(secret.clone()),
            Self::File(path) => {
                let secret = std::fs::read_to_string(path)
                    .map_err(|source| ConfigError::Read {
                        path: path.clone(),
                        source,
                    })?
                    .trim()
                    .to_owned();
                if secret.is_empty() {
                    return Err(ConfigError::EmptySecret { path: path.clone() });
                }
                Ok(secret)
            }
        }
    }
}

/// One named credential an index accepts, and what it may do there.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TokenConfig {
    pub name: String,
    pub secret: SecretSource,
    /// The project globs the token may act on; `*` covers the index.
    pub projects: Vec<String>,
    pub actions: BTreeSet<Action>,
    /// Unix seconds after which the token stops authenticating.
    pub expires_at: Option<i64>,
}

/// One configured index, addressed at `route`.
#[derive(Debug, Clone, PartialEq)]
pub struct IndexConfig {
    /// Identifier other indexes reference in their `layers`.
    pub name: String,
    /// URL prefix the index is served under, for example `root/pypi`.
    pub route: String,
    /// The package ecosystem this index serves. Immutable once created.
    pub ecosystem: Ecosystem,
    pub kind: IndexKind,
    /// Whether a request with no credential may read here. `None` takes the value of
    /// [`AuthConfig::default_anonymous_read`].
    pub anonymous_read: Option<bool>,
    /// The credentials this index accepts beyond the `upload_token` shorthand.
    pub tokens: Vec<TokenConfig>,
    pub policy: PolicyConfig,
    /// The `[policy]` keys the neutral engine did not claim, left raw for this index's ecosystem
    /// driver to compile into artifact rules. Empty when an operator set no ecosystem-specific policy.
    pub ecosystem_policy: Table,
    /// The `[index.settings]` table: this index's ecosystem-specific settings (an OCI cache's
    /// `library_prefix`), left raw for the composition root to compile against its ecosystem. Empty
    /// when an operator set none.
    pub ecosystem_settings: Table,
    pub webhooks: Vec<WebhookConfig>,
}

impl IndexConfig {
    /// The index's access rules, with every secret read from wherever it lives: the `upload_token`
    /// shorthand becomes one write-and-delete-everywhere credential, and each `[[index.token]]` its
    /// own named one.
    ///
    /// # Errors
    /// Returns [`ConfigError::Read`] when a secret file cannot be read and [`ConfigError::EmptySecret`]
    /// when one holds nothing: an empty secret would authenticate an empty password.
    pub fn acl(&self, auth: &AuthConfig) -> Result<IndexAcl, ConfigError> {
        let mut tokens = Vec::with_capacity(self.tokens.len() + 1);
        if let IndexKind::Hosted {
            upload_token: Some(source),
            ..
        } = &self.kind
        {
            tokens.push(NamedToken::upload(source.read()?));
        }
        for token in &self.tokens {
            tokens.push(NamedToken {
                name: token.name.clone(),
                secret: token.secret.read()?,
                grants: vec![Grant {
                    projects: token.projects.iter().cloned().map(Glob::new).collect(),
                    actions: token.actions.clone(),
                }],
                expires_at: token.expires_at,
            });
        }
        Ok(IndexAcl {
            anonymous_read: self.anonymous_read.unwrap_or(auth.default_anonymous_read),
            tokens,
        })
    }
}

/// The three composable index roles: a read-through cache, a writable hosted store, or a virtual
/// index that aggregates other indexes under one route.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum IndexKind {
    /// Cache an upstream simple index, fetching on demand.
    Cached {
        upstream: String,
        username: Option<String>,
        /// Upstream password; a `password_file` sibling keeps it out of the config file.
        password: Option<SecretSource>,
        /// Bearer token; takes precedence over username/password. A `token_file` sibling keeps it out
        /// of the config file.
        token: Option<SecretSource>,
        /// Concurrent upstream fetches allowed for this cached index in this process; `0` disables the cap.
        upstream_concurrency: usize,
        /// Serve only cached data for this index.
        offline: bool,
        /// Optional package set and artifact filters for `peryx prefetch`.
        prefetch: Box<PrefetchConfig>,
    },
    /// A hosted store that accepts uploads. `upload_token` is the shorthand for a single credential
    /// that writes and deletes everywhere here (`None` disables uploads unless `[[index.token]]`
    /// grants them); `volatile` allows delete and overwrite.
    Hosted {
        upload_token: Option<SecretSource>,
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
            writer_identity: None,
            offline: false,
            read_only: false,
            cache_ttl_secs: 300,
            hot_cache_bytes: DEFAULT_HOT_CACHE_BYTES,
            max_stale_secs: DEFAULT_MAX_STALE_SECS,
            indexes: default_indexes(),
            tls: None,
            log: LogConfig::default(),
            rate_limit: RateLimitConfig::default(),
            auth: AuthConfig::default(),
            replication: None,
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
    let cache = |upstream: &str| IndexKind::Cached {
        upstream: upstream.to_owned(),
        username: None,
        password: None,
        token: None,
        upstream_concurrency: DEFAULT_UPSTREAM_CONCURRENCY,
        offline: false,
        prefetch: Box::default(),
    };
    let store = || IndexKind::Hosted {
        upload_token: None,
        volatile: true,
    };
    let overlay = |hosted: &str, cached: &str| IndexKind::Virtual {
        layers: vec![hosted.to_owned(), cached.to_owned()],
        upload: Some(hosted.to_owned()),
    };
    vec![
        default_index("pypi", Ecosystem::Pypi, cache("https://pypi.org/simple/")),
        default_index("hosted", Ecosystem::Pypi, store()),
        default_index("root/pypi", Ecosystem::Pypi, overlay("hosted", "pypi")),
        default_index("dockerhub", Ecosystem::Oci, cache("https://registry-1.docker.io")),
        default_index("images", Ecosystem::Oci, store()),
        default_index("root/oci", Ecosystem::Oci, overlay("images", "dockerhub")),
    ]
}

/// One default index: served at its own name, with no policy, no webhooks, and no access rules beyond
/// the open reads every index starts with.
fn default_index(name: &str, ecosystem: Ecosystem, kind: IndexKind) -> IndexConfig {
    IndexConfig {
        name: name.to_owned(),
        route: name.to_owned(),
        ecosystem,
        anonymous_read: None,
        tokens: Vec::new(),
        policy: PolicyConfig::default(),
        ecosystem_policy: Table::new(),
        ecosystem_settings: Table::new(),
        webhooks: Vec::new(),
        kind,
    }
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
