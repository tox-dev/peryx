//! Overlay merging and raw-table classification: how a [`PartialConfig`] resolves onto defaults.

use std::path::PathBuf;

use velodex_format::Ecosystem;
use velodex_http::rate_limit::{DEFAULT_UPSTREAM_CONCURRENCY, RateLimitConfig, RouteLimit};

use super::ConfigError;
use super::model::{AcmeConfig, Config, IndexConfig, IndexKind, LogConfig, TlsConfig, WebhookConfig, WebhookSecret};
use super::raw::{
    PartialConfig, PartialLogConfig, PartialRateLimitConfig, PartialRouteLimit, RawAcme, RawIndex, RawTls, RawWebhook,
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
        if let Some(offline) = partial.offline {
            self.offline = offline;
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
        Ok(self)
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
    let kind = if let Some(layers) = raw.layers {
        IndexKind::Virtual {
            layers,
            upload: raw.upload,
        }
    } else if let Some(upstream) = raw.cached {
        IndexKind::Cached {
            upstream,
            username: raw.username,
            password: raw.password,
            token: raw.token,
            upstream_concurrency: raw.upstream_concurrency.unwrap_or(DEFAULT_UPSTREAM_CONCURRENCY),
            offline: raw.offline.unwrap_or(false),
            prefetch: Box::new(raw.prefetch.unwrap_or_default().resolve()),
        }
    } else if raw.hosted == Some(true) || raw.upload_token.is_some() {
        IndexKind::Hosted {
            upload_token: raw.upload_token,
            volatile: raw.volatile.unwrap_or(true),
        }
    } else {
        return Err(ConfigError::Index {
            name: raw.name,
            reason: "index needs one of `cached`, `hosted`, or `layers`",
        });
    };
    Ok(IndexConfig {
        name: raw.name,
        route,
        ecosystem,
        kind,
        policy: raw.policy.neutral,
        pypi_policy: raw.policy.pypi,
        webhooks: raw
            .webhooks
            .into_iter()
            .map(classify_webhook)
            .collect::<Result<_, _>>()?,
    })
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

const fn apply_rate_limit(mut base: RateLimitConfig, partial: PartialRateLimitConfig) -> RateLimitConfig {
    if let Some(enabled) = partial.enabled {
        base.enabled = enabled;
    }
    if let Some(max_clients) = partial.max_clients {
        base.max_clients = max_clients;
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
