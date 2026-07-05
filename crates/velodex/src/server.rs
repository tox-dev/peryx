//! Assembling the HTTP server from configuration.

use std::collections::HashMap;
use std::sync::Arc;

use anyhow::{Context as _, bail};
use axum::Router;
use velodex_http::webhook::{WebhookRuntime, WebhookTargetConfig};
use velodex_http::{AppState, Index, IndexKind, RuntimeOptions, path_safety, router, webhook};
use velodex_policy::Policy;
use velodex_storage::blob::BlobStore;
use velodex_storage::meta::MetaStore;
use velodex_upstream::{Auth, UpstreamClient, redact_url};

use crate::config::{Config, IndexConfig, IndexKind as ConfigKind, WebhookSecret};

/// Build the velodex router from configuration.
///
/// Opens the stores under the data directory and resolves the configured indexes (mirrors, local
/// stores, and overlays) into their runtime form. Does not bind a socket, so it is testable in
/// isolation.
///
/// # Errors
/// Returns an error if the data directory or stores cannot be opened, an upstream URL is invalid, or
/// an overlay references an unknown or non-local index.
pub fn build_router(config: &Config) -> anyhow::Result<Router> {
    Ok(router_for(build_state(config)?))
}

/// Open the stores and resolve the configured indexes into the shared application state, without
/// building routes, so the serve entrypoint can reach the upstream clients before traffic.
///
/// # Errors
/// Returns an error if the data directory or stores cannot be opened, an upstream URL is invalid,
/// or an overlay references an unknown or non-local index.
pub fn build_state(config: &Config) -> anyhow::Result<Arc<AppState>> {
    std::fs::create_dir_all(&config.data_dir)
        .with_context(|| format!("create data directory {}", config.data_dir.display()))?;
    let meta_path = config.data_dir.join("velodex.redb");
    let meta = MetaStore::open(&meta_path).with_context(|| format!("open metadata store {}", meta_path.display()))?;
    let blobs = BlobStore::new(config.data_dir.join("blobs"));
    let indexes = build_indexes(&config.indexes, config.offline)?;
    let webhooks = build_webhooks(&config.indexes)?;
    let search_path = config.data_dir.join("search-v1");
    let mut state = AppState::with_search_path_and_runtime(
        meta,
        blobs,
        config.cache_ttl_secs,
        indexes,
        &search_path,
        RuntimeOptions {
            rate_limit: config.rate_limit.clone(),
            upstream_concurrency: upstream_concurrency(&config.indexes),
            webhooks,
        },
    )
    .context(format!("open search index {}", search_path.display()))?;
    velodex_ecosystem_pypi::install(&mut state);
    let state = Arc::new(state);
    if !state.webhooks.is_empty() {
        webhook::kick(state.clone());
    }
    Ok(state)
}

/// The full router over prepared state. The web UI mounts first: its routes (`/`, `/browse`,
/// `/pkg`) are all outside the index namespace, and everything else falls through to the API's
/// catch-all.
pub fn router_for(state: Arc<AppState>) -> Router {
    velodex_web::ssr::ui_router(state.clone()).merge(router(state))
}

/// Resolve configured indexes into their runtime form, mapping overlay layer names to positions and
/// building each mirror's authenticated upstream client.
pub(crate) fn build_indexes(configs: &[IndexConfig], offline: bool) -> anyhow::Result<Vec<Index>> {
    let mut positions = HashMap::with_capacity(configs.len());
    let mut routes = HashMap::with_capacity(configs.len());
    for (pos, index) in configs.iter().enumerate() {
        path_safety::validate_route(&index.route).with_context(|| format!("invalid index route {}", index.route))?;
        if positions.insert(index.name.as_str(), pos).is_some() {
            bail!("duplicate index name {}", index.name);
        }
        if routes.insert(index.route.as_str(), pos).is_some() {
            bail!("duplicate index route {}", index.route);
        }
    }
    configs
        .iter()
        .map(|index| {
            Ok(Index {
                name: index.name.clone(),
                route: index.route.clone(),
                ecosystem: index.ecosystem,
                kind: build_kind(index, configs, &positions, offline)?,
                policy: Policy::compile(&index.policy).with_context(|| format!("compile policy for {}", index.name))?,
            })
        })
        .collect()
}

fn build_webhooks(configs: &[IndexConfig]) -> anyhow::Result<WebhookRuntime> {
    let mut targets = Vec::new();
    for index in configs {
        for webhook in &index.webhooks {
            targets.push(WebhookTargetConfig {
                index: index.name.clone(),
                name: webhook.name.clone(),
                url: webhook.url.clone(),
                secret: webhook_secret(&webhook.secret, &webhook.name)?,
                events: webhook.events.clone(),
            });
        }
    }
    WebhookRuntime::new(targets).context("build webhook targets")
}

fn webhook_secret(secret: &WebhookSecret, name: &str) -> anyhow::Result<String> {
    match secret {
        WebhookSecret::Literal(secret) => Ok(secret.clone()),
        WebhookSecret::Env(var) => {
            std::env::var(var).with_context(|| format!("read webhook secret env var {var} for target {name}"))
        }
    }
}

fn build_kind(
    index: &IndexConfig,
    configs: &[IndexConfig],
    positions: &HashMap<&str, usize>,
    global_offline: bool,
) -> anyhow::Result<IndexKind> {
    match &index.kind {
        ConfigKind::Cached {
            upstream,
            username,
            password,
            token,
            offline,
            ..
        } => {
            let auth = mirror_auth(token.as_deref(), username.as_deref(), password.as_deref());
            Ok(IndexKind::Cached {
                client: UpstreamClient::with_auth(upstream, auth).with_context(|| {
                    format!(
                        "build mirror index {} with upstream {}",
                        index.name,
                        redact_url(upstream)
                    )
                })?,
                offline: global_offline || *offline,
            })
        }
        ConfigKind::Hosted { upload_token, volatile } => Ok(IndexKind::Hosted {
            upload_token: upload_token.clone(),
            volatile: *volatile,
        }),
        ConfigKind::Virtual { layers, upload } => {
            let layer_positions = layers
                .iter()
                .map(|name| resolve_name(&index.name, name, positions))
                .collect::<anyhow::Result<Vec<_>>>()?;
            let upload_pos = resolve_upload(index, upload.as_deref(), &layer_positions, configs, positions)?;
            Ok(IndexKind::Virtual {
                layers: layer_positions,
                upload: upload_pos,
            })
        }
    }
}

fn upstream_concurrency(configs: &[IndexConfig]) -> Vec<(String, usize)> {
    configs
        .iter()
        .filter_map(|index| match &index.kind {
            ConfigKind::Cached {
                upstream_concurrency, ..
            } => Some((index.name.clone(), *upstream_concurrency)),
            ConfigKind::Hosted { .. } | ConfigKind::Virtual { .. } => None,
        })
        .collect()
}

fn resolve_name(overlay: &str, name: &str, positions: &HashMap<&str, usize>) -> anyhow::Result<usize> {
    positions
        .get(name)
        .copied()
        .with_context(|| format!("overlay {overlay} references unknown index {name}"))
}

/// The overlay's upload target: the named local index, or (when unset) the first local layer.
fn resolve_upload(
    index: &IndexConfig,
    upload: Option<&str>,
    layers: &[usize],
    configs: &[IndexConfig],
    positions: &HashMap<&str, usize>,
) -> anyhow::Result<Option<usize>> {
    match upload {
        Some(name) => {
            let pos = resolve_name(&index.name, name, positions)?;
            if !matches!(configs[pos].kind, ConfigKind::Hosted { .. }) {
                bail!(
                    "virtual index {} upload target {name} is not a hosted index",
                    index.name
                );
            }
            Ok(Some(pos))
        }
        None => Ok(layers
            .iter()
            .copied()
            .find(|&pos| matches!(configs[pos].kind, ConfigKind::Hosted { .. }))),
    }
}

/// Derive upstream authentication: a bearer token takes precedence over a username/password pair;
/// otherwise the mirror is anonymous.
pub(crate) fn mirror_auth(token: Option<&str>, username: Option<&str>, password: Option<&str>) -> Auth {
    match (token, username, password) {
        (Some(token), _, _) => Auth::Bearer(token.to_owned()),
        (None, Some(username), Some(password)) => Auth::Basic {
            username: username.to_owned(),
            password: password.to_owned(),
        },
        _ => Auth::None,
    }
}
