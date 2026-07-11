//! The raw deserialization schema: partial overlays and unclassified `[[index]]` tables.

use std::path::PathBuf;

use peryx_policy::PolicyConfig;
use serde::Deserialize;
use toml::Table;

use super::model::{LogFormat, LogSink, PrefetchConfig, PrefetchMode};

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
    pub offline: Option<bool>,
    pub cache_ttl_secs: Option<i64>,
    pub hot_cache_bytes: Option<u64>,
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

/// A raw `[[index]]` table before classification. Exactly one of `cached`, `hosted`, or `layers`
/// selects the kind; [`classify_index`](super::classify_index) enforces that.
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
    pub username: Option<String>,
    pub password: Option<String>,
    pub token: Option<String>,
    pub upstream_concurrency: Option<usize>,
    pub offline: Option<bool>,
    pub prefetch: Option<RawPrefetchConfig>,
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
