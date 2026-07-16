//! Process-level replication configuration and follower scheduling.

use std::sync::Arc;
use std::time::Duration;

use anyhow::Context as _;
use axum::Router;
use peryx_driver::AppState;
use peryx_replication::{HttpPrimary, Replica, primary_router};
use peryx_storage::blob::BlobStore;
use peryx_storage::meta::MetaStore;

use crate::config::{Config, ReplicationConfig};

struct ReplicaLoop {
    primary: HttpPrimary,
    meta: MetaStore,
    blobs: BlobStore,
    page_size: std::num::NonZeroUsize,
    poll_interval: Duration,
}

impl ReplicaLoop {
    async fn run(self) {
        loop {
            if self.cycle().await {
                tokio::time::sleep(self.poll_interval).await;
            }
        }
    }

    async fn cycle(&self) -> bool {
        match Replica::new(&self.meta, &self.blobs, self.page_size)
            .sync_once(&self.primary)
            .await
        {
            Ok(outcome) => {
                if outcome.changes > 0 {
                    tracing::info!(
                        changes = outcome.changes,
                        blobs = outcome.blobs,
                        serial = outcome.serial,
                        primary_serial = outcome.primary_serial,
                        "replica page applied"
                    );
                }
                outcome.caught_up()
            }
            Err(error) => {
                tracing::error!(%error, "replica synchronization failed");
                true
            }
        }
    }
}

/// Replication routes and follower work prepared from one resolved configuration.
pub struct ReplicationRuntime {
    primary: Option<Router>,
    replica: Option<ReplicaLoop>,
}

impl ReplicationRuntime {
    /// Prepare the configured replication role without starting background work.
    ///
    /// # Errors
    /// Returns an error if a secret cannot be read, the upstream URL is invalid, or the primary
    /// router rejects its identity or token.
    pub fn new(config: &Config, state: &Arc<AppState>) -> anyhow::Result<Self> {
        let (primary, replica) = match &config.replication {
            None => (None, None),
            Some(ReplicationConfig::Primary { source, token }) => {
                let token = token.read().context("read the primary replication token")?;
                let router = primary_router(
                    source.clone(),
                    token,
                    state.serving.meta.clone(),
                    state.serving.blobs.clone(),
                )
                .context("build primary replication routes")?;
                (Some(router), None)
            }
            Some(ReplicationConfig::Replica {
                upstream,
                token,
                poll_interval,
                page_size,
            }) => {
                let token = token.read().context("read the replica replication token")?;
                let primary = HttpPrimary::new(upstream, token).context("build replica HTTP client")?;
                (
                    None,
                    Some(ReplicaLoop {
                        primary,
                        meta: state.serving.meta.clone(),
                        blobs: state.serving.blobs.clone(),
                        page_size: *page_size,
                        poll_interval: *poll_interval,
                    }),
                )
            }
        };
        Ok(Self { primary, replica })
    }

    /// Whether this process follows a primary and must avoid local writers.
    #[must_use]
    pub const fn is_replica(&self) -> bool {
        self.replica.is_some()
    }

    /// Mount primary routes, when configured, on the process router.
    pub fn mount(&self, router: Router) -> Router {
        match &self.primary {
            Some(primary) => router.merge(primary.clone()),
            None => router,
        }
    }

    /// Start the replica loop, when configured.
    #[must_use]
    pub fn start(self) -> Option<tokio::task::JoinHandle<()>> {
        self.replica.map(|replica| tokio::spawn(replica.run()))
    }

    #[cfg(test)]
    pub(crate) async fn sync_cycle(&self) -> Option<bool> {
        match &self.replica {
            Some(replica) => Some(replica.cycle().await),
            None => None,
        }
    }
}
