//! The fully resolved configuration types and their defaults.

use std::collections::{BTreeMap, BTreeSet, HashSet};
use std::fmt;
use std::num::{NonZeroU32, NonZeroUsize};
use std::path::{Path, PathBuf};
use std::time::Duration;

use peryx_core::Ecosystem;
use peryx_driver::jobs::{MAINTENANCE_INTERVAL, Schedule, ScheduledJob};
use peryx_driver::rate_limit::{DEFAULT_UPSTREAM_CONCURRENCY, RateLimitConfig};
use peryx_http::{DEFAULT_HOT_CACHE_BYTES, DEFAULT_MAX_STALE_SECS};
use peryx_identity::{Action, ExternalGroupGrant, Glob, Grant, IndexAcl, NamedToken, ProviderId};
use peryx_policy::PolicyConfig;
use peryx_upstream::ExecCredentialConfig;
use serde::Deserialize;
use toml::Table;
use url::Url;

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
    /// An opt-in netrc file read once at startup for upstream Basic credentials.
    pub netrc: Option<PathBuf>,
    /// Bound on stale-on-error serving, in seconds; `0` serves stale without limit.
    pub max_stale_secs: i64,
    /// Days of daily version-and-source usage buckets to retain; `None` keeps them without limit.
    /// Expiry runs off the request path, so a tighter window only bounds durable storage.
    pub usage_retention_days: Option<u32>,
    /// The configured indexes: caches, hosted stores, and virtual indexes that compose them.
    pub indexes: Vec<IndexConfig>,
    /// How the server terminates TLS, or `None` for plain HTTP (the zero-config default, which
    /// docker/podman accept over loopback). Serving it costs nothing until set.
    pub tls: Option<TlsConfig>,
    pub log: LogConfig,
    pub rate_limit: RateLimitConfig,
    pub auth: AuthConfig,
    pub availability: AvailabilityConfig,
    pub jobs: JobsConfig,
    /// Where blobs are stored: the local filesystem (default) or an S3-compatible object store.
    pub blob: BlobStorageConfig,
}

/// The selected blob storage backend. Credentials for S3 never live here; they resolve from the
/// environment at startup.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum BlobStorageConfig {
    /// Blobs live under `data_dir/blobs`.
    Filesystem,
    /// Blobs live in an S3-compatible bucket.
    S3(S3StorageConfig),
}

/// The non-secret settings that address an S3-compatible bucket.
#[derive(Clone, PartialEq, Eq)]
pub struct S3StorageConfig {
    pub endpoint: String,
    pub bucket: String,
    pub prefix: String,
    pub region: String,
    pub path_style: bool,
    pub request_timeout: Duration,
    pub max_retries: u32,
    pub multipart_threshold: u64,
    pub part_size: u64,
    pub upload_concurrency: usize,
}

impl fmt::Debug for S3StorageConfig {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("S3StorageConfig")
            .field("endpoint", &"<redacted>")
            .field("bucket", &self.bucket)
            .field("prefix", &self.prefix)
            .field("region", &self.region)
            .field("path_style", &self.path_style)
            .field("request_timeout", &self.request_timeout)
            .field("max_retries", &self.max_retries)
            .field("multipart_threshold", &self.multipart_threshold)
            .field("part_size", &self.part_size)
            .field("upload_concurrency", &self.upload_concurrency)
            .finish()
    }
}

impl From<&S3StorageConfig> for peryx_storage::blob::S3Settings {
    fn from(config: &S3StorageConfig) -> Self {
        Self {
            endpoint: config.endpoint.clone(),
            bucket: config.bucket.clone(),
            prefix: config.prefix.clone(),
            region: config.region.clone(),
            path_style: config.path_style,
            request_timeout: config.request_timeout,
            max_retries: config.max_retries,
            multipart_threshold: config.multipart_threshold,
            part_size: config.part_size,
            upload_concurrency: config.upload_concurrency,
        }
    }
}

/// The `[jobs]` table: how this node runs its background maintenance.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct JobsConfig {
    pub mode: JobsMode,
    /// The node-local jobs this node runs on a timer, each on its own interval. Defaults to cache
    /// maintenance once a minute; a `[[jobs.schedule]]` array replaces the default set.
    pub schedules: Vec<Schedule>,
}

impl Default for JobsConfig {
    fn default() -> Self {
        Self {
            mode: JobsMode::default(),
            schedules: vec![Schedule {
                job: ScheduledJob::CacheMaintenance,
                interval: MAINTENANCE_INTERVAL,
            }],
        }
    }
}

/// Whether a node runs its own background maintenance jobs.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum JobsMode {
    /// Run maintenance on this node under the node-local job scheduler.
    #[default]
    Local,
    /// Run no maintenance: start no scheduler, timer, or worker.
    None,
}

pub const DEFAULT_REPLICA_PAGE_SIZE: usize = 100;
pub const DEFAULT_REPLICA_POLL_INTERVAL_SECS: u64 = 1;

/// The runtime availability contract a node promises for authoritative mutations.
///
/// The `[availability]` table's `mode` chooses one; each fixes what an acknowledgement guarantees is
/// durable, as the [availability contracts](@/core/availability-contracts.md) page states normatively.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum AvailabilityMode {
    /// Single writer, local durability, operator-driven failover: the zero-config default.
    #[default]
    None,
    /// A configured writer and explicit read replicas within one datacenter.
    Dc,
    /// Metadata durability in a remote datacenter.
    Ha,
}

/// The resolved `[availability]` table: the selected mode and its topology.
///
/// `dc` and `ha` carry the replication role that fulfills them; `none` holds nothing, so a single-node
/// process allocates no availability state beyond this enum's discriminant.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AvailabilityConfig {
    None,
    Dc(ReplicationConfig),
    Ha(ReplicationConfig),
}

impl AvailabilityConfig {
    /// The selected mode, independent of any topology a stronger mode carries.
    #[must_use]
    pub const fn mode(&self) -> AvailabilityMode {
        match self {
            Self::None => AvailabilityMode::None,
            Self::Dc(_) => AvailabilityMode::Dc,
            Self::Ha(_) => AvailabilityMode::Ha,
        }
    }

    /// The replication role a `dc` or `ha` node drives, or `None` under single-node `none`.
    #[must_use]
    pub const fn replication(&self) -> Option<&ReplicationConfig> {
        match self {
            Self::None => None,
            Self::Dc(replication) | Self::Ha(replication) => Some(replication),
        }
    }
}

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

impl Config {
    /// Validate settings that depend on the fully resolved configuration.
    ///
    /// # Errors
    /// Returns [`ConfigError::WriterIdentity`] when an identity is blank or replica mode has no
    /// identity to use during promotion.
    pub fn validate(&self) -> Result<(), ConfigError> {
        if !self.auth.trusted_publishers.is_empty() && self.auth.signing_key.is_none() {
            return Err(ConfigError::Auth {
                reason: "`signing_key` is required when trusted publishers are configured",
            });
        }
        let mut publisher_ids = HashSet::new();
        for publisher in &self.auth.trusted_publishers {
            if !publisher_ids.insert(&publisher.id) {
                return Err(ConfigError::TrustedPublisher {
                    id: publisher.id.clone(),
                    reason: "publisher IDs must be unique",
                });
            }
            let repository = self.indexes.iter().find(|index| index.name == publisher.repository);
            if !repository.is_some_and(|index| {
                index.ecosystem == Ecosystem::Pypi
                    && matches!(
                        index.kind,
                        IndexKind::Hosted { .. } | IndexKind::Virtual { upload: Some(_), .. }
                    )
            }) {
                return Err(ConfigError::TrustedPublisher {
                    id: publisher.id.clone(),
                    reason: "repository must name a writable PyPI index",
                });
            }
        }
        let mut provider_ids = HashSet::new();
        for provider in &self.auth.ldap_providers {
            if !provider_ids.insert(&provider.id) {
                return Err(ConfigError::LdapProvider {
                    id: provider.id.to_string(),
                    reason: "provider IDs must be unique",
                });
            }
            if provider.group_mappings.iter().any(|mapping| {
                matches!(
                    &mapping.scope,
                    peryx_identity::GrantScope::Repository { name }
                        if !self.indexes.iter().any(|index| index.name == *name)
                )
            }) {
                return Err(ConfigError::LdapProvider {
                    id: provider.id.to_string(),
                    reason: "group mapping repository must name a configured index",
                });
            }
        }
        let mut oidc_ids = HashSet::new();
        for provider in &self.auth.oidc_providers {
            if !oidc_ids.insert(&provider.id) {
                return Err(ConfigError::OidcProvider {
                    id: provider.id.to_string(),
                    reason: "provider IDs must be unique",
                });
            }
            if provider.group_mappings.iter().any(|mapping| {
                matches!(
                    &mapping.scope,
                    peryx_identity::GrantScope::Repository { name }
                        if !self.indexes.iter().any(|index| index.name == *name)
                )
            }) {
                return Err(ConfigError::OidcProvider {
                    id: provider.id.to_string(),
                    reason: "group mapping repository must name a configured index",
                });
            }
        }
        match self.writer_identity.as_deref() {
            Some(identity) if identity.trim().is_empty() => Err(ConfigError::WriterIdentity {
                reason: "must not be blank",
            }),
            None if self.read_only
                || matches!(self.availability.replication(), Some(ReplicationConfig::Replica { .. })) =>
            {
                Err(ConfigError::WriterIdentity {
                    reason: "required in read replica mode",
                })
            }
            _ => Ok(()),
        }
    }
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
    /// Audience CI providers must mint identities for.
    pub oidc_audience: String,
    pub trusted_publishers: Vec<TrustedPublisherConfig>,
    pub ldap_providers: Vec<LdapProviderConfig>,
    pub oidc_providers: Vec<OidcProviderConfig>,
}

impl Default for AuthConfig {
    fn default() -> Self {
        Self {
            signing_key: None,
            token_ttl_secs: 300,
            default_anonymous_read: true,
            oidc_audience: "peryx".to_owned(),
            trusted_publishers: Vec::new(),
            ldap_providers: Vec::new(),
            oidc_providers: Vec::new(),
        }
    }
}

/// One configured browser OIDC login provider, resolved from `[[auth.oidc_provider]]`.
#[derive(Clone, PartialEq, Eq)]
pub struct OidcProviderConfig {
    pub id: ProviderId,
    pub issuer: Url,
    pub client_id: String,
    /// The confidential client secret, or `None` for a public client that relies on PKCE alone.
    pub client_secret: Option<SecretSource>,
    pub redirect_uri: Url,
    pub scopes: Vec<String>,
    pub subject_claim: String,
    pub display_name_claim: String,
    pub groups_claim: Option<String>,
    pub clock_skew: Duration,
    pub request_timeout: Duration,
    pub group_mappings: Vec<ExternalGroupGrant>,
}

impl fmt::Debug for OidcProviderConfig {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("OidcProviderConfig")
            .field("id", &self.id)
            .field("issuer", &self.issuer)
            .field("client_id", &self.client_id)
            .field("client_secret", &self.client_secret.as_ref().map(|_| "[redacted]"))
            .field("redirect_uri", &self.redirect_uri)
            .field("scopes", &self.scopes)
            .field("subject_claim", &self.subject_claim)
            .field("display_name_claim", &self.display_name_claim)
            .field("groups_claim", &self.groups_claim)
            .field("clock_skew", &self.clock_skew)
            .field("request_timeout", &self.request_timeout)
            .field("group_mappings", &self.group_mappings)
            .finish()
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LdapProviderConfig {
    pub id: ProviderId,
    pub url: Url,
    pub base_dn: String,
    pub bind: LdapBindConfig,
    pub subject_attribute: String,
    pub display_name_attribute: String,
    pub group_attribute: Option<String>,
    pub ca_file: Option<PathBuf>,
    pub connect_timeout: Duration,
    pub request_timeout: Duration,
    pub max_connections: NonZeroU32,
    pub group_mappings: Vec<ExternalGroupGrant>,
}

#[derive(Clone, PartialEq, Eq)]
pub enum LdapBindConfig {
    Direct {
        dn_attribute: String,
    },
    Search {
        username_attribute: String,
        bind_dn: String,
        bind_password: SecretSource,
    },
}

impl std::fmt::Debug for LdapBindConfig {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Direct { dn_attribute } => formatter
                .debug_struct("Direct")
                .field("dn_attribute", dn_attribute)
                .finish(),
            Self::Search {
                username_attribute,
                bind_dn,
                ..
            } => formatter
                .debug_struct("Search")
                .field("username_attribute", username_attribute)
                .field("bind_dn", bind_dn)
                .field("bind_password", &"[redacted]")
                .finish(),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TrustedPublisherConfig {
    pub id: String,
    pub issuer: String,
    pub repository: String,
    pub subject: String,
    pub projects: Vec<String>,
    pub claims: BTreeMap<String, String>,
}

/// A secret file above this size is a misconfiguration, not a credential: a systemd credential or a
/// Kubernetes secret holds a token, never a megabyte. Capping the read keeps a wrong path (a log, a
/// device) from being slurped into memory before it fails.
const MAX_SECRET_FILE_BYTES: u64 = 1 << 20;

/// Where a secret's value comes from.
///
/// A `*_file` sibling keeps the value out of the config file, so a mounted Docker or Kubernetes secret,
/// a systemd credential, or a Vault-rendered file can hold it; an `*_env` sibling reads it from an
/// environment variable the process manager injects.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SecretSource {
    Literal(String),
    File(PathBuf),
    Env(String),
}

/// Request behavior when a dynamic credential source cannot be read.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum CredentialFailureMode {
    /// Reject requests until a later refresh succeeds.
    #[default]
    Fail,
    /// Retry without authentication.
    Anonymous,
}

/// Dynamic credential reload policy for an environment variable or file.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CredentialRefreshConfig {
    /// Minimum time between source reads.
    pub interval: Duration,
    /// Reload once when an upstream rejects the current generation.
    pub on_unauthorized: bool,
    /// Request behavior when the source read fails.
    pub failure: CredentialFailureMode,
}

impl SecretSource {
    #[must_use]
    pub(super) const fn supports_refresh(&self) -> bool {
        matches!(self, Self::File(_) | Self::Env(_))
    }

    /// The secret's value, reading the file or environment variable when that is where it lives.
    /// Surrounding whitespace goes: a secret file written by `echo` or a Kubernetes mount ends in a
    /// newline that is not part of it. Every error path names only the location, never the value.
    ///
    /// # Errors
    /// Returns [`ConfigError::Read`] when a file cannot be read, [`ConfigError::OversizeSecret`] when it
    /// exceeds [`MAX_SECRET_FILE_BYTES`], [`ConfigError::EmptySecret`] when a file holds only whitespace,
    /// and [`ConfigError::EnvSecret`] when the variable is unset, empty, or not valid UTF-8.
    pub fn read(&self) -> Result<String, ConfigError> {
        match self {
            Self::Literal(secret) => Ok(secret.clone()),
            Self::File(path) => Self::read_file(path),
            Self::Env(var) => Self::read_env(var),
        }
    }

    fn read_file(path: &Path) -> Result<String, ConfigError> {
        use std::io::Read as _;

        let mut buf = String::new();
        let read = std::fs::File::open(path)
            .and_then(|file| file.take(MAX_SECRET_FILE_BYTES + 1).read_to_string(&mut buf))
            .map_err(|source| ConfigError::Read {
                path: path.to_owned(),
                source,
            })?;
        if read as u64 > MAX_SECRET_FILE_BYTES {
            return Err(ConfigError::OversizeSecret {
                path: path.to_owned(),
                limit: MAX_SECRET_FILE_BYTES,
            });
        }
        let secret = buf.trim();
        if secret.is_empty() {
            return Err(ConfigError::EmptySecret { path: path.to_owned() });
        }
        Ok(secret.to_owned())
    }

    fn read_env(var: &str) -> Result<String, ConfigError> {
        std::env::var(var)
            .ok()
            .map(|value| value.trim().to_owned())
            .filter(|secret| !secret.is_empty())
            .ok_or_else(|| ConfigError::EnvSecret { var: var.to_owned() })
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
        credential_exec: Option<ExecCredentialConfig>,
        credential_refresh: Option<CredentialRefreshConfig>,
        /// Per-upstream trust and client identity files.
        tls: UpstreamTlsConfig,
        /// Ordered named sources and fallback controls. `None` preserves the legacy single-upstream
        /// behavior of `cached = URL`.
        routing: Option<Box<UpstreamRoutingConfig>>,
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

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UpstreamConfig {
    pub name: String,
    pub url: String,
    pub artifact_url: Option<String>,
    pub username: Option<String>,
    pub password: Option<SecretSource>,
    pub token: Option<SecretSource>,
    pub credential_exec: Option<ExecCredentialConfig>,
    pub credential_refresh: Option<CredentialRefreshConfig>,
    pub tls: UpstreamTlsConfig,
}

/// Paths to TLS material read when an upstream client is constructed.
#[derive(Clone, Default, PartialEq, Eq)]
pub struct UpstreamTlsConfig {
    pub ca_file: Option<PathBuf>,
    pub client_cert_file: Option<PathBuf>,
    pub client_key_file: Option<PathBuf>,
}

impl std::fmt::Debug for UpstreamTlsConfig {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("UpstreamTlsConfig")
            .field("custom_ca", &self.ca_file.is_some())
            .field(
                "client_identity",
                &(self.client_cert_file.is_some() || self.client_key_file.is_some()),
            )
            .finish()
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UpstreamRoutingConfig {
    pub upstreams: Vec<UpstreamConfig>,
    pub fallback: bool,
    pub protected: Vec<String>,
    pub pins: std::collections::BTreeMap<String, String>,
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
            netrc: None,
            max_stale_secs: DEFAULT_MAX_STALE_SECS,
            usage_retention_days: None,
            indexes: default_indexes(),
            tls: None,
            log: LogConfig::default(),
            rate_limit: RateLimitConfig::default(),
            auth: AuthConfig::default(),
            availability: AvailabilityConfig::None,
            jobs: JobsConfig::default(),
            blob: BlobStorageConfig::Filesystem,
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
        credential_exec: None,
        credential_refresh: None,
        tls: UpstreamTlsConfig::default(),
        routing: None,
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
