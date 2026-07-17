//! The raw deserialization schema: partial overlays and unclassified `[[index]]` tables.

use std::collections::BTreeMap;
use std::path::PathBuf;

use ipnet::IpNet;
use peryx_identity::Action;
use peryx_policy::PolicyConfig;
use serde::Deserialize;
use toml::Table;

use super::model::{JobsMode, LogFormat, LogSink, PrefetchConfig, PrefetchMode};

#[derive(Debug, Default, Clone, PartialEq, Eq, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct RawPrefetchConfig {
    pub mode: Option<PrefetchMode>,
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

impl RawPrefetchConfig {
    #[must_use]
    pub fn resolve(self) -> PrefetchConfig {
        let mode = self.mode.unwrap_or(PrefetchMode::Selected);
        PrefetchConfig {
            mode,
            packages: self.packages.unwrap_or_default(),
            requirements: self.requirements.unwrap_or_default(),
            include_wheels: self.include_wheels.unwrap_or(true),
            include_sdists: self.include_sdists.unwrap_or(true),
            python_tags: self.python_tags.unwrap_or_default(),
            abi_tags: self.abi_tags.unwrap_or_default(),
            platform_tags: self.platform_tags.unwrap_or_default(),
            max_file_size_bytes: self.max_file_size_bytes,
            metadata_only: self.metadata_only.unwrap_or(matches!(mode, PrefetchMode::MetadataOnly)),
        }
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
    pub offline: Option<bool>,
    pub read_only: Option<bool>,
    pub cache_ttl_secs: Option<i64>,
    pub hot_cache_bytes: Option<u64>,
    /// An operator-selected netrc file for upstream Basic credentials.
    pub netrc: Option<PathBuf>,
    /// Bound on stale-on-error serving, in seconds; `0` serves stale without limit.
    pub max_stale_secs: Option<i64>,
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
    pub replication: Option<RawReplication>,
    pub jobs: PartialJobsConfig,
}

/// The `[jobs]` half of [`PartialConfig`].
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct PartialJobsConfig {
    pub mode: Option<JobsMode>,
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
#[derive(Debug, Default, Clone, PartialEq, Eq, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct PartialAuthConfig {
    pub signing_key: Option<String>,
    pub signing_key_file: Option<PathBuf>,
    pub token_ttl_secs: Option<i64>,
    pub default_anonymous_read: Option<bool>,
    pub oidc_audience: Option<String>,
    #[serde(rename = "trusted_publisher")]
    pub trusted_publishers: Option<Vec<RawTrustedPublisher>>,
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RawTrustedPublisher {
    pub id: String,
    pub issuer: String,
    pub repository: String,
    pub subject: String,
    #[serde(default)]
    pub projects: Vec<String>,
    #[serde(default)]
    pub claims: BTreeMap<String, String>,
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

/// A raw `[[index]]` table before classification. `cached` or `[[index.upstream]]`, `hosted`, or
/// `layers` selects the kind; [`classify_index`](super::classify_index) enforces that.
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
    pub cached: Option<String>,
    #[serde(default, rename = "upstream")]
    pub upstreams: Vec<RawUpstream>,
    pub fallback: Option<bool>,
    #[serde(default)]
    pub protected: Vec<String>,
    #[serde(default)]
    pub pins: BTreeMap<String, String>,
    pub username: Option<String>,
    pub password: Option<String>,
    pub password_file: Option<PathBuf>,
    pub token: Option<String>,
    pub token_file: Option<PathBuf>,
    pub ca_file: Option<PathBuf>,
    pub client_cert_file: Option<PathBuf>,
    pub client_key_file: Option<PathBuf>,
    pub upstream_concurrency: Option<usize>,
    pub offline: Option<bool>,
    pub prefetch: Option<RawPrefetchConfig>,
    pub hosted: Option<bool>,
    pub upload_token: Option<String>,
    pub upload_token_file: Option<PathBuf>,
    pub volatile: Option<bool>,
    pub layers: Option<Vec<String>>,
    pub upload: Option<String>,
    pub anonymous_read: Option<bool>,
    /// The `[[index.access_token]]` tables: credentials clients present to peryx, as opposed to
    /// `token`, the credential peryx presents to this index's upstream.
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
    pub token: Option<String>,
    pub token_file: Option<PathBuf>,
    pub ca_file: Option<PathBuf>,
    pub client_cert_file: Option<PathBuf>,
    pub client_key_file: Option<PathBuf>,
}

/// A raw `[[index.access_token]]` table: one named credential, its grant, and when it stops working.
#[derive(Debug, Default, Clone, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RawToken {
    pub name: String,
    pub secret: Option<String>,
    pub secret_file: Option<PathBuf>,
    /// Project globs the token may act on; empty means the whole index.
    #[serde(default)]
    pub projects: Vec<String>,
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
