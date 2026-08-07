//! Runtime wiring for analytics replication: the producer endpoint a replica pulls, and the receiver
//! worker that folds pulled batches into a durable apply state.
//!
//! The data layer lives in `peryx-ha-distributed`/`peryx-events` (the [`AnalyticsReceiver`] fold, the
//! `export_sealed_day_batches` producer, the pull transport). This is only the runtime glue: a producer
//! serves its sealed daily batches on `+replication/v1/analytics`, and a replica runs a background worker
//! that pulls from its upstream, applies each batch once, and persists the converged state off the
//! request path.

use std::sync::Arc;
use std::time::Duration;

use anyhow::Context as _;
use axum::extract::{Query, State};
use axum::http::{HeaderMap, StatusCode, header};
use axum::response::{IntoResponse, Response};
use axum::routing::get;
use axum::{Json, Router};
use peryx_events::metrics::Metrics;
use peryx_ha_distributed::{
    AnalyticsReceiver, AuthorityEpoch, DEFAULT_APPLY_LIMITS, HttpAnalyticsSource, ProducerId, TransferLimits, pull,
};
use peryx_storage::meta::AnalyticsHandle;
use serde::{Deserialize, Serialize};

/// How long a single analytics pull may run before it is a retryable loss.
const ANALYTICS_FETCH_TIMEOUT: Duration = Duration::from_secs(30);

/// The producing node's durable analytics generation, assigned once and reused across restarts so a
/// re-served sealed day keeps the same interval identity and a replica recognizes the replay.
#[derive(Debug, Serialize, Deserialize)]
struct ProducerRecord {
    epoch: u64,
}

/// Resolve this producer's durable analytics epoch, assigning generation one on first start.
///
/// # Errors
/// Returns a store error when the record cannot be read or written.
pub fn resolve_producer_epoch(analytics: &AnalyticsHandle) -> anyhow::Result<AuthorityEpoch> {
    if let Some(bytes) = analytics.load_producer().context("read analytics producer record")?
        && let Ok(record) = serde_json::from_slice::<ProducerRecord>(&bytes)
    {
        return Ok(AuthorityEpoch(record.epoch));
    }
    let record = ProducerRecord { epoch: 1 };
    let bytes = serde_json::to_vec(&record).expect("a producer record always serializes");
    analytics
        .save_producer(&bytes)
        .context("persist analytics producer record")?;
    Ok(AuthorityEpoch(record.epoch))
}

#[derive(Clone)]
struct EndpointState {
    token: Arc<str>,
    metrics: Metrics,
    producer: ProducerId,
    epoch: AuthorityEpoch,
}

#[derive(Deserialize)]
struct AfterQuery {
    after: i64,
}

fn authorized(headers: &HeaderMap, token: &str) -> bool {
    headers
        .get(header::AUTHORIZATION)
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.strip_prefix("Bearer "))
        .is_some_and(|presented| presented == token)
}

async fn serve_analytics(
    State(state): State<EndpointState>,
    headers: HeaderMap,
    Query(query): Query<AfterQuery>,
) -> Response {
    if !authorized(&headers, &state.token) {
        return StatusCode::UNAUTHORIZED.into_response();
    }
    let batches = state
        .metrics
        .export_sealed_day_batches(&state.producer, state.epoch, query.after);
    Json(batches).into_response()
}

/// The route that serves this producer's sealed analytics batches after a requested day, bearer-gated by
/// the replication token.
pub fn analytics_router(
    token: impl Into<String>,
    metrics: Metrics,
    producer: ProducerId,
    epoch: AuthorityEpoch,
) -> Router {
    let state = EndpointState {
        token: Arc::from(token.into()),
        metrics,
        producer,
        epoch,
    };
    Router::new()
        .route("/+replication/v1/analytics", get(serve_analytics))
        .with_state(state)
}

/// Durably record the receiver's converged apply state. The one seam a test swaps to drive the persist
/// failure arm, since the redb store cannot be faulted from outside its own crate.
type PersistApply = Box<dyn Fn(&[u8]) -> anyhow::Result<()> + Send + Sync>;

/// A replica's background analytics worker: it pulls its upstream's sealed batches, folds each into the
/// durable [`AnalyticsReceiver`], and persists the converged state after every pull that applied.
pub struct AnalyticsPuller {
    source: HttpAnalyticsSource,
    persist: PersistApply,
    receiver: AnalyticsReceiver,
    poll_interval: Duration,
}

impl AnalyticsPuller {
    /// Build a puller against `upstream`, restoring the durable receiver or starting empty.
    ///
    /// # Errors
    /// Returns an error when the upstream URL is unusable or the persisted receiver cannot be restored.
    pub fn new(
        upstream: &str,
        token: impl Into<String>,
        analytics: AnalyticsHandle,
        poll_interval: Duration,
    ) -> anyhow::Result<Self> {
        let source = HttpAnalyticsSource::new(upstream, token, TransferLimits::default(), ANALYTICS_FETCH_TIMEOUT)
            .context("build analytics pull transport")?;
        let receiver = match analytics.load_apply().context("read analytics apply state")? {
            Some(bytes) => {
                AnalyticsReceiver::restore(&bytes, DEFAULT_APPLY_LIMITS).context("restore analytics apply state")?
            }
            None => AnalyticsReceiver::new(DEFAULT_APPLY_LIMITS),
        };
        let persist: PersistApply =
            Box::new(move |snapshot| analytics.save_apply(snapshot).map_err(anyhow::Error::from));
        Ok(Self {
            source,
            persist,
            receiver,
            poll_interval,
        })
    }

    /// Pull once and fold the result: persist the converged state when a batch applied, drop an
    /// already-accepted pull, and log a transport loss or an apply-bound breach without stopping. The
    /// single iteration [`run`](Self::run) repeats, split out so each arm is driven on its own.
    async fn pull_once(&mut self) {
        match pull(&self.source, &mut self.receiver).await {
            Ok(report) if report.applied > 0 => {
                if let Err(error) = (self.persist)(&self.receiver.encode()) {
                    tracing::error!(%error, "persist analytics apply state failed");
                }
            }
            Ok(_) => {}
            Err(error) => tracing::warn!(%error, "analytics pull failed"),
        }
    }

    /// Run the pull loop forever: pull, persist any progress, then wait the poll interval. A transport
    /// loss or an apply-bound breach is logged and retried at the next tick rather than stopping the
    /// worker, so a transient outage never kills the analytics plane.
    pub async fn run(mut self) {
        loop {
            self.pull_once().await;
            tokio::time::sleep(self.poll_interval).await;
        }
    }
}

#[cfg(test)]
mod tests;
