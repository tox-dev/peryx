use std::collections::{BTreeMap, BTreeSet, HashSet};
use std::fmt;
use std::net::{SocketAddr, ToSocketAddrs as _};
use std::num::{NonZeroU32, NonZeroUsize};
use std::path::{Path, PathBuf};
use std::time::Duration;

use peryx_core::Ecosystem;
use peryx_driver::jobs::{MAINTENANCE_INTERVAL, Schedule, ScheduledJob};
use peryx_driver::rate_limit::{DEFAULT_UPSTREAM_CONCURRENCY, RateLimitConfig};
use peryx_ha::{AvailabilityMode, DurabilityPolicy};
use peryx_ha_distributed::read_through::ReadThroughLimits;
use peryx_http::{DEFAULT_HOT_CACHE_BYTES, DEFAULT_MAX_STALE_SECS};
use peryx_identity::{Action, ExternalGroupGrant, Glob, Grant, IndexAcl, NamedToken, ProviderId};
use peryx_policy::PolicyConfig;
use peryx_storage::blob::DurabilityCapabilities;
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
    /// This node's own identity in an `ha` consensus roster, naming its `[[availability.member]]` entry
    /// so the ownership Raft node runs under its own voter identity. It is distinct from
    /// [`writer_identity`](Self::writer_identity), which names the one writer every node claims and
    /// follows on the metadata plane and is the same on every node.
    pub node_identity: Option<String>,
    /// Disable upstream network access and serve only cached data.
    pub offline: bool,
    /// Reject client mutations and disable upstream cache fills on a read replica.
    pub read_only: bool,
    /// Fallback freshness for cached metadata documents, in seconds. Upstream `Cache-Control` lifetimes
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
    /// clients accept over loopback). Serving it costs nothing until set.
    pub tls: Option<TlsConfig>,
    pub log: LogConfig,
    pub rate_limit: RateLimitConfig,
    pub auth: AuthConfig,
    pub availability: AvailabilityConfig,
    /// The resolved write-ack quorum and deadline hosted writes are acknowledged against.
    pub write_ack: WriteAckConfig,
    /// The static datacenter replication group, when one is configured. Absent under single-node
    /// `none` mode and whenever no `[[availability.member]]` roster is given.
    pub dc_membership: Option<DcMembership>,
    /// The private availability control listener a `dc` or `ha` node exposes, when one is configured.
    /// Single-node `none` opens none, so the field is `None` there and the runtime allocates no socket.
    pub availability_listener: Option<AvailabilityListenerConfig>,
    /// The bounds a serving read-through of a remote placement runs under, when the operator tuned them.
    /// Absent leaves the built-in defaults, which the runtime applies when it installs the capability.
    pub read_through: Option<ReadThroughLimits>,
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

impl BlobStorageConfig {
    #[must_use]
    pub const fn durability(&self) -> DurabilityCapabilities {
        match self {
            Self::Filesystem => DurabilityCapabilities::FILESYSTEM,
            Self::S3(config) => DurabilityCapabilities::object_store(config.conditional_writes, config.checksum_writes),
        }
    }
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
    /// The endpoint enforces `If-None-Match` create-if-absent writes.
    pub conditional_writes: bool,
    /// The endpoint validates the SHA-256 checksum the backend sends with each write.
    pub checksum_writes: bool,
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
            .field("conditional_writes", &self.conditional_writes)
            .field("checksum_writes", &self.checksum_writes)
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
            conditional_writes: config.conditional_writes,
            checksum_writes: config.checksum_writes,
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

impl JobsConfig {
    pub(super) fn validate_mode(&self, mode: AvailabilityMode) -> Result<(), ConfigError> {
        if mode == AvailabilityMode::None
            && let Some((index, _)) = self
                .schedules
                .iter()
                .enumerate()
                .find(|(_, schedule)| peryx_ha_distributed::is_scheduled_job_kind(schedule.job.as_str()))
        {
            return Err(ConfigError::Jobs {
                index,
                reason: "`none` availability cannot schedule distributed jobs",
            });
        }
        Ok(())
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
    #[must_use]
    pub const fn mode(&self) -> AvailabilityMode {
        match self {
            Self::None => AvailabilityMode::None,
            Self::Dc(_) => AvailabilityMode::Dc,
            Self::Ha(_) => AvailabilityMode::Ha,
        }
    }

    #[must_use]
    pub const fn is_replica_mode(&self) -> bool {
        matches!(self.replication(), Some(ReplicationConfig::Replica { .. }))
    }

    #[must_use]
    pub const fn replication(&self) -> Option<&ReplicationConfig> {
        match self {
            Self::None => None,
            Self::Dc(replication) | Self::Ha(replication) => Some(replication),
        }
    }
}

/// The default client write-ack deadline, in seconds, when `[availability.write_ack]` names none.
pub const DEFAULT_WRITE_ACK_DEADLINE_SECS: u64 = 5;

/// The resolved `[availability.write_ack]` contract: the durability quorum a hosted write must reach and
/// the deadline the client waits before the write is reported retry-safe-unknown rather than durable.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct WriteAckConfig {
    pub policy: DurabilityPolicy,
    pub deadline: Duration,
}

impl Default for WriteAckConfig {
    fn default() -> Self {
        Self {
            policy: DurabilityPolicy::Local,
            deadline: Duration::from_secs(DEFAULT_WRITE_ACK_DEADLINE_SECS),
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

/// A node's role in a static datacenter replication group.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum DcRole {
    /// The single node that accepts authoritative writes. A group has exactly one, chosen by
    /// configuration; losing it stops writes rather than promoting a replica.
    Writer,
    /// A read replica that applies the writer's changes and may serve reads under the staleness
    /// contract. No timeout ever turns it into the writer.
    Replica,
}

/// One configured member of a datacenter replication group.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DcMember {
    /// The member's stable identity, unique within the group.
    pub node: String,
    /// The datacenter the member runs in. `dc` members may share it; `ha` members may not.
    pub dc: String,
    /// The HTTP(S) base URL peers reach this member on, unique within the group.
    pub address: String,
    pub role: DcRole,
}

/// A static datacenter replication group: one writer and its read replicas, fixed by configuration.
///
/// Membership never changes from a network broadcast or a liveness timeout; only an operator editing
/// this configuration adds, removes, or replaces a member. Validation guarantees the roster the runtime
/// consumes can never hold two writers or omit the writer, so a replacement is an explicit, reviewed
/// edit rather than an automatic promotion.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DcMembership {
    /// The group's identity, distinct from every member identity within it.
    pub group: String,
    pub members: Vec<DcMember>,
}

/// The private control listener a `dc` or `ha` node exposes for availability administration.
///
/// Availability controls never share public artifact routes: this listener carries the
/// administrator-scoped surface on its own socket, private-bound by default so the control plane is not
/// reachable from public traffic unless an operator deliberately widens the bind and grants it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AvailabilityListenerConfig {
    /// The socket the listener binds. Defaults to a loopback address; a non-loopback bind is refused
    /// unless [`tls`](Self::tls) terminates the connection or [`allow_remote_plaintext`] permits it.
    ///
    /// [`allow_remote_plaintext`]: Self::allow_remote_plaintext
    pub bind: SocketAddr,
    /// The certificate and key that terminate TLS, or `None` to serve the listener over plain HTTP.
    pub tls: Option<AvailabilityListenerTls>,
    /// Whether a non-loopback bind may serve plain HTTP. Off by default so an operator cannot expose the
    /// control plane to the network unencrypted without stating the intent.
    pub allow_remote_plaintext: bool,
}

/// A PEM certificate chain and private key that terminate TLS on the availability listener.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AvailabilityListenerTls {
    pub cert: PathBuf,
    pub key: PathBuf,
}

impl Config {
    /// # Errors
    /// Returns [`ConfigError::ListenAddress`] when the host cannot resolve to a socket address.
    pub fn listen_address(&self) -> Result<SocketAddr, ConfigError> {
        let listen_error = |source| ConfigError::ListenAddress {
            host: self.host.clone(),
            port: self.port,
            source,
        };
        let address = (self.host.as_str(), self.port)
            .to_socket_addrs()
            .map_err(&listen_error)?
            .next();
        let unavailable = listen_error(std::io::ErrorKind::AddrNotAvailable.into());
        address.ok_or(unavailable)
    }

    /// # Errors
    /// Returns the first validation error using the compiled plugins.
    pub fn validate(&self) -> Result<(), ConfigError> {
        self.validate_with_plugins(&crate::compiled_plugins())
    }

    /// # Errors
    /// Returns an error when cache timing, plugin authentication, identity providers, jobs,
    /// durability, or writer identity conflict with the resolved configuration.
    pub fn validate_with_plugins(&self, plugins: &peryx_plugin_registry::PluginRegistry) -> Result<(), ConfigError> {
        for (field, value) in [
            ("cache_ttl_secs", self.cache_ttl_secs),
            ("max_stale_secs", self.max_stale_secs),
        ] {
            if value < 0 {
                return Err(ConfigError::CacheTiming { field, value });
            }
        }
        let plugin_indexes = self
            .indexes
            .iter()
            .map(|index| peryx_driver::serving::PluginIndexConfig {
                name: index.name.as_str(),
                ecosystem: index.ecosystem.clone(),
                writable: matches!(
                    index.kind,
                    IndexKind::Hosted { .. }
                        | IndexKind::Virtual {
                            write_target: Some(_),
                            ..
                        }
                ),
            })
            .collect::<Vec<_>>();
        plugins
            .validate_auth_extensions(
                &self.auth.extensions,
                self.auth.signing_key.is_some(),
                self.auth.token_ttl_secs,
                &plugin_indexes,
            )
            .map_err(ConfigError::Plugin)?;
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
        if !self.auth.oidc_providers.is_empty() && self.auth.signing_key.is_none() {
            return Err(ConfigError::Auth {
                reason: "`signing_key` is required when OIDC login providers are configured",
            });
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
        let mode = self.availability.mode();
        self.jobs.validate_mode(mode)?;
        self.validate_scheduled_jobs()?;
        if let Err(shortfall) = self.blob.durability().check(mode.durability_requirement()) {
            return Err(ConfigError::Durability {
                mode: mode.as_str(),
                shortfall: shortfall.as_str(),
            });
        }
        self.validate_topology(mode)
    }

    fn validate_scheduled_jobs(&self) -> Result<(), ConfigError> {
        let local = self
            .node_identity
            .as_deref()
            .or(self.writer_identity.as_deref())
            .and_then(|identity| {
                self.dc_membership
                    .as_ref()?
                    .members
                    .iter()
                    .find(|member| member.node == identity)
            });
        for (index, schedule) in self.jobs.schedules.iter().enumerate() {
            let job = schedule.job.as_str();
            if !peryx_ha_distributed::is_scheduled_job_kind(job) {
                continue;
            }
            let reason = if !matches!(self.availability.replication(), Some(ReplicationConfig::Primary { .. })) {
                Some("distributed jobs require a primary availability node")
            } else if local.is_none() {
                Some("distributed jobs require a local member roster")
            } else if matches!(job, "dc_copy" | "placement_reconcile")
                && !matches!(self.blob, BlobStorageConfig::Filesystem)
            {
                Some("copy and placement jobs require filesystem blob storage")
            } else if job == "dc_copy"
                && !self.dc_membership.as_ref().is_some_and(|membership| {
                    membership
                        .members
                        .iter()
                        .any(|member| Some(member.dc.as_str()) != local.map(|local| local.dc.as_str()))
                })
            {
                Some("`dc_copy` requires a remote datacenter")
            } else {
                None
            };
            if let Some(reason) = reason {
                return Err(ConfigError::Jobs { index, reason });
            }
        }
        Ok(())
    }

    fn validate_topology(&self, mode: AvailabilityMode) -> Result<(), ConfigError> {
        if let Some(membership) = &self.dc_membership {
            membership
                .members
                .iter()
                .try_for_each(|member| validate_member_address(&member.address))?;
        }
        if mode.is_distributed() && self.write_ack.policy != DurabilityPolicy::Local && self.dc_membership.is_none() {
            return Err(ConfigError::Availability {
                reason: "`write_ack.policy` stronger than `local` requires `[[availability.member]]`",
            });
        }
        match self.writer_identity.as_deref() {
            Some(identity) if identity.trim().is_empty() => Err(ConfigError::WriterIdentity {
                reason: "must not be blank",
            }),
            Some(_) if self.availability.replication().is_none() => Err(ConfigError::WriterIdentity {
                reason: "requires `dc` or `ha` availability",
            }),
            None if self.availability.is_replica_mode()
                || (self.read_only && self.availability.replication().is_some()) =>
            {
                Err(ConfigError::WriterIdentity {
                    reason: "required in read replica mode",
                })
            }
            _ => Ok(()),
        }?;
        if let Some(identity) = self.writer_identity.as_deref()
            && !self.identity_is_configured(identity)
        {
            return Err(ConfigError::WriterIdentity {
                reason: "`writer_identity` must name a configured `[[availability.member]]`",
            });
        }
        if let Some(identity) = self.node_identity.as_deref() {
            if mode != AvailabilityMode::Ha {
                return Err(ConfigError::Availability {
                    reason: "`node_identity` requires `ha` mode",
                });
            }
            if !self.identity_is_configured(identity) {
                return Err(ConfigError::Availability {
                    reason: "`node_identity` must name a configured `[[availability.member]]`",
                });
            }
        }
        Ok(())
    }

    fn identity_is_configured(&self, identity: &str) -> bool {
        self.dc_membership
            .as_ref()
            .is_none_or(|membership| membership.members.iter().any(|member| member.node == identity))
    }
}

pub(super) fn validate_member_address(address: &str) -> Result<(), ConfigError> {
    if address.trim().is_empty() {
        return Err(ConfigError::DcMembership {
            reason: "member `address` must not be empty".to_owned(),
        });
    }
    if !Url::parse(address).is_ok_and(|url| matches!(url.scheme(), "http" | "https") && !url.cannot_be_a_base()) {
        return Err(ConfigError::DcMembership {
            reason: format!("member `address` {address:?} must be an http or https URL"),
        });
    }
    Ok(())
}

/// One day keeps realm token expiry arithmetic within the configured credential lifetime.
pub const MAX_TOKEN_TTL_SECS: i64 = 86_400;

/// The `[auth]` table: the settings every index's access rules share.
#[derive(Debug, Clone, PartialEq)]
pub struct AuthConfig {
    /// The key peryx signs its own tokens with. Unset leaves the token realm without a key.
    pub signing_key: Option<SecretSource>,
    /// How long a minted token stays valid, in seconds.
    pub token_ttl_secs: i64,
    /// What an index's `anonymous_read` defaults to. Set it to `false` to close a whole server's reads
    /// with one key instead of one per index.
    pub default_anonymous_read: bool,
    pub ldap_providers: Vec<LdapProviderConfig>,
    pub oidc_providers: Vec<OidcProviderConfig>,
    pub extensions: Table,
}

impl Default for AuthConfig {
    fn default() -> Self {
        Self {
            signing_key: None,
            token_ttl_secs: 300,
            default_anonymous_read: true,
            ldap_providers: Vec::new(),
            oidc_providers: Vec::new(),
            extensions: Table::new(),
        }
    }
}

/// One configured browser OIDC login provider, resolved from `[[auth.oidc_provider]]`.
#[derive(Clone, PartialEq, Eq)]
pub struct OidcProviderConfig {
    pub id: ProviderId,
    pub issuer: String,
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

/// A secret file above this size is a misconfiguration, not a credential: a systemd credential or a
/// Kubernetes secret holds a token, never a megabyte. Capping the read keeps a wrong path (a log, a
/// device) from being slurped into memory before it fails.
const MAX_SECRET_FILE_BYTES: u64 = 1 << 20;

/// Where a secret's value comes from.
///
/// A `*_file` sibling keeps the value out of the config file, so a mounted container secret,
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
    /// exceeds 1 MiB, [`ConfigError::EmptySecret`] when a file holds only whitespace,
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
    /// Resource patterns the token may act on; `*` covers the index.
    pub resources: Vec<String>,
    pub actions: BTreeSet<Action>,
    /// Unix seconds after which the token stops authenticating.
    pub expires_at: Option<i64>,
}

/// One configured index, addressed at `route`.
#[derive(Debug, Clone, PartialEq)]
pub struct IndexConfig {
    /// Identifier other indexes reference in their `layers`.
    pub name: String,
    /// URL prefix the index is served under, for example `root/artifacts`.
    pub route: String,
    /// The artifact ecosystem this index serves. Immutable once created.
    pub ecosystem: Ecosystem,
    pub kind: IndexKind,
    /// Whether a request with no credential may read here. `None` takes the value of
    /// [`AuthConfig::default_anonymous_read`].
    pub anonymous_read: Option<bool>,
    /// The named credentials this index accepts, each from a `[[index.access_token]]` table.
    pub tokens: Vec<TokenConfig>,
    pub policy: PolicyConfig,
    /// The `[policy]` keys the neutral engine did not claim, left raw for this index's ecosystem
    /// driver to compile into artifact rules. Empty when an operator set no ecosystem-specific policy.
    pub ecosystem_policy: Table,
    /// The opaque `[index.settings]` table compiled by this index's ecosystem adapter.
    pub ecosystem_settings: Table,
    pub webhooks: Vec<WebhookConfig>,
}

impl IndexConfig {
    /// # Errors
    /// Returns [`ConfigError::Read`] when a secret file cannot be read and [`ConfigError::EmptySecret`]
    /// when one holds nothing: an empty secret would authenticate an empty password.
    pub fn acl(&self, auth: &AuthConfig) -> Result<IndexAcl, ConfigError> {
        let tokens = self
            .tokens
            .iter()
            .map(|token| {
                Ok(NamedToken {
                    name: token.name.clone(),
                    secret: token.secret.read()?,
                    grants: vec![Grant {
                        resources: token.resources.iter().cloned().map(Glob::new).collect(),
                        actions: token.actions.clone(),
                    }],
                    expires_at: token.expires_at,
                })
            })
            .collect::<Result<_, ConfigError>>()?;
        Ok(IndexAcl {
            anonymous_read: self.anonymous_read.unwrap_or(auth.default_anonymous_read),
            tokens,
        })
    }
}

/// The three composable index roles: a read-through cache, a writable hosted store, or a virtual
/// index that aggregates other indexes under one route.
#[derive(Debug, Clone, PartialEq)]
pub enum IndexKind {
    /// Cache an upstream repository, fetching on demand.
    Cached {
        /// The ordered `[[index.upstream]]` sources and their fallback controls, each carrying its own
        /// URL, credentials, and TLS; a single source is the one-element case.
        routing: UpstreamRoutingConfig,
        /// Concurrent upstream fetches allowed for this cached index in this process; `0` disables the cap.
        upstream_concurrency: usize,
        /// Serve only cached data for this index.
        offline: bool,
        /// Optional resource and artifact filters for `peryx prefetch`.
        prefetch: Box<PrefetchConfig>,
    },
    /// A hosted store that accepts writes. `[[index.access_token]]` grants enable writes; `volatile`
    /// allows delete and overwrite.
    Hosted { volatile: bool },
    /// An ordered aggregation of other indexes (its members, by name, in `layers`). Resolution merges
    Virtual {
        layers: Vec<String>,
        write_target: Option<String>,
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
#[derive(Debug, Default, Clone, PartialEq)]
pub struct PrefetchConfig {
    pub options: toml::Table,
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
        Self::with_plugins(&crate::compiled_plugins())
    }
}

impl Config {
    #[must_use]
    pub fn with_plugins(plugins: &peryx_plugin_registry::PluginRegistry) -> Self {
        Self {
            host: "127.0.0.1".to_owned(),
            port: 4433,
            data_dir: PathBuf::from("peryx-data"),
            writer_identity: None,
            node_identity: None,
            offline: false,
            read_only: false,
            cache_ttl_secs: 300,
            hot_cache_bytes: DEFAULT_HOT_CACHE_BYTES,
            netrc: None,
            max_stale_secs: DEFAULT_MAX_STALE_SECS,
            usage_retention_days: None,
            indexes: default_indexes(plugins),
            tls: None,
            log: LogConfig::default(),
            rate_limit: RateLimitConfig::default(),
            auth: AuthConfig::default(),
            availability: AvailabilityConfig::None,
            write_ack: WriteAckConfig::default(),
            dc_membership: None,
            availability_listener: None,
            read_through: None,
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

fn default_indexes(plugins: &peryx_plugin_registry::PluginRegistry) -> Vec<IndexConfig> {
    plugins
        .default_indexes()
        .map(|index| {
            default_index(
                index.name,
                index.route,
                index.ecosystem.clone(),
                default_index_kind(index.kind),
            )
        })
        .collect()
}

fn default_index_kind(kind: peryx_core::DefaultIndexKind) -> IndexKind {
    match kind {
        peryx_core::DefaultIndexKind::Cached { upstream } => IndexKind::Cached {
            routing: UpstreamRoutingConfig {
                upstreams: vec![UpstreamConfig {
                    name: "primary".to_owned(),
                    url: upstream.to_owned(),
                    artifact_url: None,
                    username: None,
                    password: None,
                    token: None,
                    credential_exec: None,
                    credential_refresh: None,
                    tls: UpstreamTlsConfig::default(),
                }],
                fallback: true,
                protected: Vec::new(),
                pins: BTreeMap::new(),
            },
            upstream_concurrency: DEFAULT_UPSTREAM_CONCURRENCY,
            offline: false,
            prefetch: Box::default(),
        },
        peryx_core::DefaultIndexKind::Hosted => IndexKind::Hosted { volatile: true },
        peryx_core::DefaultIndexKind::Virtual { layers, write_target } => IndexKind::Virtual {
            layers: layers.iter().map(|layer| (*layer).to_owned()).collect(),
            write_target: Some(write_target.to_owned()),
        },
    }
}

fn default_index(name: &str, route: &str, ecosystem: Ecosystem, kind: IndexKind) -> IndexConfig {
    IndexConfig {
        name: name.to_owned(),
        route: route.to_owned(),
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
