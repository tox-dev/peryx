//! Assembling the HTTP server from configuration.

use std::collections::HashMap;
use std::sync::Arc;

use anyhow::{Context as _, bail};
use axum::Router;
use velodex_http::{AppState, Index, IndexKind, router};
use velodex_storage::blob::BlobStore;
use velodex_storage::meta::MetaStore;
use velodex_upstream::{Auth, UpstreamClient};

use crate::config::{Config, IndexConfig, IndexKind as ConfigKind};

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
    let meta = MetaStore::open(config.data_dir.join("velodex.redb"))?;
    let blobs = BlobStore::new(config.data_dir.join("blobs"));
    let indexes = build_indexes(&config.indexes)?;
    Ok(Arc::new(AppState::new(meta, blobs, config.cache_ttl_secs, indexes)))
}

/// The full router over prepared state. The web UI mounts first: its routes (`/`, `/browse`,
/// `/pkg`) are all outside the index namespace, and everything else falls through to the API's
/// catch-all.
pub fn router_for(state: Arc<AppState>) -> Router {
    velodex_web::ssr::ui_router(state.clone()).merge(router(state))
}

/// Resolve configured indexes into their runtime form, mapping overlay layer names to positions and
/// building each mirror's authenticated upstream client.
pub(crate) fn build_indexes(configs: &[IndexConfig]) -> anyhow::Result<Vec<Index>> {
    let mut positions = HashMap::with_capacity(configs.len());
    let mut routes = HashMap::with_capacity(configs.len());
    for (pos, index) in configs.iter().enumerate() {
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
                kind: build_kind(index, configs, &positions)?,
            })
        })
        .collect()
}

fn build_kind(
    index: &IndexConfig,
    configs: &[IndexConfig],
    positions: &HashMap<&str, usize>,
) -> anyhow::Result<IndexKind> {
    match &index.kind {
        ConfigKind::Mirror {
            upstream,
            username,
            password,
            token,
        } => {
            let auth = mirror_auth(token.as_deref(), username.as_deref(), password.as_deref());
            Ok(IndexKind::Mirror(UpstreamClient::with_auth(upstream, auth)?))
        }
        ConfigKind::Local { upload_token, volatile } => Ok(IndexKind::Local {
            upload_token: upload_token.clone(),
            volatile: *volatile,
        }),
        ConfigKind::Overlay { layers, upload } => {
            let layer_positions = layers
                .iter()
                .map(|name| resolve_name(&index.name, name, positions))
                .collect::<anyhow::Result<Vec<_>>>()?;
            let upload_pos = resolve_upload(index, upload.as_deref(), &layer_positions, configs, positions)?;
            Ok(IndexKind::Overlay {
                layers: layer_positions,
                upload: upload_pos,
            })
        }
    }
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
            if !matches!(configs[pos].kind, ConfigKind::Local { .. }) {
                bail!("overlay {} upload target {name} is not a local index", index.name);
            }
            Ok(Some(pos))
        }
        None => Ok(layers
            .iter()
            .copied()
            .find(|&pos| matches!(configs[pos].kind, ConfigKind::Local { .. }))),
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
