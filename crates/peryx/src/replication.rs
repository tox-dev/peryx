//! Process-level replication configuration and follower scheduling.

use std::sync::Arc;
use std::time::Duration;
use std::{fmt::Write as _, sync::Mutex};

use anyhow::Context as _;
use axum::extract::State;
use axum::http::StatusCode;
use axum::response::{IntoResponse as _, Response};
use axum::routing::get;
use axum::{Json, Router};
use peryx_driver::{AppState, PrometheusSource};
use peryx_replication::{HttpPrimary, Replica, SyncOutcome, primary_router};
use peryx_storage::blob::BlobStore;
use peryx_storage::meta::MetaStore;

use crate::config::{Config, ReplicationConfig};

#[derive(Clone, Copy, serde::Serialize)]
#[serde(rename_all = "snake_case")]
enum ReplicaHealthStatus {
    Starting,
    CatchingUp,
    CaughtUp,
    Error,
}

#[derive(Clone, Copy)]
struct ReplicaObservation {
    status: ReplicaHealthStatus,
    serial: u64,
    primary_serial: Option<u64>,
    changes: u64,
    blobs: u64,
    errors: u64,
}

#[derive(serde::Serialize)]
struct ReplicaHealth {
    status: ReplicaHealthStatus,
    serial: u64,
    primary_serial: Option<u64>,
    lag: Option<u64>,
}

struct ReplicaMonitor {
    observation: Mutex<ReplicaObservation>,
}

impl ReplicaMonitor {
    const fn new(serial: u64) -> Self {
        Self {
            observation: Mutex::new(ReplicaObservation {
                status: ReplicaHealthStatus::Starting,
                serial,
                primary_serial: None,
                changes: 0,
                blobs: 0,
                errors: 0,
            }),
        }
    }

    fn record(&self, outcome: SyncOutcome) {
        let mut observation = self
            .observation
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        observation.status = if outcome.caught_up() {
            ReplicaHealthStatus::CaughtUp
        } else {
            ReplicaHealthStatus::CatchingUp
        };
        observation.serial = outcome.serial;
        observation.primary_serial = Some(outcome.primary_serial);
        observation.changes = observation
            .changes
            .saturating_add(u64::try_from(outcome.changes).unwrap_or(u64::MAX));
        observation.blobs = observation
            .blobs
            .saturating_add(u64::try_from(outcome.blobs).unwrap_or(u64::MAX));
    }

    fn record_error(&self) {
        let mut observation = self
            .observation
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        observation.status = ReplicaHealthStatus::Error;
        observation.errors = observation.errors.saturating_add(1);
    }

    fn health(&self) -> ReplicaHealth {
        let observation = *self
            .observation
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        ReplicaHealth {
            status: observation.status,
            serial: observation.serial,
            primary_serial: observation.primary_serial,
            lag: observation
                .primary_serial
                .map(|primary_serial| primary_serial.saturating_sub(observation.serial)),
        }
    }
}

impl PrometheusSource for ReplicaMonitor {
    fn write_metrics(&self, body: &mut String) {
        let observation = *self
            .observation
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let caught_up = u8::from(matches!(observation.status, ReplicaHealthStatus::CaughtUp));
        let _ = write!(
            body,
            "# HELP peryx_replication_caught_up Whether the replica has reached the latest observed primary serial.\n\
             # TYPE peryx_replication_caught_up gauge\n\
             peryx_replication_caught_up {caught_up}\n\
             # HELP peryx_replication_serial Last serial committed by the replica.\n\
             # TYPE peryx_replication_serial gauge\n\
             peryx_replication_serial {}\n\
             # HELP peryx_replication_changes_total Metadata changes committed by the replica.\n\
             # TYPE peryx_replication_changes_total counter\n\
             peryx_replication_changes_total {}\n\
             # HELP peryx_replication_blobs_total Blobs fetched by the replica.\n\
             # TYPE peryx_replication_blobs_total counter\n\
             peryx_replication_blobs_total {}\n\
             # HELP peryx_replication_sync_errors_total Replica synchronization failures.\n\
             # TYPE peryx_replication_sync_errors_total counter\n\
             peryx_replication_sync_errors_total {}\n",
            observation.serial, observation.changes, observation.blobs, observation.errors
        );
        if let Some(primary_serial) = observation.primary_serial {
            let _ = write!(
                body,
                "# HELP peryx_replication_primary_serial Latest serial reported by the primary.\n\
                 # TYPE peryx_replication_primary_serial gauge\n\
                 peryx_replication_primary_serial {primary_serial}\n\
                 # HELP peryx_replication_lag Serial distance between the primary and replica.\n\
                 # TYPE peryx_replication_lag gauge\n\
                 peryx_replication_lag {}\n",
                primary_serial.saturating_sub(observation.serial)
            );
        }
    }
}

async fn replica_health(State(monitor): State<Arc<ReplicaMonitor>>) -> Response {
    let health = monitor.health();
    let status = if matches!(health.status, ReplicaHealthStatus::CaughtUp) {
        StatusCode::OK
    } else {
        StatusCode::SERVICE_UNAVAILABLE
    };
    (status, Json(health)).into_response()
}

struct ReplicaLoop {
    primary: HttpPrimary,
    meta: MetaStore,
    blobs: BlobStore,
    page_size: std::num::NonZeroUsize,
    poll_interval: Duration,
    monitor: Arc<ReplicaMonitor>,
}

fn log_replica_page(outcome: SyncOutcome) {
    tracing::info!(
        changes = outcome.changes,
        blobs = outcome.blobs,
        serial = outcome.serial,
        primary_serial = outcome.primary_serial,
        "replica page applied"
    );
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
                    log_replica_page(outcome);
                }
                self.monitor.record(outcome);
                outcome.caught_up()
            }
            Err(error) => {
                self.monitor.record_error();
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
                let monitor = Arc::new(ReplicaMonitor::new(
                    state.meta.current_serial().context("read the replica serial")?,
                ));
                state.register_prometheus(monitor.clone());
                (
                    None,
                    Some(ReplicaLoop {
                        primary,
                        meta: state.serving.meta.clone(),
                        blobs: state.serving.blobs.clone(),
                        page_size: *page_size,
                        poll_interval: *poll_interval,
                        monitor,
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
        let router = match &self.primary {
            Some(primary) => router.merge(primary.clone()),
            None => router,
        };
        match &self.replica {
            Some(replica) => router.merge(
                Router::new()
                    .route("/+replication/v1/health", get(replica_health))
                    .with_state(replica.monitor.clone()),
            ),
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
