//! Assembling the HTTP server from configuration.

use std::sync::Arc;

use anyhow::Context as _;
use axum::Router;
use velox_http::{AppState, StateConfig, router};
use velox_storage::blob::BlobStore;
use velox_storage::meta::MetaStore;
use velox_upstream::{Auth, UpstreamClient};

use crate::config::Config;

/// Build the velox router: open the metadata store and blob store under the data directory and wire
/// up the upstream mirror. Does not bind a socket, so it is testable in isolation.
///
/// # Errors
/// Returns an error if the data directory cannot be created, the store cannot be opened, or the
/// upstream URL is invalid.
pub fn build_router(config: &Config) -> anyhow::Result<Router> {
    std::fs::create_dir_all(&config.data_dir)
        .with_context(|| format!("create data directory {}", config.data_dir.display()))?;
    let meta = MetaStore::open(config.data_dir.join("velox.redb"))?;
    let blobs = BlobStore::new(config.data_dir.join("blobs"));
    let upstream = UpstreamClient::with_auth(&config.upstream_url, upstream_auth(config))?;
    let state = Arc::new(AppState::new(StateConfig {
        meta,
        blobs,
        upstream,
        index: config.index.clone(),
        upload_index: config.upload_index.clone(),
        upload_token: config.upload_token.clone(),
        ttl_secs: config.cache_ttl_secs,
    }));
    Ok(router(state))
}

/// Derive the upstream authentication from config: a bearer token takes precedence over a
/// username/password pair; otherwise the mirror is anonymous.
pub(crate) fn upstream_auth(config: &Config) -> Auth {
    match (
        &config.upstream_token,
        &config.upstream_username,
        &config.upstream_password,
    ) {
        (Some(token), _, _) => Auth::Bearer(token.clone()),
        (None, Some(username), Some(password)) => Auth::Basic {
            username: username.clone(),
            password: password.clone(),
        },
        _ => Auth::None,
    }
}
