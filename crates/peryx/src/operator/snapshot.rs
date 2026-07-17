//! Config snapshot serialization for backup manifests.

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};

use ipnet::IpNet;
use peryx_core::Ecosystem;
use peryx_driver::rate_limit::{RateLimitConfig, RouteLimit};
use peryx_identity::Action;
use peryx_policy::PolicyConfig;
use serde::Serialize;
use time::OffsetDateTime;
use time::format_description::well_known::Rfc3339;
use toml::{Table, Value};

use crate::config::{
    AcmeConfig, AuthConfig, Config, IndexConfig, IndexKind, JobsConfig, JobsMode, LogConfig, LogFormat, LogSink,
    PrefetchConfig, PrefetchMode, ReplicationConfig, SecretSource, TlsConfig, TokenConfig, WebhookConfig,
    WebhookSecret,
};

#[derive(Serialize)]
struct SnapshotConfig<'a> {
    host: &'a str,
    port: u16,
    data_dir: &'a Path,
    #[serde(skip_serializing_if = "Option::is_none")]
    netrc: Option<&'a Path>,
    #[serde(skip_serializing_if = "Option::is_none")]
    writer_identity: Option<&'a str>,
    offline: bool,
    read_only: bool,
    cache_ttl_secs: i64,
    hot_cache_bytes: u64,
    max_stale_secs: i64,
    #[serde(skip_serializing_if = "Option::is_none")]
    usage_retention_days: Option<u32>,
    index: Vec<SnapshotIndex<'a>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    tls: Option<SnapshotTls<'a>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    acme: Option<SnapshotAcme<'a>>,
    log: SnapshotLog<'a>,
    rate_limit: SnapshotRateLimit<'a>,
    auth: SnapshotAuth<'a>,
    #[serde(skip_serializing_if = "Option::is_none")]
    replication: Option<SnapshotReplication<'a>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    jobs: Option<SnapshotJobs>,
}

#[derive(Serialize)]
struct SnapshotJobs {
    mode: &'static str,
}

#[derive(Serialize)]
struct SnapshotIndex<'a> {
    name: &'a str,
    route: &'a str,
    ecosystem: Ecosystem,
    #[serde(flatten)]
    kind: SnapshotIndexKind<'a>,
    #[serde(skip_serializing_if = "Option::is_none")]
    anonymous_read: Option<bool>,
    policy: Table,
    #[serde(skip_serializing_if = "Option::is_none")]
    settings: Option<&'a Table>,
    #[serde(rename = "access_token", skip_serializing_if = "Vec::is_empty")]
    access_tokens: Vec<SnapshotToken<'a>>,
    #[serde(rename = "webhook", skip_serializing_if = "Vec::is_empty")]
    webhooks: Vec<SnapshotWebhook<'a>>,
}

#[derive(Serialize)]
#[serde(untagged)]
enum SnapshotIndexKind<'a> {
    Cached {
        cached: &'a str,
        #[serde(skip_serializing_if = "Option::is_none")]
        username: Option<&'a str>,
        #[serde(skip_serializing_if = "Option::is_none")]
        password: Option<&'a str>,
        #[serde(skip_serializing_if = "Option::is_none")]
        password_file: Option<&'a Path>,
        #[serde(skip_serializing_if = "Option::is_none")]
        password_env: Option<&'a str>,
        #[serde(skip_serializing_if = "Option::is_none")]
        token: Option<&'a str>,
        #[serde(skip_serializing_if = "Option::is_none")]
        token_file: Option<&'a Path>,
        #[serde(skip_serializing_if = "Option::is_none")]
        token_env: Option<&'a str>,
        #[serde(skip_serializing_if = "Option::is_none")]
        ca_file: Option<&'a Path>,
        #[serde(skip_serializing_if = "Option::is_none")]
        client_cert_file: Option<&'a Path>,
        #[serde(skip_serializing_if = "Option::is_none")]
        client_key_file: Option<&'a Path>,
        upstream_concurrency: usize,
        offline: bool,
        prefetch: SnapshotPrefetch<'a>,
    },
    Routed {
        #[serde(rename = "upstream")]
        upstreams: Vec<SnapshotUpstream<'a>>,
        fallback: bool,
        protected: &'a [String],
        pins: &'a std::collections::BTreeMap<String, String>,
        upstream_concurrency: usize,
        offline: bool,
        prefetch: SnapshotPrefetch<'a>,
    },
    Hosted {
        hosted: bool,
        #[serde(skip_serializing_if = "Option::is_none")]
        upload_token: Option<&'a str>,
        #[serde(skip_serializing_if = "Option::is_none")]
        upload_token_file: Option<&'a Path>,
        volatile: bool,
    },
    Virtual {
        layers: &'a [String],
        #[serde(skip_serializing_if = "Option::is_none")]
        upload: Option<&'a str>,
    },
}

#[derive(Serialize)]
struct SnapshotUpstream<'a> {
    name: &'a str,
    url: &'a str,
    #[serde(skip_serializing_if = "Option::is_none")]
    artifact_url: Option<&'a str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    username: Option<&'a str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    password: Option<&'a str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    password_file: Option<&'a Path>,
    #[serde(skip_serializing_if = "Option::is_none")]
    password_env: Option<&'a str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    token: Option<&'a str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    token_file: Option<&'a Path>,
    #[serde(skip_serializing_if = "Option::is_none")]
    token_env: Option<&'a str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    ca_file: Option<&'a Path>,
    #[serde(skip_serializing_if = "Option::is_none")]
    client_cert_file: Option<&'a Path>,
    #[serde(skip_serializing_if = "Option::is_none")]
    client_key_file: Option<&'a Path>,
}

#[derive(Serialize)]
struct SnapshotPrefetch<'a> {
    mode: &'static str,
    packages: &'a [String],
    requirements: &'a [PathBuf],
    include_wheels: bool,
    include_sdists: bool,
    python_tags: &'a [String],
    abi_tags: &'a [String],
    platform_tags: &'a [String],
    #[serde(skip_serializing_if = "Option::is_none")]
    max_file_size_bytes: Option<u64>,
    metadata_only: bool,
}

#[derive(Serialize)]
struct SnapshotToken<'a> {
    name: &'a str,
    #[serde(skip_serializing_if = "Option::is_none")]
    secret: Option<&'a str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    secret_file: Option<&'a Path>,
    projects: &'a [String],
    actions: &'a BTreeSet<Action>,
    #[serde(skip_serializing_if = "Option::is_none")]
    expires_at: Option<String>,
}

#[derive(Serialize)]
struct SnapshotWebhook<'a> {
    name: &'a str,
    url: &'a str,
    #[serde(skip_serializing_if = "Option::is_none")]
    secret: Option<&'a str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    secret_env: Option<&'a str>,
    events: &'a [String],
}

#[derive(Serialize)]
struct SnapshotTls<'a> {
    cert: &'a Path,
    key: &'a Path,
}

#[derive(Serialize)]
#[serde(rename_all = "kebab-case")]
struct SnapshotAcme<'a> {
    domains: &'a [String],
    contact: &'a str,
    cache_dir: &'a Path,
    staging: bool,
}

#[derive(Serialize)]
struct SnapshotLog<'a> {
    level: &'a str,
    format: &'static str,
    sink: &'static str,
    #[serde(skip_serializing_if = "Option::is_none")]
    file: Option<&'a Path>,
}

#[derive(Serialize)]
struct SnapshotRateLimit<'a> {
    enabled: bool,
    max_clients: u64,
    trusted_proxies: &'a [IpNet],
    listing: SnapshotRouteLimit,
    metadata: SnapshotRouteLimit,
    artifact: SnapshotRouteLimit,
    upload: SnapshotRouteLimit,
    admin: SnapshotRouteLimit,
}

#[derive(Serialize)]
struct SnapshotRouteLimit {
    requests: u64,
    window_secs: u64,
}

#[derive(Serialize)]
struct SnapshotAuth<'a> {
    #[serde(skip_serializing_if = "Option::is_none")]
    signing_key: Option<&'a str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    signing_key_file: Option<&'a Path>,
    token_ttl_secs: i64,
    default_anonymous_read: bool,
    oidc_audience: &'a str,
    #[serde(rename = "trusted_publisher", skip_serializing_if = "Vec::is_empty")]
    trusted_publishers: Vec<SnapshotTrustedPublisher<'a>>,
}

#[derive(Serialize)]
struct SnapshotTrustedPublisher<'a> {
    id: &'a str,
    issuer: &'a str,
    repository: &'a str,
    subject: &'a str,
    projects: &'a [String],
    claims: &'a std::collections::BTreeMap<String, String>,
}

#[derive(Serialize)]
#[serde(tag = "role", rename_all = "lowercase")]
enum SnapshotReplication<'a> {
    Primary {
        source: &'a str,
        #[serde(skip_serializing_if = "Option::is_none")]
        token: Option<&'a str>,
        #[serde(skip_serializing_if = "Option::is_none")]
        token_file: Option<&'a Path>,
    },
    Replica {
        upstream: &'a str,
        #[serde(skip_serializing_if = "Option::is_none")]
        token: Option<&'a str>,
        #[serde(skip_serializing_if = "Option::is_none")]
        token_file: Option<&'a Path>,
        poll_interval_secs: u64,
        page_size: usize,
    },
}

pub(super) fn config_snapshot(config: &Config) -> anyhow::Result<String> {
    let Config {
        host,
        port,
        data_dir,
        netrc,
        writer_identity,
        offline,
        read_only,
        cache_ttl_secs,
        hot_cache_bytes,
        max_stale_secs,
        usage_retention_days,
        indexes,
        tls,
        log,
        rate_limit,
        auth,
        replication,
        jobs,
    } = config;
    let LogConfig {
        level,
        format,
        sink,
        file,
    } = log;
    let AuthConfig {
        signing_key,
        token_ttl_secs,
        default_anonymous_read,
        oidc_audience,
        trusted_publishers,
    } = auth;
    let (tls, acme) = snapshot_tls(tls.as_ref());
    let (signing_key, signing_key_file, _) = secret_parts(signing_key.as_ref());
    let snapshot = SnapshotConfig {
        host,
        port: *port,
        data_dir,
        netrc: netrc.as_deref(),
        writer_identity: writer_identity.as_deref(),
        offline: *offline,
        read_only: *read_only,
        cache_ttl_secs: *cache_ttl_secs,
        hot_cache_bytes: *hot_cache_bytes,
        max_stale_secs: *max_stale_secs,
        usage_retention_days: *usage_retention_days,
        index: indexes.iter().map(snapshot_index).collect::<anyhow::Result<_>>()?,
        tls,
        acme,
        log: SnapshotLog {
            level,
            format: log_format(*format),
            sink: log_sink(*sink),
            file: file.as_deref(),
        },
        rate_limit: snapshot_rate_limit(rate_limit),
        auth: SnapshotAuth {
            signing_key,
            signing_key_file,
            token_ttl_secs: *token_ttl_secs,
            default_anonymous_read: *default_anonymous_read,
            oidc_audience,
            trusted_publishers: trusted_publishers
                .iter()
                .map(|publisher| SnapshotTrustedPublisher {
                    id: &publisher.id,
                    issuer: &publisher.issuer,
                    repository: &publisher.repository,
                    subject: &publisher.subject,
                    projects: &publisher.projects,
                    claims: &publisher.claims,
                })
                .collect(),
        },
        replication: snapshot_replication(replication.as_ref()),
        jobs: snapshot_jobs(*jobs),
    };
    Ok(toml::to_string_pretty(&snapshot)?)
}

/// A snapshot carries the `[jobs]` table only when it departs from the default, so an unset backup
/// stays terse and restores to the same default.
const fn snapshot_jobs(jobs: JobsConfig) -> Option<SnapshotJobs> {
    match jobs.mode {
        JobsMode::Local => None,
        JobsMode::None => Some(SnapshotJobs { mode: "none" }),
    }
}

fn snapshot_replication(replication: Option<&ReplicationConfig>) -> Option<SnapshotReplication<'_>> {
    match replication {
        Some(ReplicationConfig::Primary { source, token }) => {
            let (token, token_file, _) = secret_parts(Some(token));
            Some(SnapshotReplication::Primary {
                source,
                token,
                token_file,
            })
        }
        Some(ReplicationConfig::Replica {
            upstream,
            token,
            poll_interval,
            page_size,
        }) => {
            let (token, token_file, _) = secret_parts(Some(token));
            Some(SnapshotReplication::Replica {
                upstream,
                token,
                token_file,
                poll_interval_secs: poll_interval.as_secs(),
                page_size: page_size.get(),
            })
        }
        None => None,
    }
}

fn snapshot_index(index: &IndexConfig) -> anyhow::Result<SnapshotIndex<'_>> {
    let IndexConfig {
        name,
        route,
        ecosystem,
        kind,
        anonymous_read,
        tokens,
        policy,
        ecosystem_policy,
        ecosystem_settings,
        webhooks,
    } = index;
    let kind = match kind {
        IndexKind::Cached {
            upstream,
            username,
            password,
            token,
            tls,
            routing,
            upstream_concurrency,
            offline,
            prefetch,
        } => routing.as_ref().map_or_else(
            || {
                let (password, password_file, password_env) = secret_parts(password.as_ref());
                let (token, token_file, token_env) = secret_parts(token.as_ref());
                SnapshotIndexKind::Cached {
                    cached: upstream,
                    username: username.as_deref(),
                    password,
                    password_file,
                    password_env,
                    token,
                    token_file,
                    token_env,
                    ca_file: tls.ca_file.as_deref(),
                    client_cert_file: tls.client_cert_file.as_deref(),
                    client_key_file: tls.client_key_file.as_deref(),
                    upstream_concurrency: *upstream_concurrency,
                    offline: *offline,
                    prefetch: snapshot_prefetch(prefetch),
                }
            },
            |routing| SnapshotIndexKind::Routed {
                upstreams: routing.upstreams.iter().map(snapshot_upstream).collect(),
                fallback: routing.fallback,
                protected: &routing.protected,
                pins: &routing.pins,
                upstream_concurrency: *upstream_concurrency,
                offline: *offline,
                prefetch: snapshot_prefetch(prefetch),
            },
        ),
        IndexKind::Hosted { upload_token, volatile } => {
            let (upload_token, upload_token_file, _) = secret_parts(upload_token.as_ref());
            SnapshotIndexKind::Hosted {
                hosted: true,
                upload_token,
                upload_token_file,
                volatile: *volatile,
            }
        }
        IndexKind::Virtual { layers, upload } => SnapshotIndexKind::Virtual {
            layers,
            upload: upload.as_deref(),
        },
    };
    Ok(SnapshotIndex {
        name,
        route,
        ecosystem: *ecosystem,
        kind,
        anonymous_read: *anonymous_read,
        policy: snapshot_policy(policy, ecosystem_policy)?,
        settings: (!ecosystem_settings.is_empty()).then_some(ecosystem_settings),
        access_tokens: tokens.iter().map(snapshot_token).collect::<anyhow::Result<_>>()?,
        webhooks: webhooks.iter().map(snapshot_webhook).collect(),
    })
}

fn snapshot_upstream(upstream: &crate::config::UpstreamConfig) -> SnapshotUpstream<'_> {
    let (password, password_file, password_env) = secret_parts(upstream.password.as_ref());
    let (token, token_file, token_env) = secret_parts(upstream.token.as_ref());
    SnapshotUpstream {
        name: &upstream.name,
        url: &upstream.url,
        artifact_url: upstream.artifact_url.as_deref(),
        username: upstream.username.as_deref(),
        password,
        password_file,
        password_env,
        token,
        token_file,
        token_env,
        ca_file: upstream.tls.ca_file.as_deref(),
        client_cert_file: upstream.tls.client_cert_file.as_deref(),
        client_key_file: upstream.tls.client_key_file.as_deref(),
    }
}

fn snapshot_prefetch(prefetch: &PrefetchConfig) -> SnapshotPrefetch<'_> {
    let PrefetchConfig {
        mode,
        packages,
        requirements,
        include_wheels,
        include_sdists,
        python_tags,
        abi_tags,
        platform_tags,
        max_file_size_bytes,
        metadata_only,
    } = prefetch;
    SnapshotPrefetch {
        mode: prefetch_mode(*mode),
        packages,
        requirements,
        include_wheels: *include_wheels,
        include_sdists: *include_sdists,
        python_tags,
        abi_tags,
        platform_tags,
        max_file_size_bytes: *max_file_size_bytes,
        metadata_only: *metadata_only,
    }
}

fn snapshot_policy(config: &PolicyConfig, ecosystem: &Table) -> anyhow::Result<Table> {
    let PolicyConfig {
        allow_projects,
        block_projects,
        protected_names,
        max_file_size_bytes,
        max_project_size_bytes,
    } = config;
    let mut policy = ecosystem.clone();
    policy.insert(
        "allow_projects".to_owned(),
        Value::Array(allow_projects.iter().cloned().map(Value::String).collect()),
    );
    policy.insert(
        "block_projects".to_owned(),
        Value::Array(block_projects.iter().cloned().map(Value::String).collect()),
    );
    policy.insert(
        "protected_names".to_owned(),
        Value::Array(protected_names.iter().cloned().map(Value::String).collect()),
    );
    if let Some(value) = max_file_size_bytes {
        policy.insert("max_file_size_bytes".to_owned(), Value::Integer((*value).try_into()?));
    }
    if let Some(value) = max_project_size_bytes {
        policy.insert(
            "max_project_size_bytes".to_owned(),
            Value::Integer((*value).try_into()?),
        );
    }
    Ok(policy)
}

fn snapshot_token(token: &TokenConfig) -> anyhow::Result<SnapshotToken<'_>> {
    let TokenConfig {
        name,
        secret,
        projects,
        actions,
        expires_at,
    } = token;
    let (secret, secret_file, _) = secret_parts(Some(secret));
    Ok(SnapshotToken {
        name,
        secret,
        secret_file,
        projects,
        actions,
        expires_at: expires_at
            .map(|timestamp| OffsetDateTime::from_unix_timestamp(timestamp)?.format(&Rfc3339))
            .transpose()?,
    })
}

fn snapshot_webhook(webhook: &WebhookConfig) -> SnapshotWebhook<'_> {
    let WebhookConfig {
        name,
        url,
        secret,
        events,
    } = webhook;
    let (secret, secret_env) = match secret {
        WebhookSecret::Literal(secret) => (Some(secret.as_str()), None),
        WebhookSecret::Env(name) => (None, Some(name.as_str())),
    };
    SnapshotWebhook {
        name,
        url,
        secret,
        secret_env,
        events,
    }
}

fn snapshot_tls(tls: Option<&TlsConfig>) -> (Option<SnapshotTls<'_>>, Option<SnapshotAcme<'_>>) {
    match tls {
        Some(TlsConfig::Manual { cert, key }) => (
            Some(SnapshotTls {
                cert: cert.as_path(),
                key: key.as_path(),
            }),
            None,
        ),
        Some(TlsConfig::Acme(AcmeConfig {
            domains,
            contact,
            cache_dir,
            staging,
        })) => (
            None,
            Some(SnapshotAcme {
                domains,
                contact,
                cache_dir,
                staging: *staging,
            }),
        ),
        None => (None, None),
    }
}

fn snapshot_rate_limit(rate_limit: &RateLimitConfig) -> SnapshotRateLimit<'_> {
    let RateLimitConfig {
        enabled,
        max_clients,
        trusted_proxies,
        listing,
        metadata,
        artifact,
        upload,
        admin,
    } = rate_limit;
    SnapshotRateLimit {
        enabled: *enabled,
        max_clients: *max_clients,
        trusted_proxies,
        listing: snapshot_route_limit(*listing),
        metadata: snapshot_route_limit(*metadata),
        artifact: snapshot_route_limit(*artifact),
        upload: snapshot_route_limit(*upload),
        admin: snapshot_route_limit(*admin),
    }
}

const fn snapshot_route_limit(limit: RouteLimit) -> SnapshotRouteLimit {
    let RouteLimit { requests, window_secs } = limit;
    SnapshotRouteLimit { requests, window_secs }
}

// Preserve file- and env-backed secret references so snapshots hold the location, never the contents.
fn secret_parts(source: Option<&SecretSource>) -> (Option<&str>, Option<&Path>, Option<&str>) {
    match source {
        Some(SecretSource::Literal(secret)) => (Some(secret), None, None),
        Some(SecretSource::File(path)) => (None, Some(path), None),
        Some(SecretSource::Env(var)) => (None, None, Some(var)),
        None => (None, None, None),
    }
}

const fn prefetch_mode(mode: PrefetchMode) -> &'static str {
    match mode {
        PrefetchMode::All => "all",
        PrefetchMode::Selected => "selected",
        PrefetchMode::MetadataOnly => "metadata-only",
    }
}

const fn log_format(format: LogFormat) -> &'static str {
    match format {
        LogFormat::Pretty => "pretty",
        LogFormat::Json => "json",
    }
}

const fn log_sink(sink: LogSink) -> &'static str {
    match sink {
        LogSink::Stdout => "stdout",
        LogSink::File => "file",
        LogSink::Journald => "journald",
        LogSink::Syslog => "syslog",
    }
}
