//! Assembling the HTTP server from configuration.

use std::collections::HashMap;
use std::sync::{Arc, OnceLock};

use anyhow::{Context as _, bail};
use axum::Router;
use peryx_core::{Ecosystem, path};
use peryx_driver::state::RuntimeOptions;
use peryx_driver::{AppState, DriverSet, Index, IndexKind};
use peryx_ecosystem_oci::IndexSettings;
use peryx_events::webhook::{WebhookRuntime, WebhookTargetConfig};
use peryx_http::router;
use peryx_identity::Signer;
use peryx_policy::Policy;
use peryx_storage::blob::BlobStore;
use peryx_storage::meta::MetaStore;
use peryx_upstream::{Auth, UpstreamClient, redact_url};

use crate::config::{AuthConfig, Config, IndexConfig, IndexKind as ConfigKind, WebhookSecret};

/// Build the peryx router from configuration.
///
/// Opens the stores under the data directory and resolves the configured indexes (cached indexes, hosted
/// stores, and virtual indexes) into their runtime form. Does not bind a socket, so it is testable in
/// isolation.
///
/// # Errors
/// Returns an error if the data directory or stores cannot be opened, an upstream URL is invalid, or
/// a virtual index references an unknown or non-hosted index.
pub fn build_router(config: &Config) -> anyhow::Result<Router> {
    Ok(router_for(build_state(config)?))
}

/// Open the stores and resolve the configured indexes into the shared application state, without
/// building routes, so the serve entrypoint can reach the upstream clients before traffic.
///
/// # Errors
/// Returns an error if the data directory or stores cannot be opened, an upstream URL is invalid,
/// or a virtual index references an unknown or non-hosted index.
pub fn build_state(config: &Config) -> anyhow::Result<Arc<AppState>> {
    std::fs::create_dir_all(&config.data_dir)
        .with_context(|| format!("create data directory {}", config.data_dir.display()))?;
    let meta_path = config.data_dir.join("peryx.redb");
    let meta = MetaStore::open(&meta_path).with_context(|| format!("open metadata store {}", meta_path.display()))?;
    let blobs = BlobStore::new(config.data_dir.join("blobs"));
    let indexes = build_indexes(&config.indexes, &config.auth, config.offline)?;
    let oci_settings = build_index_settings(&config.indexes)?;
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
            hot_cache_bytes: config.hot_cache_bytes,
            max_stale_secs: config.max_stale_secs,
        },
    )
    .context(format!("open search index {}", search_path.display()))?;
    peryx_ecosystem_pypi::install(&mut state);
    peryx_ecosystem_oci::install(&mut state, oci_settings);
    if let Some(source) = &config.auth.signing_key {
        let key = source.read().context("read the token realm signing key")?;
        if key.trim().is_empty() {
            bail!("token realm signing key must not be empty");
        }
        state.set_token_realm(
            Signer::new(key.as_bytes(), peryx_ecosystem_oci::TOKEN_SERVICE),
            config.auth.token_ttl_secs,
        );
    }
    state.set_openapi(crate::api::openapi_json());
    let state = Arc::new(state);
    if !state.webhooks.is_empty() {
        peryx_events::webhook::kick(state.serving.clone());
    }
    Ok(state)
}

/// The full router over prepared state. The web UI mounts first: its routes (`/`, `/browse`,
/// `/pkg`) are all outside the index namespace, and everything else falls through to the API's
/// catch-all.
pub fn router_for(state: Arc<AppState>) -> Router {
    peryx_web::ssr::ui_router(state.clone()).merge(router(state))
}

/// The ecosystem drivers this build of peryx ships, named once here at the composition root. The
/// config-build and admin paths dispatch through it by an index's ecosystem, so no neutral code
/// names an ecosystem.
pub(crate) fn drivers() -> &'static DriverSet {
    static DRIVERS: OnceLock<DriverSet> = OnceLock::new();
    DRIVERS.get_or_init(|| {
        DriverSet::default()
            .with(Arc::new(peryx_ecosystem_pypi::PypiServing))
            .with(Arc::new(peryx_ecosystem_oci::OciRegistry::default()))
    })
}

/// Resolve configured indexes into their runtime form, mapping virtual-index member names to positions,
/// building each cached index's authenticated upstream client, and reading each index's access rules
/// (which is where a secret kept in a file is read).
pub(crate) fn build_indexes(configs: &[IndexConfig], auth: &AuthConfig, offline: bool) -> anyhow::Result<Vec<Index>> {
    let mut positions = HashMap::with_capacity(configs.len());
    let mut routes = HashMap::with_capacity(configs.len());
    for (pos, index) in configs.iter().enumerate() {
        path::validate_route(&index.route).with_context(|| format!("invalid index route {}", index.route))?;
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
            let driver = drivers()
                .get(index.ecosystem)
                .expect("every configured ecosystem has a registered driver");
            let rules = driver
                .compile_policy(&index.ecosystem_policy)
                .map_err(|reason| anyhow::anyhow!("compile policy for {}: {reason}", index.name))?;
            Ok(Index {
                name: index.name.clone(),
                route: index.route.clone(),
                ecosystem: index.ecosystem,
                kind: build_kind(index, configs, &positions, offline)?,
                policy: Policy::compile(&index.policy, |name| driver.normalize_name(name)).with_rules(rules),
                acl: index
                    .acl(auth)
                    .with_context(|| format!("read the access rules of index {}", index.name))?,
            })
        })
        .collect()
}

/// Compile each index's `[index.settings]` table against the ecosystem it serves, keyed by index name.
///
/// The settings vocabulary is a format's own — an OCI cache's `library_prefix` means nothing to a
/// `PyPI` index — so the table travels raw through the neutral config and is compiled here, in the one
/// crate that names ecosystems. An ecosystem with no settings of its own claims no key, so a key on
/// one of its indexes is configuration that would otherwise be silently ignored.
pub(crate) fn build_index_settings(configs: &[IndexConfig]) -> anyhow::Result<HashMap<String, IndexSettings>> {
    let mut settings = HashMap::new();
    for index in configs {
        match index.ecosystem {
            Ecosystem::Oci => {
                let compiled = IndexSettings::compile(&index.ecosystem_settings)
                    .map_err(|reason| anyhow::anyhow!("compile settings for {}: {reason}", index.name))?;
                settings.insert(index.name.clone(), compiled);
            }
            Ecosystem::Pypi => {
                if let Some(key) = index.ecosystem_settings.keys().next() {
                    bail!(
                        "compile settings for {}: unknown field `{key}` in `[index.settings]`",
                        index.name
                    );
                }
            }
        }
    }
    Ok(settings)
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
            let auth = upstream_auth(token.as_deref(), username.as_deref(), password.as_deref());
            Ok(IndexKind::Cached {
                client: UpstreamClient::with_auth(upstream, auth).with_context(|| {
                    format!(
                        "build cached index {} with upstream {}",
                        index.name,
                        redact_url(upstream)
                    )
                })?,
                offline: global_offline || *offline,
            })
        }
        ConfigKind::Hosted { volatile, .. } => Ok(IndexKind::Hosted { volatile: *volatile }),
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

fn resolve_name(virtual_route: &str, name: &str, positions: &HashMap<&str, usize>) -> anyhow::Result<usize> {
    positions
        .get(name)
        .copied()
        .with_context(|| format!("virtual index {virtual_route} references unknown index {name}"))
}

/// The virtual index's upload target: the named hosted index, or (when unset) the first hosted layer.
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
/// otherwise the upstream is anonymous.
pub(crate) fn upstream_auth(token: Option<&str>, username: Option<&str>, password: Option<&str>) -> Auth {
    match (token, username, password) {
        (Some(token), _, _) => Auth::Bearer(token.to_owned()),
        (None, Some(username), Some(password)) => Auth::Basic {
            username: username.to_owned(),
            password: password.to_owned(),
        },
        _ => Auth::None,
    }
}
