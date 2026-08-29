use std::collections::BTreeSet;
use std::path::Path;

use ipnet::IpNet;
use peryx_core::Ecosystem;
use peryx_driver::rate_limit::{RateLimitConfig, RouteLimit};
use peryx_identity::{Action, ExternalGroupGrant, GrantScope, Role};
use peryx_policy::PolicyConfig;
use peryx_upstream::{CredentialFailure, ExecCredentialConfig};
use serde::Serialize;
use time::OffsetDateTime;
use time::format_description::well_known::Rfc3339;
use toml::{Table, Value};

use crate::config::{
    AcmeConfig, AuthConfig, AvailabilityConfig, Config, CredentialFailureMode, CredentialRefreshConfig, IndexConfig,
    IndexKind, JobsConfig, JobsMode, LdapBindConfig, LdapProviderConfig, LogConfig, LogFormat, LogSink,
    OidcProviderConfig, ReplicationConfig, SecretSource, TlsConfig, TokenConfig, WebhookConfig, WebhookSecret,
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
    availability: Option<SnapshotAvailability<'a>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    jobs: Option<SnapshotJobs>,
}

#[derive(Serialize)]
struct SnapshotJobs {
    #[serde(skip_serializing_if = "Option::is_none")]
    mode: Option<&'static str>,
    #[serde(rename = "schedule", skip_serializing_if = "Vec::is_empty")]
    schedules: Vec<SnapshotSchedule>,
}

#[derive(Serialize)]
struct SnapshotSchedule {
    job: String,
    interval_secs: u64,
    #[serde(flatten)]
    settings: Table,
}

#[derive(Serialize)]
struct SnapshotIndex<'a> {
    name: &'a str,
    route: &'a str,
    ecosystem: &'a Ecosystem,
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
    Routed {
        #[serde(rename = "upstream")]
        upstreams: Vec<SnapshotUpstream<'a>>,
        fallback: bool,
        protected: &'a [String],
        pins: &'a std::collections::BTreeMap<String, String>,
        upstream_concurrency: usize,
        offline: bool,
        prefetch: &'a Table,
    },
    Hosted {
        hosted: bool,
        volatile: bool,
    },
    Virtual {
        layers: &'a [String],
        #[serde(skip_serializing_if = "Option::is_none")]
        write_target: Option<&'a str>,
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
    #[serde(flatten)]
    credential_refresh: Option<SnapshotCredentialRefresh>,
    #[serde(skip_serializing_if = "Option::is_none")]
    credential_exec: Option<SnapshotCredentialExec<'a>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    ca_file: Option<&'a Path>,
    #[serde(skip_serializing_if = "Option::is_none")]
    client_cert_file: Option<&'a Path>,
    #[serde(skip_serializing_if = "Option::is_none")]
    client_key_file: Option<&'a Path>,
}

#[derive(Serialize)]
struct SnapshotCredentialRefresh {
    #[serde(rename = "credential_refresh_secs")]
    interval_secs: u64,
    #[serde(rename = "credential_refresh_on_unauthorized")]
    on_unauthorized: bool,
    #[serde(rename = "credential_failure")]
    failure: &'static str,
}

#[derive(Serialize)]
struct SnapshotCredentialExec<'a> {
    argv: &'a [String],
    timeout_secs: u64,
    environment: &'a [String],
    failure: &'static str,
}

#[derive(Serialize)]
struct SnapshotToken<'a> {
    name: &'a str,
    #[serde(skip_serializing_if = "Option::is_none")]
    secret: Option<&'a str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    secret_file: Option<&'a Path>,
    resources: &'a [String],
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
    authentication: SnapshotRouteLimit,
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
    #[serde(rename = "ldap_provider", skip_serializing_if = "Vec::is_empty")]
    ldap_providers: Vec<SnapshotLdapProvider<'a>>,
    #[serde(rename = "oidc_provider", skip_serializing_if = "Vec::is_empty")]
    oidc_providers: Vec<SnapshotOidcProvider<'a>>,
    #[serde(flatten)]
    extensions: &'a toml::Table,
}

#[derive(Serialize)]
struct SnapshotOidcProvider<'a> {
    id: &'a str,
    issuer: &'a str,
    client_id: &'a str,
    #[serde(skip_serializing_if = "Option::is_none")]
    client_secret: Option<&'a str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    client_secret_file: Option<&'a Path>,
    #[serde(skip_serializing_if = "Option::is_none")]
    client_secret_env: Option<&'a str>,
    redirect_uri: &'a str,
    #[serde(skip_serializing_if = "<[_]>::is_empty")]
    scopes: &'a [String],
    subject_claim: &'a str,
    display_name_claim: &'a str,
    #[serde(skip_serializing_if = "Option::is_none")]
    groups_claim: Option<&'a str>,
    clock_skew_secs: u64,
    request_timeout_secs: u64,
    #[serde(rename = "group_mapping", skip_serializing_if = "Vec::is_empty")]
    group_mappings: Vec<SnapshotExternalGroupGrant<'a>>,
}

#[derive(Serialize)]
struct SnapshotLdapProvider<'a> {
    id: &'a str,
    url: &'a str,
    base_dn: &'a str,
    #[serde(flatten)]
    bind: SnapshotLdapBind<'a>,
    subject_attribute: &'a str,
    display_name_attribute: &'a str,
    #[serde(skip_serializing_if = "Option::is_none")]
    group_attribute: Option<&'a str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    ca_file: Option<&'a Path>,
    connect_timeout_secs: u64,
    request_timeout_secs: u64,
    max_connections: u32,
    #[serde(rename = "group_mapping", skip_serializing_if = "Vec::is_empty")]
    group_mappings: Vec<SnapshotExternalGroupGrant<'a>>,
}

#[derive(Serialize)]
#[serde(tag = "mode", rename_all = "kebab-case")]
enum SnapshotLdapBind<'a> {
    DirectBind {
        dn_attribute: &'a str,
    },
    ServiceSearch {
        username_attribute: &'a str,
        bind_dn: &'a str,
        #[serde(skip_serializing_if = "Option::is_none")]
        bind_password: Option<&'a str>,
        #[serde(skip_serializing_if = "Option::is_none")]
        bind_password_file: Option<&'a Path>,
        #[serde(skip_serializing_if = "Option::is_none")]
        bind_password_env: Option<&'a str>,
    },
}

#[derive(Serialize)]
struct SnapshotExternalGroupGrant<'a> {
    group: &'a str,
    role: Role,
    #[serde(skip_serializing_if = "Option::is_none")]
    repository: Option<&'a str>,
}

#[derive(Serialize)]
struct SnapshotAvailability<'a> {
    mode: &'static str,
    #[serde(skip_serializing_if = "Option::is_none")]
    replication: Option<SnapshotReplication<'a>>,
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
        // Snapshots omit node-local and runtime-resolved fields; restore reads them from target configuration.
        node_identity: _,
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
        availability,
        write_ack: _,
        dc_membership: _,
        availability_listener: _,
        read_through: _,
        jobs,
        // A backup only ever captures a filesystem-backed repository: an object-store backend is
        // rejected before any snapshot runs, so the effective config restores to the filesystem default.
        blob: _,
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
        ldap_providers,
        oidc_providers,
        extensions,
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
            ldap_providers: ldap_providers.iter().map(snapshot_ldap_provider).collect(),
            oidc_providers: oidc_providers.iter().map(snapshot_oidc_provider).collect(),
            extensions,
        },
        availability: snapshot_availability(availability),
        jobs: snapshot_jobs(jobs),
    };
    Ok(toml::to_string_pretty(&snapshot)?)
}

fn snapshot_ldap_provider(provider: &LdapProviderConfig) -> SnapshotLdapProvider<'_> {
    SnapshotLdapProvider {
        id: provider.id.as_str(),
        url: provider.url.as_str(),
        base_dn: &provider.base_dn,
        bind: match &provider.bind {
            LdapBindConfig::Direct { dn_attribute } => SnapshotLdapBind::DirectBind { dn_attribute },
            LdapBindConfig::Search {
                username_attribute,
                bind_dn,
                bind_password,
            } => {
                let (bind_password, bind_password_file, bind_password_env) = secret_parts(Some(bind_password));
                SnapshotLdapBind::ServiceSearch {
                    username_attribute,
                    bind_dn,
                    bind_password,
                    bind_password_file,
                    bind_password_env,
                }
            }
        },
        subject_attribute: &provider.subject_attribute,
        display_name_attribute: &provider.display_name_attribute,
        group_attribute: provider.group_attribute.as_deref(),
        ca_file: provider.ca_file.as_deref(),
        connect_timeout_secs: provider.connect_timeout.as_secs(),
        request_timeout_secs: provider.request_timeout.as_secs(),
        max_connections: provider.max_connections.get(),
        group_mappings: provider.group_mappings.iter().map(snapshot_group_mapping).collect(),
    }
}

fn snapshot_oidc_provider(provider: &OidcProviderConfig) -> SnapshotOidcProvider<'_> {
    let (client_secret, client_secret_file, client_secret_env) = secret_parts(provider.client_secret.as_ref());
    SnapshotOidcProvider {
        id: provider.id.as_str(),
        issuer: provider.issuer.as_str(),
        client_id: &provider.client_id,
        client_secret,
        client_secret_file,
        client_secret_env,
        redirect_uri: provider.redirect_uri.as_str(),
        scopes: &provider.scopes,
        subject_claim: &provider.subject_claim,
        display_name_claim: &provider.display_name_claim,
        groups_claim: provider.groups_claim.as_deref(),
        clock_skew_secs: provider.clock_skew.as_secs(),
        request_timeout_secs: provider.request_timeout.as_secs(),
        group_mappings: provider.group_mappings.iter().map(snapshot_group_mapping).collect(),
    }
}

fn snapshot_group_mapping(mapping: &ExternalGroupGrant) -> SnapshotExternalGroupGrant<'_> {
    SnapshotExternalGroupGrant {
        group: mapping.group.as_str(),
        role: mapping.role,
        repository: match &mapping.scope {
            GrantScope::Server => None,
            GrantScope::Repository { name } => Some(name.as_str()),
        },
    }
}

/// A snapshot carries the `[jobs]` table only when it departs from the default, so an unset backup
/// stays terse and restores to the same default. It keeps a non-default `mode` or a schedule set
/// other than the built-in cache-maintenance default, and omits the default schedule set so restore
/// rebuilds it.
fn snapshot_jobs(jobs: &JobsConfig) -> Option<SnapshotJobs> {
    let mode = match jobs.mode {
        JobsMode::Local => None,
        JobsMode::None => Some("none"),
    };
    let schedules = if jobs.schedules == JobsConfig::default().schedules {
        Vec::new()
    } else {
        jobs.schedules
            .iter()
            .map(|schedule| SnapshotSchedule {
                job: schedule.job.as_str().to_owned(),
                interval_secs: schedule.interval.as_secs(),
                settings: schedule.job.settings(),
            })
            .collect()
    };
    if mode.is_none() && schedules.is_empty() {
        return None;
    }
    Some(SnapshotJobs { mode, schedules })
}

/// A snapshot carries the `[availability]` table only for a `dc` or `ha` node, so a single-node `none`
/// backup omits it and restores to the same default. The nested `[availability.replication]` role
/// round-trips the configured topology.
fn snapshot_availability(availability: &AvailabilityConfig) -> Option<SnapshotAvailability<'_>> {
    let (mode, replication) = match availability {
        AvailabilityConfig::None => return None,
        AvailabilityConfig::Dc(replication) => ("dc", replication),
        AvailabilityConfig::Ha(replication) => ("ha", replication),
    };
    Some(SnapshotAvailability {
        mode,
        replication: Some(snapshot_replication(replication)),
    })
}

fn snapshot_replication(replication: &ReplicationConfig) -> SnapshotReplication<'_> {
    match replication {
        ReplicationConfig::Primary { source, token } => {
            let (token, token_file, _) = secret_parts(Some(token));
            SnapshotReplication::Primary {
                source,
                token,
                token_file,
            }
        }
        ReplicationConfig::Replica {
            upstream,
            token,
            poll_interval,
            page_size,
        } => {
            let (token, token_file, _) = secret_parts(Some(token));
            SnapshotReplication::Replica {
                upstream,
                token,
                token_file,
                poll_interval_secs: poll_interval.as_secs(),
                page_size: page_size.get(),
            }
        }
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
            routing,
            upstream_concurrency,
            offline,
            prefetch,
        } => SnapshotIndexKind::Routed {
            upstreams: routing.upstreams.iter().map(snapshot_upstream).collect(),
            fallback: routing.fallback,
            protected: &routing.protected,
            pins: &routing.pins,
            upstream_concurrency: *upstream_concurrency,
            offline: *offline,
            prefetch: &prefetch.options,
        },
        IndexKind::Hosted { volatile } => SnapshotIndexKind::Hosted {
            hosted: true,
            volatile: *volatile,
        },
        IndexKind::Virtual { layers, write_target } => SnapshotIndexKind::Virtual {
            layers,
            write_target: write_target.as_deref(),
        },
    };
    Ok(SnapshotIndex {
        name,
        route,
        ecosystem,
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
        credential_refresh: upstream.credential_refresh.map(snapshot_credential_refresh),
        credential_exec: upstream.credential_exec.as_ref().map(snapshot_credential_exec),
        ca_file: upstream.tls.ca_file.as_deref(),
        client_cert_file: upstream.tls.client_cert_file.as_deref(),
        client_key_file: upstream.tls.client_key_file.as_deref(),
    }
}

fn snapshot_credential_exec(config: &ExecCredentialConfig) -> SnapshotCredentialExec<'_> {
    SnapshotCredentialExec {
        argv: config.argv(),
        timeout_secs: config.timeout().as_secs(),
        environment: config.environment(),
        failure: match config.failure() {
            CredentialFailure::Fail => "fail",
            CredentialFailure::Anonymous => "anonymous",
        },
    }
}

const fn snapshot_credential_refresh(refresh: CredentialRefreshConfig) -> SnapshotCredentialRefresh {
    SnapshotCredentialRefresh {
        interval_secs: refresh.interval.as_secs(),
        on_unauthorized: refresh.on_unauthorized,
        failure: match refresh.failure {
            CredentialFailureMode::Fail => "fail",
            CredentialFailureMode::Anonymous => "anonymous",
        },
    }
}

fn snapshot_policy(config: &PolicyConfig, ecosystem: &Table) -> anyhow::Result<Table> {
    let PolicyConfig {
        allow_resources,
        block_resources,
        protected_resources,
        max_artifact_size_bytes,
        max_resource_size_bytes,
        max_accounted_bytes,
        max_resources,
        quota_audit,
    } = config;
    let mut policy = ecosystem.clone();
    policy.insert(
        "allow_resources".to_owned(),
        Value::Array(allow_resources.iter().cloned().map(Value::String).collect()),
    );
    policy.insert(
        "block_resources".to_owned(),
        Value::Array(block_resources.iter().cloned().map(Value::String).collect()),
    );
    policy.insert(
        "protected_resources".to_owned(),
        Value::Array(protected_resources.iter().cloned().map(Value::String).collect()),
    );
    if let Some(value) = max_artifact_size_bytes {
        policy.insert(
            "max_artifact_size_bytes".to_owned(),
            Value::Integer((*value).try_into()?),
        );
    }
    if let Some(value) = max_resource_size_bytes {
        policy.insert(
            "max_resource_size_bytes".to_owned(),
            Value::Integer((*value).try_into()?),
        );
    }
    if let Some(value) = max_accounted_bytes {
        policy.insert("max_accounted_bytes".to_owned(), Value::Integer((*value).try_into()?));
    }
    if let Some(value) = max_resources {
        policy.insert("max_resources".to_owned(), Value::Integer((*value).try_into()?));
    }
    if *quota_audit {
        policy.insert("quota_audit".to_owned(), Value::Boolean(true));
    }
    Ok(policy)
}

fn snapshot_token(token: &TokenConfig) -> anyhow::Result<SnapshotToken<'_>> {
    let TokenConfig {
        name,
        secret,
        resources,
        actions,
        expires_at,
    } = token;
    let (secret, secret_file, _) = secret_parts(Some(secret));
    Ok(SnapshotToken {
        name,
        secret,
        secret_file,
        resources,
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
        authentication,
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
        authentication: snapshot_route_limit(*authentication),
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
