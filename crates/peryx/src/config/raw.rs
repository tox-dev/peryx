use std::collections::BTreeMap;
use std::num::{NonZeroU32, NonZeroU64, NonZeroUsize};
use std::path::PathBuf;

use ipnet::IpNet;
use peryx_identity::{Action, Role};
use peryx_policy::PolicyConfig;
use serde::Deserialize;
use toml::Table;

use peryx_ha::AvailabilityMode;

use super::model::{CredentialFailureMode, DcRole, JobsMode, LogFormat, LogSink, PrefetchConfig};

#[derive(Debug, Default, Clone, PartialEq, Deserialize)]
#[serde(default)]
pub struct RawPrefetchConfig {
    #[serde(flatten)]
    pub options: Table,
}

impl RawPrefetchConfig {
    #[must_use]
    pub fn resolve(self) -> PrefetchConfig {
        PrefetchConfig { options: self.options }
    }
}

/// A configuration source with every field optional, used for the file and CLI overlays.
#[derive(Debug, Default, Clone, PartialEq, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct PartialConfig {
    pub host: Option<String>,
    pub port: Option<u16>,
    pub data_dir: Option<PathBuf>,
    pub writer_identity: Option<String>,
    pub node_identity: Option<String>,
    pub offline: Option<bool>,
    pub read_only: Option<bool>,
    pub cache_ttl_secs: Option<i64>,
    pub hot_cache_bytes: Option<u64>,
    /// An operator-selected netrc file for upstream Basic credentials.
    pub netrc: Option<PathBuf>,
    /// Bound on stale-on-error serving, in seconds; `0` serves stale without limit.
    pub max_stale_secs: Option<i64>,
    /// Days of daily usage buckets to retain; absent keeps them without limit.
    pub usage_retention_days: Option<u32>,
    /// The `[[index]]` array from the TOML file. When present it replaces the default topology.
    #[serde(rename = "index")]
    pub indexes: Option<Vec<RawIndex>>,
    /// A `[tls]` table: bring-your-own certificate.
    pub tls: Option<RawTls>,
    /// An `[acme]` table: automatic certificates. Mutually exclusive with `[tls]`.
    pub acme: Option<RawAcme>,
    pub log: PartialLogConfig,
    pub rate_limit: PartialRateLimitConfig,
    pub auth: PartialAuthConfig,
    /// The `[availability]` table: the runtime availability mode and the replication topology a
    /// stronger mode carries. Absent, like `mode = "none"`, selects single-node operation.
    pub availability: Option<RawAvailability>,
    pub jobs: PartialJobsConfig,
    /// A `[blob]` table selecting the blob storage backend.
    pub blob: Option<RawBlobStorage>,
}

/// The raw `[blob]` table selecting the blob storage backend before validation. The AWS default
/// provider chain supplies S3 credentials outside configuration.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(tag = "backend", rename_all = "lowercase", deny_unknown_fields)]
pub enum RawBlobStorage {
    Filesystem,
    S3 {
        endpoint: String,
        bucket: String,
        region: String,
        prefix: Option<String>,
        path_style: Option<bool>,
        timeout_secs: Option<u64>,
        max_retries: Option<u32>,
        multipart_threshold_bytes: Option<u64>,
        part_size_bytes: Option<u64>,
        upload_concurrency: Option<usize>,
        conditional_writes: Option<bool>,
        checksum_writes: Option<bool>,
    },
}

/// The raw `[availability]` table: the mode selector and its replication role.
///
/// A `dc` or `ha` node carries a role; an omitted table, and an explicit `mode = "none"`, both resolve
/// to single-node operation with no replication.
#[derive(Debug, Default, Clone, PartialEq, Eq, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct RawAvailability {
    pub mode: Option<AvailabilityMode>,
    pub replication: Option<RawReplication>,
    /// The `group` identity a `[[availability.member]]` roster belongs to.
    pub group: Option<String>,
    /// The `[[availability.member]]` array: the static datacenter group roster.
    #[serde(rename = "member")]
    pub members: Option<Vec<RawDcMember>>,
    /// The `[availability.listener]` table: the private control listener a `dc` or `ha` node exposes.
    pub listener: Option<RawAvailabilityListener>,
    /// The `[availability.write_ack]` table: the durability quorum and client deadline a hosted write
    /// must reach before it is acknowledged. Absent takes the mode's default.
    pub write_ack: Option<RawWriteAck>,
    /// The `[availability.read-through]` table: the bounds a serving read-through of a remote placement
    /// runs under.
    #[serde(rename = "read-through")]
    pub read_through: Option<RawReadThrough>,
}

/// The raw `[availability.write_ack]` table before quorum and deadline resolution.
#[derive(Debug, Default, Clone, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "kebab-case", deny_unknown_fields)]
pub struct RawWriteAck {
    pub policy: Option<RawWriteAckPolicy>,
    /// The client write-ack deadline in seconds; a rosterless default applies when omitted.
    pub deadline_secs: Option<u64>,
}

/// The durability quorum a hosted write must reach across the datacenter's members.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum RawWriteAckPolicy {
    Local,
    Majority,
    Everywhere,
}

/// The raw `[availability.read-through]` table before its bounds are resolved against their defaults.
///
/// Every field is optional and falls back to the built-in default; a zero is rejected at parse time by
/// the non-zero field types.
#[derive(Debug, Default, Clone, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "kebab-case", deny_unknown_fields)]
pub struct RawReadThrough {
    pub concurrency: Option<NonZeroUsize>,
    pub per_fetch_bytes: Option<NonZeroU64>,
    pub chunk_bytes: Option<NonZeroUsize>,
    pub max_fanout: Option<NonZeroUsize>,
    pub trip_after: Option<u32>,
    pub cooldown_secs: Option<u64>,
    /// The `[availability.read-through.retry]` sub-table: the whole reconnect schedule, tuned together or
    /// left at its default.
    pub retry: Option<RawReadThroughRetry>,
}

/// The raw `[availability.read-through.retry]` sub-table. Present means every field is given, so the
/// schedule is set as a whole rather than half-overridden.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "kebab-case", deny_unknown_fields)]
pub struct RawReadThroughRetry {
    pub base_ms: u64,
    pub multiplier: NonZeroU32,
    pub max_delay_secs: u64,
    pub max_attempts: NonZeroU32,
}

/// The raw `[availability.listener]` table before address and TLS validation.
#[derive(Debug, Default, Clone, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "kebab-case", deny_unknown_fields)]
pub struct RawAvailabilityListener {
    pub bind: Option<String>,
    pub tls: Option<RawTls>,
    pub allow_remote_plaintext: Option<bool>,
}

/// One `[[availability.member]]` table before identity and role validation.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RawDcMember {
    pub node: String,
    pub dc: String,
    pub address: String,
    pub role: DcRole,
}

/// The `[jobs]` half of [`PartialConfig`].
#[derive(Debug, Default, Clone, PartialEq, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct PartialJobsConfig {
    pub mode: Option<JobsMode>,
    /// The `[[jobs.schedule]]` array. When present it replaces the default schedule set.
    #[serde(rename = "schedule")]
    pub schedules: Option<Vec<RawJobSchedule>>,
}

/// One `[[jobs.schedule]]` table before cadence validation.
#[derive(Debug, Clone, PartialEq, Deserialize)]
pub struct RawJobSchedule {
    pub job: String,
    /// Seconds between runs; validated positive.
    pub interval_secs: u64,
    #[serde(flatten)]
    pub settings: Table,
}

/// One process replication role before secret and numeric validation.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(tag = "role", rename_all = "lowercase", deny_unknown_fields)]
pub enum RawReplication {
    Primary {
        source: String,
        token: Option<String>,
        token_file: Option<PathBuf>,
    },
    Replica {
        upstream: String,
        token: Option<String>,
        token_file: Option<PathBuf>,
        poll_interval_secs: Option<u64>,
        page_size: Option<usize>,
    },
}

/// The raw `[auth]` table: the signing key of peryx's token realm, and the defaults every index's
/// access rules take.
#[derive(Debug, Default, Clone, PartialEq, Deserialize)]
#[serde(default)]
pub struct PartialAuthConfig {
    pub signing_key: Option<String>,
    pub signing_key_file: Option<PathBuf>,
    pub token_ttl_secs: Option<i64>,
    pub default_anonymous_read: Option<bool>,
    #[serde(rename = "ldap_provider")]
    pub ldap_providers: Option<Vec<RawLdapProvider>>,
    #[serde(rename = "oidc_provider")]
    pub oidc_providers: Option<Vec<RawOidcProvider>>,
    #[serde(flatten)]
    pub extensions: Table,
}

/// The raw `[[auth.oidc_provider]]` table: one browser OIDC login provider before validation.
#[derive(Clone, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RawOidcProvider {
    pub id: String,
    pub issuer: String,
    pub client_id: String,
    pub client_secret: Option<String>,
    pub client_secret_file: Option<PathBuf>,
    pub client_secret_env: Option<String>,
    pub redirect_uri: String,
    #[serde(default)]
    pub scopes: Vec<String>,
    pub subject_claim: Option<String>,
    pub display_name_claim: Option<String>,
    pub groups_claim: Option<String>,
    pub clock_skew_secs: Option<u64>,
    pub request_timeout_secs: Option<u64>,
    #[serde(default, rename = "group_mapping")]
    pub group_mappings: Vec<RawExternalGroupGrant>,
}

impl std::fmt::Debug for RawOidcProvider {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("RawOidcProvider")
            .field("id", &self.id)
            .field("issuer", &self.issuer)
            .field("client_id", &self.client_id)
            .field("client_secret", &"[redacted]")
            .field("client_secret_file", &self.client_secret_file)
            .field("client_secret_env", &self.client_secret_env)
            .field("redirect_uri", &self.redirect_uri)
            .field("scopes", &self.scopes)
            .finish_non_exhaustive()
    }
}

#[derive(Clone, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RawLdapProvider {
    pub id: String,
    pub url: String,
    pub base_dn: String,
    pub mode: RawLdapMode,
    pub dn_attribute: Option<String>,
    pub username_attribute: Option<String>,
    pub bind_dn: Option<String>,
    pub bind_password: Option<String>,
    pub bind_password_file: Option<PathBuf>,
    pub bind_password_env: Option<String>,
    pub subject_attribute: String,
    pub display_name_attribute: String,
    pub group_attribute: Option<String>,
    pub ca_file: Option<PathBuf>,
    pub connect_timeout_secs: Option<u64>,
    pub request_timeout_secs: Option<u64>,
    pub max_connections: Option<u32>,
    #[serde(default, rename = "group_mapping")]
    pub group_mappings: Vec<RawExternalGroupGrant>,
}

impl std::fmt::Debug for RawLdapProvider {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("RawLdapProvider")
            .field("id", &self.id)
            .field("url", &self.url)
            .field("base_dn", &self.base_dn)
            .field("mode", &self.mode)
            .field("dn_attribute", &self.dn_attribute)
            .field("username_attribute", &self.username_attribute)
            .field("bind_dn", &self.bind_dn)
            .field("bind_password", &"[redacted]")
            .field("bind_password_file", &self.bind_password_file)
            .field("bind_password_env", &self.bind_password_env)
            .finish_non_exhaustive()
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum RawLdapMode {
    DirectBind,
    ServiceSearch,
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RawExternalGroupGrant {
    pub group: String,
    pub role: Role,
    pub repository: Option<String>,
}

/// The raw `[tls]` table before validation.
#[derive(Debug, Default, Clone, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RawTls {
    pub cert: Option<PathBuf>,
    pub key: Option<PathBuf>,
}

/// The raw `[acme]` table before validation.
#[derive(Debug, Default, Clone, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "kebab-case", deny_unknown_fields)]
pub struct RawAcme {
    #[serde(default)]
    pub domains: Vec<String>,
    #[serde(default)]
    pub contact: String,
    pub cache_dir: Option<PathBuf>,
    #[serde(default)]
    pub staging: bool,
}

/// One index's `[index.policy]` table, split into the ecosystem-neutral keys and the raw remainder
/// left for the index's ecosystem driver to compile.
///
/// An operator writes one flat policy block; the neutral engine claims its keys here, and every other
/// key is carried through untouched. Whether an unclaimed key is valid depends on the ecosystem, so
/// that verdict is the driver's at compile time, not this layer's.
#[derive(Debug, Default, Clone, PartialEq)]
pub struct RawPolicy {
    pub neutral: PolicyConfig,
    pub ecosystem: Table,
}

impl<'de> serde::Deserialize<'de> for RawPolicy {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        use serde::de::Error as _;
        let value = toml::Value::deserialize(deserializer)?;
        let table = value
            .as_table()
            .ok_or_else(|| D::Error::custom("[index.policy] must be a table"))?;
        let ecosystem = table
            .iter()
            .filter(|(key, _)| !PolicyConfig::KEYS.contains(&key.as_str()))
            .map(|(key, value)| (key.clone(), value.clone()))
            .collect();
        Ok(Self {
            neutral: value.try_into().map_err(D::Error::custom)?,
            ecosystem,
        })
    }
}

/// A raw `[[index]]` table before classification. `[[index.upstream]]`, `hosted`, or `layers` selects
/// the kind; index classification enforces that.
#[derive(Debug, Default, Clone, PartialEq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RawIndex {
    pub name: String,
    pub route: Option<String>,
    pub ecosystem: Option<String>,
    #[serde(default)]
    pub policy: RawPolicy,
    /// The `[index.settings]` table: this index's ecosystem-specific settings, carried raw for its
    /// driver to compile. Which keys are valid depends on the ecosystem, so this layer claims none.
    #[serde(default)]
    pub settings: Table,
    #[serde(default, rename = "upstream")]
    pub upstreams: Vec<RawUpstream>,
    pub fallback: Option<bool>,
    #[serde(default)]
    pub protected: Vec<String>,
    #[serde(default)]
    pub pins: BTreeMap<String, String>,
    pub upstream_concurrency: Option<usize>,
    pub offline: Option<bool>,
    pub prefetch: Option<RawPrefetchConfig>,
    pub hosted: Option<bool>,
    pub volatile: Option<bool>,
    pub layers: Option<Vec<String>>,
    pub write_target: Option<String>,
    pub anonymous_read: Option<bool>,
    /// The `[[index.access_token]]` tables: credentials clients present to peryx. The credentials peryx
    /// presents to an upstream live on each `[[index.upstream]]` source.
    #[serde(default, rename = "access_token")]
    pub tokens: Vec<RawToken>,
    #[serde(default, rename = "webhook")]
    pub webhooks: Vec<RawWebhook>,
}

/// One named source in an index's ordered `[[index.upstream]]` route.
#[derive(Debug, Default, Clone, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RawUpstream {
    pub name: String,
    pub url: String,
    pub artifact_url: Option<String>,
    pub username: Option<String>,
    pub password: Option<String>,
    pub password_file: Option<PathBuf>,
    pub password_env: Option<String>,
    pub token: Option<String>,
    pub token_file: Option<PathBuf>,
    pub token_env: Option<String>,
    pub credential_exec: Option<RawCredentialExec>,
    pub credential_refresh_secs: Option<u64>,
    pub credential_refresh_on_unauthorized: Option<bool>,
    pub credential_failure: Option<CredentialFailureMode>,
    pub ca_file: Option<PathBuf>,
    pub client_cert_file: Option<PathBuf>,
    pub client_key_file: Option<PathBuf>,
}

/// One short-lived upstream credential helper before command validation.
#[derive(Debug, Default, Clone, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RawCredentialExec {
    #[serde(default)]
    pub argv: Vec<String>,
    pub timeout_secs: Option<u64>,
    #[serde(default)]
    pub environment: Vec<String>,
    pub failure: Option<CredentialFailureMode>,
}

/// A raw `[[index.access_token]]` table: one named credential, its grant, and when it stops working.
#[derive(Debug, Default, Clone, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RawToken {
    pub name: String,
    pub secret: Option<String>,
    pub secret_file: Option<PathBuf>,
    /// Resource patterns the token may act on; empty means the whole index.
    #[serde(default)]
    pub resources: Vec<String>,
    #[serde(default)]
    pub actions: Vec<Action>,
    /// An RFC 3339 timestamp, for example `2027-01-01T00:00:00Z`.
    pub expires_at: Option<String>,
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
#[derive(Debug, Default, Clone, PartialEq, Eq, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct PartialRateLimitConfig {
    pub enabled: Option<bool>,
    pub max_clients: Option<u64>,
    pub trusted_proxies: Option<Vec<IpNet>>,
    pub listing: PartialRouteLimit,
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
