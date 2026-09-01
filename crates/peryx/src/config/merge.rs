use std::net::SocketAddr;
use std::path::PathBuf;
use std::time::Duration;

use peryx_driver::jobs::{Schedule, ScheduledJob};
use peryx_driver::rate_limit::{DEFAULT_UPSTREAM_CONCURRENCY, RateLimitConfig, RouteLimit};
use peryx_driver::serving::{JobConfig, JobIndexConfig};
use peryx_ha_distributed::DurabilityPolicy;
use peryx_identity::{ExternalGroup, ExternalGroupGrant, GrantScope, ProviderId};
use peryx_upstream::{CredentialFailure, ExecCredentialConfig, ExecCredentialConfigError};
use std::collections::{HashMap, HashSet};
use time::OffsetDateTime;
use time::format_description::well_known::Rfc3339;

use super::ConfigError;
use super::model::{
    AcmeConfig, AuthConfig, AvailabilityConfig, AvailabilityListenerConfig, AvailabilityListenerTls, BlobStorageConfig,
    Config, CredentialFailureMode, CredentialRefreshConfig, DEFAULT_REPLICA_PAGE_SIZE,
    DEFAULT_REPLICA_POLL_INTERVAL_SECS, DEFAULT_WRITE_ACK_DEADLINE_SECS, DcMember, DcMembership, DcRole, IndexConfig,
    IndexKind, JobsConfig, LdapBindConfig, LdapProviderConfig, LogConfig, MAX_TOKEN_TTL_SECS, OidcProviderConfig,
    ReplicationConfig, S3StorageConfig, SecretSource, TlsConfig, TokenConfig, UpstreamConfig, UpstreamRoutingConfig,
    UpstreamTlsConfig, WebhookConfig, WebhookSecret, WriteAckConfig, member_endpoint,
};
use super::raw::{
    PartialAuthConfig, PartialConfig, PartialJobsConfig, PartialLogConfig, PartialRateLimitConfig, PartialRouteLimit,
    RawAcme, RawAvailability, RawAvailabilityListener, RawBlobStorage, RawCredentialExec, RawDcMember,
    RawExternalGroupGrant, RawIndex, RawJobSchedule, RawLdapMode, RawLdapProvider, RawOidcProvider, RawReadThrough,
    RawReplication, RawTls, RawToken, RawUpstream, RawWebhook, RawWriteAck, RawWriteAckPolicy,
};
use peryx_core::Ecosystem;
use peryx_ha::{AvailabilityMode, MemberEndpoint};
use peryx_ha_distributed::read_through::{DEFAULT_READ_THROUGH_LIMITS, ReadThroughLimits};
use peryx_ha_distributed::{CircuitConfig, ReconnectPolicy};

impl Config {
    /// # Errors
    /// Returns [`ConfigError::Index`] if the partial defines indexes but one is not classifiable as a
    /// cached, hosted, or virtual.
    pub fn apply(self, partial: PartialConfig) -> Result<Self, ConfigError> {
        self.apply_with_plugins(partial, &crate::compiled_plugins())
    }

    /// # Errors
    /// Returns an error when the source contains invalid index, TLS, authentication, availability,
    /// storage, or job settings.
    pub fn apply_with_plugins(
        mut self,
        partial: PartialConfig,
        plugins: &peryx_plugin_registry::PluginRegistry,
    ) -> Result<Self, ConfigError> {
        if let Some(host) = partial.host {
            self.host = host;
        }
        if let Some(port) = partial.port {
            self.port = port;
        }
        if let Some(data_dir) = partial.data_dir {
            self.data_dir = data_dir;
        }
        if let Some(writer_identity) = partial.writer_identity {
            self.writer_identity = Some(writer_identity);
        }
        if let Some(node_identity) = partial.node_identity {
            self.node_identity = Some(node_identity);
        }
        if let Some(offline) = partial.offline {
            self.offline = offline;
        }
        if let Some(read_only) = partial.read_only {
            self.read_only = read_only;
        }
        if let Some(max_stale_secs) = partial.max_stale_secs {
            self.max_stale_secs = max_stale_secs;
        }
        if let Some(usage_retention_days) = partial.usage_retention_days {
            self.usage_retention_days = Some(usage_retention_days);
        }
        if let Some(hot_cache_bytes) = partial.hot_cache_bytes {
            self.hot_cache_bytes = hot_cache_bytes;
        }
        if let Some(netrc) = partial.netrc {
            self.netrc = Some(netrc);
        }
        if let Some(cache_ttl_secs) = partial.cache_ttl_secs {
            self.cache_ttl_secs = cache_ttl_secs;
        }
        if let Some(raw) = partial.indexes {
            self.indexes = raw
                .into_iter()
                .map(|index| classify_index(index, plugins))
                .collect::<Result<_, _>>()?;
        }
        if partial.tls.is_some() || partial.acme.is_some() {
            self.tls = classify_tls(partial.tls, partial.acme)?;
        }
        self.log = self.log.apply(partial.log);
        self.rate_limit = apply_rate_limit(self.rate_limit, partial.rate_limit);
        self.auth = self.auth.apply(partial.auth)?;
        if let Some(mut availability) = partial.availability {
            let group = availability.group.take();
            let members = availability.members.take();
            let listener = availability.listener.take();
            let write_ack = availability.write_ack.take();
            let read_through = availability.read_through.take();
            self.availability = classify_availability(availability)?;
            self.dc_membership = classify_membership(self.availability.mode(), group, members)?;
            self.availability_listener = classify_listener(self.availability.mode(), listener)?;
            self.write_ack = classify_write_ack(self.availability.mode(), write_ack)?;
            self.read_through = classify_read_through(self.availability.mode(), read_through)?;
        }
        if let Some(blob) = partial.blob {
            self.blob = classify_blob(blob)?;
        }
        self.jobs = self
            .jobs
            .apply(partial.jobs, &self.indexes, self.availability.mode(), plugins)?;
        self.jobs.validate_mode(self.availability.mode())?;
        Ok(self)
    }
}

impl JobsConfig {
    fn apply(
        mut self,
        partial: PartialJobsConfig,
        indexes: &[IndexConfig],
        availability: AvailabilityMode,
        plugins: &peryx_plugin_registry::PluginRegistry,
    ) -> Result<Self, ConfigError> {
        if let Some(mode) = partial.mode {
            self.mode = mode;
        }
        if let Some(schedules) = partial.schedules {
            self.schedules = schedules
                .into_iter()
                .enumerate()
                .map(|(index, raw)| classify_schedule(index, &raw, indexes, availability, plugins))
                .collect::<Result<_, _>>()?;
        }
        Ok(self)
    }
}

fn classify_schedule(
    index: usize,
    raw: &RawJobSchedule,
    indexes: &[IndexConfig],
    availability: AvailabilityMode,
    plugins: &peryx_plugin_registry::PluginRegistry,
) -> Result<Schedule, ConfigError> {
    if availability == AvailabilityMode::None && peryx_ha_distributed::is_scheduled_job_kind(&raw.job) {
        return Err(ConfigError::Jobs {
            index,
            reason: "`none` availability cannot schedule distributed jobs",
        });
    }
    let distributed = peryx_ha_distributed::compile_scheduled_job(&raw.job, &raw.settings);
    let Some(interval) = std::num::NonZeroU64::new(raw.interval_secs) else {
        return Err(ConfigError::Jobs {
            index,
            reason: "`interval_secs` must be positive",
        });
    };
    let job = match raw.job.as_str() {
        "cache_maintenance" => {
            if !raw.settings.is_empty() {
                return Err(ConfigError::Jobs {
                    index,
                    reason: "cache maintenance accepts no job-specific fields",
                });
            }
            ScheduledJob::CacheMaintenance
        }
        _ => match distributed {
            Some(result) => result.map_err(|reason| ConfigError::Jobs { index, reason })?,
            None => compile_plugin_job(index, &raw.job, &raw.settings, indexes, plugins)?,
        },
    };
    Ok(Schedule {
        job,
        interval: Duration::from_secs(interval.get()),
    })
}

fn compile_plugin_job(
    index: usize,
    kind: &str,
    settings: &toml::Table,
    indexes: &[IndexConfig],
    plugins: &peryx_plugin_registry::PluginRegistry,
) -> Result<ScheduledJob, ConfigError> {
    let configured = indexes
        .iter()
        .map(|configured| {
            let (cached, offline, upstreams) = match &configured.kind {
                IndexKind::Cached { routing, offline, .. } => (
                    true,
                    *offline,
                    routing
                        .upstreams
                        .iter()
                        .map(|upstream| upstream.name.as_str())
                        .collect(),
                ),
                IndexKind::Hosted { .. } | IndexKind::Virtual { .. } => (false, false, Vec::new()),
            };
            JobIndexConfig {
                name: &configured.name,
                ecosystem: configured.ecosystem.clone(),
                cached,
                offline,
                upstreams,
            }
        })
        .collect::<Vec<_>>();
    let plugins = plugins
        .activate(indexes.iter().map(|configured| configured.ecosystem.clone()))
        .expect("classified indexes use installed ecosystems");
    plugins
        .compile_job(JobConfig {
            kind,
            settings,
            indexes: &configured,
        })
        .map(ScheduledJob::Plugin)
        .map_err(|reason| ConfigError::Plugin(format!("jobs schedule [{index}]: {reason}")))
}

fn classify_blob(raw: RawBlobStorage) -> Result<BlobStorageConfig, ConfigError> {
    let RawBlobStorage::S3 {
        endpoint,
        bucket,
        region,
        prefix,
        path_style,
        timeout_secs,
        max_retries,
        multipart_threshold_bytes,
        part_size_bytes,
        upload_concurrency,
        conditional_writes,
        checksum_writes,
    } = raw
    else {
        return Ok(BlobStorageConfig::Filesystem);
    };
    let config = S3StorageConfig {
        endpoint,
        bucket,
        region,
        prefix: prefix.unwrap_or_default().trim_matches('/').to_owned(),
        path_style: path_style.unwrap_or(false),
        request_timeout: Duration::from_secs(timeout_secs.unwrap_or(30)),
        max_retries: max_retries.unwrap_or(3),
        multipart_threshold: multipart_threshold_bytes.unwrap_or(16 << 20),
        part_size: part_size_bytes.unwrap_or(16 << 20),
        upload_concurrency: upload_concurrency.unwrap_or(4),
        conditional_writes: conditional_writes.unwrap_or(true),
        checksum_writes: checksum_writes.unwrap_or(true),
    };
    // Validate through the backend's own builder so config and runtime agree on what is usable.
    peryx_storage::blob::S3Config::new((&config).into()).map_err(|error| ConfigError::Blob {
        reason: error.to_string(),
    })?;
    Ok(BlobStorageConfig::S3(config))
}

/// Resolve the `[availability]` table into a mode and its topology. `none` carries no replication, so
/// pairing it with a role is a configuration error; `dc` and `ha` require the role that carries them.
fn classify_availability(raw: RawAvailability) -> Result<AvailabilityConfig, ConfigError> {
    match (raw.mode.unwrap_or_default(), raw.replication) {
        (AvailabilityMode::None, None) => Ok(AvailabilityConfig::None),
        (AvailabilityMode::None, Some(_)) => Err(ConfigError::Availability {
            reason: "`none` mode configures no replication; select `dc` or `ha` to add a role",
        }),
        (AvailabilityMode::Dc, Some(role)) => Ok(AvailabilityConfig::Dc(classify_replication(role)?)),
        (AvailabilityMode::Ha, Some(role)) => Ok(AvailabilityConfig::Ha(classify_replication(role)?)),
        (AvailabilityMode::Dc | AvailabilityMode::Ha, None) => Err(ConfigError::Availability {
            reason: "`dc` and `ha` modes need a `[availability.replication]` role",
        }),
    }
}

/// Resolve `[availability.write_ack]`. `none` mode acknowledges from local durability and rejects a
/// stronger quorum override; `dc` and `ha` default to a majority quorum and accept `local`, `majority`,
/// or `everywhere`. A zero deadline is rejected so a write always has a positive window to prove durable.
fn classify_write_ack(mode: AvailabilityMode, raw: Option<RawWriteAck>) -> Result<WriteAckConfig, ConfigError> {
    let raw = raw.unwrap_or_default();
    let deadline_secs = raw.deadline_secs.unwrap_or(DEFAULT_WRITE_ACK_DEADLINE_SECS);
    if deadline_secs == 0 {
        return Err(ConfigError::Availability {
            reason: "`write_ack.deadline-secs` must be positive",
        });
    }
    let policy = match (mode, raw.policy) {
        (AvailabilityMode::None, Some(_)) => {
            return Err(ConfigError::Availability {
                reason: "`none` mode acknowledges from local durability; remove `write_ack.policy`",
            });
        }
        (AvailabilityMode::None, None) | (_, Some(RawWriteAckPolicy::Local)) => DurabilityPolicy::Local,
        (AvailabilityMode::Dc | AvailabilityMode::Ha, None | Some(RawWriteAckPolicy::Majority)) => {
            DurabilityPolicy::Majority
        }
        (_, Some(RawWriteAckPolicy::Everywhere)) => DurabilityPolicy::Everywhere,
    };
    Ok(WriteAckConfig {
        policy,
        deadline: Duration::from_secs(deadline_secs),
    })
}

fn classify_replication(raw: RawReplication) -> Result<ReplicationConfig, ConfigError> {
    let required_token = |token, token_file| {
        let token = secret_source(token, token_file).map_err(|reason| ConfigError::Replication { reason })?;
        match token {
            Some(SecretSource::Literal(token)) if token.trim().is_empty() => Err(ConfigError::Replication {
                reason: "`token` must not be empty",
            }),
            Some(token) => Ok(token),
            None => Err(ConfigError::Replication {
                reason: "role needs a `token` or a `token_file`",
            }),
        }
    };
    match raw {
        RawReplication::Primary {
            source,
            token,
            token_file,
        } => {
            if source.trim().is_empty() {
                return Err(ConfigError::Replication {
                    reason: "primary `source` must not be empty",
                });
            }
            Ok(ReplicationConfig::Primary {
                source,
                token: required_token(token, token_file)?,
            })
        }
        RawReplication::Replica {
            upstream,
            token,
            token_file,
            poll_interval_secs,
            page_size,
        } => {
            if upstream.trim().is_empty() {
                return Err(ConfigError::Replication {
                    reason: "replica `upstream` must not be empty",
                });
            }
            let poll_interval_secs = poll_interval_secs.unwrap_or(DEFAULT_REPLICA_POLL_INTERVAL_SECS);
            if poll_interval_secs == 0 {
                return Err(ConfigError::Replication {
                    reason: "`poll_interval_secs` must be positive",
                });
            }
            let page_size = page_size.unwrap_or(DEFAULT_REPLICA_PAGE_SIZE);
            let Some(page_size) = std::num::NonZeroUsize::new(page_size) else {
                return Err(ConfigError::Replication {
                    reason: "`page_size` must be positive",
                });
            };
            if page_size.get() > peryx_ha_distributed::DEFAULT_MAX_CHANGE_PAGE_SIZE {
                return Err(ConfigError::Replication {
                    reason: "`page_size` exceeds the primary limit",
                });
            }
            Ok(ReplicationConfig::Replica {
                upstream,
                token: required_token(token, token_file)?,
                poll_interval: Duration::from_secs(poll_interval_secs),
                page_size,
            })
        }
    }
}

/// Resolve a `[[availability.member]]` roster into a validated [`DcMembership`], or `None` when no
/// roster is configured. A roster is meaningful only under `dc` or `ha` mode; the runtime never probes
/// a member's reachability here, so an unreachable configured peer is a valid topology, not an error.
fn classify_membership(
    mode: AvailabilityMode,
    group: Option<String>,
    members: Option<Vec<RawDcMember>>,
) -> Result<Option<DcMembership>, ConfigError> {
    let Some(members) = members else {
        return match group {
            Some(_) => Err(membership_error("`group` needs at least one `[[availability.member]]`")),
            None => Ok(None),
        };
    };
    if !matches!(mode, AvailabilityMode::Dc | AvailabilityMode::Ha) {
        return Err(membership_error("a member roster requires `dc` or `ha` mode"));
    }
    let group = non_blank(
        group.ok_or_else(|| membership_error("a member roster needs a `group` identity"))?,
        "group",
    )?;
    Ok(Some(DcMembership {
        members: resolve_members(mode, &group, members)?,
        group,
    }))
}

/// The loopback socket the availability listener binds when `[availability.listener]` names none, so the
/// control plane defaults to a private bind an operator must widen deliberately.
const DEFAULT_AVAILABILITY_LISTENER_BIND: &str = "127.0.0.1:4460";

/// Resolve the `[availability.listener]` table into a validated listener, or `None` when no table is
/// configured. Single-node `none` opens no listener, so pairing it with a table is a configuration
/// error; a non-loopback bind must terminate TLS or opt in to plaintext so the control plane is never
/// exposed to the network unencrypted by omission.
fn classify_listener(
    mode: AvailabilityMode,
    raw: Option<RawAvailabilityListener>,
) -> Result<Option<AvailabilityListenerConfig>, ConfigError> {
    let Some(raw) = raw else {
        return if mode == AvailabilityMode::Ha {
            Err(ConfigError::Availability {
                reason: "`ha` mode requires `[availability.listener]` for ownership control commands",
            })
        } else {
            Ok(None)
        };
    };
    if mode == AvailabilityMode::None {
        return Err(ConfigError::Availability {
            reason: "`none` mode opens no availability listener; select `dc` or `ha` first",
        });
    }
    let bind: SocketAddr = raw
        .bind
        .as_deref()
        .unwrap_or(DEFAULT_AVAILABILITY_LISTENER_BIND)
        .parse()
        .map_err(|_| ConfigError::Availability {
            reason: "`[availability.listener] bind` must be a `host:port` socket address",
        })?;
    let tls = raw.tls.map(classify_listener_tls).transpose()?;
    let allow_remote_plaintext = raw.allow_remote_plaintext.unwrap_or(false);
    if tls.is_none() && !allow_remote_plaintext && !bind.ip().is_loopback() {
        return Err(ConfigError::Availability {
            reason: "a non-loopback availability listener needs `[availability.listener.tls]` or \
                     `allow-remote-plaintext = true`",
        });
    }
    Ok(Some(AvailabilityListenerConfig {
        bind,
        tls,
        allow_remote_plaintext,
    }))
}

/// Resolve the `[availability.read-through]` table into the bounds a remote-placement read-through runs
/// under, or `None` when no table is configured and the runtime keeps its built-in defaults. Single-node
/// `none` mode streams no remote placements, so pairing it with a table is a configuration error. Each
/// absent field falls back to its default; the retry sub-table, when present, sets the whole schedule.
fn classify_read_through(
    mode: AvailabilityMode,
    raw: Option<RawReadThrough>,
) -> Result<Option<ReadThroughLimits>, ConfigError> {
    let Some(raw) = raw else { return Ok(None) };
    if mode == AvailabilityMode::None {
        return Err(ConfigError::Availability {
            reason: "`none` mode streams no remote placements; select `dc` or `ha` first",
        });
    }
    let base = DEFAULT_READ_THROUGH_LIMITS;
    Ok(Some(ReadThroughLimits {
        concurrency: raw.concurrency.unwrap_or(base.concurrency),
        per_fetch_bytes: raw.per_fetch_bytes.unwrap_or(base.per_fetch_bytes),
        chunk_bytes: raw.chunk_bytes.unwrap_or(base.chunk_bytes),
        max_fanout: raw.max_fanout.unwrap_or(base.max_fanout),
        circuit: CircuitConfig {
            trip_after: raw.trip_after.unwrap_or(base.circuit.trip_after),
            cooldown: raw.cooldown_secs.map_or(base.circuit.cooldown, Duration::from_secs),
            probe_timeout: raw
                .probe_timeout_secs
                .map_or(base.circuit.probe_timeout, |seconds| Duration::from_secs(seconds.get())),
        },
        policy: raw.retry.map_or(base.policy, |retry| {
            ReconnectPolicy::new(
                Duration::from_millis(retry.base_ms),
                retry.multiplier,
                Duration::from_secs(retry.max_delay_secs),
                retry.max_attempts,
            )
        }),
    }))
}

fn classify_listener_tls(raw: RawTls) -> Result<AvailabilityListenerTls, ConfigError> {
    match (raw.cert, raw.key) {
        (Some(cert), Some(key)) => Ok(AvailabilityListenerTls { cert, key }),
        _ => Err(ConfigError::Availability {
            reason: "`[availability.listener.tls]` needs both `cert` and `key`",
        }),
    }
}

fn resolve_members(mode: AvailabilityMode, group: &str, raw: Vec<RawDcMember>) -> Result<Vec<DcMember>, ConfigError> {
    let mut members = Vec::with_capacity(raw.len());
    let (mut writers, mut replicas) = (0usize, 0usize);
    for member in raw {
        let node = non_blank(member.node, "member `node`")?;
        if node == group {
            return Err(membership_error(format!(
                "node identity {node:?} collides with the group identity"
            )));
        }
        match member.role {
            DcRole::Writer => writers += 1,
            DcRole::Replica => replicas += 1,
        }
        members.push(DcMember {
            node,
            dc: non_blank(member.dc, "member `dc`")?,
            address: member_address(&member.address)?,
            role: member.role,
        });
    }
    reject_duplicate(members.iter().map(|member| member.node.as_str()), "node identity")?;
    reject_duplicate(
        members.iter().map(|member| member.address.as_str()),
        "advertised address",
    )?;
    if mode == AvailabilityMode::Ha {
        reject_duplicate_datacenter(&members)?;
    }
    if writers == 0 {
        return Err(membership_error("a datacenter group needs exactly one writer"));
    }
    if writers > 1 {
        return Err(membership_error("a datacenter group allows only one writer"));
    }
    if replicas == 0 {
        return Err(membership_error(
            "a datacenter group needs at least one configured replica",
        ));
    }
    Ok(members)
}

fn reject_duplicate_datacenter(members: &[DcMember]) -> Result<(), ConfigError> {
    let mut nodes_by_datacenter = HashMap::new();
    for member in members {
        if let Some(first_node) = nodes_by_datacenter.insert(member.dc.as_str(), member.node.as_str()) {
            return Err(membership_error(format!(
                "duplicate datacenter {:?} for node identities {first_node:?} and {:?} in `ha` mode",
                member.dc, member.node
            )));
        }
    }
    Ok(())
}

fn reject_duplicate<'a>(values: impl Iterator<Item = &'a str>, kind: &str) -> Result<(), ConfigError> {
    let mut seen = HashSet::new();
    for value in values {
        if !seen.insert(value) {
            return Err(membership_error(format!("duplicate {kind} {value:?}")));
        }
    }
    Ok(())
}

fn non_blank(value: String, field: &str) -> Result<String, ConfigError> {
    if value.trim().is_empty() {
        return Err(membership_error(format!("{field} must not be empty")));
    }
    Ok(value)
}

/// Stores the canonical rendering so duplicate detection and every peer transport compare one spelling of
/// each member.
fn member_address(value: &str) -> Result<String, ConfigError> {
    member_endpoint(value).map(MemberEndpoint::into_string)
}

fn membership_error(reason: impl Into<String>) -> ConfigError {
    ConfigError::DcMembership { reason: reason.into() }
}

fn classify_index(raw: RawIndex, plugins: &peryx_plugin_registry::PluginRegistry) -> Result<IndexConfig, ConfigError> {
    let mut raw = raw;
    let route = raw.route.clone().unwrap_or_else(|| raw.name.clone());
    let ecosystem = match &raw.ecosystem {
        Some(value) => {
            let ecosystem = value.parse::<Ecosystem>().map_err(|_| ConfigError::Index {
                name: raw.name.clone(),
                reason: "unknown ecosystem",
            })?;
            if !plugins.is_installed(&ecosystem) {
                return Err(ConfigError::Index {
                    name: raw.name.clone(),
                    reason: "unknown ecosystem",
                });
            }
            ecosystem
        }
        None => plugins.default_ecosystem(),
    };
    let kind = classify_index_kind(&mut raw)?;
    let tokens = classify_tokens(&raw.name, raw.tokens)?;
    Ok(IndexConfig {
        name: raw.name,
        route,
        ecosystem,
        kind,
        anonymous_read: raw.anonymous_read,
        tokens,
        policy: raw.policy.neutral,
        ecosystem_policy: raw.policy.ecosystem,
        ecosystem_settings: raw.settings,
        webhooks: raw
            .webhooks
            .into_iter()
            .map(classify_webhook)
            .collect::<Result<_, _>>()?,
    })
}

fn classify_index_kind(raw: &mut RawIndex) -> Result<IndexKind, ConfigError> {
    validate_index_kind(raw)?;
    if let Some(layers) = raw.layers.take() {
        return Ok(IndexKind::Virtual {
            layers,
            write_target: raw.write_target.take(),
        });
    }
    if !raw.upstreams.is_empty() {
        return classify_routed_cached(raw);
    }
    if raw.hosted == Some(true) {
        return Ok(IndexKind::Hosted {
            volatile: raw.volatile.unwrap_or(true),
        });
    }
    Err(ConfigError::Index {
        name: raw.name.clone(),
        reason: "index needs one of `[[index.upstream]]`, `hosted`, or `layers`",
    })
}

fn validate_index_kind(raw: &RawIndex) -> Result<(), ConfigError> {
    if raw.write_target.is_some() && raw.layers.is_none() {
        return Err(ConfigError::Index {
            name: raw.name.clone(),
            reason: "`write_target` requires `layers`",
        });
    }
    if raw.layers.is_some() && !raw.upstreams.is_empty() {
        return Err(ConfigError::Index {
            name: raw.name.clone(),
            reason: "`layers` and `[[index.upstream]]` are mutually exclusive",
        });
    }
    if raw.upstreams.is_empty() && (raw.fallback.is_some() || !raw.protected.is_empty() || !raw.pins.is_empty()) {
        return Err(ConfigError::Index {
            name: raw.name.clone(),
            reason: "`fallback`, `protected`, and `pins` require `[[index.upstream]]`",
        });
    }
    Ok(())
}

fn classify_routed_cached(raw: &mut RawIndex) -> Result<IndexKind, ConfigError> {
    let upstreams = std::mem::take(&mut raw.upstreams)
        .into_iter()
        .map(|upstream| classify_upstream(&raw.name, upstream))
        .collect::<Result<Vec<_>, _>>()?;
    Ok(IndexKind::Cached {
        routing: UpstreamRoutingConfig {
            upstreams,
            fallback: raw.fallback.unwrap_or(true),
            protected: std::mem::take(&mut raw.protected),
            pins: std::mem::take(&mut raw.pins),
        },
        upstream_concurrency: raw.upstream_concurrency.unwrap_or(DEFAULT_UPSTREAM_CONCURRENCY),
        offline: raw.offline.unwrap_or(false),
        prefetch: Box::new(raw.prefetch.take().unwrap_or_default().resolve()),
    })
}

fn classify_upstream(index: &str, raw: RawUpstream) -> Result<UpstreamConfig, ConfigError> {
    if raw
        .trusted_hosts
        .iter()
        .any(|host| host.is_empty() || url::Host::parse(host).is_err())
    {
        return Err(ConfigError::Index {
            name: index.to_owned(),
            reason: "`trusted_hosts` entries must be hostnames or IP literals",
        });
    }
    let password = upstream_secret_source(raw.password, raw.password_file, raw.password_env).map_err(|reason| {
        ConfigError::Index {
            name: index.to_owned(),
            reason,
        }
    })?;
    let token =
        upstream_secret_source(raw.token, raw.token_file, raw.token_env).map_err(|reason| ConfigError::Index {
            name: index.to_owned(),
            reason,
        })?;
    let static_credentials = raw.username.is_some() || password.is_some() || token.is_some();
    let refresh_controls = raw.credential_refresh_secs.is_some()
        || raw.credential_refresh_on_unauthorized.is_some()
        || raw.credential_failure.is_some();
    let credential_exec = credential_exec(index, raw.credential_exec, static_credentials, refresh_controls)?;
    let credential_refresh = credential_refresh(
        index,
        token.as_ref().or_else(|| raw.username.as_ref().and(password.as_ref())),
        raw.credential_refresh_secs,
        raw.credential_refresh_on_unauthorized,
        raw.credential_failure,
    )?;
    Ok(UpstreamConfig {
        name: raw.name,
        url: raw.url,
        artifact_url: raw.artifact_url,
        trusted_hosts: raw.trusted_hosts,
        username: raw.username,
        credential_refresh,
        password,
        token,
        credential_exec,
        tls: upstream_tls(index, raw.ca_file, raw.client_cert_file, raw.client_key_file)?,
    })
}

fn credential_exec(
    index: &str,
    raw: Option<RawCredentialExec>,
    static_credential: bool,
    refresh_controls: bool,
) -> Result<Option<ExecCredentialConfig>, ConfigError> {
    let Some(raw) = raw else { return Ok(None) };
    if static_credential {
        return Err(ConfigError::Index {
            name: index.to_owned(),
            reason: "`credential_exec` is mutually exclusive with username, password, and token settings",
        });
    }
    if refresh_controls {
        return Err(ConfigError::Index {
            name: index.to_owned(),
            reason: "`credential_exec` controls its own expiry and failure behavior",
        });
    }
    ExecCredentialConfig::new(
        raw.argv,
        Duration::from_secs(raw.timeout_secs.unwrap_or(30)),
        raw.environment,
        match raw.failure.unwrap_or_default() {
            CredentialFailureMode::Fail => CredentialFailure::Fail,
            CredentialFailureMode::Anonymous => CredentialFailure::Anonymous,
        },
    )
    .map(Some)
    .map_err(|error| ConfigError::Index {
        name: index.to_owned(),
        reason: exec_credential_error(error),
    })
}

const fn exec_credential_error(error: ExecCredentialConfigError) -> &'static str {
    match error {
        ExecCredentialConfigError::EmptyArgv => "`credential_exec.argv` must not be empty",
        ExecCredentialConfigError::RelativeExecutable => {
            "`credential_exec.argv` must start with an absolute executable path"
        }
        ExecCredentialConfigError::ArgumentNul => "`credential_exec.argv` contains a null byte",
        ExecCredentialConfigError::ArgvLimit => "`credential_exec.argv` exceeds its item or byte limit",
        ExecCredentialConfigError::EnvironmentLimit => "`credential_exec.environment` exceeds its item limit",
        ExecCredentialConfigError::EnvironmentName => "`credential_exec.environment` contains an invalid name",
        ExecCredentialConfigError::Timeout => "`credential_exec.timeout_secs` must be between 1 and 300",
    }
}

fn credential_refresh(
    index: &str,
    source: Option<&SecretSource>,
    interval_secs: Option<u64>,
    on_unauthorized: Option<bool>,
    failure: Option<CredentialFailureMode>,
) -> Result<Option<CredentialRefreshConfig>, ConfigError> {
    let Some(interval_secs) = interval_secs else {
        if on_unauthorized.is_some() || failure.is_some() {
            return Err(ConfigError::Index {
                name: index.to_owned(),
                reason: "`credential_refresh_secs` is required for credential refresh controls",
            });
        }
        return Ok(None);
    };
    if interval_secs == 0 {
        return Err(ConfigError::Index {
            name: index.to_owned(),
            reason: "`credential_refresh_secs` must be positive",
        });
    }
    if !source.is_some_and(SecretSource::supports_refresh) {
        return Err(ConfigError::Index {
            name: index.to_owned(),
            reason: "credential refresh requires `token_file`/`token_env` or `username` with `password_file`/`password_env`",
        });
    }
    Ok(Some(CredentialRefreshConfig {
        interval: Duration::from_secs(interval_secs),
        on_unauthorized: on_unauthorized.unwrap_or(true),
        failure: failure.unwrap_or_default(),
    }))
}

fn upstream_tls(
    index: &str,
    ca_file: Option<PathBuf>,
    client_cert_file: Option<PathBuf>,
    client_key_file: Option<PathBuf>,
) -> Result<UpstreamTlsConfig, ConfigError> {
    if client_cert_file.is_some() != client_key_file.is_some() {
        return Err(ConfigError::Index {
            name: index.to_owned(),
            reason: "`client_cert_file` and `client_key_file` must be configured together",
        });
    }
    Ok(UpstreamTlsConfig {
        ca_file,
        client_cert_file,
        client_key_file,
    })
}

impl AuthConfig {
    fn apply(self, partial: PartialAuthConfig) -> Result<Self, ConfigError> {
        let signing_key = secret_source(partial.signing_key, partial.signing_key_file)
            .map_err(|reason| ConfigError::Auth { reason })?;
        let token_ttl_secs = partial.token_ttl_secs.unwrap_or(self.token_ttl_secs);
        if token_ttl_secs <= 0 {
            return Err(ConfigError::Auth {
                reason: "`token_ttl_secs` must be positive",
            });
        }
        if token_ttl_secs > MAX_TOKEN_TTL_SECS {
            return Err(ConfigError::Auth {
                reason: "`token_ttl_secs` must not exceed 86400 (one day)",
            });
        }
        let ldap_providers = partial.ldap_providers.map_or(Ok(self.ldap_providers), |providers| {
            providers.into_iter().map(classify_ldap_provider).collect()
        })?;
        let oidc_providers = partial.oidc_providers.map_or(Ok(self.oidc_providers), |providers| {
            providers.into_iter().map(classify_oidc_provider).collect()
        })?;
        let mut extensions = self.extensions;
        extensions.extend(partial.extensions);
        Ok(Self {
            signing_key: signing_key.or(self.signing_key),
            token_ttl_secs,
            default_anonymous_read: partial.default_anonymous_read.unwrap_or(self.default_anonymous_read),
            ldap_providers,
            oidc_providers,
            extensions,
        })
    }
}

fn classify_oidc_provider(mut raw: RawOidcProvider) -> Result<OidcProviderConfig, ConfigError> {
    let provider_id = raw.id.clone();
    let id = ProviderId::new(&provider_id).map_err(|_| ConfigError::OidcProvider {
        id: provider_id.clone(),
        reason: "invalid provider ID",
    })?;
    let error = |reason| ConfigError::OidcProvider {
        id: provider_id.clone(),
        reason,
    };
    let issuer_url = secure_https(&raw.issuer).ok_or_else(|| error("`issuer` must be an https URL"))?;
    if !issuer_url.username().is_empty()
        || issuer_url.password().is_some()
        || issuer_url.query().is_some()
        || issuer_url.fragment().is_some()
        || (issuer_url.as_str() != raw.issuer
            && !(issuer_url.path() == "/" && issuer_url[..url::Position::BeforePath] == raw.issuer))
    {
        return Err(error("`issuer` must preserve its configured https URL"));
    }
    let redirect_uri = secure_https(&raw.redirect_uri).ok_or_else(|| error("`redirect_uri` must be an https URL"))?;
    if raw.client_id.trim().is_empty() {
        return Err(error("`client_id` must not be empty"));
    }
    let client_secret = upstream_secret_source(
        raw.client_secret.take(),
        raw.client_secret_file.take(),
        raw.client_secret_env.take(),
    )
    .map_err(error)?;
    if matches!(&client_secret, Some(SecretSource::Literal(secret)) if secret.is_empty()) {
        return Err(error("`client_secret` must not be empty"));
    }
    let subject_claim = claim(raw.subject_claim, "sub").ok_or_else(|| error("`subject_claim` must not be empty"))?;
    let display_name_claim =
        claim(raw.display_name_claim, "name").ok_or_else(|| error("`display_name_claim` must not be empty"))?;
    if raw.groups_claim.as_ref().is_some_and(String::is_empty) {
        return Err(error("`groups_claim` must not be empty"));
    }
    let request_timeout = std::num::NonZeroU64::new(raw.request_timeout_secs.unwrap_or(10))
        .ok_or_else(|| error("`request_timeout_secs` must be positive"))?;
    if raw.trusted_endpoint_hosts.iter().any(|host| host.trim().is_empty()) {
        return Err(error("`trusted_endpoint_hosts` entries must not be empty"));
    }
    let group_mappings = raw
        .group_mappings
        .into_iter()
        .map(|mapping| classify_group_mapping(mapping, || error("group mapping has an invalid group")))
        .collect::<Result<_, _>>()?;
    Ok(OidcProviderConfig {
        id,
        issuer: raw.issuer,
        client_id: raw.client_id,
        client_secret,
        redirect_uri,
        scopes: raw.scopes,
        subject_claim,
        display_name_claim,
        groups_claim: raw.groups_claim,
        clock_skew: Duration::from_secs(raw.clock_skew_secs.unwrap_or(60)),
        request_timeout: Duration::from_secs(request_timeout.get()),
        trusted_endpoint_hosts: raw.trusted_endpoint_hosts,
        group_mappings,
    })
}

fn secure_https(value: &str) -> Option<url::Url> {
    match url::Url::parse(value) {
        Ok(url) if url.scheme() == "https" && url.host_str().is_some() => Some(url),
        Ok(_) | Err(_) => None,
    }
}

fn claim(value: Option<String>, default: &str) -> Option<String> {
    match value {
        Some(claim) if claim.is_empty() => None,
        Some(claim) => Some(claim),
        None => Some(default.to_owned()),
    }
}

fn classify_ldap_provider(mut raw: RawLdapProvider) -> Result<LdapProviderConfig, ConfigError> {
    let provider_id = raw.id.clone();
    let id = ProviderId::new(&provider_id).map_err(|_| ConfigError::LdapProvider {
        id: provider_id.clone(),
        reason: "invalid provider ID",
    })?;
    let error = |reason| ConfigError::LdapProvider {
        id: provider_id.clone(),
        reason,
    };
    let url = url::Url::parse(&raw.url).map_err(|_| error("`url` must be a valid LDAP URL"))?;
    let bind = match raw.mode {
        RawLdapMode::DirectBind => {
            if raw.username_attribute.is_some()
                || raw.bind_dn.is_some()
                || raw.bind_password.is_some()
                || raw.bind_password_file.is_some()
                || raw.bind_password_env.is_some()
            {
                return Err(error("direct bind accepts only `dn_attribute` bind fields"));
            }
            LdapBindConfig::Direct {
                dn_attribute: raw
                    .dn_attribute
                    .take()
                    .ok_or_else(|| error("direct bind requires `dn_attribute`"))?,
            }
        }
        RawLdapMode::ServiceSearch => {
            if raw.dn_attribute.is_some() {
                return Err(error("service search does not accept `dn_attribute`"));
            }
            let bind_password = upstream_secret_source(
                raw.bind_password.take(),
                raw.bind_password_file.take(),
                raw.bind_password_env.take(),
            )
            .map_err(error)?
            .ok_or_else(|| error("service search requires a bind password source"))?;
            if matches!(&bind_password, SecretSource::Literal(password) if password.is_empty()) {
                return Err(error("service bind password must not be empty"));
            }
            LdapBindConfig::Search {
                username_attribute: raw
                    .username_attribute
                    .take()
                    .ok_or_else(|| error("service search requires `username_attribute`"))?,
                bind_dn: raw
                    .bind_dn
                    .take()
                    .ok_or_else(|| error("service search requires `bind_dn`"))?,
                bind_password,
            }
        }
    };
    let connect_timeout = std::num::NonZeroU64::new(raw.connect_timeout_secs.unwrap_or(3))
        .ok_or_else(|| error("`connect_timeout_secs` must be positive"))?;
    let request_timeout = std::num::NonZeroU64::new(raw.request_timeout_secs.unwrap_or(5))
        .ok_or_else(|| error("`request_timeout_secs` must be positive"))?;
    let max_connections = std::num::NonZeroU32::new(raw.max_connections.unwrap_or(8))
        .ok_or_else(|| error("`max_connections` must be positive"))?;
    Ok(LdapProviderConfig {
        id,
        url,
        base_dn: raw.base_dn,
        bind,
        subject_attribute: raw.subject_attribute,
        display_name_attribute: raw.display_name_attribute,
        group_attribute: raw.group_attribute,
        ca_file: raw.ca_file,
        connect_timeout: Duration::from_secs(connect_timeout.get()),
        request_timeout: Duration::from_secs(request_timeout.get()),
        max_connections,
        group_mappings: raw
            .group_mappings
            .into_iter()
            .map(|mapping| {
                classify_group_mapping(mapping, || ConfigError::LdapProvider {
                    id: provider_id.clone(),
                    reason: "group mapping has an invalid group",
                })
            })
            .collect::<Result<_, _>>()?,
    })
}

fn classify_group_mapping(
    raw: RawExternalGroupGrant,
    invalid: impl Fn() -> ConfigError,
) -> Result<ExternalGroupGrant, ConfigError> {
    Ok(ExternalGroupGrant {
        group: ExternalGroup::new(&raw.group).map_err(|_| invalid())?,
        role: raw.role,
        scope: raw
            .repository
            .map_or(GrantScope::Server, |name| GrantScope::Repository { name }),
    })
}

/// A secret from either its literal key or its `*_file` sibling, never both.
fn secret_source(literal: Option<String>, file: Option<PathBuf>) -> Result<Option<SecretSource>, &'static str> {
    match (literal, file) {
        (Some(_), Some(_)) => Err("set at most one of a secret and its `_file` sibling"),
        (Some(secret), None) => Ok(Some(SecretSource::Literal(secret))),
        (None, Some(path)) => Ok(Some(SecretSource::File(path))),
        (None, None) => Ok(None),
    }
}

/// An upstream credential from exactly one of its literal key, its `*_file` sibling, or its `*_env`
/// sibling. Upstream credentials add the environment source that file-only secrets do not offer.
fn upstream_secret_source(
    literal: Option<String>,
    file: Option<PathBuf>,
    env: Option<String>,
) -> Result<Option<SecretSource>, &'static str> {
    match (literal, file, env) {
        (None, None, None) => Ok(None),
        (Some(secret), None, None) => Ok(Some(SecretSource::Literal(secret))),
        (None, Some(path), None) => Ok(Some(SecretSource::File(path))),
        (None, None, Some(var)) if !var.trim().is_empty() => Ok(Some(SecretSource::Env(var))),
        (None, None, Some(_)) => Err("`_env` names an environment variable and must not be empty"),
        _ => Err("set at most one of a secret, its `_file` sibling, and its `_env` sibling"),
    }
}

/// Classify an index's `[[index.access_token]]` tables, rejecting names that collide with each other.
fn classify_tokens(index: &str, raw: Vec<RawToken>) -> Result<Vec<TokenConfig>, ConfigError> {
    let mut names = HashSet::with_capacity(raw.len());
    raw.into_iter()
        .map(|token| {
            let classified = classify_token(index, token)?;
            if !names.insert(classified.name.clone()) {
                return Err(ConfigError::Token {
                    index: index.to_owned(),
                    name: classified.name,
                    reason: "duplicate token name",
                });
            }
            Ok(classified)
        })
        .collect()
}

fn classify_token(index: &str, raw: RawToken) -> Result<TokenConfig, ConfigError> {
    let fail = |name: &str, reason| ConfigError::Token {
        index: index.to_owned(),
        name: name.to_owned(),
        reason,
    };
    if raw.name.is_empty() {
        return Err(fail(&raw.name, "token name is required"));
    }
    let secret = match secret_source(raw.secret, raw.secret_file) {
        Ok(Some(SecretSource::Literal(secret))) if secret.is_empty() => {
            return Err(fail(&raw.name, "`secret` must not be empty"));
        }
        Ok(Some(secret)) => secret,
        Ok(None) => return Err(fail(&raw.name, "token needs a `secret` or a `secret_file`")),
        Err(reason) => return Err(fail(&raw.name, reason)),
    };
    if raw.actions.is_empty() {
        return Err(fail(&raw.name, "token needs at least one action"));
    }
    let expires_at = raw
        .expires_at
        .map(|value| parse_timestamp(&value))
        .transpose()
        .map_err(|reason| fail(&raw.name, reason))?;
    Ok(TokenConfig {
        name: raw.name,
        secret,
        resources: if raw.resources.is_empty() {
            vec!["*".to_owned()]
        } else {
            raw.resources
        },
        actions: raw.actions.into_iter().collect(),
        expires_at,
    })
}

/// An RFC 3339 timestamp as unix seconds, the form an expiry is compared in.
fn parse_timestamp(value: &str) -> Result<i64, &'static str> {
    OffsetDateTime::parse(value, &Rfc3339)
        .map(OffsetDateTime::unix_timestamp)
        .map_err(|_| "`expires_at` must be an RFC 3339 timestamp, for example 2027-01-01T00:00:00Z")
}

pub(super) fn classify_tls(tls: Option<RawTls>, acme: Option<RawAcme>) -> Result<Option<TlsConfig>, ConfigError> {
    match (tls, acme) {
        (Some(_), Some(_)) => Err(ConfigError::Tls {
            reason: "set at most one of `[tls]` or `[acme]`",
        }),
        (Some(tls), None) => match (tls.cert, tls.key) {
            (Some(cert), Some(key)) => Ok(Some(TlsConfig::Manual { cert, key })),
            _ => Err(ConfigError::Tls {
                reason: "`[tls]` needs both `cert` and `key`",
            }),
        },
        (None, Some(acme)) => {
            if acme.domains.is_empty() {
                return Err(ConfigError::Tls {
                    reason: "`[acme]` needs at least one domain",
                });
            }
            if acme.contact.is_empty() {
                return Err(ConfigError::Tls {
                    reason: "`[acme]` needs a contact email",
                });
            }
            Ok(Some(TlsConfig::Acme(AcmeConfig {
                domains: acme.domains,
                contact: acme.contact,
                cache_dir: acme.cache_dir.unwrap_or_else(|| PathBuf::from("acme-cache")),
                staging: acme.staging,
            })))
        }
        (None, None) => Ok(None),
    }
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

fn apply_rate_limit(mut base: RateLimitConfig, partial: PartialRateLimitConfig) -> RateLimitConfig {
    if let Some(enabled) = partial.enabled {
        base.enabled = enabled;
    }
    if let Some(max_clients) = partial.max_clients {
        base.max_clients = max_clients;
    }
    if let Some(trusted_proxies) = partial.trusted_proxies {
        base.trusted_proxies = trusted_proxies;
    }
    base.listing = apply_route_limit(base.listing, partial.listing);
    base.metadata = apply_route_limit(base.metadata, partial.metadata);
    base.artifact = apply_route_limit(base.artifact, partial.artifact);
    base.upload = apply_route_limit(base.upload, partial.upload);
    base.admin = apply_route_limit(base.admin, partial.admin);
    base.authentication = apply_route_limit(base.authentication, partial.authentication);
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
