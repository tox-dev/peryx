//! Overlay merging and raw-table classification: how a [`PartialConfig`] resolves onto defaults.

use std::path::PathBuf;
use std::time::Duration;

use peryx_core::Ecosystem;
use peryx_driver::rate_limit::{DEFAULT_UPSTREAM_CONCURRENCY, RateLimitConfig, RouteLimit};
use peryx_identity::UPLOAD_TOKEN_NAME;
use time::OffsetDateTime;
use time::format_description::well_known::Rfc3339;

use std::collections::HashSet;

use super::ConfigError;
use super::model::{
    AcmeConfig, AuthConfig, Config, DEFAULT_REPLICA_PAGE_SIZE, DEFAULT_REPLICA_POLL_INTERVAL_SECS, IndexConfig,
    IndexKind, LogConfig, ReplicationConfig, SecretSource, TlsConfig, TokenConfig, WebhookConfig, WebhookSecret,
};
use super::raw::{
    PartialAuthConfig, PartialConfig, PartialLogConfig, PartialRateLimitConfig, PartialRouteLimit, RawAcme, RawIndex,
    RawReplication, RawTls, RawToken, RawWebhook,
};

impl Config {
    /// Overlay a partial source on top of these values, returning the merged config.
    ///
    /// # Errors
    /// Returns [`ConfigError::Index`] if the partial defines indexes but one is not classifiable as a
    /// cached, hosted, or virtual.
    pub fn apply(mut self, partial: PartialConfig) -> Result<Self, ConfigError> {
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
        if let Some(offline) = partial.offline {
            self.offline = offline;
        }
        if let Some(read_only) = partial.read_only {
            self.read_only = read_only;
        }
        if let Some(max_stale_secs) = partial.max_stale_secs {
            self.max_stale_secs = max_stale_secs;
        }
        if let Some(hot_cache_bytes) = partial.hot_cache_bytes {
            self.hot_cache_bytes = hot_cache_bytes;
        }
        if let Some(cache_ttl_secs) = partial.cache_ttl_secs {
            self.cache_ttl_secs = cache_ttl_secs;
        }
        if let Some(raw) = partial.indexes {
            self.indexes = raw.into_iter().map(classify_index).collect::<Result<_, _>>()?;
        }
        if partial.tls.is_some() || partial.acme.is_some() {
            self.tls = classify_tls(partial.tls, partial.acme)?;
        }
        self.log = self.log.apply(partial.log);
        self.rate_limit = apply_rate_limit(self.rate_limit, partial.rate_limit);
        self.auth = self.auth.apply(partial.auth)?;
        if let Some(replication) = partial.replication {
            self.replication = Some(classify_replication(replication)?);
        }
        Ok(self)
    }
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
            if page_size.get() > peryx_replication::DEFAULT_MAX_CHANGE_PAGE_SIZE {
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

/// Turn a raw `[[index]]` table into a classified [`IndexConfig`]: `layers` makes a virtual index, else
/// `cached` makes a cached index, else `hosted`/`upload_token` makes a hosted store.
fn classify_index(raw: RawIndex) -> Result<IndexConfig, ConfigError> {
    let route = raw.route.clone().unwrap_or_else(|| raw.name.clone());
    let ecosystem = match &raw.ecosystem {
        Some(value) => value.parse().map_err(|_| ConfigError::Index {
            name: raw.name.clone(),
            reason: "unknown ecosystem",
        })?,
        None => Ecosystem::default(),
    };
    if raw.upload_token.as_deref() == Some("") {
        // An empty token authorizes any request whose Basic password is empty, so it is a
        // configuration error, not "uploads with no token" (which is `hosted = true`).
        return Err(ConfigError::Index {
            name: raw.name,
            reason: "`upload_token` must not be empty",
        });
    }
    let kind = if let Some(layers) = raw.layers {
        IndexKind::Virtual {
            layers,
            upload: raw.upload,
        }
    } else if let Some(upstream) = raw.cached {
        IndexKind::Cached {
            upstream,
            username: raw.username,
            password: secret_source(raw.password, raw.password_file).map_err(|reason| ConfigError::Index {
                name: raw.name.clone(),
                reason,
            })?,
            token: secret_source(raw.token, raw.token_file).map_err(|reason| ConfigError::Index {
                name: raw.name.clone(),
                reason,
            })?,
            upstream_concurrency: raw.upstream_concurrency.unwrap_or(DEFAULT_UPSTREAM_CONCURRENCY),
            offline: raw.offline.unwrap_or(false),
            prefetch: Box::new(raw.prefetch.unwrap_or_default().resolve()),
        }
    } else if raw.hosted == Some(true) || raw.upload_token.is_some() || raw.upload_token_file.is_some() {
        IndexKind::Hosted {
            upload_token: secret_source(raw.upload_token, raw.upload_token_file).map_err(|reason| {
                ConfigError::Index {
                    name: raw.name.clone(),
                    reason,
                }
            })?,
            volatile: raw.volatile.unwrap_or(true),
        }
    } else {
        return Err(ConfigError::Index {
            name: raw.name,
            reason: "index needs one of `cached`, `hosted`, or `layers`",
        });
    };
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
        Ok(Self {
            signing_key: signing_key.or(self.signing_key),
            token_ttl_secs,
            default_anonymous_read: partial.default_anonymous_read.unwrap_or(self.default_anonymous_read),
        })
    }
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

/// Classify an index's `[[index.access_token]]` tables, rejecting names that collide with each other
/// or with the `upload_token` shorthand, which occupies the name it would authenticate as.
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
    if raw.name == UPLOAD_TOKEN_NAME {
        return Err(fail(
            &raw.name,
            "token name is reserved for the `upload_token` shorthand",
        ));
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
        projects: if raw.projects.is_empty() {
            vec!["*".to_owned()]
        } else {
            raw.projects
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

/// Resolve the mutually exclusive `[tls]` and `[acme]` tables into one TLS mode. Manual TLS needs both
/// a certificate and a key; ACME needs at least one domain and a contact.
pub fn classify_tls(tls: Option<RawTls>, acme: Option<RawAcme>) -> Result<Option<TlsConfig>, ConfigError> {
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
